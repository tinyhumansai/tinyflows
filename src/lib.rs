//! Core library surface for **tinyflows** — a Rust-native workflow
//! engine.
//!
//! tinyflows models an automation as a [`model::WorkflowGraph`]: a directed graph
//! of typed [`model::Node`]s connected by [`model::Edge`]s. A [`compiler::compile`]
//! step validates the graph and (from stage A1) lowers it onto the
//! [`tinyagents`](https://crates.io/crates/tinyagents) state-graph engine, which
//! the [`engine::run`] entry point drives.
//!
//! The crate is deliberately **host-agnostic**: anything that touches the outside
//! world — LLM calls, integration tools, HTTP, code execution, persistence — is
//! expressed through the [`caps`] capability traits that the embedding
//! application implements.
//!
//! ```
//! assert_eq!(tinyflows::product_name(), "tinyflows");
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod browser;
pub mod caps;
pub mod catalog;
pub mod companion;
pub mod compiler;
pub mod data;
pub mod engine;
pub mod error;
pub mod expr;
pub mod graph_ops;
pub mod migrate;
pub mod model;
pub mod nodes;
pub mod observability;
pub mod validate;

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

    #[test]
    fn version_looks_like_semver() {
        // At least `major.minor.patch`, each a run of digits.
        let core = VERSION.split(['-', '+']).next().expect("version core");
        let parts: Vec<&str> = core.split('.').collect();
        assert!(
            parts.len() >= 3,
            "version {VERSION:?} should have at least major.minor.patch"
        );
        for part in &parts[..3] {
            assert!(
                !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()),
                "version {VERSION:?} component {part:?} should be numeric"
            );
        }
    }

    #[test]
    fn crate_name_and_product_name_agree() {
        assert_eq!(CRATE_NAME, "tinyflows");
        assert_eq!(product_name(), CRATE_NAME);
    }
}
