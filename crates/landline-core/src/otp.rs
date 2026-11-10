//! One-time codes in message text. Not implemented yet.

/// A one-time code found in a message or notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtpCode {
    /// The digits, without the spaces or hyphens some senders add.
    pub code: String,
    /// The app or sender the text came from.
    pub source: String,
}

/// The one-time code in `text`, if it carries one.
pub fn extract(_text: &str) -> Option<String> {
    None
}
