//! The SSH algorithms offered to the phone.
//!
//! The phone runs Apache MINA SSHD (2.x in current KDE Connect releases)
//! with BouncyCastle switched off, so what it can offer depends on the
//! Android version's own crypto provider:
//!
//! - Key exchange: SSHD offers only what it finds a provider for at
//!   runtime. `ecdh-sha2-nistp256/384/521` and the finite-field
//!   Diffie-Hellman groups work on every Android version; `curve25519-sha256`
//!   needs X25519 in the platform provider; the post-quantum hybrids normally
//!   come from BouncyCastle. They are left out here: a one-off session with
//!   a key generated per run gains little from them, and they are the newest
//!   and least exercised code on both sides.
//! - Host key: the phone signs with its KDE Connect identity key, which is
//!   EC P-256 (`ecdsa-sha2-nistp256`) for recent installs and RSA for
//!   older ones (`rsa-sha2-512/256`, or `ssh-rsa` on old SSHD releases).
//! - Ciphers and MACs: AES-CTR and AES-GCM with SHA-2 HMACs everywhere,
//!   ChaCha20-Poly1305 on recent SSHD.
//!
//! russh's defaults cover most of that but leave out the NIST ECDH key
//! exchanges, `diffie-hellman-group14-sha1` and the SHA-1 HMACs. The NIST
//! curves are the one modern exchange every Android version can do, so they
//! come right after X25519. The SHA-1 entries are last resorts for old app
//! releases and are only chosen when nothing better is shared.

use std::borrow::Cow;

use russh::keys::{Algorithm, EcdsaCurve, HashAlg};
use russh::{Preferred, cipher, compression, kex, mac};

const KEX: &[kex::Name] = &[
    kex::CURVE25519,
    kex::CURVE25519_PRE_RFC_8731,
    kex::ECDH_SHA2_NISTP256,
    kex::ECDH_SHA2_NISTP384,
    kex::ECDH_SHA2_NISTP521,
    kex::DH_GEX_SHA256,
    kex::DH_G16_SHA512,
    kex::DH_G14_SHA256,
    kex::DH_G14_SHA1,
    kex::EXTENSION_SUPPORT_AS_CLIENT,
    kex::EXTENSION_OPENSSH_STRICT_KEX_AS_CLIENT,
];

const HOST_KEY: &[Algorithm] = &[
    Algorithm::Ecdsa {
        curve: EcdsaCurve::NistP256,
    },
    Algorithm::Ed25519,
    Algorithm::Ecdsa {
        curve: EcdsaCurve::NistP384,
    },
    Algorithm::Ecdsa {
        curve: EcdsaCurve::NistP521,
    },
    Algorithm::Rsa {
        hash: Some(HashAlg::Sha512),
    },
    Algorithm::Rsa {
        hash: Some(HashAlg::Sha256),
    },
    // `ssh-rsa`: SHA-1 signatures, for RSA identity keys on old SSHD.
    Algorithm::Rsa { hash: None },
];

const CIPHER: &[cipher::Name] = &[
    cipher::CHACHA20_POLY1305,
    cipher::AES_128_GCM,
    cipher::AES_256_GCM,
    cipher::AES_128_CTR,
    cipher::AES_192_CTR,
    cipher::AES_256_CTR,
];

const MAC: &[mac::Name] = &[
    mac::HMAC_SHA256_ETM,
    mac::HMAC_SHA512_ETM,
    mac::HMAC_SHA256,
    mac::HMAC_SHA512,
    mac::HMAC_SHA1_ETM,
    mac::HMAC_SHA1,
];

// Photos and videos do not compress, and zlib would cost the phone CPU.
const COMPRESSION: &[compression::Name] = &[compression::NONE];

pub(crate) fn preferred() -> Preferred {
    Preferred {
        kex: Cow::Borrowed(KEX),
        key: Cow::Borrowed(HOST_KEY),
        host_key_certificates: Cow::Borrowed(&[]),
        cipher: Cow::Borrowed(CIPHER),
        mac: Cow::Borrowed(MAC),
        compression: Cow::Borrowed(COMPRESSION),
    }
}
