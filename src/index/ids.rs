//! Typed identifiers derived from indexed content.
//!
//! - [`ContentHash`]: the lowercase hex SHA-256 of one file's text. Range
//!   handles, edit preconditions and language-server synchronization compare
//!   it.
//! - [`SnapshotId`]: the identity of an indexed file set at one repository
//!   HEAD (`snap_` plus hex). Requests and continuations carry it as a
//!   precondition.
//!
//! Both serialize as their plain string, so the wire format is the string
//! itself. They compare equal to `str` so a client-supplied precondition can
//! be checked without converting it.
use super::handle::hex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::ops::Deref;

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }
        }

        /// A value that crossed the wire or a test fixture; it is not checked.
        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl Deref for $name {
            type Target = str;

            fn deref(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.0 == *other
            }
        }

        impl PartialEq<String> for $name {
            fn eq(&self, other: &String) -> bool {
                &self.0 == other
            }
        }

        impl PartialEq<$name> for str {
            fn eq(&self, other: &$name) -> bool {
                self == other.0
            }
        }

        impl PartialEq<$name> for &str {
            fn eq(&self, other: &$name) -> bool {
                *self == other.0
            }
        }

        impl PartialEq<$name> for String {
            fn eq(&self, other: &$name) -> bool {
                *self == other.0
            }
        }
    };
}

string_id! {
    /// Lowercase hex SHA-256 of a file's text.
    ContentHash
}

string_id! {
    /// Identity of the indexed file set at one repository HEAD.
    SnapshotId
}

impl ContentHash {
    /// The hash of `content`.
    pub fn of(content: &str) -> Self {
        Self(hex(&Sha256::digest(content.as_bytes())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_serializes_as_its_hex_string() {
        let hash = ContentHash::of("x");
        assert_eq!(
            serde_json::to_value(&hash).unwrap(),
            serde_json::json!("2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881")
        );
        let parsed: ContentHash = serde_json::from_str("\"abc\"").unwrap();
        assert_eq!(parsed, "abc");
    }

    #[test]
    fn snapshot_id_compares_with_client_strings() {
        let snapshot = SnapshotId::from("snap_1");
        assert!("snap_1" == snapshot);
        assert!(snapshot != "snap_2");
        assert_eq!(serde_json::to_string(&snapshot).unwrap(), "\"snap_1\"");
    }
}
