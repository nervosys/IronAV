use crate::model::{Entity, Kind, Ontology, Predicate, Relation, Status};
use ironav_core::{Error, ErrorCode, Result};
use ironav_hwaccel::Platform;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

impl Ontology {
    /// Resolve a loose reference — full id (`codec/h264`), bare name (`h264`), label or alias.
    pub fn resolve(&self, name: &str) -> Option<&Entity> {
        let key = name.trim().to_ascii_lowercase();
        if let Some(e) = self.entities.get(&key) {
            return Some(e);
        }
        for k in Kind::ALL {
            if let Some(e) = self.entities.get(&format!("{}/{}", k.prefix(), key)) {
                return Some(e);
            }
        }
        self.entities
            .values()
            .find(|e| e.label.to_ascii_lowercase() == key || e.aliases.iter().any(|a| a.to_ascii_lowercase() == key))
    }

    /// Ranked free-text search over ids, labels, aliases and descriptions.
    pub fn search(&self, text: &str, kind: Option<Kind>, limit: usize) -> Vec<&Entity> {
        let tokens: Vec<String> = text.split_whitespace().map(str::to_ascii_lowercase).collect();
        let mut scored: Vec<(u32, &Entity)> = self
            .entities
            .values()
            .filter(|e| kind.is_none_or(|k| e.kind == k))
            .filter_map(|e| {
                if tokens.is_empty() {
                    return Some((0, e));
                }
                let (id, label, desc) = (e.id.to_ascii_lowercase(), e.label.to_ascii_lowercase(), e.description.to_ascii_lowercase());
                let score: u32 = tokens
                    .iter()
                    .map(|t| {
                        let mut s = 0;
                        if id.contains(t.as_str()) || label.contains(t.as_str()) {
                            s += 3;
                        }
                        if e.aliases.iter().any(|a| a.to_ascii_lowercase().contains(t.as_str())) {
                            s += 3;
                        }
                        if desc.contains(t.as_str()) {
                            s += 1;
                        }
                        s
                    })
                    .sum();
                (score > 0).then_some((score, e))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
        scored.into_iter().take(limit).map(|(_, e)| e).collect()
    }

    fn require(&self, name: &str) -> Result<&Entity> {
        self.resolve(name).ok_or_else(|| {
            let near: Vec<String> = self.search(name, None, 5).iter().map(|e| e.id.clone()).collect();
            Error::new(ErrorCode::NotFound, format!("no ontology entity matches '{name}'"))
                .hint("use `ironav ontology search <text>` to find ids")
                .alternatives(near)
        })
    }

    fn edge_json(&self, r: &Relation, outgoing: bool) -> Value {
        let other = if outgoing { &r.object } else { &r.subject };
        let e = self.entities.get(other);
        json!({
            "predicate": r.predicate,
            "id": other,
            "label": e.map(|e| e.label.as_str()),
            "status": e.map(|e| e.status),
            "note": r.note,
        })
    }

    /// Everything known about one entity, including incoming and outgoing relations.
    pub fn describe(&self, name: &str) -> Result<Value> {
        let e = self.require(name)?;
        let out: Vec<Value> = self.relations.iter().filter(|r| r.subject == e.id).map(|r| self.edge_json(r, true)).collect();
        let inc: Vec<Value> = self.relations.iter().filter(|r| r.object == e.id).map(|r| self.edge_json(r, false)).collect();
        Ok(json!({ "entity": e, "outgoing": out, "incoming": inc }))
    }

    fn codec_flag(&self, e: &Entity, key: &str) -> bool {
        e.properties.get(key).and_then(Value::as_bool).unwrap_or(false)
    }

    /// Can IronAV convert audio/video from codec `from` to codec `to`, and how?
    /// Answers with blockers and alternatives rather than a bare yes/no.
    pub fn explain_conversion(&self, from: &str, to: &str) -> Result<Value> {
        let a = self.require(&format!("codec/{}", from.trim_start_matches("codec/")))?;
        let b = self.require(&format!("codec/{}", to.trim_start_matches("codec/")))?;
        let platform = Platform::current().name();
        let mut blockers = Vec::new();
        if a.properties.get("media") != b.properties.get("media") {
            blockers.push(format!("{} and {} are different media types; transcoding cannot change audio into video", a.label, b.label));
        }
        if !self.codec_flag(a, "software_decode") {
            blockers.push(format!("no software decoder for {} yet (status: planned)", a.id));
        }
        if !self.codec_flag(b, "software_encode") {
            blockers.push(format!("no software encoder for {} yet (status: planned)", b.id));
        }
        let hw = |codec: &Entity, dir: &str| -> Vec<Value> {
            self.relations
                .iter()
                .filter(|r| r.predicate == Predicate::Accelerates && r.object == codec.id)
                .filter(|r| r.note.as_deref().is_some_and(|n| n.contains(dir)))
                .filter(|r| {
                    self.relations
                        .iter()
                        .any(|p| p.predicate == Predicate::AvailableOn && p.subject == r.subject && p.object == format!("platform/{platform}"))
                })
                .map(|r| {
                    let linked = self.get(&r.subject).and_then(|h| h.properties.get("linked")).and_then(Value::as_bool).unwrap_or(false);
                    json!({ "backend": r.subject, "linked_in_this_build": linked })
                })
                .collect()
        };
        let containers: Vec<&str> = self
            .relations
            .iter()
            .filter(|r| r.predicate == Predicate::Carries && r.object == b.id)
            .filter_map(|r| self.get(&r.subject))
            .filter(|c| c.status == Status::Implemented)
            .map(|c| c.id.as_str())
            .collect();
        Ok(json!({
            "from": a.id,
            "to": b.id,
            "feasible_in_this_build": blockers.is_empty(),
            "blockers": blockers,
            "platform": platform,
            "hardware_decode_candidates": hw(a, "decode"),
            "hardware_encode_candidates": hw(b, "encode"),
            "writable_containers_for_target": containers,
        }))
    }

    pub fn summary(&self) -> Value {
        let mut by_kind: BTreeMap<&str, BTreeMap<&str, u32>> = BTreeMap::new();
        for e in self.entities.values() {
            let status = if e.status == Status::Implemented { "implemented" } else { "planned" };
            *by_kind.entry(e.kind.prefix()).or_default().entry(status).or_default() += 1;
        }
        json!({
            "entities": self.entities.len(),
            "relations": self.relations.len(),
            "by_kind": by_kind,
            "predicates": Predicate::ALL.iter().map(|p| json!({
                "name": p.name(),
                "domain": p.signature().0.class_name(),
                "range": p.signature().1.class_name(),
                "description": p.description(),
            })).collect::<Vec<_>>(),
            "id_format": "<kind-prefix>/<name>, e.g. codec/h264, hw/cuda",
            "kinds": Kind::ALL.iter().map(|k| k.prefix()).collect::<Vec<_>>(),
        })
    }

    /// JSON-LD export (schema.org-style `@graph`) for RDF tooling and LLM context packing.
    pub fn to_jsonld(&self) -> Value {
        let iri = |id: &str| format!("ironav:{id}");
        let mut graph: Vec<Value> = Vec::new();
        for k in Kind::ALL {
            graph.push(json!({ "@id": format!("ironav:{}", k.class_name()), "@type": "rdfs:Class", "rdfs:label": k.class_name() }));
        }
        for p in Predicate::ALL {
            let (d, r) = p.signature();
            graph.push(json!({
                "@id": format!("ironav:{}", p.name()),
                "@type": "rdf:Property",
                "rdfs:comment": p.description(),
                "rdfs:domain": { "@id": format!("ironav:{}", d.class_name()) },
                "rdfs:range": { "@id": format!("ironav:{}", r.class_name()) },
            }));
        }
        for e in self.entities.values() {
            let mut node = Map::new();
            node.insert("@id".into(), iri(&e.id).into());
            node.insert("@type".into(), format!("ironav:{}", e.kind.class_name()).into());
            node.insert("rdfs:label".into(), e.label.clone().into());
            node.insert("rdfs:comment".into(), e.description.clone().into());
            node.insert("ironav:status".into(), serde_json::to_value(e.status).unwrap_or_default());
            if !e.aliases.is_empty() {
                node.insert("skos:altLabel".into(), json!(e.aliases));
            }
            for (k, v) in &e.properties {
                node.insert(format!("ironav:{k}"), v.clone());
            }
            let mut edges: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
            for r in self.relations.iter().filter(|r| r.subject == e.id) {
                edges.entry(r.predicate.name()).or_default().push(json!({ "@id": iri(&r.object) }));
            }
            for (p, targets) in edges {
                node.insert(format!("ironav:{p}"), Value::Array(targets));
            }
            graph.push(Value::Object(node));
        }
        json!({
            "@context": {
                "ironav": "https://nervosys.ai/ironav/ontology#",
                "rdfs": "http://www.w3.org/2000/01/rdf-schema#",
                "rdf": "http://www.w3.org/1999/02/22-rdf-syntax-ns#",
                "skos": "http://www.w3.org/2004/02/skos/core#"
            },
            "@graph": graph
        })
    }
}
