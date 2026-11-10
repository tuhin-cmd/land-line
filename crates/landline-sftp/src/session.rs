use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use russh::client;
use russh::keys::PrivateKeyWithHashAlg;
use russh_sftp::client::SftpSession;
use russh_sftp::client::error::Error as ClientError;
use russh_sftp::protocol::StatusCode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::debug;

use crate::{Endpoint, HostKey, SftpError, Timeouts, algorithms, join};

/// Bytes moved per local read or write. Big enough that a gigabyte is a few
/// thousand iterations; russh-sftp splits it into pipelined requests sized
/// to what the server accepts.
const CHUNK: usize = 256 * 1024;

/// The progress callback runs at most this often, plus once at the start
/// and once at the end.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// How long a request that timed out waits for an SSH ping before the
/// session is declared dead.
const PING_TIMEOUT: Duration = Duration::from_secs(3);

/// A file or folder in a listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The name within its folder.
    pub name: String,
    /// The full SFTP path, the listed folder joined with `name`.
    pub path: String,
    /// Whether it is a folder, following symbolic links.
    pub is_dir: bool,
    /// Size in bytes, 0 when the server did not say.
    pub size: u64,
    /// Last modification, in seconds since the Unix epoch.
    pub modified_secs: Option<i64>,
}

/// One logged-in SFTP session with the phone.
///
/// Methods take `&self` and may run concurrently; the session pipelines
/// their requests over one SSH channel.
pub struct Session {
    ssh: client::Handle<Client>,
    sftp: SftpSession,
    dead: AtomicBool,
}

impl Session {
    /// Connects and logs in with the default [`Timeouts`].
    pub async fn connect(endpoint: &Endpoint) -> Result<Session, SftpError> {
        Self::connect_with(endpoint, Timeouts::default()).await
    }

    /// Connects and logs in, giving up after `timeouts.connect`.
    pub async fn connect_with(
        endpoint: &Endpoint,
        timeouts: Timeouts,
    ) -> Result<Session, SftpError> {
        debug!(address = %endpoint.address, user = %endpoint.user, "sftp connecting");
        let session = tokio::time::timeout(timeouts.connect, establish(endpoint, timeouts))
            .await
            .map_err(|_| SftpError::Timeout)??;
        debug!(address = %endpoint.address, "sftp session ready");
        Ok(session)
    }

    /// Lists a folder: folders first, then files, each sorted by name
    /// ignoring case. `.` and `..` are left out; other names starting with a
    /// dot are kept, for the caller to show or hide.
    pub async fn list_dir(&self, path: &str) -> Result<Vec<Entry>, SftpError> {
        self.ensure_open()?;
        let listing = match self.sftp.read_dir(path).await {
            Ok(listing) => listing,
            Err(e) => return Err(self.client_error(e, path).await),
        };
        let mut entries = Vec::new();
        for item in listing {
            let name = item.file_name();
            if name.is_empty() || name == "." || name == ".." {
                continue;
            }
            let entry_path = join(path, &name);
            let mut attrs = item.metadata();
            // Android exposes `/sdcard` and friends as links to folders, and
            // a listing describes the link itself.
            if attrs.is_symlink()
                && let Ok(target) = self.sftp.metadata(entry_path.as_str()).await
            {
                attrs = target;
            }
            entries.push(Entry {
                name,
                path: entry_path,
                is_dir: attrs.is_dir(),
                size: attrs.size.unwrap_or(0),
                modified_secs: attrs.mtime.map(i64::from),
            });
        }
        entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(entries)
    }

    /// Copies a file from the phone to `local`, replacing whatever is there.
    ///
    /// The data goes to a hidden temporary file in the same folder, renamed
    /// over `local` only once it is complete and, when the phone reported a
    /// size, the sizes match. On failure the temporary file is removed, so
    /// `local` is either untouched or complete. `progress` receives bytes
    /// done and the total if known, at most a few times a second, and always
    /// once with the final count.
    pub async fn download(
        &self,
        remote: &str,
        local: &Path,
        mut progress: impl FnMut(u64, Option<u64>) + Send,
    ) -> Result<(), SftpError> {
        self.ensure_open()?;
        debug!(remote, local = %local.display(), "sftp download");
        let mut source = match self.sftp.open(remote).await {
            Ok(file) => file,
            Err(e) => return Err(self.client_error(e, remote).await),
        };
        let expected = match source.metadata().await {
            Ok(attrs) if attrs.is_dir() => {
                return Err(SftpError::Other(format!("{remote} is a folder")));
            }
            Ok(attrs) => attrs.size,
            Err(e) => return Err(self.client_error(e, remote).await),
        };

        let temp = TempFile::next_to(local)?;
        let mut sink = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp.path)
            .await
            .map_err(SftpError::local(&temp.path))?;

