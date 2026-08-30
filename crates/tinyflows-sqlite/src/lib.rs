//! SQLite (and, for drafts, plain-JSON) persistence for a tinyflows host.
//!
//! Two independent stores, deliberately kept apart:
//!
//! - [`flows`] — the saved-flow catalog: definitions, revision history, run
//!   records and their steps, authoring suggestions, and a namespaced key/value
//!   table a host can bind to `tinyflows::caps::StateStore`. One `flows.db`.
//! - [`checkpoint`] — the engine's own durable checkpoint store, so a run can
//!   be interrupted and resumed. Its own `checkpoints.db`.
//! - [`drafts`] — authoring drafts, one JSON file each. Not a table on purpose:
//!   a draft is a working copy, and a file is inspectable and deletable without
//!   a migration.
//!
//! Keeping the checkpoint database separate from the catalog is not tidiness.
//! Checkpoints are written at engine cadence — several times per node, per live
//! run — while the catalog is read by every listing the UI draws. One file
//! would put those two access patterns behind one write lock.
//!
//! # Paths, not host config
//!
//! Every entry point takes a directory as its first argument. The crate does
//! not know how a host chooses that directory, does not read a config file, and
//! creates what it needs on first use. That is what lets a second host adopt
//! this store without adopting anything else.

pub mod checkpoint;
pub mod drafts;
pub mod flows;

#[cfg(test)]
#[path = "checkpoint_tests.rs"]
mod checkpoint_tests;
