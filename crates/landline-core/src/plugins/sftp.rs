//! Storage: browsing the phone's files over SFTP.
//!
//! The phone runs an SFTP server only on request. We send
//! `kdeconnect.sftp.request`, it starts the server and answers with
//! `kdeconnect.sftp`: a port, a user, a password made up for this session,
//! and the folders worth showing. The session is then kept until it breaks
//! or the device disconnects, and asked for again when next needed.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use landline_sftp::{Endpoint, HostKey, Session, SftpError};
use serde_json::Map;
use tokio::sync::oneshot;
use tracing::debug;

use super::share::{self, Transfer, TransferDirection, TransferState};
use super::{Command, Context, LinkContext, Plugin, Registration};
use crate::packet::Packet;
use crate::store;
use crate::{CoreDelegate, CoreError};

const TYPE_SFTP: &str = "kdeconnect.sftp";
const TYPE_SFTP_REQUEST: &str = "kdeconnect.sftp.request";

/// How long the phone gets to start its server and answer. It has to ask
/// Android for storage access first, which can take a few seconds.
const START_TIMEOUT: Duration = Duration::from_secs(20);

/// A file or folder on the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    pub name: String,
    /// Absolute path on the device, for further calls.
    pub path: String,
    pub is_dir: bool,
    /// In bytes. Zero for folders.
    pub size: u64,
    /// Seconds since the Unix epoch, if the device reported it.
    pub modified_secs: Option<i64>,
}

/// A place on the device to start browsing from, such as internal storage
/// or an SD card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageRoot {
    pub name: String,
    pub path: String,
}

pub(crate) type Reply<T> = oneshot::Sender<Result<T, CoreError>>;

/// A storage operation and where to send its result.
pub(crate) enum Request {
    Roots {
        reply: Reply<Vec<StorageRoot>>,
    },
    List {
        path: String,
        reply: Reply<Vec<RemoteEntry>>,
    },
    Download {
        remote: String,
        local: PathBuf,
        reply: Reply<()>,
    },
    Upload {
        local: PathBuf,
        remote: String,
        reply: Reply<()>,
    },
}

impl Request {
    /// Answers the request with `error`.
    pub(crate) fn fail(self, error: CoreError) {
        match self {
            Self::Roots { reply } => drop(reply.send(Err(error))),
            Self::List { reply, .. } => drop(reply.send(Err(error))),
            Self::Download { reply, .. } | Self::Upload { reply, .. } => {
                drop(reply.send(Err(error)));
            }
        }
    }
}

pub(crate) const REGISTRATION: Registration = Registration {
    name: "sftp",
    incoming: &[TYPE_SFTP],
    outgoing: &[TYPE_SFTP_REQUEST],
    create: || Box::new(Sftp::default()),
};

/// A live session and the roots the phone named for it.
#[derive(Clone)]
struct Mount {
    session: Arc<Session>,
    roots: Vec<StorageRoot>,
}

type Waiter = oneshot::Sender<Result<Mount, CoreError>>;

#[derive(Default)]
struct State {
    mount: Option<Mount>,
    /// Requests waiting for the phone to start its server.
    waiters: Vec<Waiter>,
    /// Bumped on disconnect, so a session that finishes connecting
    /// afterwards is discarded rather than kept for a link that is gone.
    generation: u64,
    /// How to stop each running download or upload, by transfer ID.
    running: HashMap<u64, oneshot::Sender<()>>,
}

