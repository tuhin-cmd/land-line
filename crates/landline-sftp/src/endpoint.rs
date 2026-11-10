use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use russh::keys::PrivateKey;

use crate::{HostKey, SftpError};

/// The user name the KDE Connect Android app's SFTP server accepts. The
/// `kdeconnect.sftp` packet carries it too, so prefer the packet's value.
pub const DEFAULT_USER: &str = "kdeconnect";

/// Where and how to log in, from one `kdeconnect.sftp` packet.
///
/// The phone generates a new password every time browsing starts, so an
/// `Endpoint` is good for one server run. When a call fails with
/// [`SftpError::Closed`] or [`SftpError::Auth`], ask the phone again
/// (`kdeconnect.sftp.request` with `startBrowsing: true`) for a fresh one.
#[derive(Clone)]
pub struct Endpoint {
    /// The phone's address and the port its server bound (1739 to 1764).
    pub address: SocketAddr,
    /// The packet's `user`, which is always [`DEFAULT_USER`] in practice.
    pub user: String,
    /// The packet's `password`. Leave it empty to log in with
    /// `client_key` alone.
    pub password: String,
    /// This Mac's KDE Connect identity key, as a second way in. The phone's
    /// server also accepts a public-key login with the key of the paired
    /// device it is talking to, which is what the desktop app's sshfs mount
    /// uses. The password alone is enough, so this is optional; it is tried
    /// only when the password is empty or rejected.
    pub client_key: Option<ClientKey>,
    /// The paired phone's identity key, which its SSH server presents as
    /// its host key. Always set this when the phone's certificate is at
    /// hand: without it any host key is accepted, and someone able to
    /// intercept the connection could read the one-time password and use it
    /// against the real phone.
    pub host_key: Option<HostKey>,
}

impl Endpoint {
    /// An endpoint that logs in with a password.
    pub fn new(address: SocketAddr, user: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            address,
            user: user.into(),
            password: password.into(),
            client_key: None,
            host_key: None,
        }
    }

    /// Requires the phone's SSH host key to be `key`.
    pub fn with_host_key(mut self, key: HostKey) -> Self {
        self.host_key = Some(key);
        self
    }

    /// Adds this device's KDE Connect key as a second way to log in.
    pub fn with_client_key(mut self, key: ClientKey) -> Self {
        self.client_key = Some(key);
        self
    }
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoint")
            .field("address", &self.address)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("client_key", &self.client_key)
            .field("host_key", &self.host_key)
            .finish()
    }
}

/// A private key to authenticate with.
#[derive(Clone)]
pub struct ClientKey(pub(crate) Arc<PrivateKey>);

impl ClientKey {
    /// Reads an unencrypted PKCS#8 DER private key, the format the KDE
    /// Connect identity key is stored in (EC P-256 for Landline; RSA and
    /// Ed25519 also parse).
    pub fn from_pkcs8_der(der: &[u8]) -> Result<Self, SftpError> {
        russh::keys::pkcs8::decode_pkcs8(der, None)
            .map(|key| Self(Arc::new(key)))
            .map_err(|e| SftpError::Other(format!("unusable client key: {e}")))
    }
}

impl fmt::Debug for ClientKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ClientKey({})", self.0.algorithm())
    }
}

/// How long to wait for the phone before giving up.
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    /// For the whole of [`Session::connect`](crate::Session::connect): TCP,
    /// SSH handshake, login and starting SFTP.
    pub connect: Duration,
    /// For each SFTP request. A transfer is many requests, so this bounds
    /// a stall, not the transfer. Rounded up to whole seconds.
    pub request: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        // The phone's server runs on a handset that may be busy or have
        // its radio dozing; a listing of a large folder through Android's
        // storage framework can take seconds.
        Self {
            connect: Duration::from_secs(15),
            request: Duration::from_secs(20),
        }
    }
}
