//! Storage: browsing the phone's files over SFTP. Not implemented yet.

use std::path::PathBuf;

use tokio::sync::oneshot;

use super::{Command, Context, Plugin, Registration};
use crate::CoreError;
use crate::packet::Packet;

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
    incoming: &[],
    outgoing: &[],
    create: || Box::new(Sftp),
};

struct Sftp;

impl Plugin for Sftp {
    fn packet(&mut self, _packet: &Packet, _ctx: &mut Context<'_>) {}

    fn command(&mut self, command: Command, _ctx: &mut Context<'_>) {
        if let Command::Storage(request) = command {
            request.fail(CoreError::Storage("not implemented yet".into()));
        }
    }
}