#[derive(Default)]
struct Sftp {
    state: Arc<Mutex<State>>,
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Plugin for Sftp {
    // The SFTP session is its own connection and outlives the link it was
    // set up over, so browsing and transfers carry on.
    fn link_replaced(&mut self, _ctx: &mut Context<'_>) {}

    fn disconnected(&mut self, _ctx: &mut Context<'_>) {
        let mut state = lock(&self.state);
        state.generation += 1;
        state.mount = None;
        // Dropping the senders stops every running transfer.
        state.running.clear();
        for waiter in state.waiters.drain(..) {
            let _ = waiter.send(Err(CoreError::NotReachable));
        }
    }

    fn packet(&mut self, packet: &Packet, ctx: &mut Context<'_>) {
        let Some(link) = ctx.link.clone() else {
            return;
        };
        if let Some(message) = packet.body_str("errorMessage") {
            fail_waiters(&self.state, &CoreError::Storage(message));
            return;
        }
        // The phone also announces `serverRunning: false` when its plugin
        // loads; only an answer with a port is a server to connect to.
        let Some(port) = packet
            .body_i64("port")
            .and_then(|port| u16::try_from(port).ok())
        else {
            return;
        };
        let roots = roots(packet);
        // The address in the packet is the phone's guess at its own IP and
        // can name the wrong interface; the link's address is known to work.
        let mut endpoint = Endpoint::new(
            SocketAddr::new(link.peer.address, port),
            packet
                .body_str("user")
                .unwrap_or_else(|| landline_sftp::DEFAULT_USER.to_owned()),
            packet.body_str("password").unwrap_or_default(),
        );
        // The phone's SSH host key is its KDE Connect identity key, the one
        // in the certificate pinned at pairing, so the server can be checked.
        let host_key = store::subject_public_key_info(&link.peer.certificate)
            .ok()
            .and_then(|key| HostKey::from_spki_der(&key).ok());
        if let Some(host_key) = host_key {
            endpoint = endpoint.with_host_key(host_key);
        }

        let state = Arc::clone(&self.state);
        let generation = lock(&state).generation;
        tokio::spawn(async move {
            let connected = Session::connect(&endpoint).await;
            let mut state = lock(&state);
            if state.generation != generation {
                return;
            }
            let result = match connected {
                Ok(session) => {
                    let mount = Mount {
                        session: Arc::new(session),
                        roots,
                    };
                    state.mount = Some(mount.clone());
                    Ok(mount)
                }
                Err(error) => {
                    debug!(%error, "could not open the device's storage");
                    Err(storage_error(&error))
                }
            };
            for waiter in state.waiters.drain(..) {
                let _ = waiter.send(clone_result(&result));
            }
        });
    }

    fn command(&mut self, command: Command, ctx: &mut Context<'_>) {
        let request = match command {
            Command::Storage(request) => request,
            Command::CancelTransfer { id } => {
                if let Some(stop) = lock(&self.state).running.remove(&id) {
                    let _ = stop.send(());
                }
                return;
            }
            _ => return,
        };
        let Some(link) = ctx.link.clone() else {
            request.fail(CoreError::NotReachable);
            return;
        };
        let state = Arc::clone(&self.state);
        let reporter = Reporter {
            delegate: Arc::clone(ctx.delegate),
            device_id: ctx.device_id.to_owned(),
        };
        tokio::spawn(async move {
            let mount = match mount(&state, &link).await {
                Ok(mount) => mount,
                Err(error) => return request.fail(error),
            };
            let broken = run(request, &mount, &reporter, &state).await;
            if broken {
                // Forget the session so the next request asks the phone for
                // a new one.
                let mut state = lock(&state);
                if state
                    .mount
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(&current.session, &mount.session))
                {
                    state.mount = None;
                }
            }
        });
    }
}

/// The current session, asking the phone to start its server if there is
/// none.
async fn mount(state: &Mutex<State>, link: &LinkContext) -> Result<Mount, CoreError> {
    let waiting = {
        let mut state = lock(state);
        if let Some(mount) = &state.mount
            && !mount.session.is_closed()
        {
            return Ok(mount.clone());
        }
        state.mount = None;
        let (waiter, waiting) = oneshot::channel();
        // One request serves everyone already waiting.
        if state.waiters.is_empty() {
            let mut body = Map::new();
            body.insert("startBrowsing".into(), true.into());
            if !link.sender.send(Packet::new(TYPE_SFTP_REQUEST, body)) {
                return Err(CoreError::NotReachable);
            }
        }
        state.waiters.push(waiter);
        waiting
    };
    match tokio::time::timeout(START_TIMEOUT, waiting).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(CoreError::NotReachable),
        Err(_) => {
            // Let the next request ask again instead of joining a wait that
            // will never be answered.
            lock(state).waiters.clear();
            Err(CoreError::Storage(
                "The phone did not open its storage. Check that the Filesystem expose plugin is \
                 enabled in KDE Connect and has access to files."
                    .into(),
            ))
        }
    }
}

