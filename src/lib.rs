//! Core library surface for tinyflows.
//!
//! The crate is intentionally small while the workflow runtime takes shape. It
//! exposes stable package identity helpers that the binary and downstream
//! integrations can share.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// The crate name published to crates.io.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

/// The crate version from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Returns the user-facing product name.
pub fn product_name() -> &'static str {
    CRATE_NAME
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_name_matches_crate_name() {
        assert_eq!(product_name(), "tinyflows");
    }

    #[test]
    fn exposes_package_version() {
        assert!(!VERSION.is_empty());
    }
}
