//! Cross-platform hardware acceleration model.
//!
//! Three separate questions are answered, and kept separate on purpose:
//!
//! 1. **Matrix** – what *could* each backend do, and on which platforms ([`BACKENDS`])?
//! 2. **Probe** – does this machine *look like* it has the backend's driver stack ([`probe_all`])?
//!    Probing checks for driver libraries / device nodes; it does not open a device.
//! 3. **Linked** – was a native provider for the backend compiled into this binary
//!    ([`linked_backends`])? A backend is only *usable* when all three agree.
//!
//! Selection ([`select`]) reports every candidate with the reason it was or was not chosen,
//! so an agent can explain or repair a failed hardware request.

#[cfg(any(windows, target_os = "linux"))]
pub mod nvdec;
#[cfg(any(windows, target_os = "linux"))]
pub mod nvenc;

use ironav_core::{CodecId, Error, ErrorCode, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Windows,
    Linux,
    MacOs,
    Android,
    Ios,
}

impl Platform {
    pub const ALL: [Platform; 5] = [Self::Windows, Self::Linux, Self::MacOs, Self::Android, Self::Ios];
    pub fn current() -> Platform {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "android") {
            Self::Android
        } else if cfg!(target_os = "ios") {
            Self::Ios
        } else {
            Self::Linux
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Linux => "linux",
            Self::MacOs => "macos",
            Self::Android => "android",
            Self::Ios => "ios",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Decode,
    Encode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Cuda,
    Vaapi,
    Qsv,
    VideoToolbox,
    D3d11va,
    Amf,
    Vulkan,
    MediaCodec,
    OpenCl,
}

impl Backend {
    pub const ALL: [Backend; 9] = [
        Self::Cuda,
        Self::Vaapi,
        Self::Qsv,
        Self::VideoToolbox,
        Self::D3d11va,
        Self::Amf,
        Self::Vulkan,
        Self::MediaCodec,
        Self::OpenCl,
    ];
    pub fn name(self) -> &'static str {
        self.info().name
    }
    pub fn parse(s: &str) -> Option<Backend> {
        let s = s.to_ascii_lowercase();
        let s = match s.as_str() {
            "nvenc" | "nvdec" | "nvidia" => "cuda",
            "videotoolbox" | "vt" => "videotoolbox",
            "d3d11" | "dxva2" => "d3d11va",
            "mediacodec" => "mediacodec",
            other => other,
        };
        Self::ALL.iter().copied().find(|b| b.name() == s)
    }
    pub fn info(self) -> &'static BackendInfo {
        BACKENDS.iter().find(|i| i.backend == self).expect("every backend has a table row")
    }
}

/// Static capability row for one backend.
#[derive(Debug, Serialize)]
pub struct BackendInfo {
    #[serde(skip)]
    pub backend: Backend,
    pub name: &'static str,
    pub label: &'static str,
    pub vendor: &'static str,
    pub platforms: &'static [Platform],
    pub decode: &'static [CodecId],
    pub encode: &'static [CodecId],
    /// Backend can run filter kernels (scale, convert) on the device.
    pub compute: bool,
}

use CodecId::*;
use Platform::*;

