//! The IronAV engine: job specs, planning, threaded execution, and the agent tool registry.
//!
//! Every surface (CLI subcommands, MCP server, `ironav tools` schema dump) is generated
//! from [`tools::tools`], so they cannot drift apart.

pub mod job;
pub mod plan;
pub mod run;
pub mod tools;

pub use job::{FilterCall, Job, StreamOpts};
pub use plan::{plan, Plan};
pub use run::{run, Progress, Report};
pub use tools::{call_tool, ontology, tools, Tool};