        let mut throttle = Throttle::new();
        let mut done = 0u64;
        let mut buf = vec![0; CHUNK];
        progress(0, expected);
        loop {
            let n = match source.read(&mut buf).await {
                Ok(n) => n,
                Err(e) => return Err(self.io_error(e, remote).await),
            };
            if n == 0 {
                break;
            }
            sink.write_all(&buf[..n])
                .await
                .map_err(SftpError::local(&temp.path))?;
            done += n as u64;
            if throttle.due() {
                progress(done, expected);
            }
        }
        sink.sync_all()
            .await
            .map_err(SftpError::local(&temp.path))?;
        drop(sink);
        // A read handle has nothing left to lose; a failed close is the
        // server's problem.
        let _ = source.close().await;

        if let Some(expected) = expected
            && expected != done
        {
            return Err(SftpError::Incomplete {
                expected,
                actual: done,
            });
        }
        tokio::fs::rename(&temp.path, local)
            .await
            .map_err(SftpError::local(local))?;
        temp.keep();
        progress(done, expected.or(Some(done)));
        debug!(remote, bytes = done, "sftp download done");
        Ok(())
    }

    /// Copies `local` to the phone, replacing any file at `remote`.
    ///
    /// The file is streamed from disk. If the upload fails part way, the
    /// partial file on the phone is removed when the session still allows
    /// it. If the returned future is dropped mid-transfer, a partial file is
    /// left behind. `progress` behaves as for [`download`](Self::download).
    pub async fn upload(
        &self,
        local: &Path,
        remote: &str,
        mut progress: impl FnMut(u64, Option<u64>) + Send,
    ) -> Result<(), SftpError> {
        self.ensure_open()?;
        debug!(local = %local.display(), remote, "sftp upload");
        let mut source = tokio::fs::File::open(local)
            .await
            .map_err(SftpError::local(local))?;
        let metadata = source.metadata().await.map_err(SftpError::local(local))?;
        if metadata.is_dir() {
            return Err(SftpError::Other(format!("{} is a folder", local.display())));
        }
        let total = metadata.len();

        let mut sink = match self.sftp.create(remote).await {
            Ok(file) => file,
            Err(e) => return Err(self.client_error(e, remote).await),
        };

        let result: Result<u64, SftpError> = async {
            let mut throttle = Throttle::new();
            let mut done = 0u64;
            let mut buf = vec![0; CHUNK];
            progress(0, Some(total));
            loop {
                let n = source
                    .read(&mut buf)
                    .await
                    .map_err(SftpError::local(local))?;
                if n == 0 {
                    break;
                }
                if let Err(e) = sink.write_all(&buf[..n]).await {
                    return Err(self.io_error(e, remote).await);
                }
                done += n as u64;
                if throttle.due() {
                    progress(done, Some(total));
                }
            }
            // Waits for every outstanding write to be acknowledged.
            if let Err(e) = sink.shutdown().await {
                return Err(self.io_error(e, remote).await);
            }
            Ok(done)
        }
        .await;

        let result = match result {
            Ok(done) if done != total => Err(SftpError::Incomplete {
                expected: total,
                actual: done,
            }),
            Ok(done) => match self.sftp.metadata(remote).await {
                Ok(attrs) => match attrs.size {
                    Some(size) if size != done => Err(SftpError::Incomplete {
                        expected: done,
                        actual: size,
                    }),
                    _ => Ok(done),
                },
                Err(e) => Err(self.client_error(e, remote).await),
            },
            Err(e) => Err(e),
        };

        match result {
            Ok(done) => {
                progress(done, Some(total));
                debug!(remote, bytes = done, "sftp upload done");
                Ok(())
            }
            Err(e) => {
                if !self.is_closed() {
                    let _ = self.sftp.remove_file(remote).await;
                }
                Err(e)
            }
        }
    }

    /// Deletes a file, or a folder that is empty.
    pub async fn remove(&self, path: &str) -> Result<(), SftpError> {
        self.ensure_open()?;
        debug!(path, "sftp remove");
        let attrs = match self.sftp.symlink_metadata(path).await {
            Ok(attrs) => attrs,
            Err(e) => return Err(self.client_error(e, path).await),
        };
        let result = if attrs.is_dir() {
            self.sftp.remove_dir(path).await
        } else {
            self.sftp.remove_file(path).await
        };
        match result {
            Ok(()) => Ok(()),
            Err(e) => Err(self.client_error(e, path).await),
        }
    }

    /// Renames or moves a file or folder. Fails if `to` already exists.
    pub async fn rename(&self, from: &str, to: &str) -> Result<(), SftpError> {
        self.ensure_open()?;
        debug!(from, to, "sftp rename");
        match self.sftp.rename(from, to).await {
            Ok(()) => Ok(()),
            Err(e) => Err(self.client_error(e, from).await),
        }
    }

    /// Creates a folder. Its parent must exist.
    pub async fn create_dir(&self, path: &str) -> Result<(), SftpError> {
        self.ensure_open()?;
        debug!(path, "sftp create_dir");
        match self.sftp.create_dir(path).await {
            Ok(()) => Ok(()),
            Err(e) => Err(self.client_error(e, path).await),
        }
    }

    /// Whether the session has ended. Once true, every call fails with
    /// [`SftpError::Closed`]; ask the phone for new connection details.
    pub fn is_closed(&self) -> bool {
        self.dead.load(Ordering::Relaxed) || self.ssh.is_closed()
    }

    /// Ends the session politely. Dropping a `Session` also ends it.
    pub async fn close(self) {
        debug!("sftp closing");
        let _ = self.sftp.close().await;
        let _ = tokio::time::timeout(
            PING_TIMEOUT,
            self.ssh
                .disconnect(russh::Disconnect::ByApplication, "", "en"),
        )
        .await;
    }

    fn ensure_open(&self) -> Result<(), SftpError> {
        if self.is_closed() {
            Err(SftpError::Closed)
        } else {
            Ok(())
        }
    }

    fn mark_dead(&self) {
        if !self.dead.swap(true, Ordering::Relaxed) {
            debug!("sftp session marked closed");
        }
    }

    /// After a request timed out, finds out whether the phone is still
    /// there, so a phone that went to sleep makes later calls fail at once
    /// instead of each waiting out its own timeout.
    async fn timed_out(&self) -> SftpError {
        let alive = matches!(
            tokio::time::timeout(PING_TIMEOUT, self.ssh.send_ping()).await,
            Ok(Ok(()))
        ) && !self.ssh.is_closed();
        if alive {
            SftpError::Timeout
        } else {
            self.mark_dead();
            SftpError::Closed
        }
    }

    async fn client_error(&self, error: ClientError, path: &str) -> SftpError {
        match error {
            ClientError::Status(status) => match status.status_code {
                StatusCode::NoSuchFile => SftpError::NotFound(path.to_owned()),
                StatusCode::PermissionDenied => SftpError::PermissionDenied(path.to_owned()),
                StatusCode::NoConnection | StatusCode::ConnectionLost => {
                    self.mark_dead();
                    SftpError::Closed
                }
                code if status.error_message.is_empty() => {
                    SftpError::Other(format!("{path}: {code}"))
                }
                _ => SftpError::Other(format!("{path}: {}", status.error_message)),
            },
            ClientError::Timeout => self.timed_out().await,
            ClientError::Limited(message) => SftpError::Other(message),
            // Anything else means the SFTP stream itself broke or went out of
            // step, and no later request on it can be trusted.
            other => {
                debug!(error = %other, "sftp stream failed");
                self.mark_dead();
                SftpError::Closed
            }
        }
    }

    /// Errors from a file's read or write stream, which russh-sftp reduces
    /// to `io::Error` with the status message as text.
    async fn io_error(&self, error: std::io::Error, path: &str) -> SftpError {
        if error.kind() == std::io::ErrorKind::TimedOut {
            self.timed_out().await
        } else if self.is_closed() {
            SftpError::Closed
        } else {
            SftpError::Other(format!("{path}: {error}"))
        }
    }
}