pub static BACKENDS: &[BackendInfo] = &[
    BackendInfo {
        backend: Backend::Cuda,
        name: "cuda",
        label: "NVIDIA CUDA (NVENC/NVDEC)",
        vendor: "NVIDIA",
        platforms: &[Windows, Linux],
        decode: &[H264, Hevc, Av1, Vp8, Vp9, Mpeg4, Mjpeg],
        encode: &[H264, Hevc, Av1],
        compute: true,
    },
    BackendInfo {
        backend: Backend::Vaapi,
        name: "vaapi",
        label: "VA-API",
        vendor: "Intel/AMD (Mesa)",
        platforms: &[Linux],
        decode: &[H264, Hevc, Av1, Vp8, Vp9, Mpeg4, Mjpeg],
        encode: &[H264, Hevc, Av1, Vp8, Vp9, Mjpeg],
        compute: true,
    },
    BackendInfo {
        backend: Backend::Qsv,
        name: "qsv",
        label: "Intel Quick Sync Video (oneVPL)",
        vendor: "Intel",
        platforms: &[Windows, Linux],
        decode: &[H264, Hevc, Av1, Vp9, Mjpeg],
        encode: &[H264, Hevc, Av1, Vp9, Mjpeg],
        compute: true,
    },
    BackendInfo {
        backend: Backend::VideoToolbox,
        name: "videotoolbox",
        label: "Apple VideoToolbox",
        vendor: "Apple",
        platforms: &[MacOs, Ios],
        decode: &[H264, Hevc, ProRes, Mjpeg],
        encode: &[H264, Hevc, ProRes],
        compute: false,
    },
    BackendInfo {
        backend: Backend::D3d11va,
        name: "d3d11va",
        label: "Direct3D 11 Video Acceleration",
        vendor: "Microsoft (any GPU)",
        platforms: &[Windows],
        decode: &[H264, Hevc, Av1, Vp9],
        encode: &[],
        compute: false,
    },
    BackendInfo {
        backend: Backend::Amf,
        name: "amf",
        label: "AMD Advanced Media Framework",
        vendor: "AMD",
        platforms: &[Windows, Linux],
        decode: &[H264, Hevc, Av1, Vp9],
        encode: &[H264, Hevc, Av1],
        compute: false,
    },
    BackendInfo {
        backend: Backend::Vulkan,
        name: "vulkan",
        label: "Vulkan Video",
        vendor: "Khronos (any GPU)",
        platforms: &[Windows, Linux],
        decode: &[H264, Hevc, Av1, Vp9],
        encode: &[H264, Hevc, Av1],
        compute: true,
    },
    BackendInfo {
        backend: Backend::MediaCodec,
        name: "mediacodec",
        label: "Android MediaCodec",
        vendor: "Android",
        platforms: &[Android],
        decode: &[H264, Hevc, Av1, Vp8, Vp9, Mpeg4],
        encode: &[H264, Hevc, Vp8, Vp9],
        compute: false,
    },
    BackendInfo {
        backend: Backend::OpenCl,
        name: "opencl",
        label: "OpenCL compute",
        vendor: "Khronos (any GPU)",
        platforms: &[Windows, Linux, MacOs],
        decode: &[],
        encode: &[],
        compute: true,
    },
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProbeState {
    Detected,
    NotDetected,
    NotApplicable,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProbeResult {
    pub backend: Backend,
    pub state: ProbeState,
    pub evidence: String,
    /// A native provider for this backend is compiled into this binary.
    pub linked: bool,
}

/// What a compiled-in native provider can actually do. This is narrower than [`BACKENDS`]:
/// the matrix says what the hardware family supports, this says what IronAV has implemented.
pub struct Provider {
    pub backend: Backend,
    pub direction: Direction,
    pub codecs: &'static [CodecId],
}

/// Providers compiled into this build. NVENC and NVDEC are loaded at run time from the NVIDIA
/// driver, so they are present on Windows and Linux whether or not the machine has an NVIDIA
/// GPU; the probe says whether they can be used here.
#[cfg(any(windows, target_os = "linux"))]
pub static PROVIDERS: &[Provider] = &[
    Provider { backend: Backend::Cuda, direction: Direction::Encode, codecs: nvenc::ENCODE_CODECS },
    Provider { backend: Backend::Cuda, direction: Direction::Decode, codecs: nvdec::DECODE_CODECS },
];
#[cfg(not(any(windows, target_os = "linux")))]
pub static PROVIDERS: &[Provider] = &[];

/// Backends with at least one native provider compiled in.
pub fn linked_backends() -> &'static [Backend] {
    if PROVIDERS.is_empty() {
        &[]
    } else {
        &[Backend::Cuda]
    }
}

/// Rate control for hardware video encoders.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RateControl {
    /// Constant quantizer 0 (best) to 51 (worst); quality-targeted, size varies.
    ConstQp(u8),
    /// Average bitrate in bits per second.
    Bitrate(u32),
}

/// Quantizer used when a job sets neither `qp` nor `bitrate_kbps`.
pub const DEFAULT_QP: u8 = 23;

/// Codecs some compiled-in provider can handle in `direction`.
pub fn provider_codecs(direction: Direction) -> Vec<CodecId> {
    let mut v: Vec<CodecId> = PROVIDERS.iter().filter(|p| p.direction == direction).flat_map(|p| p.codecs.iter().copied()).collect();
    v.dedup();
    v
}

/// Backends whose provider can encode `codec`.
pub fn providers_for(codec: CodecId, direction: Direction) -> Vec<Backend> {
    PROVIDERS.iter().filter(|p| p.direction == direction && p.codecs.contains(&codec)).map(|p| p.backend).collect()
}

/// Open a hardware video encoder on `backend`.
pub fn new_video_encoder(
    backend: Backend,
    codec: CodecId,
    params: &ironav_core::StreamParams,
    rate: RateControl,
) -> Result<Box<dyn ironav_core::Encoder>> {
    #[cfg(any(windows, target_os = "linux"))]
    if backend == Backend::Cuda {
        let ironav_core::StreamParams::Video { width, height, frame_rate, .. } = params else {
            return Err(Error::invalid_arg("hardware video encoder needs a video stream"));
        };
        return Ok(Box::new(nvenc::NvencEncoder::new(codec, *width, *height, *frame_rate, rate)?));
    }
    let _ = (params, rate);
    Err(Error::new(
        ErrorCode::HardwareUnavailable,
        format!("no hardware encoder provider for {} on {}", codec.name(), backend.name()),
    ))
}

