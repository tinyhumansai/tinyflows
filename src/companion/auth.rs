//! Pairing-secret persistence and WebSocket upgrade authentication.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// The WebSocket subprotocol prefix carrying the relay protocol version.
pub const PROTOCOL_SUBPROTOCOL: &str = "tinyflows.v1";

/// The WebSocket subprotocol prefix carrying the pairing secret.
pub const AUTH_SUBPROTOCOL_PREFIX: &str = "tinyflows.auth.";

/// Errors produced while authenticating an extension connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// The request did not originate from the paired extension.
    OriginMismatch,
    /// The client did not offer the supported relay protocol.
    ProtocolMismatch,
    /// The authentication subprotocol was absent or malformed.
    MissingAuthentication,
    /// The supplied authentication secret was invalid.
    InvalidAuthentication,
    /// More than one authentication subprotocol was supplied.
    AmbiguousAuthentication,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::OriginMismatch => "websocket origin does not match the paired extension",
            Self::ProtocolMismatch => "no supported relay protocol was offered",
            Self::MissingAuthentication => "authentication websocket subprotocol is missing",
            Self::InvalidAuthentication => "authentication secret is invalid",
            Self::AmbiguousAuthentication => "multiple authentication secrets were supplied",
        })
    }
}

impl std::error::Error for AuthError {}

/// A secret used to pair and authenticate the extension.
///
/// Its debug representation is intentionally redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct PairingSecret(String);

impl PairingSecret {
    /// Parses a pairing secret, rejecting values too small to resist guessing.
    pub fn parse(value: impl Into<String>) -> io::Result<Self> {
        let value = value.into();
        if value.len() < 32 || !value.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "pairing secret must contain at least 32 ASCII alphanumeric characters",
            ));
        }
        Ok(Self(value))
    }

    /// Generates a 256-bit secret from operating-system randomness.
    pub fn generate() -> io::Result<Self> {
        let mut bytes = [0_u8; 32];
        OpenOptions::new()
            .read(true)
            .open("/dev/urandom")?
            .read_exact(&mut bytes)?;
        let mut encoded = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            use std::fmt::Write as _;
            write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
        }
        Ok(Self(encoded))
    }

    /// Returns the secret for placement in a WebSocket subprotocol value.
    pub fn expose(&self) -> &str {
        &self.0
    }

    fn matches(&self, candidate: &str) -> bool {
        let expected = self.0.as_bytes();
        let candidate = candidate.as_bytes();
        let mut difference = expected.len() ^ candidate.len();
        let compared_len = expected.len().max(candidate.len());
        for index in 0..compared_len {
            let left = expected.get(index).copied().unwrap_or_default();
            let right = candidate.get(index).copied().unwrap_or_default();
            difference |= usize::from(left ^ right);
        }
        difference == 0
    }
}

impl std::fmt::Debug for PairingSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PairingSecret([REDACTED])")
    }
}

/// Owner-only persistent storage for the host-local pairing secret.
#[derive(Debug, Clone)]
pub struct SecretStore {
    path: PathBuf,
}

impl SecretStore {
    /// Creates a store targeting `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Returns the secret file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads the current secret, generating it atomically when absent.
    pub fn load_or_create(&self) -> io::Result<PairingSecret> {
        match self.load() {
            Ok(secret) => Ok(secret),
            Err(error) if error.kind() == io::ErrorKind::NotFound => self.rotate(),
            Err(error) => Err(error),
        }
    }

    /// Loads the persisted secret and verifies owner-only permissions on Unix.
    pub fn load(&self) -> io::Result<PairingSecret> {
        let metadata = fs::metadata(&self.path)?;
        verify_owner_only(&metadata)?;
        let value = fs::read_to_string(&self.path)?;
        PairingSecret::parse(value.trim().to_owned())
    }

    /// Replaces the current secret and returns the new value.
    pub fn rotate(&self) -> io::Result<PairingSecret> {
        let secret = PairingSecret::generate()?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
            set_owner_only_directory(parent)?;
        }

        let temporary = self.path.with_extension("tmp");
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        set_owner_only_file_options(&mut options);
        let mut file = options.open(&temporary)?;
        file.write_all(secret.expose().as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, &self.path)?;
        set_owner_only_file(&self.path)?;
        Ok(secret)
    }
}

/// Headers relevant to an incoming WebSocket upgrade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSocketHandshake<'a> {
    /// Exact value of the HTTP `Origin` header.
    pub origin: &'a str,
    /// Values offered in `Sec-WebSocket-Protocol`, already split on commas.
    pub subprotocols: &'a [&'a str],
}

