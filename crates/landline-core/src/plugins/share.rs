//! Share: files, text and links sent between the devices.
//!
//! Everything travels as `kdeconnect.share.request`, whose body holds one
//! of:
//!
//! - `text`: the phone's "share text" target. Shown, never written to disk.
//! - `url`: a shared link.
//! - `filename`, with the bytes as the packet's payload (none for an empty
//!   file), plus `lastModified` (ms since the epoch, when known),
//!   `numberOfFiles` and `totalPayloadSize` for the whole batch, and `open`
//!   when the sender wants the file opened on arrival. The desktop app also
//!   sends `creationTime`.
//!
//! A batch of files goes one file at a time: the sender sends a packet,
//! waits for the receiver to fetch its payload, and only then sends the
//! next. Each payload has its own listening port on the phone. The phone's
//! receiver keeps a batch open for one second after a file ends and fails
//! it if fewer than `numberOfFiles` arrived by then, so the next packet must
//! follow promptly. When files join a batch that is already under way,
//! `kdeconnect.share.request.update` carries the new `numberOfFiles` and
//! `totalPayloadSize` so the phone's progress shows the whole batch.

use std::collections::{HashMap, VecDeque};
use std::future::pending;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Map;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{oneshot, watch};
use tracing::debug;

use super::{Command, Context, LinkContext, Plugin, Registration};
use crate::CoreDelegate;
use crate::packet::Packet;
use crate::payload::{Incoming, Offer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    /// From the device to this computer.
    Incoming,
    /// From this computer to the device.
    Outgoing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferState {
    InProgress,
    /// For an incoming file, `path` is where it was saved; for an outgoing
    /// one, the file that was sent.
    Finished {
        path: PathBuf,
    },
    Failed {
        reason: String,
    },
    Cancelled,
}

/// One file moving between the devices. Reported through
/// [`crate::CoreDelegate::transfer_updated`] when it starts, a few times a
/// second while it runs, and when it ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    /// Unique for the life of the core.
    pub id: u64,
    pub device_id: String,
    pub file_name: String,
    pub direction: TransferDirection,
    pub bytes_done: u64,
    /// `None` when the sender did not say how large the file is.
    pub bytes_total: Option<u64>,
    pub state: TransferState,
}

const TYPE_SHARE_REQUEST: &str = "kdeconnect.share.request";
const TYPE_SHARE_REQUEST_UPDATE: &str = "kdeconnect.share.request.update";

pub(crate) const REGISTRATION: Registration = Registration {
    name: "share",
    // The update packet only refines the phone's batch totals, which no
    // transfer we report carries, so we do not ask for it.
    incoming: &[TYPE_SHARE_REQUEST],
    outgoing: &[TYPE_SHARE_REQUEST, TYPE_SHARE_REQUEST_UPDATE],
    create: || Box::new(Share::default()),
};

/// Appended to a file's name while it is being received, so a partial file
/// never looks finished.
const PART_SUFFIX: &str = ".landline-part";

/// APFS allows 255 bytes per name. Leave room for the suffix and a
/// collision number.
const MAX_NAME_BYTES: usize = 255 - PART_SUFFIX.len() - 8;

/// For a file whose name is missing or is nothing but path syntax.
const FALLBACK_NAME: &str = "Received file";

const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

const CHUNK: usize = 256 * 1024;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Why a running transfer is being stopped from outside.
#[derive(Debug, Clone, Copy)]
enum Stop {
    Cancelled,
    Disconnected,
}

impl Stop {
    fn state(self) -> TransferState {
        match self {
            Self::Cancelled => TransferState::Cancelled,
            Self::Disconnected => TransferState::Failed {
                reason: "the device disconnected".into(),
            },
        }
    }
}

/// How to stop each running transfer, by id. Shared with the tasks, which
/// remove their own entry when they end.
type Stops = Arc<Mutex<HashMap<u64, watch::Sender<Option<Stop>>>>>;

#[derive(Default)]
struct Share {
    stops: Stops,
    /// Resolves when the incoming file queued last is done. Incoming files
    /// are fetched one at a time, in order: the phone sends them that way
    /// anyway, and a sender that announces the next file early (the desktop
    /// app does) serves it only after the current one.
    receive_lane: Option<oneshot::Receiver<()>>,
    /// Files waiting to go to the device on the current link.
    outbox: Arc<Mutex<Outbox>>,
}

impl Plugin for Share {
    fn disconnected(&mut self, _ctx: &mut Context<'_>) {
        for stop in lock(&self.stops).values() {
            stop.send_replace(Some(Stop::Disconnected));
        }
        self.receive_lane = None;
        // The old outbox's worker drains it, failing each file.
        self.outbox = Arc::default();
    }

    fn packet(&mut self, packet: &Packet, ctx: &mut Context<'_>) {
        if packet.kind != TYPE_SHARE_REQUEST {
            return;
        }
        // As the desktop app does: a payload without a name is still a file.
        if packet.has("filename") || packet.payload_size.is_some() {
            self.receive_file(packet, ctx);
        } else if let Some(text) = packet.body_str("text") {
            ctx.delegate.text_shared(ctx.device_id, &text);
        } else if let Some(url) = packet.body_str("url") {
            // The scheme is the sender's choice; the app decides what is
            // safe to open.
            ctx.delegate.url_shared(ctx.device_id, &url);
        } else {
            debug!("share request with nothing in it");
        }
    }

