//! The IronAV ontology: a typed knowledge graph describing every codec, container, pixel
//! format, sample format, filter, hardware backend, platform and operation IronAV knows.
//!
//! It is **generated from the live registries** (`ironav-codec`, `ironav-formats`,
//! `ironav-hwaccel`), never maintained by hand in parallel, so an entity marked
//! `implemented` is implemented. Things IronAV knows about but cannot do yet are marked
//! `planned`, which lets an agent tell "does not exist" from "not built yet".
//!
//! Agents consume it through `describe`, `search`, `explain_conversion` and the JSON-LD export.

mod build;
mod model;
mod query;

pub use build::build;
pub use model::{entity, Entity, Kind, Ontology, Predicate, Relation, Status};
