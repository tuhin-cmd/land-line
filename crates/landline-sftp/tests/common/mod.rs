//! An SFTP server over a temporary folder, to test the client against
//! without a phone.

#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use landline_sftp::{Endpoint, HostKey};
use russh::keys::{Algorithm, EcdsaCurve, PrivateKey, PublicKey};
use russh::server::{Auth, Msg, RunningServerHandle, Server as _, Session};
use russh::{Channel, ChannelId, Preferred};
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version,
};
use tempfile::TempDir;
use tokio::net::TcpListener;

pub const USER: &str = "kdeconnect";
pub const PASSWORD: &str = "correct horse battery staple";

/// Largest read reply, like Apache SSHD's default cap, so the client has to
/// cope with short reads.
const MAX_READ: u32 = 63 * 1024;

pub struct TestServer {
    pub address: SocketAddr,
    pub root: TempDir,
    pub host_key: PrivateKey,
    handle: RunningServerHandle,
}

pub struct Options {
    pub preferred: Preferred,
    pub host_key: Algorithm,
    /// A client key the server accepts besides the password.
    pub client_key: Option<PublicKey>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            preferred: Preferred::default(),
            host_key: Algorithm::Ecdsa {
                curve: EcdsaCurve::NistP256,
            },
            client_key: None,
        }
    }
}

impl TestServer {
    pub async fn start() -> Self {
        Self::start_with(Options::default()).await
    }

    pub async fn start_with(options: Options) -> Self {
        let root = tempfile::tempdir().unwrap();
        let host_key = PrivateKey::random(&mut rand::rng(), options.host_key).unwrap();
        let config = russh::server::Config {
            keys: vec![host_key.clone()],
            preferred: options.preferred,
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            ..Default::default()
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut server = SshServer {
            root: root.path().to_owned(),
            client_key: options.client_key,
        };
        // The accept loop borrows the server and the listener, so both live
        // in the task that runs it.
        let (handle_tx, handle_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let running = server.run_on_socket(Arc::new(config), &listener);
            let _ = handle_tx.send(running.handle());
            let _ = running.await;
        });
        let handle = handle_rx.await.unwrap();
        Self {
            address,
            root,
            host_key,
            handle,
        }
    }

    pub fn endpoint(&self) -> Endpoint {
        Endpoint::new(self.address, USER, PASSWORD)
    }

    /// The host key as the SubjectPublicKeyInfo of a certificate.
    pub fn host_key_spki(&self) -> HostKey {
        HostKey::from_spki_der(&spki_der(self.host_key.public_key())).unwrap()
    }

    pub fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }

    /// Stops listening and disconnects every client.
    pub fn shutdown(&self) {
        self.handle.shutdown("test over".into());
    }
}

/// Encodes an EC P-256 or RSA public key as DER SubjectPublicKeyInfo.
pub fn spki_der(key: &PublicKey) -> Vec<u8> {
    use russh::keys::ssh_key::public::KeyData;
    match key.key_data() {
        KeyData::Ecdsa(ecdsa) => {
            // id-ecPublicKey with the prime256v1 curve.
            let algorithm = hex("301306072a8648ce3d020106082a8648ce3d030107");
            let mut bits = vec![0];
            bits.extend_from_slice(ecdsa.as_sec1_bytes());
            der(0x30, &[algorithm, der(0x03, &[bits])])
        }
        KeyData::Rsa(rsa) => {
            // rsaEncryption with NULL parameters.
            let algorithm = hex("300d06092a864886f70d0101010500");
            let int = |mpint: &russh::keys::ssh_key::Mpint| der(0x02, &[mpint.as_bytes().to_vec()]);
            let mut bits = vec![0];
            bits.extend(der(0x30, &[int(rsa.n()), int(rsa.e())]));
            der(0x30, &[algorithm, der(0x03, &[bits])])
        }
        other => panic!("no SPKI encoding for {other:?}"),
    }
}

fn der(tag: u8, parts: &[Vec<u8>]) -> Vec<u8> {
    let body: Vec<u8> = parts.concat();
    let mut out = vec![tag];
    let len = body.len();
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes: Vec<u8> = len
            .to_be_bytes()
            .into_iter()
            .skip_while(|&b| b == 0)
            .collect();
        out.push(0x80 | bytes.len() as u8);
        out.extend(bytes);
    }
    out.extend(body);
    out
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[derive(Clone)]
struct SshServer {
    root: PathBuf,
    client_key: Option<PublicKey>,
}

impl russh::server::Server for SshServer {
    type Handler = SshSession;

    fn new_client(&mut self, _: Option<SocketAddr>) -> SshSession {
        SshSession {
            root: self.root.clone(),
            client_key: self.client_key.clone(),
            channels: HashMap::new(),
        }
    }
}

struct SshSession {
    root: PathBuf,
    client_key: Option<PublicKey>,
    channels: HashMap<ChannelId, Channel<Msg>>,
}

