//! The client against OpenSSH's sftp-server, a second implementation to
//! check the in-process test server's behaviour against.
//!
//! Runs the system `sshd` as the current user on a loopback port with a
//! throwaway configuration, host key and authorized key, all in a temporary
//! folder. Nothing system-wide changes. Ignored by default because it needs
//! `/usr/sbin/sshd`:
//!
//! ```sh
//! cargo test -p landline-sftp --test openssh -- --ignored
//! ```
//!
//! Password login would need PAM and the account's real password, so this
//! logs in with a client key; the password path is covered by the other
//! tests.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use landline_sftp::{ClientKey, Endpoint, HostKey, Session, SftpError};
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, EcdsaCurve, PrivateKey};

const SSHD: &str = "/usr/sbin/sshd";
const SFTP_SERVER: &str = "/usr/libexec/sftp-server";

struct Sshd(Child);

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn ec_key() -> PrivateKey {
    PrivateKey::random(
        &mut rand::rng(),
        Algorithm::Ecdsa {
            curve: EcdsaCurve::NistP256,
        },
    )
    .unwrap()
}

fn write_private(path: &Path, key: &PrivateKey) {
    fs::write(path, key.to_openssh(LineEnding::LF).unwrap().as_bytes()).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn current_user() -> String {
    let out = Command::new("id").arg("-un").output().unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[tokio::test]
#[ignore = "runs the system sshd"]
async fn works_against_openssh_sftp_server() {
    let config_dir = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let data = data_dir.path().canonicalize().unwrap();

    let host_key = ec_key();
    let client_key = ec_key();
    write_private(&config_dir.path().join("host_key"), &host_key);
    fs::write(
        config_dir.path().join("authorized_keys"),
        client_key.public_key().to_openssh().unwrap(),
    )
    .unwrap();

    let port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    };
    let dir = config_dir.path().display();
    fs::write(
        config_dir.path().join("sshd_config"),
        format!(
            "Port {port}\n\
             ListenAddress 127.0.0.1\n\
             HostKey {dir}/host_key\n\
             PidFile {dir}/sshd.pid\n\
             AuthorizedKeysFile {dir}/authorized_keys\n\
             StrictModes no\n\
             UsePAM no\n\
             PasswordAuthentication no\n\
             KbdInteractiveAuthentication no\n\
             PubkeyAuthentication yes\n\
             Subsystem sftp {SFTP_SERVER}\n"
        ),
    )
    .unwrap();
    let _sshd = Sshd(
        Command::new(SSHD)
            .args(["-D", "-e", "-f"])
            .arg(config_dir.path().join("sshd_config"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let address = format!("127.0.0.1:{port}").parse().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while tokio::net::TcpStream::connect(address).await.is_err() {
        assert!(Instant::now() < deadline, "sshd did not start");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let host_spki = common::spki_der(host_key.public_key());
    let endpoint = Endpoint::new(address, current_user(), "")
        .with_client_key(
            ClientKey::from_pkcs8_der(&russh::keys::pkcs8::encode_pkcs8(&client_key).unwrap())
                .unwrap(),
        )
        .with_host_key(HostKey::from_spki_der(&host_spki).unwrap());
    let session = Session::connect(&endpoint).await.unwrap();

    let root = data.to_str().unwrap();
    let join = |name: &str| landline_sftp::join(root, name);
    fs::create_dir(data.join("Pictures")).unwrap();
    fs::write(data.join("b.txt"), b"bb").unwrap();
    fs::write(data.join(".hidden"), b"").unwrap();
    let entries = session.list_dir(root).await.unwrap();
    let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["Pictures", ".hidden", "b.txt"]);
    assert_eq!(entries[2].size, 2);
    assert!(entries[2].modified_secs.is_some());

    let content: Vec<u8> = (0..2_000_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let local = data.join("local.bin");
    fs::write(&local, &content).unwrap();
    let mut last = None;
    session
        .upload(&local, &join("Pictures/up.bin"), |done, total| {
            last = Some((done, total))
        })
        .await
        .unwrap();
    assert_eq!(
        last,
        Some((content.len() as u64, Some(content.len() as u64)))
    );
    assert!(fs::read(data.join("Pictures/up.bin")).unwrap() == content);

    let copy = data.join("copy.bin");
    session
        .download(&join("Pictures/up.bin"), &copy, |_, _| {})
        .await
        .unwrap();
    assert!(fs::read(&copy).unwrap() == content);

    assert!(matches!(
        session
            .download(&join("absent"), &data.join("absent"), |_, _| {})
            .await,
        Err(SftpError::NotFound(_))
    ));
    assert!(!data.join("absent").exists());

    session.create_dir(&join("New")).await.unwrap();
    session
        .rename(&join("Pictures/up.bin"), &join("New/moved.bin"))
        .await
        .unwrap();
    assert!(session.remove(&join("New")).await.is_err());
    session.remove(&join("New/moved.bin")).await.unwrap();
    session.remove(&join("New")).await.unwrap();
    assert!(!data.join("New").exists());

    session.close().await;
}
