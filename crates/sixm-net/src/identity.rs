//! Server certificates: where they come from, how clients pin them, and how
//! they are stored on disk.

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

/// Where the server gets the certificate it presents to clients.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Identity {
    /// A new self-signed certificate on every start. Its fingerprint changes
    /// with every start, so clients cannot pin it.
    #[default]
    Ephemeral,
    /// A certificate kept in this file. It is created on the first start and
    /// reused afterwards, so clients can pin its fingerprint. Missing parent
    /// folders are created.
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
        for (byte, pair) in bytes.iter_mut().zip(digits.chunks_exact(2)) {
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

/// The certificate and private key the server presents.
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

    /// The fingerprint clients must pin to trust this certificate.
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

/// Creates `path` readable by its owner only, then writes `bytes` into it.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = File::create(path)?;
    #[cfg(unix)]
    restrict_to_owner(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(unix)]
fn restrict_to_owner(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

fn identity_error(path: &Path, reason: &str) -> NetError {
    NetError::Identity {
        path: path.to_path_buf(),
        reason: reason.to_owned(),
    }
}