/// Open a hardware video decoder on `backend`.
pub fn new_video_decoder(backend: Backend, codec: CodecId, params: &ironav_core::StreamParams) -> Result<Box<dyn ironav_core::Decoder>> {
    #[cfg(any(windows, target_os = "linux"))]
    if backend == Backend::Cuda {
        let ironav_core::StreamParams::Video { width, height, .. } = params else {
            return Err(Error::invalid_arg("hardware video decoder needs a video stream"));
        };
        return Ok(Box::new(nvdec::NvdecDecoder::new(codec, *width, *height)?));
    }
    let _ = params;
    Err(Error::new(
        ErrorCode::HardwareUnavailable,
        format!("no hardware decoder provider for {} on {}", codec.name(), backend.name()),
    ))
}

/// True if this build can drive `codec` in `direction` on `backend`.
pub fn provider_supports(backend: Backend, codec: CodecId, direction: Direction) -> bool {
    PROVIDERS.iter().any(|p| p.backend == backend && p.direction == direction && p.codecs.contains(&codec))
}

fn lib_dirs() -> Vec<PathBuf> {
    match Platform::current() {
        Platform::Windows => {
            let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
            vec![Path::new(&root).join("System32")]
        }
        Platform::MacOs | Platform::Ios => ["/usr/lib", "/usr/local/lib", "/opt/homebrew/lib"].map(PathBuf::from).to_vec(),
        _ => [
            "/usr/lib",
            "/usr/lib64",
            "/usr/lib/x86_64-linux-gnu",
            "/usr/lib/aarch64-linux-gnu",
            "/lib/x86_64-linux-gnu",
            "/usr/local/lib",
            "/opt/amdgpu/lib/x86_64-linux-gnu",
            "/system/lib64",
        ]
        .map(PathBuf::from)
        .to_vec(),
    }
}

fn find_lib(names: &[&str]) -> Option<PathBuf> {
    let dirs = lib_dirs();
    names.iter().find_map(|n| dirs.iter().map(|d| d.join(n)).find(|p| p.exists()))
}

fn find_dri_node() -> Option<PathBuf> {
    std::fs::read_dir("/dev/dri")
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("renderD")))
}

fn detect(backend: Backend) -> Option<String> {
    use Backend::*;
    let shown = |p: PathBuf| Some(p.display().to_string());
    match (backend, Platform::current()) {
        (Cuda, Platform::Windows) => find_lib(&["nvcuda.dll"]).and_then(shown),
        (Cuda, _) => find_lib(&["libcuda.so.1", "libcuda.so"]).and_then(shown),
        (Vaapi, _) => find_dri_node().and_then(shown),
        (Qsv, Platform::Windows) => {
            find_lib(&["libmfxhw64.dll", "libmfx64-gen11.dll", "libvpl.dll", "mfx_loader_dll_hw64.dll"]).and_then(shown)
        }
        (Qsv, _) => find_lib(&["libvpl.so.2", "libmfx-gen.so.1.2", "libmfx.so.1"]).and_then(shown),
        (VideoToolbox, _) => Some("system framework".into()),
        (D3d11va, _) => find_lib(&["d3d11.dll"]).and_then(shown),
        (Amf, Platform::Windows) => find_lib(&["amfrt64.dll"]).and_then(shown),
        (Amf, _) => find_lib(&["libamfrt64.so.1"]).and_then(shown),
        (Vulkan, Platform::Windows) => find_lib(&["vulkan-1.dll"]).and_then(shown),
        (Vulkan, _) => find_lib(&["libvulkan.so.1", "libvulkan.dylib"]).and_then(shown),
        (MediaCodec, Platform::Android) => Some("system service".into()),
        (MediaCodec, _) => None,
        (OpenCl, Platform::Windows) => find_lib(&["OpenCL.dll"]).and_then(shown),
        (OpenCl, Platform::MacOs) => Some("system framework".into()),
        (OpenCl, _) => find_lib(&["libOpenCL.so.1", "libOpenCL.so"]).and_then(shown),
    }
}