/// Reports storage downloads and uploads as transfers, so the app shows
/// them alongside shared files.
struct Reporter {
    delegate: Arc<dyn CoreDelegate>,
    device_id: String,
}

impl Reporter {
    /// Starts reporting one file. The returned value reports its progress
    /// and its end.
    fn start(&self, file: &std::path::Path, direction: TransferDirection) -> Progress<'_> {
        let transfer = Transfer {
            id: share::next_transfer_id(),
            device_id: self.device_id.clone(),
            file_name: file
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            direction,
            bytes_done: 0,
            bytes_total: None,
            state: TransferState::InProgress,
        };
        self.delegate.transfer_updated(&transfer);
        Progress {
            reporter: self,
            transfer,
        }
    }
}

struct Progress<'a> {
    reporter: &'a Reporter,
    transfer: Transfer,
}

impl Progress<'_> {
    /// The SFTP client already limits how often it calls this.
    fn advance(&mut self, done: u64, total: Option<u64>) {
        self.transfer.bytes_done = done;
        self.transfer.bytes_total = total;
        self.reporter.delegate.transfer_updated(&self.transfer);
    }

    fn cancel(mut self) {
        self.transfer.state = TransferState::Cancelled;
        self.reporter.delegate.transfer_updated(&self.transfer);
    }

    fn finish(mut self, result: &Result<(), SftpError>, path: PathBuf) {
        self.transfer.state = match result {
            Ok(()) => TransferState::Finished { path },
            Err(error) => TransferState::Failed {
                reason: error.to_string(),
            },
        };
        self.reporter.delegate.transfer_updated(&self.transfer);
    }
}

/// Carries out `request` and answers it. Returns whether the session turned
/// out to be dead.
async fn run(request: Request, mount: &Mount, reporter: &Reporter, state: &Mutex<State>) -> bool {
    let session = &mount.session;
    match request {
        Request::Roots { reply } => {
            let _ = reply.send(Ok(mount.roots.clone()));
        }
        Request::List { path, reply } => {
            let entries = session.list_dir(&path).await.map(|entries| {
                entries
                    .into_iter()
                    .map(|entry| RemoteEntry {
                        name: entry.name,
                        path: entry.path,
                        is_dir: entry.is_dir,
                        size: entry.size,
                        modified_secs: entry.modified_secs,
                    })
                    .collect()
            });
            let _ = reply.send(entries.map_err(|error| storage_error(&error)));
        }
        // A transfer ends early when the app cancels it by ID, or when the
        // caller drops its future (closing the reply channel). Either way
        // the SFTP client cleans up after itself when its future is dropped.
        Request::Download {
            remote,
            local,
            mut reply,
        } => {
            let mut progress = reporter.start(&local, TransferDirection::Incoming);
            let stop = register(state, progress.transfer.id);
            let done = tokio::select! {
                done = session.download(&remote, &local, |done, total| progress.advance(done, total)) => Some(done),
                () = reply.closed() => None,
                _ = stop => None,
            };
            lock(state).running.remove(&progress.transfer.id);
            match done {
                Some(done) => {
                    progress.finish(&done, local);
                    let _ = reply.send(done.map_err(|error| storage_error(&error)));
                }
                None => {
                    progress.cancel();
                    let _ = reply.send(Err(CoreError::Cancelled));
                }
            }
        }
        Request::Upload {
            local,
            remote,
            mut reply,
        } => {
            let mut progress = reporter.start(&local, TransferDirection::Outgoing);
            let stop = register(state, progress.transfer.id);
            let done = tokio::select! {
                done = session.upload(&local, &remote, |done, total| progress.advance(done, total)) => Some(done),
                () = reply.closed() => None,
                _ = stop => None,
            };
            lock(state).running.remove(&progress.transfer.id);
            match done {
                Some(done) => {
                    progress.finish(&done, local);
                    let _ = reply.send(done.map_err(|error| storage_error(&error)));
                }
                None => {
                    progress.cancel();
                    let _ = reply.send(Err(CoreError::Cancelled));
                }
            }
        }
    }
    session.is_closed()
}

