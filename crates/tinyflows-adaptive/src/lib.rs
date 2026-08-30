//! An adaptive loop over the tinyflows engine.
//!
//! Ingests a prompt, selects a stored workflow or authors one, runs it on the
//! engine, judges the result against evidence, and learns — updating or
//! replacing the workflow when the graph itself was at fault.
//!
//! The engine is not modified. This crate decides *which* graph to run;
//! [`tinyflows`] decides nothing and runs one graph. See the crate README for
//! why that split is structural rather than stylistic.
//!
//! The rule it enforces: **the engine may know about one run; anything that
//! spans runs lives here.**

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod closing;
pub mod contracts;
pub mod driver;
pub mod evals;
pub mod execute;
pub mod host;
pub mod intake;
pub mod inventory;
pub mod ledger;
pub mod promotion;
pub mod recall;
pub mod reuse;
pub mod storage;
pub mod workflows;
