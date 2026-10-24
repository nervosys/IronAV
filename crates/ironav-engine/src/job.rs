use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A complete, declarative description of one conversion. This is the unit of work an
/// agent submits: everything needed is in the JSON, so plans are reproducible.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub input: String,
    pub output: String,
    /// Output container (`wav`, `y4m`, ...). Inferred from the output extension when omitted.
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub audio: StreamOpts,
    #[serde(default)]
    pub video: StreamOpts,
    #[serde(default)]
    pub filters: Vec<FilterCall>,
    /// `auto` (default), `off`, `require`, or a backend name such as `cuda`.
    #[serde(default = "default_hwaccel")]
    pub hwaccel: String,
    /// Replace the output if it exists. Off by default so agents cannot clobber files by accident.
    #[serde(default)]
    pub overwrite: bool,
}

fn default_hwaccel() -> String {
    "auto".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamOpts {
    /// Target codec name, or `copy` to pass packets through untouched (the default).
    #[serde(default)]
    pub codec: Option<String>,
    /// Video only: target average bitrate in kbit/s. Mutually exclusive with `qp`.
    #[serde(default)]
    pub bitrate_kbps: Option<u32>,
    /// Video only: constant quantizer, 0 (best) to 51 (smallest). Used when no bitrate is set;
    /// defaults to 23.
    #[serde(default)]
    pub qp: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilterCall {
    pub name: String,
    #[serde(default)]
    pub args: Map<String, Value>,
}

impl FilterCall {
    /// Parse the CLI shorthand `name` or `name:key=value,key=value`.
    pub fn parse_shorthand(s: &str) -> ironav_core::Result<FilterCall> {
        let (name, rest) = s.split_once(':').unwrap_or((s, ""));
        let mut args = Map::new();
        for kv in rest.split(',').filter(|p| !p.is_empty()) {
            let (k, v) = kv.split_once('=').ok_or_else(|| {
                ironav_core::Error::invalid_arg(format!("filter argument '{kv}' must look like key=value"))
            })?;
            let value = v.parse::<f64>().map(Value::from).unwrap_or_else(|_| Value::from(v));
            args.insert(k.to_string(), value);
        }
        Ok(FilterCall { name: name.to_string(), args })
    }
}
