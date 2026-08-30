//! The in-crate state-graph runtime the engine lowers workflows onto.
//!
//! A [`WorkflowGraph`](crate::model::WorkflowGraph) is compiled (see
//! [`crate::compiler`]) and then lowered by [`crate::engine`] into the
//! LangGraph-style durable runtime that lives here: partial state updates and
//! reducers ([`reducer`]), commands and interrupts ([`command`]), a
//! builder/compile contract ([`builder`]), a superstep executor ([`compiled`]),
//! checkpointing ([`checkpoint`]), streaming events ([`stream`]), run-status
//! snapshots ([`status`]), and durable observation journalling
//! ([`observability`]).
//!
//! This runtime was previously consumed from the external `tinyagents` crate.
//! It now lives in-crate, trimmed to the surface tinyflows actually drives, so
//! the crate carries no agent-harness dependency: agents themselves are a host
//! concern, injected through [`crate::caps`].

pub mod builder;
pub mod channel;
pub mod checkpoint;
pub mod command;
pub mod compiled;
pub mod error;
pub mod ids;
pub mod observability;
pub mod recursion;
pub mod reducer;
pub mod status;
pub mod stream;
pub(crate) mod worker;

pub use builder::{
    END, ForkId, GraphBuilder, GraphDefaults, NodeContext, NodeFuture, NodeHandler, Route,
    RouterFn, START,
};
pub use channel::{
    Barrier, BinaryAggregate, Channel, ChannelSet, ChannelState, ChannelUpdate, Delta, Ephemeral,
    LastValue, Messages, NamedBarrier, Topic, Untracked,
};
pub use checkpoint::{
    BarrierArrivals, Checkpoint, CheckpointConfig, CheckpointMetadata, CheckpointSource,
    CheckpointTuple, Checkpointer, DurabilityMode, FileCheckpointer, InMemoryCheckpointer,
    PendingActivation, PendingWrite,
};
pub use command::{Command, Interrupt, NodeResult, RouteTarget, Send};
pub use compiled::{CompiledGraph, GraphExecution, GraphInput, ResumeTarget, StateSnapshot};
pub use error::{GraphError, Result};
pub use observability::{
    GraphEventJournal, GraphHealthSummary, GraphLatencyMetrics, GraphNodeHealth, GraphNodeLatency,
    GraphObservation, GraphStatusStore, GraphStepLatency, InMemoryGraphEventJournal,
    InMemoryGraphStatusStore, JournalGraphSink,
};
pub use recursion::{
    ChildRun, ChildRunSink, RecursionFrame, RecursionPolicy, RecursionStack, RunTree,
};
pub use reducer::{
    AppendReducer, ClosureReducer, ClosureStateReducer, MaxReducer, MinReducer, OverwriteReducer,
    OverwriteStateReducer, Reducer, SetUnionReducer, StateReducer,
};
pub use status::GraphRunStatus;
pub use stream::{CollectingSink, GraphEvent, GraphEventSink, NoopSink, StreamMode};