/// Probe every backend on the current platform. Cheap: a handful of `stat` calls.
pub fn probe_all() -> Vec<ProbeResult> {
    let platform = Platform::current();
    let linked = linked_backends();
    BACKENDS
        .iter()
        .map(|info| {
            let is_linked = linked.contains(&info.backend);
            if !info.platforms.contains(&platform) {
                return ProbeResult {
                    backend: info.backend,
                    state: ProbeState::NotApplicable,
                    evidence: format!("{} is not offered on {}", info.name, platform.name()),
                    linked: is_linked,
                };
            }
            match detect(info.backend) {
                Some(e) => ProbeResult { backend: info.backend, state: ProbeState::Detected, evidence: e, linked: is_linked },
                None => ProbeResult {
                    backend: info.backend,
                    state: ProbeState::NotDetected,
                    evidence: "driver library / device node not found".into(),
                    linked: is_linked,
                },
            }
        })
        .collect()
}

/// Order in which `auto` tries backends on each platform (dedicated encoders first).
pub fn preference_order(platform: Platform) -> &'static [Backend] {
    use Backend::*;
    match platform {
        Platform::Windows => &[Cuda, Qsv, Amf, D3d11va, Vulkan],
        Platform::Linux => &[Cuda, Vaapi, Qsv, Amf, Vulkan],
        Platform::MacOs | Platform::Ios => &[VideoToolbox],
        Platform::Android => &[MediaCodec],
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HwPolicy {
    /// Use hardware when a usable backend exists, otherwise fall back to software.
    Auto,
    /// Never use hardware.
    Off,
    /// Fail unless some backend is usable.
    RequireAny,
    /// Fail unless this specific backend is usable.
    Require(Backend),
}

impl HwPolicy {
    /// Accepts `auto`, `off`, `require` or a backend name (`cuda`, `vaapi`, ...).
    pub fn parse(s: &str) -> Result<HwPolicy> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "off" | "none" | "software" => Ok(Self::Off),
            "require" | "any" => Ok(Self::RequireAny),
            other => Backend::parse(other).map(Self::Require).ok_or_else(|| {
                Error::invalid_arg(format!("unknown hwaccel policy '{s}'"))
                    .alternatives(["auto", "off", "require"].into_iter().chain(Backend::ALL.iter().map(|b| b.name())))
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub backend: Backend,
    pub usable: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Selection {
    pub codec: CodecId,
    pub direction: Direction,
    pub policy: HwPolicy,
    pub chosen: Option<Backend>,
    pub considered: Vec<Candidate>,
    pub summary: String,
}

/// Decide which backend (if any) should handle `codec` in `direction`.
pub fn select(codec: CodecId, direction: Direction, policy: HwPolicy, probes: &[ProbeResult]) -> Result<Selection> {
    let platform = Platform::current();
    let mut considered = Vec::new();
    let order: Vec<Backend> = match policy {
        HwPolicy::Off => Vec::new(),
        HwPolicy::Require(b) => vec![b],
        _ => preference_order(platform).to_vec(),
    };
    for backend in order {
        let info = backend.info();
        let caps = if direction == Direction::Decode { info.decode } else { info.encode };
        let probe = probes.iter().find(|p| p.backend == backend);
        let (usable, reason) = if !info.platforms.contains(&platform) {
            (false, format!("{} is not available on {}", info.name, platform.name()))
        } else if !caps.contains(&codec) {
            (false, format!("{} cannot {:?} {}", info.name, direction, codec.name()).to_lowercase())
        } else if probe.is_none_or(|p| p.state != ProbeState::Detected) {
            (false, "driver stack not detected on this machine".to_string())
        } else if !probe.is_some_and(|p| p.linked) {
            (false, "driver detected, but this IronAV build links no native provider for it".to_string())
        } else if !provider_supports(backend, codec, direction) {
            (false, format!("driver detected, but IronAV's {} provider does not implement {:?} of {} yet", info.name, direction, codec.name()).to_lowercase())
        } else {
            (true, "driver detected and provider linked".to_string())
        };
        considered.push(Candidate { backend, usable, reason });
    }
    let chosen = considered.iter().find(|c| c.usable).map(|c| c.backend);
    let summary = match (policy, chosen) {
        (HwPolicy::Off, _) => "hardware disabled by policy; using software".to_string(),
        (_, Some(b)) => format!("using {}", b.name()),
        (HwPolicy::Auto, None) => "no usable hardware backend; falling back to software".to_string(),
        (_, None) => String::new(),
    };
    if chosen.is_none() && matches!(policy, HwPolicy::RequireAny | HwPolicy::Require(_)) {
        let detail = considered.iter().map(|c| format!("{}: {}", c.backend.name(), c.reason)).collect::<Vec<_>>().join("; ");
        return Err(Error::new(
            ErrorCode::HardwareUnavailable,
            format!("hardware {:?} of {} was required but no backend is usable ({detail})", direction, codec.name()).to_lowercase(),
        )
        .hint("retry with hwaccel \"auto\" to allow software fallback, or run `ironav hw probe` to see what is detected"));
    }
    Ok(Selection { codec, direction, policy, chosen, considered, summary })
}
