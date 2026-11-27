//! Certificates: where the server's and the clients' come from, how clients pin
//! the server's, and how they are stored on disk.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::error::NetError;
use crate::protocol::SERVER_NAME;

/// Length of a fingerprint in bytes: the SHA-256 digest size.
const FINGERPRINT_LEN: usize = 32;
/// Length of a fingerprint written as hexadecimal text.
const FINGERPRINT_HEX_LEN: usize = 2 * FINGERPRINT_LEN;

/// Where a certificate comes from. A server presents one to its clients, and a
/// client presents one to the server, which derives the player's `license`
/// identifier from it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Identity {
    /// A new self-signed certificate for every use: a server gets one per start
    /// and a client one per connection. Its fingerprint changes every time.
    #[default]
    Ephemeral,
    /// A certificate kept in this file. It is created on first use and reused
    /// afterwards, so its fingerprint stays the same. Missing parent folders are
    /// created.
    Persistent(PathBuf),
}

/// SHA-256 digest of a server's certificate, DER encoded. Clients pin it to
/// make sure they talk to the server that logged it.
///
/// Displays as 64 lowercase hexadecimal characters and parses the same text,
/// in either case.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint([u8; FINGERPRINT_LEN]);

impl Fingerprint {
    /// The fingerprint of a DER encoded certificate.
    pub(crate) fn of(certificate: &[u8]) -> Self {
        let digest = ring::digest::digest(&ring::digest::SHA256, certificate);
        let mut bytes = [0_u8; FINGERPRINT_LEN];
        bytes.copy_from_slice(digest.as_ref());
        Self(bytes)
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

/// Text that is not a fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a fingerprint is 64 hexadecimal characters")]
pub struct ParseFingerprintError;

impl FromStr for Fingerprint {
    type Err = ParseFingerprintError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let digits = text.as_bytes();
        if digits.len() != FINGERPRINT_HEX_LEN {
            return Err(ParseFingerprintError);
        }
        let mut bytes = [0_u8; FINGERPRINT_LEN];
        for (byte, pair) in bytes.iter_mut().zip(digits.as_chunks::<2>().0) {
            let high = char::from(pair[0])
                .to_digit(16)
                .ok_or(ParseFingerprintError)?;
            let low = char::from(pair[1])
                .to_digit(16)
                .ok_or(ParseFingerprintError)?;
            *byte = u8::try_from(high * 16 + low).map_err(|_| ParseFingerprintError)?;
        }
        Ok(Self(bytes))
    }
}

/// The certificate and private key of one side of a connection.
pub(crate) struct Credentials {
    certificate: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
}

impl Credentials {
    /// Obtains the credentials for `identity`: a fresh certificate, or the one
    /// stored at the persistent path, which is created when it is missing.
    pub(crate) fn load(identity: &Identity) -> Result<Self, NetError> {
        match identity {
            Identity::Ephemeral => Self::generate(),
            Identity::Persistent(path) => Self::load_or_create(path),
        }
    }

    /// The fingerprint of this certificate.
    pub(crate) fn fingerprint(&self) -> Fingerprint {
        Fingerprint::of(&self.certificate)
    }

    /// Splits the credentials into the certificate and key for TLS.
    pub(crate) fn into_parts(self) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
        (self.certificate, PrivateKeyDer::from(self.key))
    }

    fn generate() -> Result<Self, NetError> {
        let certified = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_owned()])
            .map_err(|err| NetError::Tls(err.to_string()))?;
        Ok(Self {
            certificate: certified.cert.der().clone(),
            key: PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()),
        })
    }

    fn load_or_create(path: &Path) -> Result<Self, NetError> {
        match fs::read(path) {
            Ok(bytes) => Self::decode(&bytes).ok_or_else(|| {
                identity_error(
                    path,
                    "the file is not a SixM identity; delete it to create a new one",
                )
            }),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let credentials = Self::generate()?;
                credentials.store(path)?;
                Ok(credentials)
            }
            Err(err) => Err(identity_error(path, &err.to_string())),
        }
    }

    /// Writes the credentials to `path` through a temporary file, so that a
    /// crash never leaves a half-written identity behind.
    fn store(&self, path: &Path) -> Result<(), NetError> {
        let bytes = self
            .encode()
            .map_err(|reason| identity_error(path, &reason))?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| identity_error(path, &err.to_string()))?;
        }
        let temporary = temporary_path(path);
        let written = write_private(&temporary, &bytes).and_then(|()| fs::rename(&temporary, path));
        written.map_err(|err| {
            let _ = fs::remove_file(&temporary);
            identity_error(path, &err.to_string())
        })
    }

    /// File layout: the certificate and the PKCS#8 key, each preceded by its
    /// length as a `u32` little-endian.
    fn encode(&self) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        for part in [self.certificate.as_ref(), self.key.secret_pkcs8_der()] {
            let len = u32::try_from(part.len())
                .map_err(|_| "the certificate is too large to store".to_owned())?;
            bytes.extend_from_slice(&len.to_le_bytes());
            bytes.extend_from_slice(part);
        }
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let (certificate, rest) = split_part(bytes)?;
        let (key, rest) = split_part(rest)?;
        rest.is_empty().then(|| Self {
            certificate: CertificateDer::from(certificate.to_vec()),
            key: PrivatePkcs8KeyDer::from(key.to_vec()),
        })
    }
}