/// Records how to stop the transfer with this ID, and returns the signal to
/// wait on.
fn register(state: &Mutex<State>, transfer_id: u64) -> oneshot::Receiver<()> {
    let (stop, stopped) = oneshot::channel();
    lock(state).running.insert(transfer_id, stop);
    stopped
}

/// The folders the phone suggests browsing from: each `multiPaths` entry
/// with its `pathNames` label, or the single `path` on older phones.
fn roots(packet: &Packet) -> Vec<StorageRoot> {
    let paths = packet.body_string_list("multiPaths").unwrap_or_default();
    let names = packet.body_string_list("pathNames").unwrap_or_default();
    if paths.is_empty() {
        let path = packet.body_str("path").unwrap_or_else(|| "/".to_owned());
        return vec![StorageRoot {
            name: "Phone storage".to_owned(),
            path,
        }];
    }
    paths
        .into_iter()
        .enumerate()
        .map(|(index, path)| StorageRoot {
            name: names
                .get(index)
                .filter(|name| !name.is_empty())
                .cloned()
                .unwrap_or_else(|| path.clone()),
            path,
        })
        .collect()
}

fn fail_waiters(state: &Mutex<State>, error: &CoreError) {
    for waiter in lock(state).waiters.drain(..) {
        let _ = waiter.send(Err(clone_error(error)));
    }
}

fn storage_error(error: &SftpError) -> CoreError {
    let mut message = error.to_string();
    // The phone connected to us, so it is on the network; if we cannot
    // connect back, the system is refusing, not the phone.
    if message.contains("No route to host") {
        message.push_str(
            ". macOS may be blocking this app from the local network: allow it under System \
             Settings > Privacy & Security > Local Network.",
        );
    }
    CoreError::Storage(message)
}

/// `CoreError` holds an `io::Error` in one variant and so is not `Clone`;
/// the two variants used here are.
fn clone_error(error: &CoreError) -> CoreError {
    match error {
        CoreError::NotReachable => CoreError::NotReachable,
        other => CoreError::Storage(other.to_string()),
    }
}

fn clone_result(result: &Result<Mount, CoreError>) -> Result<Mount, CoreError> {
    match result {
        Ok(mount) => Ok(mount.clone()),
        Err(error) => Err(clone_error(error)),
    }
}

/// The in-process SFTP server the SFTP crate tests itself against, standing
/// in for the phone's.
#[cfg(test)]
#[path = "../../../landline-sftp/tests/common/mod.rs"]
mod test_server;

#[cfg(test)]
mod tests {
    use super::test_server::{Options, PASSWORD, TestServer, USER};
    use super::*;
    use crate::plugins::share::{TransferDirection, TransferState};
    use crate::plugins::testing::{Event, Harness};

    /// A server set up as the phone's is: its SSH host key is the KDE
    /// Connect identity key behind the certificate pinned at pairing.
    async fn phone_server(harness: &Harness) -> TestServer {
        let identity_key = harness.peer_identity.private_key_der().secret_pkcs8_der();
        let host_key = russh::keys::pkcs8::decode_pkcs8(identity_key, None).unwrap();
        TestServer::start_with(Options {
            host_private_key: Some(host_key),
            ..Options::default()
        })
        .await
    }

