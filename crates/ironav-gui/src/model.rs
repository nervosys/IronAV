//! GUI-independent state: what the user has chosen, and how that becomes an engine [`Job`].
//! Kept free of egui so it can be unit-tested.

use ironav_codec::{implemented, FILTERS};
use ironav_core::{MediaType, Result, StreamInfo};
use ironav_engine::{FilterCall, Job};
use ironav_formats::{find_format, open_demuxer, FormatSpec, FORMATS};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::Path;

/// A probed input file.
#[derive(Debug, Clone)]
pub struct Source {
    pub path: String,
    pub format: &'static str,
    pub size: u64,
    pub duration: Option<f64>,
    pub streams: Vec<StreamInfo>,
}

impl Source {
    pub fn open(path: &str) -> Result<Source> {
        let d = open_demuxer(Path::new(path))?;
        let duration = d.streams().iter().filter_map(|s| s.duration_secs()).fold(None, |a: Option<f64>, x| Some(a.map_or(x, |a| a.max(x))));
        Ok(Source {
            path: path.to_string(),
            format: d.format_name(),
            size: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            duration,
            streams: d.streams().to_vec(),
        })
    }

    fn has_media(&self, m: MediaType) -> bool {
        self.streams.iter().any(|s| s.params.media_type() == m)
    }
}

/// Container formats that can be written and that suit the source's media types.
pub fn writable_formats(src: Option<&Source>) -> Vec<&'static FormatSpec> {
    FORMATS.iter().filter(|f| f.mux && src.is_none_or(|s| s.has_media(f.media))).collect()
}

/// Codecs the engine can encode *and* the container can hold.
pub fn encodable_codecs(f: &FormatSpec) -> Vec<&'static str> {
    let imp = implemented();
    f.codecs.iter().filter(|c| imp.iter().any(|i| i.codec == **c && i.encode)).map(|c| c.name()).collect()
}

#[derive(Debug, Clone, Default)]
pub struct FilterState {
    pub enabled: bool,
    pub params: BTreeMap<String, f64>,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub format: &'static str,
    pub vcodec: String,
    pub acodec: String,
    pub filters: BTreeMap<String, FilterState>,
    pub hwaccel: String,
    pub output: String,
    pub overwrite: bool,
}

impl Settings {
    /// Sensible defaults for a freshly opened source: same container if writable, stream copy.
    pub fn for_source(src: &Source) -> Option<Settings> {
        let f = writable_formats(Some(src)).into_iter().find(|f| f.name == src.format).or_else(|| writable_formats(Some(src)).into_iter().next())?;
        let mut s = Settings {
            format: f.name,
            vcodec: "copy".into(),
            acodec: "copy".into(),
            filters: BTreeMap::new(),
            hwaccel: "auto".into(),
            output: String::new(),
            overwrite: false,
        };
        s.set_format(Some(src), f.name);
        Some(s)
    }

    /// Switch container; keeps `copy` when the source codec fits, else picks the first valid codec.
    pub fn set_format(&mut self, src: Option<&Source>, name: &str) {
        let Ok(f) = find_format(name) else { return };
        self.format = f.name;
        let ok = encodable_codecs(f);
        let fallback = || ok.first().copied().unwrap_or("copy").to_string();
        let pick = match src.and_then(|s| s.streams.iter().find(|st| st.params.media_type() == f.media)) {
            Some(st) if ok.contains(&st.codec.name()) => "copy".to_string(),
            _ => fallback(),
        };
        match f.media {
            MediaType::Video => self.vcodec = pick,
            MediaType::Audio => self.acodec = pick,
        }
        if let Some(s) = src {
            self.output = default_output(&s.path, f);
        }
    }

    pub fn spec(&self) -> &'static FormatSpec {
        find_format(self.format).expect("format name comes from the registry")
    }

    pub fn job(&self, src: &Source) -> Job {
        let spec = self.spec();
        let mut j = Job {
            input: src.path.clone(),
            output: self.output.clone(),
            format: Some(spec.name.to_string()),
            hwaccel: self.hwaccel.clone(),
            overwrite: self.overwrite,
            ..Job::default()
        };
        match spec.media {
            MediaType::Video => j.video.codec = Some(self.vcodec.clone()),
            MediaType::Audio => j.audio.codec = Some(self.acodec.clone()),
        }
        for f in FILTERS.iter().filter(|f| f.media == spec.media) {
            let Some(st) = self.filters.get(f.name).filter(|s| s.enabled) else { continue };
            let mut args = Map::new();
            for p in f.params {
                if let Some(v) = st.params.get(p.name).copied().or(p.default) {
                    args.insert(p.name.to_string(), Value::from(v));
                }
            }
            j.filters.push(FilterCall { name: f.name.to_string(), args });
        }
        j
    }
}