    fn command(&mut self, command: Command, ctx: &mut Context<'_>) {
        match command {
            Command::SendFiles { paths } => self.send_files(paths, ctx),
            Command::CancelTransfer { id } => {
                if let Some(stop) = lock(&self.stops).get(&id) {
                    stop.send_replace(Some(Stop::Cancelled));
                }
            }
            _ => {}
        }
    }
}

impl Share {
    fn receive_file(&mut self, packet: &Packet, ctx: &mut Context<'_>) {
        let name = sanitize_file_name(packet.body_str("filename").as_deref());
        let payload = ctx.payload(packet);
        let total = match &payload {
            Some(payload) => payload.size(),
            None => Some(0),
        };
        let job = IncomingFile {
            dir: ctx.download_dir.to_owned(),
            name: name.clone(),
            payload,
            modified: packet
                .body_i64("lastModified")
                .and_then(|ms| u64::try_from(ms).ok())
                .filter(|&ms| ms > 0)
                .map(|ms| UNIX_EPOCH + Duration::from_millis(ms)),
        };

        let mut reporter = Reporter::new(
            ctx.delegate,
            ctx.device_id,
            name,
            TransferDirection::Incoming,
            total,
        );
        reporter.report();
        let (registered, mut stop) = register(&self.stops, reporter.transfer.id);
        let previous = self.receive_lane.take();
        let (done, lane) = oneshot::channel::<()>();
        self.receive_lane = Some(lane);

        tokio::spawn(async move {
            let _registered = registered;
            let _done = done;
            let state = tokio::select! {
                biased;
                stop = stopped(&mut stop) => stop.state(),
                result = async {
                    if let Some(previous) = previous {
                        let _ = previous.await;
                    }
                    receive(job, &mut reporter).await
                } => match result {
                    Ok(path) => TransferState::Finished { path },
                    Err(error) => TransferState::Failed { reason: error.to_string() },
                },
            };
            reporter.finish(state);
        });
    }

    fn send_files(&mut self, paths: Vec<PathBuf>, ctx: &mut Context<'_>) {
        let Some(link) = ctx.link.clone() else {
            return;
        };
        let delegate = Arc::clone(ctx.delegate);
        let device_id = ctx.device_id.to_owned();
        let stops = Arc::clone(&self.stops);
        let outbox = Arc::clone(&self.outbox);

        tokio::spawn(async move {
            let mut ready = Vec::new();
            for path in paths {
                let name = path.file_name().map_or_else(
                    || path.display().to_string(),
                    |name| name.to_string_lossy().into_owned(),
                );
                let mut reporter = Reporter::new(
                    &delegate,
                    &device_id,
                    name,
                    TransferDirection::Outgoing,
                    None,
                );
                match inspect(&path).await {
                    Ok(meta) => {
                        reporter.transfer.bytes_total = Some(meta.size);
                        reporter.report();
                        let (registered, stop) = register(&stops, reporter.transfer.id);
                        ready.push(OutgoingFile {
                            path,
                            meta,
                            reporter,
                            stop,
                            _registered: registered,
                        });
                    }
                    Err(reason) => reporter.finish(TransferState::Failed { reason }),
                }
            }
            if ready.is_empty() {
                return;
            }

            let start = {
                let mut outbox = lock(&outbox);
                for file in ready {
                    outbox.files += 1;
                    outbox.bytes += file.meta.size;
                    outbox.queue.push_back(file);
                }
                if outbox.announced {
                    link.sender.send(outbox.update_packet());
                }
                !std::mem::replace(&mut outbox.running, true)
            };
            if start {
                tokio::spawn(run_outbox(outbox, link));
            }
        });
    }
}

struct IncomingFile {
    dir: PathBuf,
    /// Already sanitized.
    name: String,
    /// `None` for an empty file.
    payload: Option<Incoming>,
    modified: Option<SystemTime>,
}

/// Fetches one file into the download directory and returns where it went.
/// If this fails or is dropped part way, the partial file is deleted.
async fn receive(job: IncomingFile, reporter: &mut Reporter) -> io::Result<PathBuf> {
    fs::create_dir_all(&job.dir).await?;
    let (part, mut file) = PartFile::claim(&job.dir, &job.name).await?;
    if let Some(payload) = job.payload {
        let expected = payload.size();
        let mut reader = payload.open().await?;
        let received = copy(&mut reader, &mut file, reporter).await?;
        if let Some(expected) = expected
            && received < expected
        {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("the device sent {received} of {expected} bytes"),
            ));
        }
    }
    file.flush().await?;
    let file = file.into_std().await;
    if let Some(modified) = job.modified {
        // A wrong date is not worth failing the file over; the apps ignore
        // this error too.
        let _ = tokio::task::spawn_blocking(move || file.set_modified(modified)).await;
    }
    part.publish(&job.dir, &job.name).await
}

