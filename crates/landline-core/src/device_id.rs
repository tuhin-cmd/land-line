//! Device IDs: the stable name a device uses on the network and as the
//! common name of its certificate.

/// A new random device ID: 32 lowercase hex characters, the same shape the
/// KDE Connect apps generate (a UUID without hyphens).
pub fn generate() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the operating system provides randomness");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Whether peers will accept `id`. Protocol 8 allows 32 to 38 characters from
/// `[a-zA-Z0-9_-]`; anything else is rejected at the identity exchange.
pub fn is_valid(id: &str) -> bool {
    (32..=38).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_valid_and_differ() {
        let (a, b) = (generate(), generate());
        assert_eq!(a.len(), 32);
        assert!(is_valid(&a) && is_valid(&b));
        assert_ne!(a, b);
    }

    #[test]
    fn validity_follows_the_protocol_8_rule() {
        assert!(is_valid("0123456789abcdef0123456789ABCDEF"));
        assert!(is_valid("a1b2c3d4_e5f6_7890_abcd_ef1234567890")); // 36, underscores
        assert!(is_valid("a1b2c3d4-e5f6-7890-abcd-ef1234567890"));
        assert!(!is_valid("short"));
        assert!(!is_valid(&"a".repeat(31)));
        assert!(!is_valid(&"a".repeat(39)));
        assert!(!is_valid("0123456789abcdef0123456789abcde!"));
        assert!(!is_valid("0123456789abcdef 123456789abcdef"));
    }
}
