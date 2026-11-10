//! What this device keeps on disk: its own certificate and key, and the
//! certificates of the devices it is paired with.
//!
//! Everything lives in one directory chosen by the caller (Application
//! Support on macOS). The directory is private to the user (0700), and the
//! private key file is never readable by anyone else, not even briefly.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use rcgen::{CertificateParams, DnType, KeyPair, PKCS_ECDSA_P256_SHA256, PublicKeyData};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use x509_parser::prelude::{FromDer, X509Certificate};

use crate::device_id;

pub const CERTIFICATE_FILE: &str = "certificate.pem";
pub const PRIVATE_KEY_FILE: &str = "private_key.pem";
pub const TRUSTED_DEVICES_FILE: &str = "trusted_devices.json";

const DIR_MODE: u32 = 0o700;
const KEY_MODE: u32 = 0o600;
const CERTIFICATE_MODE: u32 = 0o644;
const TRUST_STORE_MODE: u32 = 0o600;

// The KDE Connect apps put these in every certificate's subject; peers do not
// check them, but matching them keeps our certificate unremarkable.
const ORGANIZATIONAL_UNIT: &str = "KDE Connect";
const ORGANIZATION: &str = "KDE";

const YEAR: Duration = Duration::from_secs(365 * 24 * 60 * 60);

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not generate the device certificate: {0}")]
    Generate(#[source] rcgen::Error),
    #[error("invalid certificate: {0}")]
    InvalidCertificate(String),
    #[error("{path} is corrupt: {reason}")]
    CorruptTrustStore { path: PathBuf, reason: String },
    #[error("could not serialize the trust store: {0}")]
    Serialize(#[source] serde_json::Error),
}

impl StoreError {
    fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_owned(),
            source,
        }
    }
}

/// This device's identity on the network: its device ID and the certificate
/// and key it presents in every TLS handshake.
pub struct LocalIdentity {
    device_id: String,
    certificate: CertificateDer<'static>,
    private_key: PrivatePkcs8KeyDer<'static>,
    public_key: Vec<u8>,
}

impl Clone for LocalIdentity {
    fn clone(&self) -> Self {
        Self {
            device_id: self.device_id.clone(),
            certificate: self.certificate.clone(),
            private_key: self.private_key.clone_key(),
            public_key: self.public_key.clone(),
        }
    }
}

impl fmt::Debug for LocalIdentity {
    // Spelled out so the private key can never reach a log line.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalIdentity")
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

impl LocalIdentity {
    /// Loads the identity stored in `dir`, or creates and stores a new one.
    ///
    /// The returned flag is true when the identity is new. Peers pinned the
    /// old certificate, so every existing pairing is void and the caller
    /// should clear the [`TrustStore`].
    ///
    /// A stored identity that is missing, unparseable, or expired is replaced.
    /// A file that exists but cannot be read is an error instead, because
    /// replacing the identity over a transient failure would unpair every
    /// device.
    pub fn load_or_create(dir: &Path) -> Result<(Self, bool), StoreError> {
        create_private_dir(dir)?;
        let certificate_path = dir.join(CERTIFICATE_FILE);
        let key_path = dir.join(PRIVATE_KEY_FILE);

        match (read_optional(&certificate_path)?, read_optional(&key_path)?) {
            (Some(certificate), Some(key)) => match Self::from_pem(&certificate, &key) {
                Ok(identity) => {
                    restrict_mode(&key_path, KEY_MODE)?;
                    return Ok((identity, false));
                }
                Err(reason) => warn!("replacing the stored identity: {reason}"),
            },
            (None, None) => {}
            _ => warn!("replacing the stored identity: one of its files is missing"),
        }

        let identity = Self::generate()?;
        // Key first: if we stop between the two writes, the old certificate
        // no longer matches the key and the next start regenerates both.
        write_atomically(&key_path, identity.private_key_pem().as_bytes(), KEY_MODE)?;
        write_atomically(
            &certificate_path,
            identity.certificate_pem().as_bytes(),
            CERTIFICATE_MODE,
        )?;
        info!(device_id = %identity.device_id, "created a new device identity");
        Ok((identity, true))
    }

