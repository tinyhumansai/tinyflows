//! Document keys: composite ids built from parts, and entity ids from ULIDs.
//!
//! A [`Key`] is a document id that already satisfies the id rules
//! ([`validate_id`]): 1 to [`MAX_ID_LEN`] bytes, no NUL. Two constructors cover
//! the two ways a record gets its id:
//!
//! - [`Key::of`] addresses a record by what it is (`job` / `7f` / a timestamp)
//!   and is reversible by [`Key::split`] until the key outgrows the limit.
//! - [`Key::ulid`] names a new entity with a 26-character id that sorts by
//!   creation time.
//!
//! # Composite form
//!
//! Each part is escaped, then the parts are joined with `/`:
//!
//! | Byte | Written as |
//! | --- | --- |
//! | `%` | `%25` |
//! | `/` | `%2F` |
//! | NUL | `%00` |
//! | a leading `~` in the first part | `%7E` |
//!
//! The last row is what keeps the hashed form unambiguous (below): an ordinary
//! key never starts with `~`.
//!
//! If the escaped key is longer than [`MAX_ID_LEN`] bytes it is replaced by `~`
//! followed by the lowercase RFC 4648 base32 (no padding) of its SHA-256
//! digest, the full 52 characters. An id is a primary key, so the digest is not
//! truncated: 256 bits keep a collision out of reach, where the 130 bits a
//! shortened name would keep are a choice only a segment label can afford. The
//! digest covers the *escaped* key, so two part lists that join to the same
//! escaped text hash alike and none can alias another.
//!
//! ```
//! use tinyflows_storage::{Key, StorageError};
//!
//! let key = Key::of(["job", "a/b"])?;
//! assert_eq!(key.as_str(), "job/a%2Fb");
//! assert_eq!(key.split().unwrap(), ["job", "a/b"]);
//!
//! let long = Key::of(["x".repeat(600)])?;
//! assert!(long.is_hashed());
//! assert_eq!(long.split(), None);
//! # Ok::<(), StorageError>(())
//! ```

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use ulid::Ulid;

use tinystoragedrivers_types::{MAX_ID_LEN, validate_id};
use tinystoragedrivers_types::{Result, StorageError};

/// Marks a key that was replaced by its digest.
const HASH_MARK: char = '~';

/// A validated document id.
///
/// Serializes as a plain JSON string; deserializing applies the id rules, so a
/// stored id that no longer validates is rejected instead of passed along.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key(String);

impl Key {
    /// Join `parts` into one escaped key, hashing it if it exceeds
    /// [`MAX_ID_LEN`] bytes.
    ///
    /// Empty parts are allowed and encode as nothing, so `["a", "", "b"]`
    /// becomes `a//b`.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidInput`](crate::ErrorKind::InvalidInput) when `parts`
    /// is empty, or holds only one empty part (an empty id is not a valid id).
    pub fn of<I, S>(parts: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut escaped = String::new();
        let mut count = 0usize;
        for part in parts {
            if count > 0 {
                escaped.push('/');
            }
            escape_into(part.as_ref(), count == 0, &mut escaped);
            count += 1;
        }
        if count == 0 {
            return Err(StorageError::invalid_input("a key needs at least one part"));
        }
        if escaped.len() > MAX_ID_LEN {
            escaped = format!("{HASH_MARK}{}", base32_lower(&Sha256::digest(&escaped)));
        }
        Self::new(escaped)
    }

    /// Wrap an id that is already in key form, such as one read back from
    /// storage or a hand-written constant.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidInput`](crate::ErrorKind::InvalidInput) under the
    /// rules of [`validate_id`]: empty, longer than [`MAX_ID_LEN`] bytes, or
    /// holding NUL.
    pub fn new(raw: impl Into<String>) -> Result<Self> {
        let raw = raw.into();
        validate_id(&raw)?;
        Ok(Self(raw))
    }

    /// A new ULID: 26 Crockford base32 characters whose lexical order is
    /// creation order, to the millisecond. Ids made within one millisecond
    /// order randomly.
    #[must_use]
    pub fn ulid() -> Self {
        Self(Ulid::generate().to_string())
    }

    /// A ULID stamped with `timestamp_ms` (Unix epoch milliseconds) and a
    /// random tail, for a migration that keeps a record's original creation
    /// time in its new id.
    #[must_use]
    pub fn ulid_at(timestamp_ms: u64) -> Self {
        Self(Ulid::from_parts(timestamp_ms, Ulid::generate().random()).to_string())
    }

    /// The parts this key was built from, unescaped.
    ///
    /// `None` for a hashed key (the parts are not recoverable) and for text
    /// that is not valid escaped output: a `%` that does not start `%25`,
    /// `%2F`, `%00` or `%7E`.
    #[must_use]
    pub fn split(&self) -> Option<Vec<String>> {
        if self.is_hashed() {
            return None;
        }
        self.0.split('/').map(unescape).collect()
    }

    /// Whether this key is the digest of a longer one.
    #[must_use]
    pub fn is_hashed(&self) -> bool {
        self.0.starts_with(HASH_MARK)
    }

    /// The key as text, the form stored as the document id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn escape_into(part: &str, first: bool, out: &mut String) {
    for (i, c) in part.chars().enumerate() {
        match c {
            '%' => out.push_str("%25"),
            '/' => out.push_str("%2F"),
            '\0' => out.push_str("%00"),
            HASH_MARK if first && i == 0 => out.push_str("%7E"),
            c => out.push(c),
        }
    }
}

fn unescape(segment: &str) -> Option<String> {
    let mut out = String::with_capacity(segment.len());
    let mut rest = segment;
    while let Some(at) = rest.find('%') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let (decoded, len) = [("%25", '%'), ("%2F", '/'), ("%00", '\0'), ("%7E", '~')]
            .into_iter()
            .find_map(|(escape, c)| tail.starts_with(escape).then_some((c, escape.len())))?;
        out.push(decoded);
        rest = &tail[len..];
    }
    out.push_str(rest);
    Some(out)
}

/// RFC 4648 base32, lowercase, without padding.
fn base32_lower(bytes: &[u8]) -> String {
    data_encoding::BASE32_NOPAD
        .encode(bytes)
        .to_ascii_lowercase()
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Key {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Serialize for Key {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Key {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
#[path = "key_tests.rs"]
mod tests;