/// A file being received, under its temporary name. Deleted on drop unless
/// it was published.
struct PartFile {
    path: PathBuf,
    armed: bool,
}

impl PartFile {
    /// Creates the temporary file for the first free variant of `name`: one
    /// where neither the file nor its temporary twin exists.
    async fn claim(dir: &Path, name: &str) -> io::Result<(Self, File)> {
        for n in 1..=MAX_VARIANTS {
            let candidate = numbered(name, n);
            if exists(&dir.join(&candidate)).await? {
                continue;
            }
            let path = dir.join(format!("{candidate}{PART_SUFFIX}"));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .await
            {
                Ok(file) => return Ok((Self { path, armed: true }, file)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(too_many_variants())
    }

    /// Gives the finished file its real name, the first variant of `name`
    /// that is free now, without ever replacing an existing file.
    async fn publish(mut self, dir: &Path, name: &str) -> io::Result<PathBuf> {
        for n in 1..=MAX_VARIANTS {
            let path = dir.join(numbered(name, n));
            // Linking fails if the name is taken, so nothing is overwritten
            // even if another program creates the file meanwhile.
            match fs::hard_link(&self.path, &path).await {
                Ok(()) => {
                    fs::remove_file(&self.path).await?;
                    self.armed = false;
                    return Ok(path);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                // File systems without hard links (exFAT, some network
                // shares): check, then rename.
                Err(_) => {
                    if exists(&path).await? {
                        continue;
                    }
                    fs::rename(&self.path, &path).await?;
                    self.armed = false;
                    return Ok(path);
                }
            }
        }
        Err(too_many_variants())
    }
}

impl Drop for PartFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

const MAX_VARIANTS: u32 = 10_000;

fn too_many_variants() -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        "too many files with that name",
    )
}

/// Whether anything, even a dangling symlink, has that name.
async fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path).await {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// `name` for the first copy, then "photo 2.jpg", "photo 3.jpg", as Finder
/// numbers them.
fn numbered(name: &str, n: u32) -> String {
    if n == 1 {
        return name.to_owned();
    }
    let path = Path::new(name);
    match (path.file_stem(), path.extension()) {
        (Some(stem), Some(extension)) => format!(
            "{} {n}.{}",
            stem.to_string_lossy(),
            extension.to_string_lossy()
        ),
        _ => format!("{name} {n}"),
    }
}

/// Turns a file name from the network into one that is safe to create in
/// the download directory: a single, visible, non-empty path component.
fn sanitize_file_name(raw: Option<&str>) -> String {
    let raw = raw.unwrap_or_default();
    // Either separator: the sender may be any OS.
    let last = raw.rsplit(['/', '\\']).next().unwrap_or_default();
    let printable: String = last.chars().filter(|c| !c.is_control()).collect();
    // A leading dot would hide the file, and "." and ".." are not names.
    // Hiding matters if the download folder is the home folder: a received
    // ".zshrc" must not quietly become a startup file.
    let name = printable.trim().trim_start_matches('.').trim_start();
    if name.is_empty() {
        return FALLBACK_NAME.to_owned();
    }
    truncate_name(name)
}

/// Shortens `name` to [`MAX_NAME_BYTES`], keeping a short extension.
fn truncate_name(name: &str) -> String {
    if name.len() <= MAX_NAME_BYTES {
        return name.to_owned();
    }
    let extension = Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| extension.len() <= 16)
        .map(|extension| format!(".{extension}"))
        .unwrap_or_default();
    let mut stem = &name[..name.len() - extension.len()];
    let mut end = MAX_NAME_BYTES - extension.len();
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    stem = &stem[..end];
    format!("{stem}{extension}")
}

/// What a file looked like when it was queued for sending.
struct FileMeta {
    size: u64,
    /// Both in ms since the epoch.
    modified: Option<i64>,
    created: Option<i64>,
}

/// Checks that `path` is a readable regular file. The error is the reason
/// shown for the failed transfer.
async fn inspect(path: &Path) -> Result<FileMeta, String> {
    let meta = fs::metadata(path)
        .await
        .map_err(|error| error.to_string())?;
    if meta.is_dir() {
        return Err("folders cannot be sent".into());
    }
    // Reading a pipe or a device could block forever.
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    // Opened and closed again: a large batch must not hold a descriptor
    // per file while it waits.
    File::open(path).await.map_err(|error| error.to_string())?;
    let ms = |time: io::Result<SystemTime>| {
        let elapsed = time.ok()?.duration_since(UNIX_EPOCH).ok()?;
        i64::try_from(elapsed.as_millis()).ok()
    };
    Ok(FileMeta {
        size: meta.len(),
        modified: ms(meta.modified()),
        created: ms(meta.created()),
    })
}

/// Files to send on one link, and the batch the device sees them as.
#[derive(Default)]
struct Outbox {
    queue: VecDeque<OutgoingFile>,
    /// The batch's `numberOfFiles` and `totalPayloadSize`, counting the
    /// files already sent.
    files: u64,
    bytes: u64,
    /// The device has seen a packet of this batch, so changed totals need
    /// an update packet.
    announced: bool,
    /// A worker is draining the queue.
    running: bool,
}

impl Outbox {
    fn update_packet(&self) -> Packet {
        let mut body = Map::new();
        body.insert("numberOfFiles".into(), self.files.into());
        body.insert("totalPayloadSize".into(), self.bytes.into());
        Packet::new(TYPE_SHARE_REQUEST_UPDATE, body)
    }

