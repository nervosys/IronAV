use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Codec,
    Container,
    PixelFormat,
    SampleFormat,
    Filter,
    HwBackend,
    Platform,
    Operation,
    /// Turns packets or decoded frames into observations.
    Analyzer,
    /// A kind of observation an analyzer reports.
    Observation,
}

impl Kind {
    pub const ALL: [Kind; 10] = [
        Self::Codec,
        Self::Container,
        Self::PixelFormat,
        Self::SampleFormat,
        Self::Filter,
        Self::HwBackend,
        Self::Platform,
        Self::Operation,
        Self::Analyzer,
        Self::Observation,
    ];
    /// Id prefix: entity ids look like `codec/h264`.
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Codec => "codec",
            Self::Container => "container",
            Self::PixelFormat => "pixfmt",
            Self::SampleFormat => "samplefmt",
            Self::Filter => "filter",
            Self::HwBackend => "hw",
            Self::Platform => "platform",
            Self::Operation => "op",
            Self::Analyzer => "analyzer",
            Self::Observation => "observation",
        }
    }
    pub fn class_name(self) -> &'static str {
        match self {
            Self::Codec => "Codec",
            Self::Container => "Container",
            Self::PixelFormat => "PixelFormat",
            Self::SampleFormat => "SampleFormat",
            Self::Filter => "Filter",
            Self::HwBackend => "HardwareBackend",
            Self::Platform => "Platform",
            Self::Operation => "Operation",
            Self::Analyzer => "Analyzer",
            Self::Observation => "ObservationKind",
        }
    }
    pub fn parse(s: &str) -> Option<Kind> {
        let s = s.to_ascii_lowercase().replace('_', "-");
        Self::ALL.iter().copied().find(|k| k.prefix() == s || serde_json::to_value(k).ok().and_then(|v| v.as_str().map(str::to_owned)) == Some(s.clone()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// Works in this build.
    Implemented,
    /// Known and modelled, but not buildable/usable yet.
    Planned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Predicate {
    /// container → codec it can hold
    Carries,
    /// hardware backend → codec it can accelerate (note says decode / encode)
    Accelerates,
    /// hardware backend → platform it exists on
    AvailableOn,
    /// codec → sample format of its payload
    UsesSampleFormat,
    /// filter → pixel format it accepts
    AcceptsPixelFormat,
    /// filter → pixel format it produces
    ProducesPixelFormat,
    /// analyzer → kind of observation it reports
    Emits,
}

impl Predicate {
    pub const ALL: [Predicate; 7] = [
        Self::Carries,
        Self::Accelerates,
        Self::AvailableOn,
        Self::UsesSampleFormat,
        Self::AcceptsPixelFormat,
        Self::ProducesPixelFormat,
        Self::Emits,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Self::Carries => "carries",
            Self::Accelerates => "accelerates",
            Self::AvailableOn => "availableOn",
            Self::UsesSampleFormat => "usesSampleFormat",
            Self::AcceptsPixelFormat => "acceptsPixelFormat",
            Self::ProducesPixelFormat => "producesPixelFormat",
            Self::Emits => "emits",
        }
    }
    pub fn description(self) -> &'static str {
        match self {
            Self::Carries => "A container format can hold streams encoded with this codec.",
            Self::Accelerates => "A hardware backend can decode and/or encode this codec (see relation note).",
            Self::AvailableOn => "A hardware backend exists on this operating system.",
            Self::UsesSampleFormat => "A PCM codec's payload uses this sample format.",
            Self::AcceptsPixelFormat => "A filter accepts frames in this pixel format.",
            Self::ProducesPixelFormat => "A filter outputs frames in this pixel format.",
            Self::Emits => "An analyzer reports observations of this kind.",
        }
    }
    /// `(domain, range)` kinds.
    pub fn signature(self) -> (Kind, Kind) {
        match self {
            Self::Carries => (Kind::Container, Kind::Codec),
            Self::Accelerates => (Kind::HwBackend, Kind::Codec),
            Self::AvailableOn => (Kind::HwBackend, Kind::Platform),
            Self::UsesSampleFormat => (Kind::Codec, Kind::SampleFormat),
            Self::AcceptsPixelFormat | Self::ProducesPixelFormat => (Kind::Filter, Kind::PixelFormat),
            Self::Emits => (Kind::Analyzer, Kind::Observation),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Entity {
    pub id: String,
    pub kind: Kind,
    pub label: String,
    pub description: String,
    pub status: Status,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    pub properties: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Relation {
    pub subject: String,
    pub predicate: Predicate,
    pub object: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Default)]
pub struct Ontology {
    pub(crate) entities: BTreeMap<String, Entity>,
    pub(crate) relations: Vec<Relation>,
}

impl Ontology {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn add_entity(&mut self, e: Entity) {
        self.entities.insert(e.id.clone(), e);
    }
    pub fn add_relation(&mut self, subject: &str, predicate: Predicate, object: &str, note: Option<&str>) {
        debug_assert!(self.entities.contains_key(subject), "unknown subject {subject}");
        debug_assert!(self.entities.contains_key(object), "unknown object {object}");
        self.relations.push(Relation { subject: subject.into(), predicate, object: object.into(), note: note.map(Into::into) });
    }
    pub fn entities(&self) -> impl Iterator<Item = &Entity> {
        self.entities.values()
    }
    pub fn relations(&self) -> &[Relation] {
        &self.relations
    }
    pub fn get(&self, id: &str) -> Option<&Entity> {
        self.entities.get(id)
    }
}

/// Convenience constructor used by the builder and by the engine when it adds operations.
pub fn entity(kind: Kind, name: &str, label: &str, description: &str, status: Status) -> Entity {
    Entity {
        id: format!("{}/{}", kind.prefix(), name),
        kind,
        label: label.into(),
        description: description.into(),
        status,
        aliases: Vec::new(),
        properties: BTreeMap::new(),
    }
}

impl Entity {
    pub fn with_alias(mut self, a: &str) -> Self {
        self.aliases.push(a.into());
        self
    }
    pub fn with_prop(mut self, k: &str, v: impl Into<Value>) -> Self {
        self.properties.insert(k.into(), v.into());
        self
    }
}