/// Splits one length-prefixed part off the front of `bytes`.
fn split_part(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let (len, rest) = bytes.split_first_chunk::<4>()?;
    let len = usize::try_from(u32::from_le_bytes(*len)).ok()?;
    (len <= rest.len()).then(|| rest.split_at(len))
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

/// Creates `path` readable by its owner only where the platform allows it, then
/// writes `bytes` into it.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = create_private(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Creates or truncates `path` with mode `0600`, so the key is never readable
/// by other users, not even for a moment.
#[cfg(unix)]
fn create_private(path: &Path) -> io::Result<File> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

/// Creates or truncates `path`, then limits it to its owner before anything
/// is written, so the key never sits in a file other users can read.
#[cfg(windows)]
fn create_private(path: &Path) -> io::Result<File> {
    let file = File::create(path)?;
    restrict_to_owner(path);
    Ok(file)
}

/// Replaces the access list of `path` with one entry: full control for its
/// owner (`OWNER RIGHTS`, SID `S-1-3-4`), with nothing inherited from the
/// folder. `icacls` is run from the system folder by its full path, so no
/// program on `PATH` can stand in for it. A file system without access lists,
/// or a failure of `icacls`, leaves the folder's access list in place, and is
/// logged as a warning.
#[cfg(windows)]
fn restrict_to_owner(path: &Path) {
    use std::process::{Command, Stdio};

    let system_root =
        std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    let icacls = system_root.join("System32").join("icacls.exe");
    let outcome = Command::new(&icacls)
        .arg(path)
        .args(["/inheritance:r", "/grant:r", "*S-1-3-4:(F)"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match outcome {
        Ok(status) if status.success() => {}
        Ok(status) => tracing::warn!(
            "cannot limit {} to its owner: icacls exited with {status}",
            path.display()
        ),
        Err(err) => tracing::warn!(
            "cannot limit {} to its owner: cannot run {}: {err}",
            path.display(),
            icacls.display()
        ),
    }
}

/// Creates or truncates `path`. Platforms other than Unix and Windows have no
/// owner-only mode that the standard library can set, so the file takes the
/// permissions of its folder.
#[cfg(not(any(unix, windows)))]
fn create_private(path: &Path) -> io::Result<File> {
    File::create(path)
}

fn identity_error(path: &Path, reason: &str) -> NetError {
    NetError::Identity {
        path: path.to_path_buf(),
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Credentials, Identity};

    /// Creates a persistent identity in a fresh folder and returns its path.
    fn stored_identity(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let path = dir.path().join("identity.bin");
        let first = Credentials::load(&Identity::Persistent(path.clone())).expect("created");
        let again = Credentials::load(&Identity::Persistent(path.clone())).expect("loaded");
        assert_eq!(
            first.fingerprint(),
            again.fingerprint(),
            "the file is reused"
        );
        path
    }

    #[cfg(unix)]
    #[test]
    fn a_stored_key_is_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let path = stored_identity(&dir);
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(windows)]
    #[test]
    fn a_stored_key_is_limited_to_its_owner() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = stored_identity(&dir);
        let system_root = std::env::var_os("SystemRoot").expect("SystemRoot is set");
        let icacls = std::path::Path::new(&system_root)
            .join("System32")
            .join("icacls.exe");
        let output = std::process::Command::new(icacls)
            .arg(&path)
            .output()
            .expect("icacls runs");
        let listing = String::from_utf8_lossy(&output.stdout);
        // One entry, for the owner, and nothing inherited from the folder.
        assert!(listing.contains("OWNER RIGHTS:(F)"), "{listing}");
        assert!(!listing.contains("(I)"), "{listing}");
    }
}