    /// What the phone answers a browse request with.
    fn server_details(server: &TestServer) -> Packet {
        packet(serde_json::json!({
            // The phone's guess at its own address; it must be ignored.
            "ip": "203.0.113.9",
            "port": server.address.port(),
            "user": USER,
            "password": PASSWORD,
            "path": "/",
            "multiPaths": ["/"],
            "pathNames": ["Internal storage"],
        }))
    }

    fn request<T>(
        plugin: &mut Box<dyn Plugin>,
        harness: &mut Harness,
        make: impl FnOnce(Reply<T>) -> Request,
    ) -> oneshot::Receiver<Result<T, CoreError>> {
        let (reply, answer) = oneshot::channel();
        plugin.command(Command::Storage(make(reply)), &mut harness.ctx());
        answer
    }

    #[tokio::test]
    async fn the_phones_storage_is_listed_and_downloaded() {
        let mut harness = Harness::new();
        let server = phone_server(&harness).await;
        std::fs::create_dir(server.path("DCIM")).unwrap();
        let photo: Vec<u8> = (0..400_000u32).map(|i| (i % 253) as u8).collect();
        std::fs::write(server.path("DCIM/photo.jpg"), &photo).unwrap();
        let mut plugin = (REGISTRATION.create)();

        // The first request asks the phone to start its server.
        let roots = request(&mut plugin, &mut harness, |reply| Request::Roots { reply });
        assert_eq!(harness.next_sent().await.kind, TYPE_SFTP_REQUEST);
        plugin.packet(&server_details(&server), &mut harness.ctx());
        assert_eq!(
            roots.await.unwrap().unwrap(),
            [StorageRoot {
                name: "Internal storage".into(),
                path: "/".into()
            }]
        );

        // Later requests use the same session without asking again.
        let path = "/DCIM".to_owned();
        let listing = request(&mut plugin, &mut harness, |reply| Request::List {
            path,
            reply,
        });
        let entries = listing.await.unwrap().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "photo.jpg");
        assert_eq!(entries[0].path, "/DCIM/photo.jpg");
        assert_eq!(entries[0].size, 400_000);
        assert!(harness.sent().is_empty());

        let local = harness.download_dir.join("photo.jpg");
        std::fs::create_dir_all(&harness.download_dir).unwrap();
        let download = request(&mut plugin, &mut harness, |reply| Request::Download {
            remote: "/DCIM/photo.jpg".into(),
            local: local.clone(),
            reply,
        });
        download.await.unwrap().unwrap();
        assert_eq!(std::fs::read(&local).unwrap(), photo);

