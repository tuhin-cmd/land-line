use std::io;
use std::path::PathBuf;

/// Everything that can go wrong talking to the phone's SFTP server.
///
/// The messages are written to be shown to a user as they are.
#[derive(Debug, thiserror::Error)]
pub enum SftpError {
    /// The phone could not be reached, or the SSH handshake with it failed.
    #[error("could not connect to the phone: {0}")]
    Connect(String),

    /// The server presented a different key from the paired phone's. Either
    /// something else answered at that address, or the phone's identity
    /// changed and it needs pairing again.
    #[error("the phone's identity did not match; not connecting")]
    HostKeyMismatch,

    /// The phone rejected the credentials. Its password changes every time
    /// browsing starts, so the fix is to ask the phone for new details.
    #[error("the phone rejected the login; ask it to start browsing again")]
    Auth,

    /// The session ended, because the phone stopped its server, went out of
    /// range, or [`Session::close`](crate::Session::close) was called. Ask
    /// the phone for new connection details and connect again.
    #[error("the connection to the phone was closed; ask it to start browsing again")]
    Closed,

    /// The phone did not answer in time.
    #[error("the phone stopped responding")]
    Timeout,

    /// The file or folder does not exist on the phone.
    #[error("{0} does not exist on the phone")]
    NotFound(String),

    /// The phone does not allow this operation on this path. On Android that
    /// usually means KDE Connect lacks storage access for it.
    #[error("the phone does not allow access to {0}")]
    PermissionDenied(String),

    /// A file on this Mac could not be read or written.
    #[error("{}: {source}", path.display())]
    Local {
        /// The local file involved.
        path: PathBuf,
        /// What the OS reported.
        source: io::Error,
    },

    /// A transfer ended with a different number of bytes than the phone
    /// reported for the file.
    #[error("the transfer was incomplete: expected {expected} bytes, got {actual}")]
    Incomplete {
        /// The size the phone reported.
        expected: u64,
        /// The bytes actually transferred.
        actual: u64,
    },

    /// Anything else, with the server's or library's description.
    #[error("{0}")]
    Other(String),
}

impl SftpError {
    pub(crate) fn local(path: impl Into<PathBuf>) -> impl FnOnce(io::Error) -> SftpError {
        let path = path.into();
        move |source| SftpError::Local { path, source }
    }
}