    /// Starts the device's count over with what is still queued, after a
    /// failure that ended its batch.
    fn restart_batch(&mut self) {
        self.files = self.queue.len() as u64;
        self.bytes = self.queue.iter().map(|file| file.meta.size).sum();
        self.announced = false;
    }
}

struct OutgoingFile {
    path: PathBuf,
    meta: FileMeta,
    reporter: Reporter,
    stop: watch::Receiver<Option<Stop>>,
    _registered: Registered,
}

/// Sends the outbox's files one after another until it is empty. The phone
/// fetches payloads in order and only one at a time, so offering the next
/// file before the current one is fetched would only make it time out.
async fn run_outbox(outbox: Arc<Mutex<Outbox>>, link: LinkContext) {
    loop {
        let (mut file, files, bytes) = {
            let mut outbox = lock(&outbox);
            let Some(file) = outbox.queue.pop_front() else {
                *outbox = Outbox::default();
                return;
            };
            let stopped_early = *file.stop.borrow();
            if let Some(stop) = stopped_early {
                outbox.files -= 1;
                outbox.bytes -= file.meta.size;
                if outbox.announced {
                    link.sender.send(outbox.update_packet());
                }
                drop(outbox);
                file.reporter.finish(stop.state());
                continue;
            }
            outbox.announced = true;
            (file, outbox.files, outbox.bytes)
        };

        let state = tokio::select! {
            biased;
            stop = stopped(&mut file.stop) => stop.state(),
            result = send_file(&file.path, &file.meta, files, bytes, &link, &mut file.reporter) => {
                match result {
                    Ok(()) => TransferState::Finished { path: file.path.clone() },
                    Err(error) => TransferState::Failed { reason: error.to_string() },
                }
            }
        };
        if !matches!(state, TransferState::Finished { .. }) {
            // The phone gives up on its whole batch when one file fails.
            lock(&outbox).restart_batch();
        }
        file.reporter.finish(state);
    }
}

async fn send_file(
    path: &Path,
    meta: &FileMeta,
    files: u64,
    bytes: u64,
    link: &LinkContext,
    reporter: &mut Reporter,
) -> io::Result<()> {
    let mut body = Map::new();
    body.insert(
        "filename".into(),
        reporter.transfer.file_name.clone().into(),
    );
    if let Some(modified) = meta.modified {
        body.insert("lastModified".into(), modified.into());
    }
    if let Some(created) = meta.created {
        body.insert("creationTime".into(), created.into());
    }
    body.insert("numberOfFiles".into(), files.into());
    body.insert("totalPayloadSize".into(), bytes.into());
    let mut packet = Packet::new(TYPE_SHARE_REQUEST, body);

    // The apps send an empty file as a packet without payload.
    if meta.size == 0 {
        return send(link, packet);
    }
    let mut source = File::open(path).await?;
    let offer = Offer::bind(link.peer.clone()).await?;
    offer.attach(&mut packet, meta.size);
    send(link, packet)?;
    let mut stream = offer.accept().await.map_err(|error| {
        if error.kind() == io::ErrorKind::TimedOut {
            io::Error::new(error.kind(), "the device did not fetch the file")
        } else {
            error
        }
    })?;
    let sent = copy(&mut (&mut source).take(meta.size), &mut stream, reporter).await?;
    if sent < meta.size {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the file got shorter while it was being sent",
        ));
    }
    // The phone reads until the connection closes.
    stream.shutdown().await
}

fn send(link: &LinkContext, packet: Packet) -> io::Result<()> {
    if link.sender.send(packet) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "the device disconnected",
        ))
    }
}

/// Copies until `reader` ends, reporting progress, and returns the count.
/// A connection closed without a TLS goodbye counts as the end; the caller
/// compares the count with what was announced.
async fn copy(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    reporter: &mut Reporter,
) -> io::Result<u64> {
    let mut buffer = vec![0u8; CHUNK];
    let mut total = 0;
    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error),
        };
        writer.write_all(&buffer[..read]).await?;
        total += read as u64;
        reporter.advance(read as u64);
    }
    Ok(total)
}

/// One transfer's state, and when the app last heard of it.
struct Reporter {
    delegate: Arc<dyn CoreDelegate>,
    transfer: Transfer,
    last_report: Instant,
}

impl Reporter {
    fn new(
        delegate: &Arc<dyn CoreDelegate>,
        device_id: &str,
        file_name: String,
        direction: TransferDirection,
        bytes_total: Option<u64>,
    ) -> Self {
        Self {
            delegate: Arc::clone(delegate),
            transfer: Transfer {
                id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                device_id: device_id.to_owned(),
                file_name,
                direction,
                bytes_done: 0,
                bytes_total,
                state: TransferState::InProgress,
            },
            last_report: Instant::now(),
        }
    }