        // It was reported as a transfer, from start to finish.
        let transfers: Vec<_> = harness
            .recorder
            .take()
            .into_iter()
            .filter_map(|event| match event {
                Event::Transfer(transfer) => Some(transfer),
                _ => None,
            })
            .collect();
        let (first, last) = (transfers.first().unwrap(), transfers.last().unwrap());
        assert_eq!(first.state, TransferState::InProgress);
        assert_eq!(first.direction, TransferDirection::Incoming);
        assert_eq!(first.file_name, "photo.jpg");
        assert_eq!(last.state, TransferState::Finished { path: local });
        assert_eq!(last.bytes_done, 400_000);
        assert!(transfers.iter().all(|transfer| transfer.id == first.id));
    }

    #[tokio::test]
    async fn uploads_reach_the_phone() {
        let mut harness = Harness::new();
        let server = phone_server(&harness).await;
        let mut plugin = (REGISTRATION.create)();
        std::fs::create_dir_all(&harness.download_dir).unwrap();
        let local = harness.download_dir.join("notes.txt");
        std::fs::write(&local, b"from the Mac").unwrap();

        let upload = request(&mut plugin, &mut harness, |reply| Request::Upload {
            local,
            remote: "/notes.txt".into(),
            reply,
        });
        harness.next_sent().await;
        plugin.packet(&server_details(&server), &mut harness.ctx());
        upload.await.unwrap().unwrap();
        assert_eq!(
            std::fs::read(server.path("notes.txt")).unwrap(),
            b"from the Mac"
        );
    }

    #[tokio::test]
    async fn a_server_without_the_devices_key_is_refused() {
        let mut harness = Harness::new();
        // Someone else answering on that port has their own key.
        let impostor = TestServer::start().await;
        let mut plugin = (REGISTRATION.create)();

        let roots = request(&mut plugin, &mut harness, |reply| Request::Roots { reply });
        harness.next_sent().await;
        plugin.packet(&server_details(&impostor), &mut harness.ctx());
        assert!(matches!(roots.await.unwrap(), Err(CoreError::Storage(_))));
    }

    fn packet(body: serde_json::Value) -> Packet {
        let serde_json::Value::Object(body) = body else {
            panic!("a packet body is an object");
        };
        Packet::new(TYPE_SFTP, body)
    }

    #[test]
    fn roots_pair_paths_with_their_names() {
        let roots = roots(&packet(serde_json::json!({
            "path": "/",
            "multiPaths": ["/storage/emulated/0", "/storage/1234-5678"],
            "pathNames": ["Internal storage"],
        })));
        assert_eq!(
            roots,
            [
                StorageRoot {
                    name: "Internal storage".into(),
                    path: "/storage/emulated/0".into()
                },
                StorageRoot {
                    name: "/storage/1234-5678".into(),
                    path: "/storage/1234-5678".into()
                },
            ]
        );
    }

    #[test]
    fn a_single_path_is_the_only_root() {
        let roots = roots(&packet(
            serde_json::json!({ "path": "/storage/emulated/0" }),
        ));
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].path, "/storage/emulated/0");
    }

    #[tokio::test]
    async fn a_request_asks_the_phone_and_reports_its_refusal() {
        let mut harness = Harness::new();
        let mut plugin = (REGISTRATION.create)();
        let (reply, answer) = oneshot::channel();
        plugin.command(
            Command::Storage(Request::Roots { reply }),
            &mut harness.ctx(),
        );

        let asked = harness.next_sent().await;
        assert_eq!(asked.kind, TYPE_SFTP_REQUEST);
        assert_eq!(asked.body_bool("startBrowsing"), Some(true));

        let refusal = packet(serde_json::json!({ "errorMessage": "No storage access" }));
        plugin.packet(&refusal, &mut harness.ctx());
        match answer.await.unwrap() {
            Err(CoreError::Storage(message)) => assert_eq!(message, "No storage access"),
            other => panic!("expected a storage error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn waiting_requests_fail_when_the_device_disconnects() {
        let mut harness = Harness::new();
        let mut plugin = (REGISTRATION.create)();
        let (reply, answer) = oneshot::channel();
        let path = "/storage/emulated/0".to_owned();
        plugin.command(
            Command::Storage(Request::List { path, reply }),
            &mut harness.ctx(),
        );
        harness.next_sent().await;

        harness.disconnect();
        plugin.disconnected(&mut harness.ctx());
        assert!(matches!(
            answer.await.unwrap(),
            Err(CoreError::NotReachable)
        ));
    }

    #[tokio::test]
    async fn a_command_without_a_link_fails_at_once() {
        let mut harness = Harness::new();
        harness.disconnect();
        let mut plugin = (REGISTRATION.create)();
        let (reply, answer) = oneshot::channel();
        plugin.command(
            Command::Storage(Request::Roots { reply }),
            &mut harness.ctx(),
        );
        assert!(matches!(
            answer.await.unwrap(),
            Err(CoreError::NotReachable)
        ));
    }
}
