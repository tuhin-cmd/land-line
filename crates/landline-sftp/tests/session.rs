mod common;

use std::borrow::Cow;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use common::{Options, TestServer};
use landline_sftp::{ClientKey, Endpoint, Session, SftpError, Timeouts};
use russh::keys::{Algorithm, EcdsaCurve, HashAlg, PrivateKey};
use russh::{Preferred, cipher, mac};

const BIG: usize = 3 * 1024 * 1024 + 12_345;

/// Deterministic bytes that do not repeat on any short period, so an
/// out-of-order chunk would show up.
fn data(len: usize) -> Vec<u8> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

fn names(entries: &[landline_sftp::Entry]) -> Vec<&str> {
    entries.iter().map(|e| e.name.as_str()).collect()
}

fn dir_contents(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Records progress calls and checks they make sense.
#[derive(Default)]
struct Progress(Vec<(u64, Option<u64>)>);

impl Progress {
    fn check(&self, total: u64, elapsed: Duration) {
        let calls = &self.0;
        assert_eq!(calls.first(), Some(&(0, Some(total))));
        assert_eq!(calls.last(), Some(&(total, Some(total))));
        assert!(calls.windows(2).all(|w| w[0].0 <= w[1].0), "{calls:?}");
        // Start, end, and at most one per 250 ms in between.
        let allowed = 2 + (elapsed.as_millis() / 250) as usize + 1;
        assert!(
            calls.len() <= allowed,
            "{} calls in {elapsed:?}",
            calls.len()
        );
    }
}

#[tokio::test]
async fn lists_folders_first_then_by_name_ignoring_case() {
    let server = TestServer::start().await;
    for dir in ["zeta", "Alpha", "beta"] {
        fs::create_dir(server.path(dir)).unwrap();
    }
    fs::write(server.path("b.txt"), b"bb").unwrap();
    fs::write(server.path("A.txt"), b"a").unwrap();
    fs::write(server.path(".nomedia"), b"").unwrap();

    let session = Session::connect(&server.endpoint()).await.unwrap();
    let entries = session.list_dir("/").await.unwrap();

    assert_eq!(
        names(&entries),
        ["Alpha", "beta", "zeta", ".nomedia", "A.txt", "b.txt"]
    );
    let b = &entries[5];
    assert_eq!(b.path, "/b.txt");
    assert!(!b.is_dir);
    assert_eq!(b.size, 2);
    assert!(b.modified_secs.is_some_and(|t| t > 1_600_000_000));
    assert!(entries[0].is_dir);
    assert_eq!(entries[0].path, "/Alpha");
    session.close().await;
}

#[tokio::test]
async fn lists_an_empty_folder_and_nested_paths() {
    let server = TestServer::start().await;
    fs::create_dir_all(server.path("DCIM/Camera")).unwrap();
    fs::write(server.path("DCIM/Camera/IMG_0001.jpg"), b"jpeg").unwrap();
    fs::create_dir(server.path("Empty")).unwrap();

    let session = Session::connect(&server.endpoint()).await.unwrap();
    assert!(session.list_dir("/Empty").await.unwrap().is_empty());
    let camera = session.list_dir("/DCIM/Camera/").await.unwrap();
    assert_eq!(camera[0].path, "/DCIM/Camera/IMG_0001.jpg");
    assert!(matches!(
        session.list_dir("/Missing").await,
        Err(SftpError::NotFound(p)) if p == "/Missing"
    ));
    // A failed request does not end the session.
    assert!(!session.is_closed());
    assert_eq!(session.list_dir("/").await.unwrap().len(), 2);
}

#[tokio::test]
async fn wrong_password_is_an_auth_error() {
    let server = TestServer::start().await;
    let mut endpoint = server.endpoint();
    endpoint.password = "wrong".into();
    let error = Session::connect(&endpoint).await.err().unwrap();
    assert!(matches!(error, SftpError::Auth), "{error:?}");

    endpoint.password.clear();
    let error = Session::connect(&endpoint).await.err().unwrap();
    assert!(matches!(error, SftpError::Auth), "{error:?}");
}

#[tokio::test]
async fn logs_in_with_the_client_key_when_there_is_no_password() {
    let key = PrivateKey::random(
        &mut rand::rng(),
        Algorithm::Ecdsa {
            curve: EcdsaCurve::NistP256,
        },
    )
    .unwrap();
    let server = TestServer::start_with(Options {
        client_key: Some(key.public_key().clone()),
        ..Options::default()
    })
    .await;
    let der = russh::keys::pkcs8::encode_pkcs8(&key).unwrap();
    let mut endpoint = server
        .endpoint()
        .with_client_key(ClientKey::from_pkcs8_der(&der).unwrap());
    endpoint.password.clear();

    let session = Session::connect(&endpoint).await.unwrap();
    session.list_dir("/").await.unwrap();

    // A wrong password still falls back to the key.
    endpoint.password = "stale".into();
    Session::connect(&endpoint).await.unwrap();
}

#[tokio::test]
async fn checks_the_host_key_against_the_paired_phone() {
    let server = TestServer::start().await;
    let endpoint = server.endpoint().with_host_key(server.host_key_spki());
    let session = Session::connect(&endpoint).await.unwrap();
    session.list_dir("/").await.unwrap();

    let other = TestServer::start().await;
    let endpoint = server.endpoint().with_host_key(other.host_key_spki());
    let error = Session::connect(&endpoint).await.err().unwrap();
    assert!(matches!(error, SftpError::HostKeyMismatch), "{error:?}");
}

#[tokio::test]
async fn downloads_a_large_file_exactly() {
    let server = TestServer::start().await;
    let content = data(BIG);
    fs::write(server.path("video.mp4"), &content).unwrap();
    let local = tempfile::tempdir().unwrap();
    let destination = local.path().join("video.mp4");
    fs::write(&destination, b"old version").unwrap();

    let session = Session::connect(&server.endpoint()).await.unwrap();
    let mut progress = Progress::default();
    let started = Instant::now();
    session
        .download("/video.mp4", &destination, |done, total| {
            progress.0.push((done, total))
        })
        .await
        .unwrap();

    assert!(fs::read(&destination).unwrap() == content);
    progress.check(BIG as u64, started.elapsed());
    assert_eq!(dir_contents(local.path()), ["video.mp4"]);
}

#[tokio::test]
async fn failed_download_leaves_nothing_behind() {
    let server = TestServer::start().await;
    fs::create_dir(server.path("folder")).unwrap();
    let local = tempfile::tempdir().unwrap();
    let session = Session::connect(&server.endpoint()).await.unwrap();

    let error = session
        .download("/missing.jpg", &local.path().join("missing.jpg"), |_, _| {})
        .await
        .err()
        .unwrap();
    assert!(
        matches!(&error, SftpError::NotFound(p) if p == "/missing.jpg"),
        "{error:?}"
    );

    let error = session
        .download(
            "/a.jpg",
            &local.path().join("no-such-folder/a.jpg"),
            |_, _| {},
        )
        .await
        .err()
        .unwrap();
    assert!(matches!(error, SftpError::NotFound(_)), "{error:?}");

    fs::write(server.path("a.jpg"), b"x").unwrap();
    let error = session
        .download(
            "/a.jpg",
            &local.path().join("no-such-folder/a.jpg"),
            |_, _| {},
        )
        .await
        .err()
        .unwrap();
    assert!(matches!(error, SftpError::Local { .. }), "{error:?}");

    assert!(dir_contents(local.path()).is_empty());
}

#[tokio::test]
async fn uploads_a_large_file_exactly() {
    let server = TestServer::start().await;
    fs::create_dir(server.path("Download")).unwrap();
    fs::write(server.path("Download/report.pdf"), b"previous").unwrap();
    let content = data(BIG);
    let local = tempfile::tempdir().unwrap();
    let source = local.path().join("report.pdf");
    fs::write(&source, &content).unwrap();

    let session = Session::connect(&server.endpoint()).await.unwrap();
    let mut progress = Progress::default();
    let started = Instant::now();
    session
        .upload(&source, "/Download/report.pdf", |done, total| {
            progress.0.push((done, total))
        })
        .await
        .unwrap();

    assert!(fs::read(server.path("Download/report.pdf")).unwrap() == content);
    progress.check(BIG as u64, started.elapsed());

    let error = session
        .upload(&local.path().join("absent"), "/Download/absent", |_, _| {})
        .await
        .err()
        .unwrap();
    assert!(matches!(error, SftpError::Local { .. }), "{error:?}");
    let error = session
        .upload(&source, "/NoSuchFolder/report.pdf", |_, _| {})
        .await
        .err()
        .unwrap();
    assert!(matches!(error, SftpError::NotFound(_)), "{error:?}");
}

#[tokio::test]
async fn creates_renames_and_removes() {
    let server = TestServer::start().await;
    let session = Session::connect(&server.endpoint()).await.unwrap();

    session.create_dir("/Trip").await.unwrap();
    assert!(server.path("Trip").is_dir());
    fs::write(server.path("Trip/a.jpg"), b"a").unwrap();
    fs::write(server.path("Trip/b.jpg"), b"b").unwrap();

    session.rename("/Trip/a.jpg", "/Trip/c.jpg").await.unwrap();
    assert!(server.path("Trip/c.jpg").is_file());
    // Renaming onto an existing name fails rather than overwriting.
    assert!(session.rename("/Trip/b.jpg", "/Trip/c.jpg").await.is_err());
    assert_eq!(fs::read(server.path("Trip/c.jpg")).unwrap(), b"a");
    assert!(matches!(
        session.rename("/Trip/zzz.jpg", "/Trip/y.jpg").await,
        Err(SftpError::NotFound(p)) if p == "/Trip/zzz.jpg"
    ));

    // A folder with something in it stays.
    assert!(session.remove("/Trip").await.is_err());
    session.remove("/Trip/b.jpg").await.unwrap();
    session.remove("/Trip/c.jpg").await.unwrap();
    session.rename("/Trip", "/Holiday").await.unwrap();
    session.remove("/Holiday").await.unwrap();
    assert!(dir_contents(server.root.path()).is_empty());
    assert!(matches!(
        session.remove("/Holiday").await,
        Err(SftpError::NotFound(_))
    ));
    assert!(matches!(
        session.create_dir("/a/b").await,
        Err(SftpError::NotFound(_))
    ));
    assert!(!session.is_closed());
}

#[tokio::test]
async fn calls_fail_fast_once_the_server_is_gone() {
    let server = TestServer::start().await;
    let session = Session::connect(&server.endpoint()).await.unwrap();
    session.list_dir("/").await.unwrap();

    server.shutdown();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !session.is_closed() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(session.is_closed());

    let started = Instant::now();
    let error = session.list_dir("/").await.err().unwrap();
    assert!(matches!(error, SftpError::Closed), "{error:?}");
    let local = tempfile::tempdir().unwrap();
    let error = session
        .download("/x", &local.path().join("x"), |_, _| {})
        .await
        .err()
        .unwrap();
    assert!(matches!(error, SftpError::Closed), "{error:?}");
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(error.to_string().contains("start browsing again"));
}

#[tokio::test]
async fn closed_port_fails_within_the_timeout() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);

    let timeouts = Timeouts {
        connect: Duration::from_secs(3),
        ..Timeouts::default()
    };
    let started = Instant::now();
    let error = Session::connect_with(&Endpoint::new(address, "kdeconnect", "pw"), timeouts)
        .await
        .err()
        .unwrap();
    assert!(matches!(error, SftpError::Connect(_)), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn silent_server_times_out() {
    // Accepts the TCP connection and never says anything, like a phone
    // whose app froze.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let hold = tokio::spawn(async move {
        let (_socket, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    let timeouts = Timeouts {
        connect: Duration::from_millis(500),
        ..Timeouts::default()
    };
    let started = Instant::now();
    let error = Session::connect_with(&Endpoint::new(address, "kdeconnect", "pw"), timeouts)
        .await
        .err()
        .unwrap();
    assert!(matches!(error, SftpError::Timeout), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
    hold.abort();
}

/// A round trip against a server limited to what the phone is expected to
/// offer, so the client's algorithm list is what makes it work.
async fn round_trip_with(options: Options) {
    let server = TestServer::start_with(options).await;
    let content = data(200_000);
    fs::write(server.path("photo.jpg"), &content).unwrap();
    let local = tempfile::tempdir().unwrap();

    let endpoint = server.endpoint().with_host_key(server.host_key_spki());
    let session = Session::connect(&endpoint).await.unwrap();
    assert_eq!(names(&session.list_dir("/").await.unwrap()), ["photo.jpg"]);
    let copy = local.path().join("photo.jpg");
    session
        .download("/photo.jpg", &copy, |_, _| {})
        .await
        .unwrap();
    assert!(fs::read(&copy).unwrap() == content);
    session
        .upload(&copy, "/photo-2.jpg", |_, _| {})
        .await
        .unwrap();
    assert!(fs::read(server.path("photo-2.jpg")).unwrap() == content);
}

#[tokio::test]
async fn works_with_an_ec_phone_server() {
    // Current KDE Connect: Apache SSHD without BouncyCastle, an EC P-256
    // identity key as host key. Restricted to the algorithms every Android
    // version has, none of which russh enables by default for kex.
    round_trip_with(Options {
        preferred: Preferred {
            kex: Cow::Borrowed(&[russh::kex::ECDH_SHA2_NISTP256]),
            key: Cow::Borrowed(&[Algorithm::Ecdsa {
                curve: EcdsaCurve::NistP256,
            }]),
            cipher: Cow::Borrowed(&[cipher::AES_128_CTR]),
            mac: Cow::Borrowed(&[mac::HMAC_SHA256]),
            ..Preferred::default()
        },
        ..Options::default()
    })
    .await;
}

#[tokio::test]
async fn works_with_an_old_rsa_phone_server() {
    // Older installs keep an RSA identity key, and older SSHD releases sign
    // with SHA-1 and offer only SHA-1 key exchange and MACs.
    round_trip_with(Options {
        preferred: Preferred {
            kex: Cow::Borrowed(&[russh::kex::DH_G14_SHA1]),
            key: Cow::Borrowed(&[Algorithm::Rsa { hash: None }]),
            cipher: Cow::Borrowed(&[cipher::AES_128_CTR]),
            mac: Cow::Borrowed(&[mac::HMAC_SHA1]),
            ..Preferred::default()
        },
        host_key: Algorithm::Rsa {
            hash: Some(HashAlg::Sha256),
        },
        ..Options::default()
    })
    .await;
}