    fn report(&mut self) {
        self.last_report = Instant::now();
        self.delegate.transfer_updated(&self.transfer);
    }

    fn advance(&mut self, bytes: u64) {
        self.transfer.bytes_done += bytes;
        if self.last_report.elapsed() >= PROGRESS_INTERVAL {
            self.report();
        }
    }

    fn finish(mut self, state: TransferState) {
        match &state {
            TransferState::Finished { .. } => debug!(id = self.transfer.id, "transfer finished"),
            other => debug!(id = self.transfer.id, state = ?other, "transfer ended"),
        }
        self.transfer.state = state;
        self.report();
    }
}

/// Removes a transfer's stop handle when the transfer ends.
struct Registered {
    stops: Stops,
    id: u64,
}

impl Drop for Registered {
    fn drop(&mut self) {
        lock(&self.stops).remove(&self.id);
    }
}

fn register(stops: &Stops, id: u64) -> (Registered, watch::Receiver<Option<Stop>>) {
    let (sender, receiver) = watch::channel(None);
    lock(stops).insert(id, sender);
    let registered = Registered {
        stops: Arc::clone(stops),
        id,
    };
    (registered, receiver)
}

/// Resolves when the transfer is told to stop; never, if it cannot be.
async fn stopped(receiver: &mut watch::Receiver<Option<Stop>>) -> Stop {
    let stop = receiver
        .wait_for(Option::is_some)
        .await
        .map(|stop| (*stop).unwrap_or(Stop::Cancelled));
    match stop {
        Ok(stop) => stop,
        Err(_) => pending().await,
    }
}

