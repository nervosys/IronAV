//! What the operator allows an agent to do: which directories tools may read and write, whether
//! they may write at all, whether they may open a camera or microphone or a browser's cookies,
//! and how large a picture a decoder may be asked for.
//!
//! The operator sets this with environment variables (see the README, "Security and operator
//! settings"); a tool call cannot change it. Every path a tool reads or writes goes through
//! [`check_read`] or [`check_write`], so there is one place that decides.
//!
//! Without `IRONAV_ROOTS` the command line keeps its old behaviour (any path the user can reach).
//! The MCP server calls [`set_mcp_mode`], which makes the current directory the only root and
//! keeps devices and browser cookies off unless the operator opts in.
//!
//! The check is made when a tool starts, not when the file is opened, so a path that is swapped
//! for a link in between is not caught: the roots are a guard against an agent's mistakes and
//! injected instructions, not a sandbox against a hostile local process.

use ironav_core::{Error, ErrorCode, Result, StreamInfo, StreamParams};
use std::cell::RefCell;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub const ROOTS_VAR: &str = "IRONAV_ROOTS";
pub const READ_ONLY_VAR: &str = "IRONAV_READ_ONLY";
pub const DEVICES_VAR: &str = "IRONAV_ALLOW_DEVICES";
pub const COOKIES_VAR: &str = "IRONAV_ALLOW_COOKIES";
pub const MAX_PIXELS_VAR: &str = "IRONAV_MAX_PIXELS";

/// The default ceiling on the pixels of one decoded picture (16384 x 16384).
pub const DEFAULT_MAX_PIXELS: u64 = 1 << 28;
/// The default ceiling on either side of a picture.
pub const DEFAULT_MAX_SIDE: u32 = 16384;
/// The most audio channels a decoder is made for.
pub const MAX_CHANNELS: u16 = 64;

static MCP: AtomicBool = AtomicBool::new(false);

thread_local! {
    static OVERRIDE: RefCell<Option<Policy>> = const { RefCell::new(None) };
}

/// Marks this process as an MCP server: the defaults become the strict ones.
pub fn set_mcp_mode() {
    MCP.store(true, Ordering::Relaxed);
}

pub fn mcp_mode() -> bool {
    MCP.load(Ordering::Relaxed)
}

#[derive(Debug, Clone)]
pub struct Policy {
    /// Canonical directories; `None` means no limit.
    roots: Option<Vec<PathBuf>>,
    pub read_only: bool,
    pub devices: bool,
    pub cookies: bool,
    pub max_pixels: u64,
    pub max_side: u32,
}