    /// A new identity with a fresh device ID, held only in memory.
    pub fn generate() -> Result<Self, StoreError> {
        let device_id = device_id::generate();
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(StoreError::Generate)?;

        let mut params = CertificateParams::default();
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, device_id.as_str());
        params
            .distinguished_name
            .push(DnType::OrganizationalUnitName, ORGANIZATIONAL_UNIT);
        params
            .distinguished_name
            .push(DnType::OrganizationName, ORGANIZATION);
        // A year of slack covers a peer whose clock runs behind; ten years
        // ahead is what the KDE apps use, and expiry forces a re-pair.
        let now = SystemTime::now();
        params.not_before = (now - YEAR).into();
        params.not_after = (now + 10 * YEAR).into();
        // The defaults already omit every extension, which is what we want:
        // Android validates our certificate as its own trust anchor once
        // paired, and the KDE certificates it is used to have none.

        let certificate = params.self_signed(&key).map_err(StoreError::Generate)?;
        Self::from_parts(
            certificate.der().clone(),
            PrivatePkcs8KeyDer::from(key.serialize_der()),
        )
        .map_err(StoreError::InvalidCertificate)
    }

    fn from_pem(certificate_pem: &[u8], key_pem: &[u8]) -> Result<Self, String> {
        let certificate = CertificateDer::from_pem_slice(certificate_pem)
            .map_err(|error| format!("{CERTIFICATE_FILE}: {error}"))?;
        let key = PrivatePkcs8KeyDer::from_pem_slice(key_pem)
            .map_err(|error| format!("{PRIVATE_KEY_FILE}: {error}"))?;
        let identity = Self::from_parts(certificate, key)?;
        if !parse(&identity.certificate)?.validity().is_valid() {
            return Err("the certificate is outside its validity period".into());
        }
        Ok(identity)
    }

    fn from_parts(
        certificate: CertificateDer<'static>,
        private_key: PrivatePkcs8KeyDer<'static>,
    ) -> Result<Self, String> {
        let device_id = common_name(&certificate).map_err(|error| error.to_string())?;
        if !device_id::is_valid(&device_id) {
            return Err(format!("{device_id:?} is not a valid device ID"));
        }
        let public_key = subject_public_key_info(&certificate).map_err(|e| e.to_string())?;
        let key_pair = KeyPair::try_from(private_key.secret_pkcs8_der())
            .map_err(|error| format!("unusable private key: {error}"))?;
        if key_pair.subject_public_key_info() != public_key {
            return Err("the private key does not belong to the certificate".into());
        }
        Ok(Self {
            device_id,
            certificate,
            private_key,
            public_key,
        })
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// The certificate, DER encoded.
    pub fn certificate_der(&self) -> &CertificateDer<'static> {
        &self.certificate
    }

    /// The private key, PKCS#8 DER encoded.
    pub fn private_key_der(&self) -> &PrivatePkcs8KeyDer<'static> {
        &self.private_key
    }

    /// The certificate's `SubjectPublicKeyInfo`, DER encoded.
    pub fn public_key_der(&self) -> &[u8] {
        &self.public_key
    }

    fn certificate_pem(&self) -> String {
        to_pem("CERTIFICATE", &self.certificate)
    }

    fn private_key_pem(&self) -> String {
        to_pem("PRIVATE KEY", self.private_key.secret_pkcs8_der())
    }
}

/// The `SubjectPublicKeyInfo` of a DER certificate, itself DER encoded. Works
/// for any key type, so for peers with EC and RSA keys alike.
pub fn subject_public_key_info(certificate_der: &[u8]) -> Result<Vec<u8>, StoreError> {
    let certificate = parse(certificate_der).map_err(StoreError::InvalidCertificate)?;
    Ok(certificate.public_key().raw.to_vec())
}

/// The first common name in a DER certificate's subject. For KDE Connect
/// certificates this is the device ID.
pub fn common_name(certificate_der: &[u8]) -> Result<String, StoreError> {
    let certificate = parse(certificate_der).map_err(StoreError::InvalidCertificate)?;
    let name = certificate
        .subject()
        .iter_common_name()
        .next()
        .ok_or_else(|| StoreError::InvalidCertificate("no common name".into()))?;
    name.as_str()
        .map(str::to_owned)
        .map_err(|error| StoreError::InvalidCertificate(format!("common name: {error}")))
}

fn parse(der: &[u8]) -> Result<X509Certificate<'_>, String> {
    match X509Certificate::from_der(der) {
        Ok(([], certificate)) => Ok(certificate),
        Ok(_) => Err("trailing data after the certificate".into()),
        Err(error) => Err(error.to_string()),
    }
}

