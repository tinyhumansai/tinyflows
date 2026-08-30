//! The saved-workflow **catalog** model for [`tinyflows`].
//!
//! `tinyflows` owns a graph and how to run it. This crate owns the record
//! *around* a graph: what a host has to know to keep a library of workflows
//! rather than a single one.
//!
//! - [`types`] — the domain model: [`Flow`] and its [`FlowRevision`] history,
//!   a [`FlowRun`] and its [`FlowRunStep`]s, an authoring [`FlowDraft`], a
//!   [`FlowSuggestion`], and the [`FlowValidation`] result an authoring surface
//!   shows before a graph is saved.
//! - [`run_registry`] / [`build_registry`] — the in-process registries that
//!   make a live run or a live authoring build cancellable. Plain maps of
//!   cancellation tokens; no runtime, no storage.
//! - [`import`] — best-effort importers that map a foreign automation format
//!   into a [`tinyflows::model::WorkflowGraph`].
//! - [`graph_policy`] — the save/run safety predicates over a graph: whether it
//!   fires unattended, whether it can act on the world, whether it has anything
//!   to do at all.
//!
//! # What is deliberately not here
//!
//! **Storage.** This crate names no database. `tinyflows-sqlite` is one
//! backend; a host with its own catalog table implements persistence itself and
//! still shares this vocabulary.
//!
//! **Host policy.** Which node kinds a host advertises, what a `tool_call` slug
//! resolves to, whether a trigger kind actually dispatches — those are facts
//! about a host, not about a saved flow, and they belong in the host's own
//! overlay on [`tinyflows::catalog`].

pub mod build_registry;
pub mod graph_policy;
pub mod import;
pub mod run_registry;
pub mod types;

pub use types::{
    DraftOrigin, Flow, FlowConnection, FlowDraft, FlowImport, FlowRevision, FlowRun, FlowRunStep,
    FlowRunTrigger, FlowSuggestion, FlowValidation, FlowValidationError, SuggestionStatus,
};