impl russh::server::Handler for SshSession {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if user == USER && password == PASSWORD {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        let known = self
            .client_key
            .as_ref()
            .is_some_and(|k| k.key_data() == key.key_data());
        Ok(if user == USER && known {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        match self.channels.remove(&channel) {
            Some(ch) if name == "sftp" => {
                session.channel_success(channel)?;
                let handler = SftpHandler {
                    root: self.root.clone(),
                    handles: HashMap::new(),
                    next_handle: 0,
                };
                russh_sftp::server::run(ch.into_stream(), handler).await;
            }
            _ => session.channel_failure(channel)?,
        }
        Ok(())
    }
}

enum Open {
    File(fs::File),
    Dir { path: PathBuf, listed: bool },
}

struct SftpHandler {
    root: PathBuf,
    handles: HashMap<String, Open>,
    next_handle: u64,
}

impl SftpHandler {
    fn resolve(&self, path: &str) -> Result<PathBuf, StatusCode> {
        let mut out = self.root.clone();
        for part in Path::new(path).components() {
            match part {
                Component::Normal(name) => out.push(name),
                Component::RootDir | Component::CurDir => {}
                // The test server never needs to leave its root.
                _ => return Err(StatusCode::PermissionDenied),
            }
        }
        Ok(out)
    }

    fn add(&mut self, open: Open) -> String {
        self.next_handle += 1;
        let handle = self.next_handle.to_string();
        self.handles.insert(handle.clone(), open);
        handle
    }

    fn file(&mut self, handle: &str) -> Result<&mut fs::File, StatusCode> {
        match self.handles.get_mut(handle) {
            Some(Open::File(file)) => Ok(file),
            _ => Err(StatusCode::Failure),
        }
    }
}

fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: String::new(),
        language_tag: "en-US".into(),
    }
}

fn code(error: io::Error) -> StatusCode {
    match error.kind() {
        io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    }
}

impl russh_sftp::server::Handler for SftpHandler {
    type Error = StatusCode;

    fn unimplemented(&self) -> StatusCode {
        StatusCode::OpUnsupported
    }

    async fn init(&mut self, _: u32, _: HashMap<String, String>) -> Result<Version, StatusCode> {
        Ok(Version::new())
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        flags: OpenFlags,
        _: FileAttributes,
    ) -> Result<Handle, StatusCode> {
        let path = self.resolve(&filename)?;
        let file = fs::OpenOptions::new()
            .read(flags.contains(OpenFlags::READ))
            .write(flags.contains(OpenFlags::WRITE))
            .append(flags.contains(OpenFlags::APPEND))
            .create(flags.contains(OpenFlags::CREATE))
            .truncate(flags.contains(OpenFlags::TRUNCATE))
            .open(path)
            .map_err(code)?;
        let handle = self.add(Open::File(file));
        Ok(Handle { id, handle })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, StatusCode> {
        self.handles.remove(&handle).ok_or(StatusCode::Failure)?;
        Ok(ok(id))
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, StatusCode> {
        let file = self.file(&handle)?;
        file.seek(SeekFrom::Start(offset)).map_err(code)?;
        let mut data = Vec::new();
        file.take(u64::from(len.min(MAX_READ)))
            .read_to_end(&mut data)
            .map_err(code)?;
        if data.is_empty() {
            return Err(StatusCode::Eof);
        }
        Ok(Data { id, data })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, StatusCode> {
        let file = self.file(&handle)?;
        file.seek(SeekFrom::Start(offset)).map_err(code)?;
        file.write_all(&data).map_err(code)?;
        Ok(ok(id))
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, StatusCode> {
        let metadata = fs::symlink_metadata(self.resolve(&path)?).map_err(code)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&metadata),
        })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, StatusCode> {
        let metadata = fs::metadata(self.resolve(&path)?).map_err(code)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&metadata),
        })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, StatusCode> {
        let metadata = self.file(&handle)?.metadata().map_err(code)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&metadata),
        })
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, StatusCode> {
        let path = self.resolve(&path)?;
        if !fs::metadata(&path).map_err(code)?.is_dir() {
            return Err(StatusCode::NoSuchFile);
        }
        let handle = self.add(Open::Dir {
            path,
            listed: false,
        });
        Ok(Handle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, StatusCode> {
        let Some(Open::Dir { path, listed }) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::Failure);
        };
        if *listed {
            return Err(StatusCode::Eof);
        }
        *listed = true;
        // Real servers include these; the client must drop them.
        let mut files = vec![
            File::new(
                ".",
                FileAttributes::from(&fs::metadata(&*path).map_err(code)?),
            ),
            File::new(
                "..",
                FileAttributes::from(&fs::metadata(&*path).map_err(code)?),
            ),
        ];
        for entry in fs::read_dir(&*path).map_err(code)? {
            let entry = entry.map_err(code)?;
            let metadata = fs::symlink_metadata(entry.path()).map_err(code)?;
            files.push(File::new(
                entry.file_name().to_string_lossy(),
                FileAttributes::from(&metadata),
            ));
        }
        Ok(Name { id, files })
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, StatusCode> {
        fs::remove_file(self.resolve(&filename)?).map_err(code)?;
        Ok(ok(id))
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _: FileAttributes,
    ) -> Result<Status, StatusCode> {
        fs::create_dir(self.resolve(&path)?).map_err(code)?;
        Ok(ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, StatusCode> {
        fs::remove_dir(self.resolve(&path)?).map_err(code)?;
        Ok(ok(id))
    }

    async fn rename(&mut self, id: u32, from: String, to: String) -> Result<Status, StatusCode> {
        let to = self.resolve(&to)?;
        // SFTP v3 rename does not overwrite.
        if fs::symlink_metadata(&to).is_ok() {
            return Err(StatusCode::Failure);
        }
        fs::rename(self.resolve(&from)?, to).map_err(code)?;
        Ok(ok(id))
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, StatusCode> {
        Ok(Name {
            id,
            files: vec![File::dummy(path)],
        })
    }
}