/// A poisoned lock only means a task panicked; the maps stay consistent.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tokio::sync::oneshot;
    use tokio::task::JoinHandle;

    use super::*;
    use crate::plugins::testing::{Event, Harness};

    fn request(body: Value) -> Packet {
        let Value::Object(body) = body else {
            panic!("body must be an object")
        };
        Packet::new(TYPE_SHARE_REQUEST, body)
    }

    fn file_request(name: &str, files: u64, total: u64) -> Packet {
        request(json!({
            "filename": name,
            "numberOfFiles": files,
            "totalPayloadSize": total,
        }))
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// Plays the phone offering a payload with `packet`: announces
    /// `announced` bytes, sends `data`, then waits for `release` (if any)
    /// before sending `rest` and closing.
    async fn phone_offers(
        harness: &Harness,
        packet: &mut Packet,
        announced: u64,
        data: Vec<u8>,
        hold: Option<(oneshot::Receiver<()>, Vec<u8>)>,
    ) -> JoinHandle<()> {
        let offer = Offer::bind(harness.device_side_peer()).await.unwrap();
        offer.attach(packet, announced);
        tokio::spawn(async move {
            let Ok(mut stream) = offer.accept().await else {
                return;
            };
            let _ = stream.write_all(&data).await;
            let _ = stream.flush().await;
            if let Some((release, rest)) = hold {
                let _ = release.await;
                let _ = stream.write_all(&rest).await;
            }
            let _ = stream.shutdown().await;
        })
    }

    /// Collects transfer events until `count` transfers have ended.
    async fn transfers_until_ended(harness: &Harness, count: usize) -> Vec<Transfer> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        loop {
            for event in harness.recorder.take() {
                if let Event::Transfer(transfer) = event {
                    seen.push(transfer);
                }
            }
            let ended = seen
                .iter()
                .filter(|transfer| transfer.state != TransferState::InProgress)
                .count();
            if ended >= count {
                return seen;
            }
            assert!(Instant::now() < deadline, "timed out; saw {seen:?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn ended(transfers: &[Transfer]) -> Vec<&Transfer> {
        transfers
            .iter()
            .filter(|transfer| transfer.state != TransferState::InProgress)
            .collect()
    }

    fn files_in(dir: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    async fn wait_for_part_file(dir: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !files_in(dir).iter().any(|name| name.ends_with(PART_SUFFIX)) {
            assert!(Instant::now() < deadline, "no partial file appeared");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn start_id(harness: &Harness) -> u64 {
        match harness.recorder.take().as_slice() {
            [Event::Transfer(transfer)] => transfer.id,
            other => panic!("expected one start event, got {other:?}"),
        }
    }

    #[test]
    fn text_and_links_go_to_the_delegate() {
        let mut harness = Harness::new();
        let mut plugin = (REGISTRATION.create)();

        plugin.packet(&request(json!({"text": "hi there"})), &mut harness.ctx());
        plugin.packet(
            &request(json!({"url": "https://example.org/"})),
            &mut harness.ctx(),
        );
        assert_eq!(
            harness.recorder.take(),
            [
                Event::TextShared("hi there".into()),
                Event::UrlShared("https://example.org/".into()),
            ]
        );
    }

    #[tokio::test]
    async fn a_file_arrives_whole_with_its_name_and_date() {
        let mut harness = Harness::new();
        let mut plugin = (REGISTRATION.create)();
        let data = pattern(300_000);
        let len = data.len() as u64;

        let mut packet = file_request("photo.jpg", 1, len);
        packet
            .body
            .insert("lastModified".into(), 1_600_000_000_000i64.into());
        let phone = phone_offers(&harness, &mut packet, len, data.clone(), None).await;
        plugin.packet(&packet, &mut harness.ctx());

        let transfers = transfers_until_ended(&harness, 1).await;
        let expected = harness.download_dir.join("photo.jpg");
        let first = &transfers[0];
        assert_eq!(first.state, TransferState::InProgress);
        assert_eq!(first.direction, TransferDirection::Incoming);
        assert_eq!(first.file_name, "photo.jpg");
        assert_eq!(first.bytes_total, Some(len));
        assert_eq!(first.device_id, Harness::DEVICE_ID);
        assert!(transfers.iter().all(|transfer| transfer.id == first.id));
        let last = transfers.last().unwrap();
        assert_eq!(
            last.state,
            TransferState::Finished {
                path: expected.clone()
            }
        );
        assert_eq!(last.bytes_done, len);

        assert_eq!(std::fs::read(&expected).unwrap(), data);
        let modified = std::fs::metadata(&expected).unwrap().modified().unwrap();
        assert_eq!(
            modified,
            UNIX_EPOCH + Duration::from_millis(1_600_000_000_000)
        );
        assert_eq!(files_in(&harness.download_dir), ["photo.jpg"]);
        phone.await.unwrap();
    }

    #[tokio::test]
    async fn a_name_collision_keeps_both_files() {
        let mut harness = Harness::new();
        let mut plugin = (REGISTRATION.create)();
        std::fs::create_dir_all(&harness.download_dir).unwrap();
        std::fs::write(harness.download_dir.join("photo.jpg"), b"old").unwrap();

        let mut packet = file_request("photo.jpg", 1, 3);
        let phone = phone_offers(&harness, &mut packet, 3, b"new".to_vec(), None).await;
        plugin.packet(&packet, &mut harness.ctx());

        let transfers = transfers_until_ended(&harness, 1).await;
        let expected = harness.download_dir.join("photo 2.jpg");
        assert_eq!(
            ended(&transfers)[0].state,
            TransferState::Finished {
                path: expected.clone()
            }
        );
        assert_eq!(std::fs::read(&expected).unwrap(), b"new");
        assert_eq!(
            std::fs::read(harness.download_dir.join("photo.jpg")).unwrap(),
            b"old"
        );
        phone.await.unwrap();
    }

    #[test]
    fn hostile_names_become_plain_names() {
        let cases: &[(Option<&str>, &str)] = &[
            (Some("../x"), "x"),
            (Some("../../escape.txt"), "escape.txt"),
            (Some("/etc/passwd"), "passwd"),
            (Some("C:\\Windows\\win.ini"), "win.ini"),
            (Some(".."), FALLBACK_NAME),
            (Some("."), FALLBACK_NAME),
            (Some("dir/.."), FALLBACK_NAME),
            (Some("trailing/"), FALLBACK_NAME),
            (Some(""), FALLBACK_NAME),
            (Some("   "), FALLBACK_NAME),
            (None, FALLBACK_NAME),
            (Some("evil\0.txt"), "evil.txt"),
            (Some("line\nbreak.txt"), "linebreak.txt"),
            (Some(".zshrc"), "zshrc"),
            (Some("photo.jpg"), "photo.jpg"),
        ];
        for (raw, expected) in cases {
            assert_eq!(sanitize_file_name(*raw), *expected, "for {raw:?}");
        }

        let long = format!("{}.jpg", "é".repeat(200));
        let short = sanitize_file_name(Some(&long));
        assert!(short.len() <= MAX_NAME_BYTES, "{} bytes", short.len());
        assert!(short.ends_with("é.jpg"));
    }

    #[test]
    fn collisions_are_numbered_like_finder() {
        assert_eq!(numbered("photo.jpg", 1), "photo.jpg");
        assert_eq!(numbered("photo.jpg", 2), "photo 2.jpg");
        assert_eq!(numbered("archive.tar.gz", 3), "archive.tar 3.gz");
        assert_eq!(numbered("README", 2), "README 2");
    }

    #[tokio::test]
    async fn hostile_names_stay_in_the_download_folder() {
        let mut harness = Harness::new();
        let mut plugin = (REGISTRATION.create)();

        let mut phones = Vec::new();
        for name in ["../../escape.txt", "/etc/passwd", ".."] {
            let mut packet = file_request(name, 3, 9);
            phones.push(phone_offers(&harness, &mut packet, 3, b"abc".to_vec(), None).await);
            plugin.packet(&packet, &mut harness.ctx());
        }
        let transfers = transfers_until_ended(&harness, 3).await;
        assert!(
            ended(&transfers)
                .iter()
                .all(|transfer| matches!(transfer.state, TransferState::Finished { .. }))
        );
        assert_eq!(
            files_in(&harness.download_dir),
            [FALLBACK_NAME, "escape.txt", "passwd"]
        );
        let scratch = harness.download_dir.parent().unwrap();
        assert_eq!(files_in(scratch), ["downloads"]);
        for phone in phones {
            phone.await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_short_payload_fails_and_leaves_nothing() {
        let mut harness = Harness::new();
        let mut plugin = (REGISTRATION.create)();

        let mut packet = file_request("short.bin", 1, 1000);
        let phone = phone_offers(&harness, &mut packet, 1000, pattern(500), None).await;
        plugin.packet(&packet, &mut harness.ctx());

        let transfers = transfers_until_ended(&harness, 1).await;
        let last = ended(&transfers)[0];
        assert!(
            matches!(&last.state, TransferState::Failed { reason } if reason.contains("500 of 1000")),
            "{last:?}"
        );
        assert!(files_in(&harness.download_dir).is_empty());
        phone.await.unwrap();
    }

    #[tokio::test]
    async fn an_empty_file_needs_no_payload() {
        let mut harness = Harness::new();
        let mut plugin = (REGISTRATION.create)();

        plugin.packet(&file_request("empty.txt", 1, 0), &mut harness.ctx());
        let transfers = transfers_until_ended(&harness, 1).await;
        let path = harness.download_dir.join("empty.txt");
        assert_eq!(
            ended(&transfers)[0].state,
            TransferState::Finished { path: path.clone() }
        );
        assert!(std::fs::read(path).unwrap().is_empty());
    }

    #[tokio::test]
    async fn cancelling_an_incoming_file_deletes_the_partial_copy() {
        let mut harness = Harness::new();
        let mut plugin = (REGISTRATION.create)();
        let (release, held) = oneshot::channel();

        let mut packet = file_request("big.bin", 1, 200_000);
        let phone = phone_offers(
            &harness,
            &mut packet,
            200_000,
            pattern(100_000),
            Some((held, pattern(100_000))),
        )
        .await;
        plugin.packet(&packet, &mut harness.ctx());
        let id = start_id(&harness);
        wait_for_part_file(&harness.download_dir).await;

        plugin.command(Command::CancelTransfer { id }, &mut harness.ctx());
        let transfers = transfers_until_ended(&harness, 1).await;
        assert_eq!(ended(&transfers)[0].state, TransferState::Cancelled);
        assert!(files_in(&harness.download_dir).is_empty());
        let _ = release.send(());
        phone.await.unwrap();
    }

    #[tokio::test]
    async fn a_disconnect_fails_running_transfers() {
        let mut harness = Harness::new();
        let mut plugin = (REGISTRATION.create)();
        let (release, held) = oneshot::channel();

        let mut packet = file_request("big.bin", 1, 200_000);
        let phone = phone_offers(
            &harness,
            &mut packet,
            200_000,
            pattern(100_000),
            Some((held, pattern(100_000))),
        )
        .await;
        plugin.packet(&packet, &mut harness.ctx());
        wait_for_part_file(&harness.download_dir).await;

        harness.disconnect();
        plugin.disconnected(&mut harness.ctx());
        let transfers = transfers_until_ended(&harness, 1).await;
        assert_eq!(
            ended(&transfers)[0].state,
            TransferState::Failed {
                reason: "the device disconnected".into()
            }
        );
        assert!(files_in(&harness.download_dir).is_empty());
        let _ = release.send(());
        phone.await.unwrap();
    }

    #[tokio::test]
    async fn a_batch_arrives_in_order() {
        let mut harness = Harness::new();
        let mut plugin = (REGISTRATION.create)();
        let files: [(&str, &[u8]); 3] = [
            ("a.txt", b"first"),
            ("b.txt", b"second"),
            ("a.txt", b"third"),
        ];

        // All announced at once, as a sender that announces ahead would.
        let mut phones = Vec::new();
        for (name, data) in files {
            let mut packet = file_request(name, 3, 16);
            let len = data.len() as u64;
            phones.push(phone_offers(&harness, &mut packet, len, data.to_vec(), None).await);
            plugin.packet(&packet, &mut harness.ctx());
        }

        transfers_until_ended(&harness, 3).await;
        let dir = &harness.download_dir;
        assert_eq!(files_in(dir), ["a 2.txt", "a.txt", "b.txt"]);
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"first");
        assert_eq!(std::fs::read(dir.join("b.txt")).unwrap(), b"second");
        assert_eq!(std::fs::read(dir.join("a 2.txt")).unwrap(), b"third");
        for phone in phones {
            phone.await.unwrap();
        }
    }

    /// Writes a file to send into the harness's scratch space.
    fn source_file(harness: &Harness, name: &str, data: &[u8], modified_ms: u64) -> PathBuf {
        std::fs::create_dir_all(&harness.cache_dir).unwrap();
        let path = harness.cache_dir.join(name);
        std::fs::write(&path, data).unwrap();
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_millis(modified_ms))
            .unwrap();
        path
    }

    /// Plays the phone fetching the payload a packet offers.
    async fn phone_fetches(harness: &Harness, packet: &Packet) -> Vec<u8> {
        let incoming = Incoming::from_packet(packet, harness.device_side_peer()).unwrap();
        let mut data = Vec::new();
        incoming
            .open()
            .await
            .unwrap()
            .read_to_end(&mut data)
            .await
            .unwrap();
        data
    }

    #[tokio::test]
    async fn a_file_reaches_the_device_with_its_metadata() {
        let mut harness = Harness::new();
        let mut plugin = (REGISTRATION.create)();
        let data = pattern(200_000);
        let source = source_file(&harness, "notes.pdf", &data, 1_500_000_000_123);
        let missing = harness.cache_dir.join("missing.bin");

        let paths = vec![missing.clone(), source.clone()];
        plugin.command(Command::SendFiles { paths }, &mut harness.ctx());

        let packet = harness.next_sent().await;
        assert_eq!(packet.kind, TYPE_SHARE_REQUEST);
        assert_eq!(packet.body_str("filename").as_deref(), Some("notes.pdf"));
        assert_eq!(packet.body_i64("lastModified"), Some(1_500_000_000_123));
        assert_eq!(packet.body_i64("numberOfFiles"), Some(1));
        assert_eq!(packet.body_i64("totalPayloadSize"), Some(200_000));
        assert_eq!(packet.payload_size, Some(200_000));
        assert_eq!(phone_fetches(&harness, &packet).await, data);

        let transfers = transfers_until_ended(&harness, 2).await;
        let ended = ended(&transfers);
        assert_eq!(ended[0].file_name, "missing.bin");
        assert!(matches!(ended[0].state, TransferState::Failed { .. }));
        assert_eq!(ended[1].direction, TransferDirection::Outgoing);
        assert_eq!(ended[1].bytes_done, 200_000);
        assert_eq!(ended[1].state, TransferState::Finished { path: source });
        assert!(harness.sent().is_empty());
    }

    #[tokio::test]
    async fn folders_are_not_sent() {
        let mut harness = Harness::new();
        let mut plugin = (REGISTRATION.create)();
        std::fs::create_dir_all(&harness.cache_dir).unwrap();

        let paths = vec![harness.cache_dir.clone()];
        plugin.command(Command::SendFiles { paths }, &mut harness.ctx());
        let transfers = transfers_until_ended(&harness, 1).await;
        assert_eq!(
            transfers[0].state,
            TransferState::Failed {
                reason: "folders cannot be sent".into()
            }
        );
        assert!(harness.sent().is_empty());
    }

    #[tokio::test]
    async fn a_batch_goes_one_file_at_a_time() {
        let mut harness = Harness::new();
        let mut plugin = (REGISTRATION.create)();
        let first = source_file(&harness, "one.txt", b"first", 1_000);
        let second = source_file(&harness, "two.txt", b"second!", 2_000);

        let paths = vec![first, second];
        plugin.command(Command::SendFiles { paths }, &mut harness.ctx());

        let packet = harness.next_sent().await;
        assert_eq!(packet.body_str("filename").as_deref(), Some("one.txt"));
        assert_eq!(packet.body_i64("numberOfFiles"), Some(2));
        assert_eq!(packet.body_i64("totalPayloadSize"), Some(12));
        // The next file waits until this one is fetched.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(harness.sent().is_empty());
        assert_eq!(phone_fetches(&harness, &packet).await, b"first");

        let packet = harness.next_sent().await;
        assert_eq!(packet.body_str("filename").as_deref(), Some("two.txt"));
        assert_eq!(packet.body_i64("numberOfFiles"), Some(2));
        assert_eq!(phone_fetches(&harness, &packet).await, b"second!");

        let transfers = transfers_until_ended(&harness, 2).await;
        assert!(
            ended(&transfers)
                .iter()
                .all(|transfer| matches!(transfer.state, TransferState::Finished { .. }))
        );
    }

    #[tokio::test]
    async fn cancelling_an_outgoing_file_moves_on_to_the_next() {
        let mut harness = Harness::new();
        let mut plugin = (REGISTRATION.create)();
        let first = source_file(&harness, "one.txt", b"first", 1_000);
        let second = source_file(&harness, "two.txt", b"second!", 2_000);

        let paths = vec![first, second];
        plugin.command(Command::SendFiles { paths }, &mut harness.ctx());
        let packet = harness.next_sent().await;
        assert_eq!(packet.body_str("filename").as_deref(), Some("one.txt"));

        let id = transfers_until_ended(&harness, 0)
            .await
            .iter()
            .find(|transfer| transfer.file_name == "one.txt")
            .unwrap()
            .id;
        plugin.command(Command::CancelTransfer { id }, &mut harness.ctx());

        // The phone dropped its batch with the cancelled file, so the next
        // one starts a new batch of one.
        let packet = harness.next_sent().await;
        assert_eq!(packet.body_str("filename").as_deref(), Some("two.txt"));
        assert_eq!(packet.body_i64("numberOfFiles"), Some(1));
        assert_eq!(packet.body_i64("totalPayloadSize"), Some(7));
        assert_eq!(phone_fetches(&harness, &packet).await, b"second!");

        let transfers = transfers_until_ended(&harness, 2).await;
        let ended = ended(&transfers);
        assert_eq!(ended[0].state, TransferState::Cancelled);
        assert_eq!(ended[0].id, id);
        assert!(matches!(ended[1].state, TransferState::Finished { .. }));
    }
}
