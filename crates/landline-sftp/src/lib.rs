//! SFTP client for the storage a KDE Connect Android phone exposes.
//!
//! Asked with `kdeconnect.sftp.request` (`startBrowsing: true`), the phone
//! starts an SSH server and answers with a `kdeconnect.sftp` packet holding
//! `ip`, `port`, `user` and a one-time `password`. Build an [`Endpoint`] from
//! those, [`Session::connect`], and browse.
//!
//! Paths are the server's: `/`-separated strings, absolute from the root the
//! phone exposes. They are passed through untouched; build them with
//! [`join`].

#![forbid(unsafe_code)]

mod algorithms;
mod endpoint;
mod error;
mod host_key;
mod session;

pub use endpoint::{ClientKey, DEFAULT_USER, Endpoint, Timeouts};
pub use error::SftpError;
pub use host_key::HostKey;
pub use session::{Entry, Session};

/// Joins an SFTP folder path and a name with exactly one `/` between them.
///
/// `name` is appended as given, so `..` or a nested `a/b` pass through for
/// the server to interpret.
pub fn join(dir: &str, name: &str) -> String {
    let name = name.trim_start_matches('/');
    if dir.is_empty() {
        name.to_owned()
    } else if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::join;

    #[test]
    fn join_puts_one_slash_between() {
        assert_eq!(join("/", "DCIM"), "/DCIM");
        assert_eq!(
            join("/storage/emulated/0", "DCIM"),
            "/storage/emulated/0/DCIM"
        );
        assert_eq!(
            join("/storage/emulated/0/", "DCIM"),
            "/storage/emulated/0/DCIM"
        );
        assert_eq!(join("/a", "/b"), "/a/b");
        assert_eq!(join("", "b"), "b");
        assert_eq!(join("/a", ".."), "/a/..");
    }
}
