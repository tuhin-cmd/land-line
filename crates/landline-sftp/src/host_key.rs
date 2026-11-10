use der::asn1::UintRef;
use der::{Decode, Reader, SliceReader};
use russh::keys::ssh_key::public::KeyData;
use spki::SubjectPublicKeyInfoRef;

use crate::SftpError;

const EC_PUBLIC_KEY: spki::ObjectIdentifier =
    spki::ObjectIdentifier::new_unwrap("1.2.840.10045.2.1");
const RSA_ENCRYPTION: spki::ObjectIdentifier =
    spki::ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.1");

/// The key the phone's SSH server must present.
///
/// The KDE Connect Android app runs its SFTP server with the phone's own
/// KDE Connect identity key as the SSH host key: the same key pair its TLS
/// certificate is made from. So the host key can be checked against the
/// certificate of the paired phone, which makes the session as trustworthy
/// as the KDE Connect link itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKey(Material);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Material {
    /// An EC point in SEC1 form.
    Ec(Vec<u8>),
    /// RSA modulus and exponent, unsigned big-endian without leading zeros.
    Rsa { n: Vec<u8>, e: Vec<u8> },
}

impl HostKey {
    /// Reads the `SubjectPublicKeyInfo` of the paired phone's certificate,
    /// as DER. EC and RSA keys are supported, which is everything the
    /// Android app generates.
    pub fn from_spki_der(der: &[u8]) -> Result<Self, SftpError> {
        let invalid =
            |e: &dyn std::fmt::Display| SftpError::Other(format!("unusable phone key: {e}"));
        let info = SubjectPublicKeyInfoRef::from_der(der).map_err(|e| invalid(&e))?;
        let bits = info
            .subject_public_key
            .as_bytes()
            .ok_or_else(|| invalid(&"key bits are not whole bytes"))?;
        if info.algorithm.oid == EC_PUBLIC_KEY {
            return Ok(Self(Material::Ec(bits.to_vec())));
        }
        if info.algorithm.oid == RSA_ENCRYPTION {
            let (n, e) = rsa_components(bits).map_err(|e| invalid(&e))?;
            return Ok(Self(Material::Rsa { n, e }));
        }
        Err(invalid(&format_args!(
            "unsupported algorithm {}",
            info.algorithm.oid
        )))
    }

    pub(crate) fn matches(&self, key: &KeyData) -> bool {
        match (&self.0, key) {
            (Material::Ec(point), KeyData::Ecdsa(ecdsa)) => {
                ecdsa.as_sec1_bytes() == point.as_slice()
            }
            (Material::Rsa { n, e }, KeyData::Rsa(rsa)) => {
                strip_zeros(rsa.n().as_bytes()) == n.as_slice()
                    && strip_zeros(rsa.e().as_bytes()) == e.as_slice()
            }
            _ => false,
        }
    }
}

/// Parses a PKCS#1 `RSAPublicKey`: a sequence of modulus and exponent.
fn rsa_components(der: &[u8]) -> der::Result<(Vec<u8>, Vec<u8>)> {
    let mut reader = SliceReader::new(der)?;
    let parts = reader.sequence(|seq| -> der::Result<_> {
        let n = UintRef::decode(seq)?;
        let e = UintRef::decode(seq)?;
        Ok((
            strip_zeros(n.as_bytes()).to_vec(),
            strip_zeros(e.as_bytes()).to_vec(),
        ))
    })?;
    reader.finish()?;
    Ok(parts)
}

fn strip_zeros(bytes: &[u8]) -> &[u8] {
    let first = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
    &bytes[first..]
}