async fn establish(endpoint: &Endpoint, timeouts: Timeouts) -> Result<Session, SftpError> {
    let stream = TcpStream::connect(endpoint.address)
        .await
        .map_err(|e| SftpError::Connect(format!("{}: {e}", endpoint.address)))?;
    let _ = stream.set_nodelay(true);

    let config = client::Config {
        preferred: algorithms::preferred(),
        // A browsing window can sit idle for a long time; keepalives notice a
        // vanished phone instead.
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(15)),
        keepalive_max: 3,
        nodelay: true,
        ..Default::default()
    };
    let mismatch = Arc::new(AtomicBool::new(false));
    let client = Client {
        expected: endpoint.host_key.clone(),
        mismatch: mismatch.clone(),
    };
    let mut ssh = client::connect_stream(Arc::new(config), stream, client)
        .await
        .map_err(|e| {
            if mismatch.load(Ordering::Relaxed) {
                SftpError::HostKeyMismatch
            } else {
                SftpError::Connect(format!("SSH handshake failed: {e}"))
            }
        })?;

    authenticate(&mut ssh, endpoint).await?;

    let channel = ssh
        .channel_open_session()
        .await
        .map_err(|e| SftpError::Connect(format!("could not open a channel: {e}")))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| SftpError::Connect(format!("could not start SFTP: {e}")))?;
    let config = russh_sftp::client::Config {
        request_timeout_secs: timeouts.request.as_secs_f64().ceil().max(1.0) as u64,
        ..Default::default()
    };
    let sftp = SftpSession::new_with_config(channel.into_stream(), config)
        .await
        .map_err(|e| match e {
            ClientError::Timeout => SftpError::Timeout,
            e => SftpError::Connect(format!("could not start SFTP: {e}")),
        })?;

    Ok(Session {
        ssh,
        sftp,
        dead: AtomicBool::new(false),
    })
}