fn to_pem(label: &str, der: &[u8]) -> String {
    let encoded = BASE64.encode(der);
    let mut pem = format!("-----BEGIN {label}-----\n");
    for line in encoded.as_bytes().chunks(64) {
        // Base64 output is ASCII, so every chunk is valid UTF-8.
        pem.push_str(std::str::from_utf8(line).unwrap_or_default());
        pem.push('\n');
    }
    pem.push_str(&format!("-----END {label}-----\n"));
    pem
}

/// A device this one is paired with.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedDevice {
    pub device_id: String,
    /// The certificate pinned at pairing, DER encoded.
    #[serde(with = "base64_der")]
    pub certificate: Vec<u8>,
    pub name: String,
    pub device_type: String,
    pub protocol_version: u32,
    /// Where the device was last reached. Announcements also go straight
    /// there, because a phone with its screen off often ignores broadcasts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_address: Option<std::net::IpAddr>,
}

impl fmt::Debug for TrustedDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrustedDevice")
            .field("device_id", &self.device_id)
            .field("name", &self.name)
            .field("device_type", &self.device_type)
            .field("protocol_version", &self.protocol_version)
            .field(
                "certificate",
                &format_args!("{} bytes", self.certificate.len()),
            )
            .finish()
    }
}

mod base64_der {
    use super::BASE64;
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    pub fn serialize<S: Serializer>(der: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&BASE64.encode(der))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        BASE64.decode(encoded).map_err(D::Error::custom)
    }
}

#[derive(Serialize, Deserialize)]
struct TrustStoreFile {
    version: u32,
    devices: Vec<TrustedDevice>,
}

const TRUST_STORE_VERSION: u32 = 1;

/// The devices this one is paired with, kept in `trusted_devices.json`.
///
/// Every mutation is written to disk before it returns. If the write fails,
/// the store in memory is left as it was, so memory and disk never disagree.
#[derive(Debug)]
pub struct TrustStore {
    dir: PathBuf,
    devices: BTreeMap<String, TrustedDevice>,
}

impl TrustStore {
    /// Loads the store kept in `dir`. A missing file is an empty store; a file
    /// that cannot be read or parsed is an error, so that a damaged file never
    /// silently unpairs every device.
    pub fn load(dir: &Path) -> Result<Self, StoreError> {
        let path = dir.join(TRUSTED_DEVICES_FILE);
        let mut devices = BTreeMap::new();
        if let Some(bytes) = read_optional(&path)? {
            let corrupt = |reason: String| StoreError::CorruptTrustStore {
                path: path.clone(),
                reason,
            };
            let file: TrustStoreFile =
                serde_json::from_slice(&bytes).map_err(|error| corrupt(error.to_string()))?;
            if file.version != TRUST_STORE_VERSION {
                return Err(corrupt(format!("unknown version {}", file.version)));
            }
            for device in file.devices {
                let id = device.device_id.clone();
                if devices.insert(id.clone(), device).is_some() {
                    return Err(corrupt(format!("device {id} is listed twice")));
                }
            }
        }
        Ok(Self {
            dir: dir.to_owned(),
            devices,
        })
    }

    pub fn get(&self, device_id: &str) -> Option<&TrustedDevice> {
        self.devices.get(device_id)
    }

    pub fn contains(&self, device_id: &str) -> bool {
        self.devices.contains_key(device_id)
    }

    /// Devices in order of device ID.
    pub fn iter(&self) -> impl Iterator<Item = &TrustedDevice> {
        self.devices.values()
    }

    pub fn len(&self) -> usize {
        self.devices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    /// Adds `device`, replacing any device with the same ID, and returns the
    /// one it replaced.
    pub fn insert(&mut self, device: TrustedDevice) -> Result<Option<TrustedDevice>, StoreError> {
        let mut devices = self.devices.clone();
        let previous = devices.insert(device.device_id.clone(), device);
        self.commit(devices)?;
        Ok(previous)
    }

    pub fn remove(&mut self, device_id: &str) -> Result<Option<TrustedDevice>, StoreError> {
        if !self.devices.contains_key(device_id) {
            return Ok(None);
        }
        let mut devices = self.devices.clone();
        let removed = devices.remove(device_id);
        self.commit(devices)?;
        Ok(removed)
    }

    pub fn clear(&mut self) -> Result<(), StoreError> {
        self.commit(BTreeMap::new())
    }

    fn commit(&mut self, devices: BTreeMap<String, TrustedDevice>) -> Result<(), StoreError> {
        let file = TrustStoreFile {
            version: TRUST_STORE_VERSION,
            devices: devices.values().cloned().collect(),
        };
        let mut json = serde_json::to_vec_pretty(&file).map_err(StoreError::Serialize)?;
        json.push(b'\n');
        create_private_dir(&self.dir)?;
        write_atomically(
            &self.dir.join(TRUSTED_DEVICES_FILE),
            &json,
            TRUST_STORE_MODE,
        )?;
        self.devices = devices;
        Ok(())
    }
}

fn create_private_dir(dir: &Path) -> Result<(), StoreError> {
    DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(dir)
        .map_err(|error| StoreError::io(dir, error))
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(StoreError::io(path, error)),
    }
}