/// The result of successfully authenticating a WebSocket upgrade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedSession {
    /// Subprotocol the server must echo in the upgrade response.
    pub negotiated_subprotocol: &'static str,
}

/// Validates the exact extension origin and secret-bearing subprotocol.
#[derive(Debug, Clone)]
pub struct Authenticator {
    expected_origin: String,
    secret: PairingSecret,
}

impl Authenticator {
    /// Creates an authenticator for a paired Chrome extension id.
    pub fn new(extension_id: &str, secret: PairingSecret) -> io::Result<Self> {
        if extension_id.len() != 32
            || !extension_id
                .bytes()
                .all(|byte| matches!(byte, b'a'..=b'p'))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Chrome extension id must be 32 characters in the range a-p",
            ));
        }
        Ok(Self {
            expected_origin: format!("chrome-extension://{extension_id}"),
            secret,
        })
    }

    /// Authenticates one upgrade without accepting credentials from its URL.
    pub fn authenticate(
        &self,
        handshake: &WebSocketHandshake<'_>,
    ) -> Result<AuthenticatedSession, AuthError> {
        if handshake.origin != self.expected_origin {
            return Err(AuthError::OriginMismatch);
        }
        if !handshake
            .subprotocols
            .iter()
            .any(|value| value.trim() == PROTOCOL_SUBPROTOCOL)
        {
            return Err(AuthError::ProtocolMismatch);
        }

        let candidates = handshake
            .subprotocols
            .iter()
            .map(|value| value.trim())
            .filter_map(|value| value.strip_prefix(AUTH_SUBPROTOCOL_PREFIX))
            .collect::<Vec<_>>();
        let candidate = match candidates.as_slice() {
            [] => return Err(AuthError::MissingAuthentication),
            [candidate] => *candidate,
            _ => return Err(AuthError::AmbiguousAuthentication),
        };
        if !self.secret.matches(candidate) {
            return Err(AuthError::InvalidAuthentication);
        }
        Ok(AuthenticatedSession {
            negotiated_subprotocol: PROTOCOL_SUBPROTOCOL,
        })
    }

    /// Returns the one origin accepted by this authenticator.
    pub fn expected_origin(&self) -> &str {
        &self.expected_origin
    }
}

#[cfg(unix)]
fn set_owner_only_file_options(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_owner_only_file_options(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn set_owner_only_file(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_owner_only_file(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_owner_only_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn verify_owner_only(metadata: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "pairing secret must not be accessible by group or other users",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_owner_only(_metadata: &fs::Metadata) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    const EXTENSION_ID: &str = "abcdefghijklmnopabcdefghijklmnop";
    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    fn authenticator() -> Authenticator {
        Authenticator::new(EXTENSION_ID, PairingSecret::parse(SECRET).unwrap()).unwrap()
    }

    #[test]
    fn authenticates_only_exact_origin_protocol_and_secret() {
        let protocols = [PROTOCOL_SUBPROTOCOL, "tinyflows.auth.0123456789abcdef0123456789abcdef"];
        let result = authenticator().authenticate(&WebSocketHandshake {
            origin: "chrome-extension://abcdefghijklmnopabcdefghijklmnop",
            subprotocols: &protocols,
        });
        assert_eq!(result.unwrap().negotiated_subprotocol, PROTOCOL_SUBPROTOCOL);
    }

    #[test]
    fn rejects_lookalike_origin_and_url_style_auth() {
        let protocols = [PROTOCOL_SUBPROTOCOL];
        assert_eq!(
            authenticator()
                .authenticate(&WebSocketHandshake {
                    origin: "chrome-extension://abcdefghijklmnopabcdefghijklmnop.example",
                    subprotocols: &protocols,
                })
                .unwrap_err(),
            AuthError::OriginMismatch
        );
        assert_eq!(
            authenticator()
                .authenticate(&WebSocketHandshake {
                    origin: "chrome-extension://abcdefghijklmnopabcdefghijklmnop",
                    subprotocols: &protocols,
                })
                .unwrap_err(),
            AuthError::MissingAuthentication
        );
    }

    #[test]
    fn secret_store_round_trips_and_rotates() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("tinyflows-secret-{unique}"));
        let store = SecretStore::new(directory.join("pairing-secret"));
        let first = store.load_or_create().unwrap();
        assert_eq!(store.load().unwrap(), first);
        let second = store.rotate().unwrap();
        assert_ne!(first, second);
        assert_eq!(store.load().unwrap(), second);
        fs::remove_dir_all(directory).unwrap();
    }
}
