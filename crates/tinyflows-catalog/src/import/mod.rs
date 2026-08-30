//! Best-effort importers from foreign automation formats into a
//! [`tinyflows::model::WorkflowGraph`].
//!
//! Every importer here is **lossy and advisory by design**: an input this
//! crate cannot map exactly lands as an annotated placeholder node carrying the
//! original payload, and the approximation is reported as a warning string
//! rather than as a failure. An import that refuses half a graph is less useful
//! than one that loads all of it and says which parts need a human.

pub mod n8n;