async fn authenticate(
    ssh: &mut client::Handle<Client>,
    endpoint: &Endpoint,
) -> Result<(), SftpError> {
    let failed = |e: russh::Error| SftpError::Connect(format!("login failed: {e}"));
    if !endpoint.password.is_empty()
        && ssh
            .authenticate_password(&endpoint.user, &endpoint.password)
            .await
            .map_err(failed)?
            .success()
    {
        debug!("sftp logged in with password");
        return Ok(());
    }
    if let Some(key) = &endpoint.client_key {
        let hash = if key.0.algorithm().is_rsa() {
            ssh.best_supported_rsa_hash()
                .await
                .map_err(failed)?
                .flatten()
        } else {
            None
        };
        let key = PrivateKeyWithHashAlg::new(key.0.clone(), hash);
        if ssh
            .authenticate_publickey(&endpoint.user, key)
            .await
            .map_err(failed)?
            .success()
        {
            debug!("sftp logged in with client key");
            return Ok(());
        }
    }
    Err(SftpError::Auth)
}

struct Client {
    expected: Option<HostKey>,
    mismatch: Arc<AtomicBool>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            return Ok(false);
        };
        let Some(expected) = &self.expected else {
            // Without the phone's certificate there is nothing to compare
            // with: the server exists only for this browsing session, and
            // the connection details came over the authenticated, encrypted
            // KDE Connect link. Accepting is a fallback; callers should pass
            // the paired key so an interceptor cannot collect the password.
            return Ok(true);
        };
        let matches = expected.matches(key.key_data());
        if !matches {
            debug!("sftp host key does not match the paired phone");
            self.mismatch.store(true, Ordering::Relaxed);
        }
        Ok(matches)
    }
}

/// Throttles progress callbacks to [`PROGRESS_INTERVAL`].
struct Throttle(Instant);

impl Throttle {
    fn new() -> Self {
        Self(Instant::now())
    }

    fn due(&mut self) -> bool {
        let now = Instant::now();
        if now.duration_since(self.0) >= PROGRESS_INTERVAL {
            self.0 = now;
            true
        } else {
            false
        }
    }
}

/// A download's partial file, deleted unless kept.
struct TempFile {
    path: PathBuf,
    keep: bool,
}

impl TempFile {
    fn next_to(destination: &Path) -> Result<Self, SftpError> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let name = destination.file_name().ok_or_else(|| SftpError::Local {
            path: destination.to_owned(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a file path"),
        })?;
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut temp_name = std::ffi::OsString::from(".");
        temp_name.push(name);
        temp_name.push(format!(".{}-{unique}.landline-part", std::process::id()));
        Ok(Self {
            path: destination.with_file_name(temp_name),
            keep: false,
        })
    }

    fn keep(mut self) {
        self.keep = true;
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.keep {
            // Also runs when a download future is dropped mid-transfer.
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
