//! Server identities: ephemeral and persistent certificates, and the text form
//! of their fingerprints.

use std::fs;
use std::net::SocketAddr;

use sixm_net::{Fingerprint, Identity, NetError, NetServer, ServerConfig};
use tempfile::TempDir;

fn bind(identity: Identity) -> Result<NetServer, NetError> {
    let config = ServerConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        identity,
        ..ServerConfig::default()
    };
    NetServer::bind(config, |_| {})
}

#[test]
fn ephemeral_identities_differ_between_servers() {
    let first = bind(Identity::Ephemeral).expect("server binds");
    let second = bind(Identity::Ephemeral).expect("server binds");
    assert_ne!(first.fingerprint(), second.fingerprint());
}

#[test]
fn persistent_identity_is_created_on_first_bind_and_reused() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("cache").join("identity.bin");

    let created = bind(Identity::Persistent(path.clone())).expect("first bind");
    let fingerprint = created.fingerprint();
    assert!(path.is_file(), "the first bind writes the identity file");
    drop(created);

    let reused = bind(Identity::Persistent(path.clone())).expect("second bind");
    assert_eq!(reused.fingerprint(), fingerprint);
    assert_owner_only(&path);
}

#[cfg(unix)]
fn assert_owner_only(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    let mode = fs::metadata(path)
        .expect("identity file exists")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "the key is readable by its owner only");
}

#[cfg(not(unix))]
fn assert_owner_only(_path: &std::path::Path) {}

#[test]
fn corrupt_identity_file_is_reported_and_left_alone() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("identity.bin");
    fs::write(&path, b"not an identity").expect("file is written");

    assert!(matches!(
        bind(Identity::Persistent(path.clone())),
        Err(NetError::Identity { .. })
    ));
    assert_eq!(fs::read(&path).expect("file is kept"), b"not an identity");
}

#[test]
fn fingerprint_text_round_trips() {
    let server = bind(Identity::Ephemeral).expect("server binds");
    let fingerprint = server.fingerprint();

    let text = fingerprint.to_string();
    assert_eq!(text.len(), 64);
    assert!(
        text.bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    assert_eq!(text.parse::<Fingerprint>().expect("parses"), fingerprint);
    assert_eq!(
        text.to_uppercase().parse::<Fingerprint>().expect("parses"),
        fingerprint
    );
}

#[test]
fn malformed_fingerprint_text_is_rejected() {
    let valid = "ab".repeat(32);
    let too_long = format!("{valid}00");
    let signed = format!("+{}", &valid[1..]);
    let not_hex = format!("g{}", &valid[1..]);
    for bad in [
        "",
        "ab",
        &valid[..62],
        too_long.as_str(),
        signed.as_str(),
        not_hex.as_str(),
    ] {
        assert!(
            bad.parse::<Fingerprint>().is_err(),
            "{bad:?} must not parse"
        );
    }
    assert!(valid.parse::<Fingerprint>().is_ok());
}