/// `<dir>/<stem>.<ext>`, or `<stem>-converted.<ext>` when that would overwrite the source.
pub fn default_output(input: &str, f: &FormatSpec) -> String {
    let p = Path::new(input);
    let ext = f.extensions[0];
    let same = p.extension().is_some_and(|e| e.eq_ignore_ascii_case(ext));
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("output");
    let name = if same { format!("{stem}-converted.{ext}") } else { format!("{stem}.{ext}") };
    p.with_file_name(name).display().to_string()
}

/// The equivalent `ironav convert` command line, for the Agent tab.
pub fn cli_line(j: &Job) -> String {
    let q = |s: &str| if s.contains([' ', '"']) { format!("\"{}\"", s.replace('"', "\\\"")) } else { s.to_string() };
    let mut c = format!("ironav convert -i {} -o {}", q(&j.input), q(&j.output));
    if let Some(f) = &j.format {
        c += &format!(" -f {f}");
    }
    if let Some(a) = &j.audio.codec {
        c += &format!(" --acodec {a}");
    }
    if let Some(v) = &j.video.codec {
        c += &format!(" --vcodec {v}");
    }
    for f in &j.filters {
        let args: Vec<String> = f.args.iter().map(|(k, v)| format!("{k}={v}")).collect();
        c += &format!(" --filter {}{}", f.name, if args.is_empty() { String::new() } else { format!(":{}", args.join(",")) });
    }
    if j.hwaccel != "auto" {
        c += &format!(" --hwaccel {}", j.hwaccel);
    }
    if j.overwrite {
        c += " -y";
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_core::{CodecId, Rational, StreamParams};

    fn wav_source() -> Source {
        Source {
            path: "C:/media/my song.wav".into(),
            format: "wav",
            size: 1000,
            duration: Some(1.0),
            streams: vec![StreamInfo {
                index: 0,
                codec: CodecId::PcmS16Le,
                time_base: Rational::new(1, 48000),
                duration: Some(48000),
                params: StreamParams::Audio { sample_rate: 48000, channels: 2 },
            }],
        }
    }

    #[test]
    fn audio_source_only_offers_audio_containers() {
        let names: Vec<_> = writable_formats(Some(&wav_source())).iter().map(|f| f.name).collect();
        assert_eq!(names, vec!["wav"]);
    }

    #[test]
    fn defaults_copy_and_avoid_clobbering_source() {
        let src = wav_source();
        let s = Settings::for_source(&src).unwrap();
        assert_eq!(s.acodec, "copy");
        assert!(s.output.ends_with("my song-converted.wav"), "{}", s.output);
    }

    #[test]
    fn job_includes_only_enabled_filters_with_defaults() {
        let src = wav_source();
        let mut s = Settings::for_source(&src).unwrap();
        s.acodec = "pcm_f32le".into();
        s.filters.insert("volume".into(), FilterState { enabled: true, params: BTreeMap::from([("gain".to_string(), 0.5)]) });
        s.filters.insert("hflip".into(), FilterState { enabled: true, params: BTreeMap::new() }); // video filter: ignored for wav
        let j = s.job(&src);
        assert_eq!(j.audio.codec.as_deref(), Some("pcm_f32le"));
        assert!(j.video.codec.is_none());
        assert_eq!(j.filters.len(), 1);
        assert_eq!(j.filters[0].args["gain"], 0.5);
        // Paths with spaces are quoted; the output separator is platform-native.
        let expected = format!("ironav convert -i \"C:/media/my song.wav\" -o \"{}\" -f wav --acodec pcm_f32le --filter volume:gain=0.5", j.output);
        assert_eq!(cli_line(&j), expected);
        assert!(j.output.ends_with("my song-converted.wav"));
    }

    #[test]
    fn job_from_gui_passes_engine_planner() {
        let dir = std::env::temp_dir().join(format!("ironav-gui-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.wav");
        let mut f = Vec::new();
        let data = [0u8; 400];
        f.extend_from_slice(b"RIFF\0\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x40\x1f\0\0\x80\x3e\0\0\x02\0\x10\0data");
        f.extend_from_slice(&(data.len() as u32).to_le_bytes());
        f.extend_from_slice(&data);
        std::fs::write(&p, f).unwrap();
        let src = Source::open(p.to_str().unwrap()).unwrap();
        let s = Settings::for_source(&src).unwrap();
        let plan = ironav_engine::plan(&s.job(&src)).unwrap();
        assert_eq!(plan.steps.len(), 1);
    }
}