/// Tightens a file's permissions if someone loosened them.
fn restrict_mode(path: &Path, mode: u32) -> Result<(), StoreError> {
    let metadata = fs::metadata(path).map_err(|error| StoreError::io(path, error))?;
    if metadata.permissions().mode() & 0o777 & !mode != 0 {
        warn!(path = %path.display(), "tightening permissions");
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|error| StoreError::io(path, error))?;
    }
    Ok(())
}

/// Replaces `path` with `contents` so that readers see the old file or the
/// new one, never a mix. The temporary file is created with `mode`, so a key
/// is never readable by others, even for an instant.
fn write_atomically(path: &Path, contents: &[u8], mode: u32) -> Result<(), StoreError> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let temp = dir.join(format!(".{name}.{}.{nonce}.tmp", std::process::id()));

    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temp);
        return Err(StoreError::io(path, error));
    }
    // Make the rename itself durable; failing here loses nothing already
    // written, so it is not worth an error.
    if let Ok(dir) = fs::File::open(dir) {
        let _ = dir.sync_all();
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod testing {
    use std::fs;
    use std::path::{Path, PathBuf};

    /// A directory under the system temp dir, removed on drop.
    pub(crate) struct TempDir(PathBuf);

    impl TempDir {
        pub(crate) fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "landline-test-{}-{}",
                std::process::id(),
                crate::device_id::generate()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }

        /// A path inside the temp dir that does not exist yet, so tests also
        /// cover creating the store directory.
        pub(crate) fn store(&self) -> PathBuf {
            self.0.join("store")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::TempDir;
    use super::*;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn creates_an_identity_with_private_files() {
        let temp = TempDir::new();
        let dir = temp.store();
        let (identity, created) = LocalIdentity::load_or_create(&dir).unwrap();
        assert!(created);
        assert!(device_id::is_valid(identity.device_id()));
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(PRIVATE_KEY_FILE)), 0o600);
        assert!(dir.join(CERTIFICATE_FILE).exists());
        // No temporary files are left behind.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
    }

    #[test]
    fn reload_gives_the_same_identity() {
        let temp = TempDir::new();
        let (first, _) = LocalIdentity::load_or_create(&temp.store()).unwrap();
        let (second, created) = LocalIdentity::load_or_create(&temp.store()).unwrap();
        assert!(!created);
        assert_eq!(first.device_id(), second.device_id());
        assert_eq!(first.certificate_der(), second.certificate_der());
        assert_eq!(
            first.private_key_der().secret_pkcs8_der(),
            second.private_key_der().secret_pkcs8_der()
        );
    }

    #[test]
    fn reload_tightens_a_loosened_key_file() {
        let temp = TempDir::new();
        LocalIdentity::load_or_create(&temp.store()).unwrap();
        let key = temp.store().join(PRIVATE_KEY_FILE);
        fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
        LocalIdentity::load_or_create(&temp.store()).unwrap();
        assert_eq!(mode(&key), 0o600);
    }

    #[test]
    fn generated_certificate_matches_the_kde_shape() {
        let identity = LocalIdentity::generate().unwrap();
        let certificate = parse(identity.certificate_der()).unwrap();
        let subject: Vec<String> = certificate
            .subject()
            .iter_attributes()
            .map(|attribute| attribute.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(subject, [identity.device_id(), "KDE Connect", "KDE"]);
        assert_eq!(certificate.subject(), certificate.issuer());
        assert!(certificate.extensions().is_empty());

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let validity = certificate.validity();
        let not_before = validity.not_before.timestamp() as u64;
        let not_after = validity.not_after.timestamp() as u64;
        let day = 24 * 60 * 60;
        assert!(not_before.abs_diff(now.as_secs() - YEAR.as_secs()) < day);
        assert!(not_after.abs_diff(now.as_secs() + 10 * YEAR.as_secs()) < day);
    }

    #[test]
    fn public_key_and_common_name_helpers_read_the_certificate() {
        let identity = LocalIdentity::generate().unwrap();
        let certificate = identity.certificate_der();
        assert_eq!(common_name(certificate).unwrap(), identity.device_id());
        assert_eq!(
            subject_public_key_info(certificate).unwrap(),
            identity.public_key_der()
        );
        let key = KeyPair::try_from(identity.private_key_der().secret_pkcs8_der()).unwrap();
        assert_eq!(key.subject_public_key_info(), identity.public_key_der());
        assert!(common_name(b"not a certificate").is_err());
        assert!(subject_public_key_info(&[]).is_err());
    }

    fn assert_regenerates(damage: impl FnOnce(&Path)) {
        let temp = TempDir::new();
        let (first, _) = LocalIdentity::load_or_create(&temp.store()).unwrap();
        damage(&temp.store());
        let (second, created) = LocalIdentity::load_or_create(&temp.store()).unwrap();
        assert!(created);
        assert_ne!(first.device_id(), second.device_id());
        let (third, created) = LocalIdentity::load_or_create(&temp.store()).unwrap();
        assert!(!created);
        assert_eq!(second.device_id(), third.device_id());
        assert_eq!(mode(&temp.store().join(PRIVATE_KEY_FILE)), 0o600);
    }

    #[test]
    fn regenerates_when_a_file_is_missing() {
        assert_regenerates(|dir| fs::remove_file(dir.join(CERTIFICATE_FILE)).unwrap());
        assert_regenerates(|dir| fs::remove_file(dir.join(PRIVATE_KEY_FILE)).unwrap());
    }

    #[test]
    fn regenerates_when_a_file_is_unparseable() {
        assert_regenerates(|dir| fs::write(dir.join(CERTIFICATE_FILE), "garbage").unwrap());
        assert_regenerates(|dir| fs::write(dir.join(PRIVATE_KEY_FILE), "garbage").unwrap());
    }

    #[test]
    fn regenerates_when_the_key_belongs_to_another_certificate() {
        assert_regenerates(|dir| {
            let other = LocalIdentity::generate().unwrap();
            fs::write(dir.join(PRIVATE_KEY_FILE), other.private_key_pem()).unwrap();
        });
    }

    fn write_identity(dir: &Path, params: CertificateParams) {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let certificate = params.self_signed(&key).unwrap();
        fs::write(dir.join(CERTIFICATE_FILE), certificate.pem()).unwrap();
        fs::write(dir.join(PRIVATE_KEY_FILE), key.serialize_pem()).unwrap();
    }

    #[test]
    fn regenerates_when_the_common_name_is_not_a_device_id() {
        assert_regenerates(|dir| {
            let mut params = CertificateParams::default();
            params.distinguished_name = rcgen::DistinguishedName::new();
            params.distinguished_name.push(DnType::CommonName, "laptop");
            write_identity(dir, params);
        });
    }

    #[test]
    fn regenerates_when_the_certificate_has_expired() {
        assert_regenerates(|dir| {
            let mut params = CertificateParams::default();
            params.distinguished_name = rcgen::DistinguishedName::new();
            params
                .distinguished_name
                .push(DnType::CommonName, device_id::generate());
            params.not_before = rcgen::date_time_ymd(2000, 1, 1);
            params.not_after = rcgen::date_time_ymd(2001, 1, 1);
            write_identity(dir, params);
        });
    }

    #[test]
    fn regenerates_when_the_certificate_is_not_yet_valid() {
        assert_regenerates(|dir| {
            let mut params = CertificateParams::default();
            params.distinguished_name = rcgen::DistinguishedName::new();
            params
                .distinguished_name
                .push(DnType::CommonName, device_id::generate());
            params.not_before = (SystemTime::now() + YEAR).into();
            write_identity(dir, params);
        });
    }

    #[test]
    fn an_unreadable_file_is_an_error_not_a_new_identity() {
        let temp = TempDir::new();
        LocalIdentity::load_or_create(&temp.store()).unwrap();
        let certificate = temp.store().join(CERTIFICATE_FILE);
        fs::remove_file(&certificate).unwrap();
        fs::create_dir(&certificate).unwrap(); // reading a directory fails
        assert!(matches!(
            LocalIdentity::load_or_create(&temp.store()),
            Err(StoreError::Io { .. })
        ));
    }

    fn device(id: &str, name: &str) -> TrustedDevice {
        TrustedDevice {
            device_id: id.into(),
            certificate: LocalIdentity::generate()
                .unwrap()
                .certificate_der()
                .to_vec(),
            name: name.into(),
            device_type: "phone".into(),
            protocol_version: 8,
            last_address: None,
        }
    }

    #[test]
    fn missing_trust_store_is_empty() {
        let temp = TempDir::new();
        let store = TrustStore::load(&temp.store()).unwrap();
        assert!(store.is_empty());
        assert!(!temp.store().exists());
    }

    #[test]
    fn trust_store_round_trips() {
        let temp = TempDir::new();
        let a = device("a".repeat(32).as_str(), "Phone");
        let b = device("b".repeat(32).as_str(), "Tablet");

        let mut store = TrustStore::load(&temp.store()).unwrap();
        assert_eq!(store.insert(b.clone()).unwrap(), None);
        assert_eq!(store.insert(a.clone()).unwrap(), None);
        let path = temp.store().join(TRUSTED_DEVICES_FILE);
        assert_eq!(mode(&path), 0o600);

        let mut reloaded = TrustStore::load(&temp.store()).unwrap();
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded.get(&a.device_id), Some(&a));
        let ids: Vec<&str> = reloaded.iter().map(|d| d.device_id.as_str()).collect();
        assert_eq!(ids, [a.device_id.as_str(), b.device_id.as_str()]);

        let renamed = TrustedDevice {
            name: "Renamed".into(),
            ..a.clone()
        };
        assert_eq!(reloaded.insert(renamed.clone()).unwrap(), Some(a.clone()));
        assert_eq!(reloaded.remove(&b.device_id).unwrap(), Some(b.clone()));
        assert_eq!(reloaded.remove(&b.device_id).unwrap(), None);

        let again = TrustStore::load(&temp.store()).unwrap();
        assert_eq!(again.iter().collect::<Vec<_>>(), [&renamed]);

        reloaded.clear().unwrap();
        assert!(TrustStore::load(&temp.store()).unwrap().is_empty());
        // Only the store file remains; temporary files were renamed away.
        assert_eq!(fs::read_dir(temp.store()).unwrap().count(), 1);
    }

    #[test]
    fn certificates_are_stored_as_base64_der() {
        let temp = TempDir::new();
        let a = device("a".repeat(32).as_str(), "Phone");
        TrustStore::load(&temp.store())
            .unwrap()
            .insert(a.clone())
            .unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(temp.store().join(TRUSTED_DEVICES_FILE)).unwrap())
                .unwrap();
        let stored = json["devices"][0]["certificate"].as_str().unwrap();
        assert_eq!(BASE64.decode(stored).unwrap(), a.certificate);
    }

    #[test]
    fn corrupt_trust_store_is_an_error() {
        let temp = TempDir::new();
        fs::create_dir(temp.store()).unwrap();
        let path = temp.store().join(TRUSTED_DEVICES_FILE);
        for contents in [
            "{not json",
            r#"{"version": 99, "devices": []}"#,
            r#"{"version": 1, "devices": [{"device_id": "x"}]}"#,
            r#"{"version": 1, "devices": [{"device_id": "x", "certificate": "!!",
                "name": "n", "device_type": "phone", "protocol_version": 8}]}"#,
        ] {
            fs::write(&path, contents).unwrap();
            assert!(
                matches!(
                    TrustStore::load(&temp.store()),
                    Err(StoreError::CorruptTrustStore { .. })
                ),
                "{contents}"
            );
        }
    }

    #[test]
    fn failed_write_leaves_the_store_unchanged() {
        let temp = TempDir::new();
        let mut store = TrustStore::load(&temp.store()).unwrap();
        store.insert(device(&"a".repeat(32), "Phone")).unwrap();
        // Make the directory unwritable so the temporary file cannot be made.
        fs::set_permissions(temp.store(), fs::Permissions::from_mode(0o500)).unwrap();
        let result = store.insert(device(&"b".repeat(32), "Tablet"));
        fs::set_permissions(temp.store(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(result, Err(StoreError::Io { .. })));
        assert_eq!(store.len(), 1);
    }
}
