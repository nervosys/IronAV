//! The IronAV engine: job specs, planning, threaded execution, and the agent tool registry.
//!
//! Every surface (CLI subcommands, MCP server, `ironav tools` schema dump) is generated
//! from [`tools::tools`], so they cannot drift apart.

pub mod analyze;
pub mod job;
pub mod live;
pub mod plan;
pub mod policy;
pub mod record;
pub mod run;
mod sessions;
pub mod sidecar;
pub mod tap;
pub mod tools;
pub mod transcode;

pub use job::{FilterCall, Job, Latency, OpusJob, OpusMode, OpusSignal, ProResProfile, StreamOpts};
pub use plan::{plan, Plan};
pub use run::{run, Progress, Report};
pub use analyze::{analyze, AnalyzeJob};
pub use tools::{call_tool, call_tool_with, ontology, tools, Ctx, Tool};