fn truthy(v: Option<String>) -> bool {
    v.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

impl Policy {
    /// No limits at all.
    pub fn open() -> Policy {
        Policy { roots: None, read_only: false, devices: true, cookies: true, max_pixels: DEFAULT_MAX_PIXELS, max_side: DEFAULT_MAX_SIDE }
    }

    /// The policy the environment asks for. `mcp` picks the strict defaults.
    pub fn from_env(mcp: bool) -> Policy {
        Policy::from_lookup(mcp, &|k| std::env::var(k).ok(), std::env::current_dir().ok())
    }

    pub fn from_lookup(mcp: bool, get: &dyn Fn(&str) -> Option<String>, cwd: Option<PathBuf>) -> Policy {
        let listed = get(ROOTS_VAR).filter(|v| !v.trim().is_empty());
        let roots = match listed {
            Some(v) => Some(std::env::split_paths(&OsString::from(v)).filter(|p| !p.as_os_str().is_empty()).filter_map(|p| p.canonicalize().ok()).collect()),
            None if mcp => Some(cwd.and_then(|c| c.canonicalize().ok()).into_iter().collect()),
            None => None,
        };
        let (max_pixels, max_side) = match get(MAX_PIXELS_VAR).and_then(|v| v.trim().parse::<u64>().ok()).filter(|p| *p > 0) {
            Some(p) => (p, u32::MAX),
            None => (DEFAULT_MAX_PIXELS, DEFAULT_MAX_SIDE),
        };
        Policy { roots, read_only: truthy(get(READ_ONLY_VAR)), devices: !mcp || truthy(get(DEVICES_VAR)), cookies: !mcp || truthy(get(COOKIES_VAR)), max_pixels, max_side }
    }

    /// With these roots (which must exist); for tests and embedders.
    pub fn with_roots(mut self, roots: &[&Path]) -> Policy {
        self.roots = Some(roots.iter().filter_map(|r| r.canonicalize().ok()).collect());
        self
    }

    pub fn roots(&self) -> Option<&[PathBuf]> {
        self.roots.as_deref()
    }

    fn denied(&self, what: &str, shown: &str) -> Error {
        crate::audit::refused("roots", shown);
        let roots = match &self.roots {
            Some(r) if !r.is_empty() => r.iter().map(|p| p.display().to_string().trim_start_matches(r"\\?\").to_string()).collect::<Vec<_>>().join(", "),
            _ => "none".into(),
        };
        Error::new(ErrorCode::PermissionDenied, format!("{what} '{shown}' is outside the directories this server may use"))
            .hint(format!("the operator limits file access with {ROOTS_VAR} (now: {roots}); use a path inside one of them, or ask the operator to add the directory"))
    }

    /// `path` as it really is: symbolic links followed for what exists, and for what does not exist yet
    /// (an output, a new directory) the nearest directory that does, plus the rest. A `..` below a
    /// directory that does not exist, or a link that points nowhere, cannot be resolved and is refused.
    fn resolve(&self, path: &str) -> Option<PathBuf> {
        let p = Path::new(path);
        let abs = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().ok()?.join(p) };
        let mut tail: Vec<OsString> = Vec::new();
        let mut cur = abs.as_path();
        let mut base = loop {
            match cur.canonicalize() {
                Ok(c) => break c,
                Err(_) => {
                    // a link that does not lead anywhere could be written through
                    if std::fs::symlink_metadata(cur).is_ok() {
                        return None;
                    }
                    match (cur.components().next_back(), cur.parent()) {
                        (Some(Component::Normal(n)), Some(parent)) => {
                            tail.push(n.to_os_string());
                            cur = parent;
                        }
                        (Some(Component::CurDir), Some(parent)) => cur = parent,
                        _ => return None,
                    }
                }
            }
        };
        base.extend(tail.iter().rev());
        Some(base)
    }

    fn in_roots(&self, path: &str) -> bool {
        match &self.roots {
            None => true,
            Some(roots) => self.resolve(path).is_some_and(|p| roots.iter().any(|r| p.starts_with(r))),
        }
    }

    /// May a tool read `path`?
    pub fn read(&self, path: &str) -> Result<()> {
        if self.in_roots(path) { Ok(()) } else { Err(self.denied("path", path)) }
    }

    /// May a tool create `path`, as far as the roots go? (Planning writes nothing, so it asks this.)
    pub fn write_scope(&self, path: &str) -> Result<()> {
        if self.in_roots(path) { Ok(()) } else { Err(self.denied("output path", path)) }
    }

    /// May a tool write `path`: inside the roots, and not in read-only mode.
    pub fn write(&self, path: &str) -> Result<()> {
        self.require_writable()?;
        self.write_scope(path)?;
        crate::audit::event("file_write", serde_json::json!({ "path": path }));
        Ok(())
    }

    pub fn require_writable(&self) -> Result<()> {
        if self.read_only {
            crate::audit::refused("read_only", "write");
            return Err(Error::new(ErrorCode::PermissionDenied, "this server is read-only: it does not write files")
                .hint(format!("the operator set {READ_ONLY_VAR}; use plan, probe and analyze without thumbnail_dir, which write nothing")));
        }
        Ok(())
    }

    pub fn require_devices(&self, what: &str) -> Result<()> {
        if self.devices {
            return Ok(());
        }
        crate::audit::refused("devices", what);
        Err(Error::new(ErrorCode::PermissionDenied, format!("{what}: cameras and microphones are not available to this server"))
            .hint(format!("the operator must set {DEVICES_VAR}=1 to allow device:// sources")))
    }

    pub fn require_cookies(&self) -> Result<()> {
        if self.cookies {
            return Ok(());
        }
        crate::audit::refused("cookies", "cookies option");
        Err(Error::new(ErrorCode::PermissionDenied, "the cookies option (a browser's signed-in session) is not enabled on this server")
            .hint(format!("the operator must set {COOKIES_VAR}=1 to allow it")))
    }

    /// The gate every tool call passes first: read-only mode refuses a call that would write.
    pub fn check_call(&self, tool: &str, args: &serde_json::Value) -> Result<()> {
        if self.read_only && writes_files(tool, args) {
            return self.require_writable();
        }
        Ok(())
    }

    /// May a decoder be made for this stream?
    pub fn check_decode(&self, s: &StreamInfo) -> Result<()> {
        match s.params {
            StreamParams::Video { width, height, .. } => {
                let pixels = width as u64 * height as u64;
                if width > self.max_side || height > self.max_side || pixels > self.max_pixels {
                    crate::audit::refused("caps", &format!("{width}x{height} video"));
                    return Err(Error::new(ErrorCode::InvalidInput, format!("stream {} is {width}x{height}, larger than this build decodes", s.index))
                        .hint(format!("the limit is {} pixels (and {} on a side); the operator can raise it with {MAX_PIXELS_VAR}", self.max_pixels, self.max_side.min(self.max_pixels.min(u32::MAX as u64) as u32))));
                }
            }
            StreamParams::Audio { channels, .. } => {
                if channels > MAX_CHANNELS {
                    crate::audit::refused("caps", &format!("{channels} audio channels"));
                    return Err(Error::new(ErrorCode::InvalidInput, format!("stream {} has {channels} audio channels; at most {MAX_CHANNELS} are decoded", s.index)));
                }
            }
        }
        Ok(())
    }
}

/// The policy in force on this thread: a test's or embedder's, else the environment's.
pub fn current() -> Policy {
    OVERRIDE.with(|o| o.borrow().clone()).unwrap_or_else(|| Policy::from_env(mcp_mode()))
}

/// Runs `f` with `p` in force on this thread only (the checks are made on the calling thread, before
/// work is handed to others).
pub fn with_policy<T>(p: Policy, f: impl FnOnce() -> T) -> T {
    let old = OVERRIDE.with(|o| o.borrow_mut().replace(p));
    struct Restore(Option<Policy>);
    impl Drop for Restore {
        fn drop(&mut self) {
            OVERRIDE.with(|o| *o.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(old);
    f()
}

pub fn check_read(path: &str) -> Result<()> {
    current().read(path)
}
pub fn check_write(path: &str) -> Result<()> {
    current().write(path)
}
pub fn check_write_scope(path: &str) -> Result<()> {
    current().write_scope(path)
}

/// [`ironav_codec::new_decoder_for`], after the size limits.
pub fn new_decoder_for(s: &StreamInfo) -> Result<Box<dyn ironav_core::Decoder>> {
    current().check_decode(s)?;
    ironav_codec::new_decoder_for(s)
}

/// [`ironav_codec::new_decoder_with`], after the size limits.
pub fn new_decoder_with(s: &StreamInfo, o: &ironav_codec::DecoderOptions) -> Result<Box<dyn ironav_core::Decoder>> {
    current().check_decode(s)?;
    ironav_codec::new_decoder_with(s, o)
}

/// May a live session be opened on this source URL? File and pipe paths must be inside the roots,
/// device capture and the cookies option must be enabled.
pub fn check_source(raw: &str, u: &ironav_live::SourceUrl) -> Result<()> {
    let pol = current();
    match u.scheme.as_str() {
        "file" | "pipe" if !u.target.is_empty() => pol.read(&u.target)?,
        "device" => {
            pol.require_devices(&format!("device://{}", u.target))?;
            crate::audit::event("device_open", serde_json::json!({ "device": u.target }));
            if mcp_mode() {
                eprintln!("ironav: opening capture device {}", u.target);
            }
        }
        "tcp" | "udp" | "srt" | "rtsp" | "rtsps" | "http" | "https" | "whip" => crate::audit::event("network_open", serde_json::json!({ "url": ironav_live::redact(raw) })),
        _ => {}
    }
    if u.option("cookies").is_some() {
        pol.require_cookies()?;
    }
    Ok(())
}

/// Whether this call to `tool` would write files (or open a way to), judged from its arguments.
pub fn writes_files(tool: &str, args: &serde_json::Value) -> bool {
    let given = |k: &str| args.get(k).is_some_and(|v| !v.is_null());
    match tool {
        "run" | "stream_clip" => true,
        "analyze" => given("thumbnail_dir"),
        "stream_open" | "stream_configure" => given("record") || given("transcode"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ironav-policy-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn p(root: &Path) -> Policy {
        Policy::open().with_roots(&[root])
    }

    #[test]
    fn paths_inside_the_roots_pass_and_others_are_refused_naming_the_variable() {
        let d = scratch("in");
        let (inside, outside) = (d.join("root"), d.join("other"));
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(inside.join("a.mp4"), b"x").unwrap();
        std::fs::write(outside.join("b.mp4"), b"x").unwrap();
        let pol = p(&inside);
        pol.read(inside.join("a.mp4").to_str().unwrap()).unwrap();
        pol.write(inside.join("new").join("deeper").join("out.mp4").to_str().unwrap()).unwrap();
        let e = pol.read(outside.join("b.mp4").to_str().unwrap()).unwrap_err();
        assert_eq!(e.code, ErrorCode::PermissionDenied);
        assert!(e.hint.unwrap().contains(ROOTS_VAR));
        assert_eq!(pol.write(outside.join("o.mp4").to_str().unwrap()).unwrap_err().code, ErrorCode::PermissionDenied);
        // a sibling that shares the root's name as a prefix is not inside it
        let sib = d.join("root-evil");
        std::fs::create_dir_all(&sib).unwrap();
        assert!(pol.read(sib.to_str().unwrap()).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn dot_dot_cannot_climb_out() {
        let d = scratch("dots");
        let root = d.join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(d.join("secret"), b"x").unwrap();
        let pol = p(&root);
        // through a directory that exists
        let up = root.join("sub").join("..").join("..").join("secret");
        assert!(pol.read(up.to_str().unwrap()).is_err());
        // through one that does not, so it cannot be resolved
        let up = root.join("nope").join("..").join("..").join("secret");
        assert!(pol.write(up.to_str().unwrap()).is_err());
        // and a harmless one stays
        pol.read(root.join("sub").join("..").join("sub").to_str().unwrap()).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_link_out_of_the_roots_is_followed_and_refused() {
        let d = scratch("link");
        let (root, out) = (d.join("root"), d.join("out"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("secret.txt"), b"x").unwrap();
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&out, root.join("dirlink")).and_then(|_| std::os::unix::fs::symlink(out.join("missing"), root.join("dangling"))).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(&out, root.join("dirlink")).is_ok() && std::os::windows::fs::symlink_file(out.join("missing"), root.join("dangling")).is_ok();
        if !made {
            // creating links needs a privilege on Windows; without it there is nothing to test
            eprintln!("skipped: cannot create symbolic links here");
            let _ = std::fs::remove_dir_all(&d);
            return;
        }
        let pol = p(&root);
        assert!(pol.read(root.join("dirlink").join("secret.txt").to_str().unwrap()).is_err());
        assert!(pol.write(root.join("dirlink").join("new.mp4").to_str().unwrap()).is_err());
        assert!(pol.write(root.join("dangling").to_str().unwrap()).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn read_only_refuses_writes_but_not_reads_or_planning() {
        let d = scratch("ro");
        let mut pol = p(&d);
        pol.read_only = true;
        let f = d.join("o.mp4");
        assert_eq!(pol.write(f.to_str().unwrap()).unwrap_err().code, ErrorCode::PermissionDenied);
        pol.write_scope(f.to_str().unwrap()).unwrap();
        pol.read(f.to_str().unwrap()).unwrap();
        assert!(writes_files("run", &serde_json::json!({})));
        assert!(writes_files("analyze", &serde_json::json!({ "thumbnail_dir": "x" })));
        assert!(!writes_files("analyze", &serde_json::json!({ "input": "x" })));
        assert!(writes_files("stream_open", &serde_json::json!({ "record": "r.m3u8" })));
        assert!(!writes_files("stream_open", &serde_json::json!({ "source": "test://video" })));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn mcp_defaults_are_strict_and_the_environment_can_open_them() {
        let d = scratch("mcp");
        let none = |_: &str| None;
        let pol = Policy::from_lookup(true, &none, Some(d.clone()));
        assert!(pol.roots().is_some_and(|r| r.len() == 1));
        assert!(!pol.devices && !pol.cookies && !pol.read_only);
        assert_eq!(pol.require_devices("x").unwrap_err().code, ErrorCode::PermissionDenied);
        assert_eq!(pol.require_cookies().unwrap_err().code, ErrorCode::PermissionDenied);
        let env = |k: &str| match k {
            DEVICES_VAR | COOKIES_VAR | READ_ONLY_VAR => Some("1".to_string()),
            _ => None,
        };
        let pol = Policy::from_lookup(true, &env, Some(d.clone()));
        assert!(pol.devices && pol.cookies && pol.read_only);
        // the command line keeps its old behaviour
        let cli = Policy::from_lookup(false, &none, Some(d.clone()));
        assert!(cli.roots().is_none() && cli.devices && cli.cookies);
        // a list of roots replaces the working directory
        let sep = if cfg!(windows) { ";" } else { ":" };
        let a = scratch("mcp-a");
        let list = format!("{}{sep}{}", a.display(), d.display());
        let env = move |k: &str| (k == ROOTS_VAR).then(|| list.clone());
        assert_eq!(Policy::from_lookup(true, &env, None).roots().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&a);
    }

    #[test]
    fn decoders_are_limited_by_size_and_channels() {
        let mk = |params| StreamInfo { index: 0, codec: ironav_core::CodecId::H264, time_base: ironav_core::Rational { num: 1, den: 90000 }, duration: None, bits: None, extradata: Vec::new(), lead_in: 0, params };
        let video = |w, h| mk(StreamParams::Video { width: w, height: h, pix_fmt: ironav_core::PixelFormat::Yuv420p, frame_rate: ironav_core::Rational { num: 30, den: 1 } });
        let pol = Policy::open();
        pol.check_decode(&video(1920, 1080)).unwrap();
        pol.check_decode(&video(16384, 16384)).unwrap();
        assert_eq!(pol.check_decode(&video(65535, 65535)).unwrap_err().code, ErrorCode::InvalidInput);
        assert!(pol.check_decode(&video(20000, 100)).is_err());
        let env = |k: &str| (k == MAX_PIXELS_VAR).then(|| "100000000000".to_string());
        Policy::from_lookup(false, &env, None).check_decode(&video(65535, 65535)).unwrap();
        let env = |k: &str| (k == MAX_PIXELS_VAR).then(|| "1000".to_string());
        assert!(Policy::from_lookup(false, &env, None).check_decode(&video(64, 64)).is_err());
        let audio = |c| mk(StreamParams::Audio { sample_rate: 48000, channels: c });
        pol.check_decode(&audio(64)).unwrap();
        assert!(pol.check_decode(&audio(65)).is_err());
    }

    #[test]
    fn tools_refuse_what_the_policy_forbids() {
        use crate::tools::call_tool;
        use serde_json::json;
        let d = scratch("tools");
        let (root, other) = (d.join("root"), d.join("other"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let (inp, outp) = (other.join("in.y4m"), other.join("out.y4m"));
        std::fs::write(&inp, b"YUV4MPEG2 W16 H16 F10:1 Ip C420jpeg\nFRAME\n").unwrap();
        std::fs::write(&outp, b"x").unwrap();
        let denied = |r: Result<serde_json::Value>| {
            let e = r.expect_err("should be refused");
            assert_eq!(e.code, ErrorCode::PermissionDenied, "{e:?}");
            e
        };
        // a strict, MCP-style policy: the root only, no devices
        let none = |_: &str| None;
        let mut strict = Policy::from_lookup(true, &none, Some(root.clone()));
        with_policy(strict.clone(), || {
            assert!(denied(call_tool("probe", &json!({ "path": inp.to_str().unwrap() }))).hint.unwrap().contains(ROOTS_VAR));
            denied(call_tool("plan", &json!({ "input": inp.to_str().unwrap(), "output": root.join("o.y4m").to_str().unwrap() })));
            denied(call_tool("run", &json!({ "input": inp.to_str().unwrap(), "output": outp.to_str().unwrap(), "overwrite": true })));
            denied(call_tool("analyze", &json!({ "input": inp.to_str().unwrap() })));
            denied(call_tool("play_open", &json!({ "path": inp.to_str().unwrap() })));
            denied(call_tool("stream_open", &json!({ "source": inp.to_str().unwrap() })));
            denied(call_tool("stream_open", &json!({ "source": format!("file:{}", inp.display()) })));
            denied(call_tool("stream_open", &json!({ "source": "test://video", "record": outp.to_str().unwrap(), "overwrite": true })));
            // the existing file was not touched
            assert_eq!(std::fs::read(&outp).unwrap(), b"x");
            // devices and cookies
            let e = denied(call_tool("stream_open", &json!({ "source": "device://video/0" })));
            assert!(e.hint.unwrap().contains(DEVICES_VAR));
            let e = denied(call_tool("stream_open", &json!({ "source": "https://youtu.be/aaaaaaaaaaa?cookies=chrome" })));
            assert!(e.hint.unwrap().contains(COOKIES_VAR));
        });
        // read-only mode, with the whole disk readable
        strict = Policy::open();
        strict.read_only = true;
        with_policy(strict, || {
            let o = root.join("o.y4m");
            let e = denied(call_tool("run", &json!({ "input": inp.to_str().unwrap(), "output": o.to_str().unwrap() })));
            assert!(e.hint.unwrap().contains(READ_ONLY_VAR));
            denied(call_tool("analyze", &json!({ "input": inp.to_str().unwrap(), "thumbnail_dir": root.to_str().unwrap() })));
            denied(call_tool("stream_open", &json!({ "source": "test://video", "record": o.to_str().unwrap() })));
            assert!(!o.exists());
            // planning and probing write nothing and still work
            call_tool("probe", &json!({ "path": inp.to_str().unwrap() })).unwrap();
            call_tool("plan", &json!({ "input": inp.to_str().unwrap(), "output": o.to_str().unwrap() })).unwrap();
        });
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_override_is_per_thread_and_restored() {
        let d = scratch("ovr");
        let strict = p(&d);
        let seen = with_policy(strict, || {
            let inner = std::thread::spawn(|| current().roots().is_none() || mcp_mode()).join().unwrap();
            (current().roots().is_some(), inner)
        });
        assert!(seen.0 && seen.1);
        assert!(OVERRIDE.with(|o| o.borrow().is_none()));
        let _ = std::fs::remove_dir_all(&d);
    }
}
