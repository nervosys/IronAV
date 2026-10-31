//! NVIDIA NVENC provider: H.264 and HEVC encoding on the GPU through the driver's own
//! `nvEncodeAPI` library, loaded at run time (no SDK, no link-time dependency, so a build
//! runs fine on machines without NVIDIA drivers).
//!
//! The NVENC structs are large, versioned and full of bit-fields, so they are modelled as
//! zeroed byte blobs with explicit field offsets. The offsets and sizes below were read from
//! NVIDIA's `nvEncodeAPI.h` (SDK 13.1) with `clang -fdump-record-layouts`, and the tests pin
//! the sizes. Struct layouts change between SDK major versions, so [`NvencEncoder::new`]
//! refuses drivers older than 13.1 instead of guessing.
//!
//! Input is planar YUV 4:2:0 (`IYUV`) copied into NVENC input surfaces; there are no B-frames
//! and no look-ahead, so output order equals input order and every frame yields a packet
//! without added latency.

use crate::RateControl;
use ironav_core::{CodecId, Encoder, Error, ErrorCode, Frame, Packet, PixelFormat, Rational, Result, StreamParams};
use libloading::Library;
use std::collections::VecDeque;
use std::ffi::{c_char, c_void, CStr};
use std::ptr;

// ---- SDK 13.1 constants (values evaluated from the header) ----------------------------------

const API_VERSION: u32 = 13 | (1 << 24);
/// `NvEncodeAPIGetMaxSupportedVersion` reports `(major << 4) | minor`.
const API_VERSION_PACKED: u32 = (13 << 4) | 1;
const fn struct_ver(v: u32) -> u32 {
    API_VERSION | (v << 16) | (0x7 << 28)
}
const FUNCTION_LIST_VER: u32 = struct_ver(2);
const OPEN_SESSION_VER: u32 = struct_ver(1);
const CREATE_INPUT_VER: u32 = struct_ver(2);
const CREATE_BITSTREAM_VER: u32 = struct_ver(1);
const LOCK_INPUT_VER: u32 = struct_ver(1);
const CONFIG_VER: u32 = struct_ver(9) | (1 << 31);
const INITIALIZE_VER: u32 = struct_ver(7) | (1 << 31);
const PRESET_CONFIG_VER: u32 = struct_ver(5) | (1 << 31);
const PIC_PARAMS_VER: u32 = struct_ver(7) | (1 << 31);
const LOCK_BITSTREAM_VER: u32 = struct_ver(2) | (1 << 31);

const NV_ENC_SUCCESS: i32 = 0;
const NV_ENC_ERR_NEED_MORE_INPUT: i32 = 17;
const DEVICE_TYPE_CUDA: u32 = 1;
const BUFFER_FORMAT_IYUV: u32 = 0x100;
/// 10-bit semi-planar (P010): Y plane then interleaved UV, 16-bit words with the value in the top 10 bits.
const BUFFER_FORMAT_YUV420_10BIT: u32 = 0x1_0000;
const PROFILE_HEVC_MAIN10: Guid = guid(0xfa4d2b6c, 0x3a5b, 0x411a, [0x80, 0x18, 0x0a, 0x3f, 0x5e, 0x3c, 0x9b, 0xe5]);
const RC_CONSTQP: u32 = 0;
const RC_VBR: u32 = 1;
const PIC_STRUCT_FRAME: u32 = 1;
const PIC_FLAG_EOS: u32 = 8;
const TUNING_HIGH_QUALITY: u32 = 1;
const TUNING_LOW_LATENCY: u32 = 2;
const RC_CBR: u32 = 2;

type Guid = [u8; 16];
const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> Guid {
    let (a, b, c) = (d1.to_le_bytes(), d2.to_le_bytes(), d3.to_le_bytes());
    [a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d4[0], d4[1], d4[2], d4[3], d4[4], d4[5], d4[6], d4[7]]
}
const CODEC_H264: Guid = guid(0x6bc82762, 0x4e63, 0x4ca4, [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf]);
const CODEC_HEVC: Guid = guid(0x790cdc88, 0x4522, 0x4d7b, [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03]);
const PRESET_P4: Guid = guid(0x90a7b826, 0xdf06, 0x4862, [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81]);

/// `GUID` passed by value in the C API; `repr(C)` gives the platform's by-value convention.
#[repr(C)]
#[derive(Clone, Copy)]
struct GuidArg {
    d1: u32,
    d2: u16,
    d3: u16,
    d4: [u8; 8],
}
impl From<Guid> for GuidArg {
    fn from(g: Guid) -> Self {
        GuidArg {
            d1: u32::from_le_bytes([g[0], g[1], g[2], g[3]]),
            d2: u16::from_le_bytes([g[4], g[5]]),
            d3: u16::from_le_bytes([g[6], g[7]]),
            d4: [g[8], g[9], g[10], g[11], g[12], g[13], g[14], g[15]],
        }
    }
}

// ---- Byte-blob structs -----------------------------------------------------------------------

/// A zeroed C struct of `N` bytes with 8-byte alignment, written at explicit offsets.
#[repr(C, align(8))]
struct Blob<const N: usize>([u8; N]);

impl<const N: usize> Blob<N> {
    fn new() -> Box<Self> {
        Box::new(Blob([0; N]))
    }
    fn u32(&mut self, off: usize, v: u32) {
        self.0[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, off: usize, v: u64) {
        self.0[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }
    fn ptr(&mut self, off: usize, p: *const c_void) {
        self.u64(off, p as usize as u64);
    }
    fn bytes(&mut self, off: usize, b: &[u8]) {
        self.0[off..off + b.len()].copy_from_slice(b);
    }
    fn bit(&mut self, byte: usize, bit: u8, on: bool) {
        if on {
            self.0[byte] |= 1 << bit;
        } else {
            self.0[byte] &= !(1 << bit);
        }
    }
    fn get_u32(&self, off: usize) -> u32 {
        u32::from_le_bytes(self.0[off..off + 4].try_into().unwrap())
    }
    fn get_ptr(&self, off: usize) -> *mut c_void {
        u64::from_le_bytes(self.0[off..off + 8].try_into().unwrap()) as usize as *mut c_void
    }
}

const SZ_FUNCTION_LIST: usize = 2552;
const SZ_OPEN_SESSION: usize = 1552;
const SZ_INITIALIZE: usize = 1800;
const SZ_CONFIG: usize = 3584;
const SZ_PRESET_CONFIG: usize = 5128;
const SZ_CREATE_INPUT: usize = 776;
const SZ_CREATE_BITSTREAM: usize = 776;
const SZ_PIC_PARAMS: usize = 3360;
const SZ_LOCK_BITSTREAM: usize = 1544;
const SZ_LOCK_INPUT: usize = 1544;

/// Offset of `NV_ENC_CONFIG` inside `NV_ENC_PRESET_CONFIG`, and of the codec union inside it.
const PRESET_CFG: usize = 8;
const CODEC_CFG: usize = 168;

// Function-list slots (byte offsets into NV_ENCODE_API_FUNCTION_LIST).
const F_INITIALIZE: usize = 96;
const F_CREATE_INPUT: usize = 104;
const F_DESTROY_INPUT: usize = 112;
const F_CREATE_BITSTREAM: usize = 120;
const F_DESTROY_BITSTREAM: usize = 128;
const F_ENCODE_PICTURE: usize = 136;
const F_LOCK_BITSTREAM: usize = 144;
const F_UNLOCK_BITSTREAM: usize = 152;
const F_LOCK_INPUT: usize = 160;
const F_UNLOCK_INPUT: usize = 168;
const F_DESTROY_ENCODER: usize = 224;
const F_OPEN_SESSION_EX: usize = 240;
const F_LAST_ERROR: usize = 304;
const F_PRESET_CONFIG_EX: usize = 320;

type Status = i32;
type FnOpenEx = unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> Status;
type FnPresetEx = unsafe extern "system" fn(*mut c_void, GuidArg, GuidArg, u32, *mut c_void) -> Status;
type FnPtr = unsafe extern "system" fn(*mut c_void, *mut c_void) -> Status;
type FnDestroy = unsafe extern "system" fn(*mut c_void) -> Status;
type FnLastError = unsafe extern "system" fn(*mut c_void) -> *const c_char;

// ---- Public API -----------------------------------------------------------------------------

/// Codecs this provider can encode.
pub const ENCODE_CODECS: &[CodecId] = &[CodecId::H264, CodecId::Hevc];

fn unavailable(msg: impl Into<String>) -> Error {
    Error::new(ErrorCode::HardwareUnavailable, msg)
        .hint("run `ironav hw probe` to see what is detected, or set hwaccel to \"off\" to use software where it exists")
}

fn load(names: &[&str]) -> std::result::Result<Library, String> {
    let mut last = String::new();
    for n in names {
        // SAFETY: loading the NVIDIA driver libraries; their initialisers are the vendor's.
        match unsafe { Library::new(n) } {
            Ok(l) => return Ok(l),
            Err(e) => last = format!("{n}: {e}"),
        }
    }
    Err(last)
}

struct Slot {
    input: *mut c_void,
    output: *mut c_void,
}

pub struct NvencEncoder {
    _nvenc: Library,
    cuda: Library,
    ctx: *mut c_void,
    dev: i32,
    funcs: Box<Blob<SZ_FUNCTION_LIST>>,
    enc: *mut c_void,
    width: u32,
    height: u32,
    /// Bits per sample of the input, 8 or 10 (10 only for HEVC).
    bits: u8,
    buffer_format: u32,
    aligned_h: usize,
    slots: Vec<Slot>,
    next: usize,
    /// Slots whose output has not been read yet, oldest first.
    pending: VecDeque<usize>,
    frame_idx: u32,
    flushed: bool,
}

// SAFETY: the NVENC session and CUDA context are used by one thread at a time (the encoder is
// moved between threads, never shared), and the CUDA context is re-bound before every call.
unsafe impl Send for NvencEncoder {}

impl NvencEncoder {
    /// Open an NVENC session on the first NVIDIA GPU and configure it. `frame_rate` and the
    /// dimensions come from the stream; width and height must be even.
    /// `low_latency` uses NVENC's low-latency tuning, an IDR picture every second rather than every
    /// two, and for a bit rate CBR with a one-picture VBV buffer, so no picture's bits back up.
    pub fn new(codec: CodecId, width: u32, height: u32, bits: u8, frame_rate: Rational, rate: RateControl, low_latency: bool) -> Result<Self> {
        let codec_guid = match codec {
            CodecId::H264 => CODEC_H264,
            CodecId::Hevc => CODEC_HEVC,
            other => return Err(Error::invalid_arg(format!("NVENC provider cannot encode {}", other.name())).alternatives(["h264", "hevc"])),
        };
        if bits != 8 && !(bits == 10 && codec == CodecId::Hevc) {
            return Err(Error::invalid_arg(format!("NVENC provider encodes {} at 8 bits{}, not {bits}", codec.name(), if codec == CodecId::Hevc { " or 10 bits" } else { "" })));
        }
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(Error::invalid_arg(format!("NVENC needs even, non-zero dimensions, got {width}x{height}"))
                .hint("crop or scale the video to even dimensions first"));
        }
        let (fps_num, fps_den) = if frame_rate.num > 0 && frame_rate.den > 0 { (frame_rate.num as u32, frame_rate.den as u32) } else { (30, 1) };

        let nvenc = load(&if cfg!(windows) { vec!["nvEncodeAPI64.dll"] } else { vec!["libnvidia-encode.so.1"] })
            .map_err(|e| unavailable(format!("NVENC driver library not found ({e})")))?;
        let cuda = load(&if cfg!(windows) { vec!["nvcuda.dll"] } else { vec!["libcuda.so.1"] })
            .map_err(|e| unavailable(format!("CUDA driver library not found ({e})")))?;

        // SAFETY: all calls below follow the documented NVENC/CUDA driver API with
        // correctly-sized, versioned argument blocks; failures are checked after each call.
        unsafe {
            let max_version: libloading::Symbol<unsafe extern "system" fn(*mut u32) -> Status> =
                nvenc.get(b"NvEncodeAPIGetMaxSupportedVersion\0").map_err(|e| unavailable(format!("NVENC entry point missing: {e}")))?;
            let mut v = 0u32;
            if max_version(&mut v) != NV_ENC_SUCCESS {
                return Err(unavailable("could not query the NVENC driver version"));
            }
            if v < API_VERSION_PACKED {
                return Err(unavailable(format!(
                    "NVENC driver supports API {}.{}, but this build needs 13.1 or newer",
                    v >> 4,
                    v & 15
                ))
                .hint("update the NVIDIA driver"));
            }

            let cu_init: libloading::Symbol<unsafe extern "system" fn(u32) -> i32> =
                cuda.get(b"cuInit\0").map_err(|e| unavailable(format!("CUDA entry point missing: {e}")))?;
            let cu_dev: libloading::Symbol<unsafe extern "system" fn(*mut i32, i32) -> i32> =
                cuda.get(b"cuDeviceGet\0").map_err(|e| unavailable(format!("CUDA entry point missing: {e}")))?;
            let cu_retain: libloading::Symbol<unsafe extern "system" fn(*mut *mut c_void, i32) -> i32> = cuda
                .get(b"cuDevicePrimaryCtxRetain\0")
                .map_err(|e| unavailable(format!("CUDA entry point missing: {e}")))?;
            let cu_set: libloading::Symbol<unsafe extern "system" fn(*mut c_void) -> i32> =
                cuda.get(b"cuCtxSetCurrent\0").map_err(|e| unavailable(format!("CUDA entry point missing: {e}")))?;
            let (mut dev, mut ctx) = (0i32, ptr::null_mut());
            let r = cu_init(0);
            if r != 0 {
                return Err(unavailable(format!("cuInit failed with CUDA error {r} (no usable NVIDIA GPU?)")));
            }
            let r = cu_dev(&mut dev, 0);
            if r != 0 {
                return Err(unavailable(format!("no CUDA device 0 (CUDA error {r})")));
            }
            let r = cu_retain(&mut ctx, dev);
            if r != 0 {
                return Err(unavailable(format!("cannot create a CUDA context (CUDA error {r})")));
            }
            cu_set(ctx);

            let create: libloading::Symbol<unsafe extern "system" fn(*mut c_void) -> Status> =
                nvenc.get(b"NvEncodeAPICreateInstance\0").map_err(|e| unavailable(format!("NVENC entry point missing: {e}")))?;
            let mut funcs = Blob::<SZ_FUNCTION_LIST>::new();
            funcs.u32(0, FUNCTION_LIST_VER);
            if create(funcs.0.as_mut_ptr().cast()) != NV_ENC_SUCCESS {
                return Err(unavailable("NvEncodeAPICreateInstance failed"));
            }

            // From here on the struct owns everything, so Drop cleans up on any error path.
            let mut this = NvencEncoder {
                _nvenc: nvenc,
                cuda,
                ctx,
                dev,
                funcs,
                enc: ptr::null_mut(),
                width,
                height,
                bits,
                buffer_format: if bits == 10 { BUFFER_FORMAT_YUV420_10BIT } else { BUFFER_FORMAT_IYUV },
                aligned_h: (height as usize + 31) & !31,
                slots: Vec::new(),
                next: 0,
                pending: VecDeque::new(),
                frame_idx: 0,
                flushed: false,
            };

            let mut open = Blob::<SZ_OPEN_SESSION>::new();
            open.u32(0, OPEN_SESSION_VER);
            open.u32(4, DEVICE_TYPE_CUDA);
            open.ptr(8, ctx);
            open.u32(24, API_VERSION);
            let f: FnOpenEx = this.func(F_OPEN_SESSION_EX)?;
            let st = f(open.0.as_mut_ptr().cast(), &mut this.enc);
            if st != NV_ENC_SUCCESS {
                this.enc = ptr::null_mut();
                return Err(unavailable(format!("cannot open an NVENC session (status {st}); the GPU may be out of encode sessions")));
            }

            let mut preset = Blob::<SZ_PRESET_CONFIG>::new();
            preset.u32(0, PRESET_CONFIG_VER);
            preset.u32(PRESET_CFG, CONFIG_VER);
            let f: FnPresetEx = this.func(F_PRESET_CONFIG_EX)?;
            let tuning = if low_latency { TUNING_LOW_LATENCY } else { TUNING_HIGH_QUALITY };
            let st = f(this.enc, codec_guid.into(), PRESET_P4.into(), tuning, preset.0.as_mut_ptr().cast());
            this.check(st, "get preset config")?;

            // Overrides on the preset's own configuration.
            // rounded, so 29.97 fps gets 30 (or 60) pictures as the software encoder does
            let gop = ((fps_num as u64 * if low_latency { 1 } else { 2 } + fps_den as u64 / 2) / fps_den as u64).clamp(1, 600) as u32;
            let c = PRESET_CFG;
            preset.u32(c + 20, gop); // gopLength
            preset.u32(c + 24, 1); // frameIntervalP: IPPP, no B-frames, so output order == input order
            match rate {
                RateControl::ConstQp(qp) => {
                    preset.u32(c + 44, RC_CONSTQP);
                    for i in 0..3 {
                        preset.u32(c + 48 + 4 * i, qp.min(51) as u32);
                    }
                }
                RateControl::Bitrate(bps) if low_latency => {
                    let frame_bits = (bps as u64 * fps_den as u64 / fps_num as u64).clamp(1, u32::MAX as u64) as u32;
                    preset.u32(c + 44, RC_CBR);
                    preset.u32(c + 60, bps);
                    preset.u32(c + 64, bps);
                    preset.u32(c + 68, frame_bits); // vbvBufferSize
                    preset.u32(c + 72, frame_bits); // vbvInitialDelay
                }
                RateControl::Bitrate(bps) => {
                    preset.u32(c + 44, RC_VBR);
                    preset.u32(c + 60, bps);
                    preset.u32(c + 64, bps.saturating_mul(2));
                }
            }
            preset.bit(c + 76, 5, false); // rcParams.enableLookahead off: no added latency
            let cc = c + CODEC_CFG;
            if codec == CodecId::H264 {
                preset.u32(cc + 8, gop); // idrPeriod
                preset.bit(cc + 1, 4, true); // repeatSPSPPS: every IDR is self-contained
                preset.u32(cc + 192, 1); // chromaFormatIDC 4:2:0
            } else {
                preset.u32(cc + 20, gop); // idrPeriod
                preset.bit(cc + 16, 7, true); // repeatSPSPPS
                preset.0[cc + 17] = (preset.0[cc + 17] & !0b110) | (1 << 1); // chromaFormatIDC = 1
                if bits == 10 {
                    preset.bytes(PRESET_CFG + 4, &PROFILE_HEVC_MAIN10);
                    preset.u32(cc + 200, 10); // outputBitDepth
                    preset.u32(cc + 204, 10); // inputBitDepth
                }
            }

            let mut init = Blob::<SZ_INITIALIZE>::new();
            init.u32(0, INITIALIZE_VER);
            init.bytes(4, &codec_guid);
            init.bytes(20, &PRESET_P4);
            init.u32(36, width);
            init.u32(40, height);
            init.u32(44, width);
            init.u32(48, height);
            init.u32(52, fps_num);
            init.u32(56, fps_den);
            init.u32(64, 1); // enablePTD: the encoder picks picture types
            init.ptr(88, preset.0[PRESET_CFG..].as_ptr().cast());
            init.u32(136, tuning);
            init.u32(140, this.buffer_format);
            let f: FnPtr = this.func(F_INITIALIZE)?;
            let st = f(this.enc, init.0.as_mut_ptr().cast());
            this.check(st, "initialize encoder")?;
            const _: () = assert!(PRESET_CFG + SZ_CONFIG <= SZ_PRESET_CONFIG);

            // A small ring: enough that a driver which holds a few frames never stalls us.
            let aligned_w = (width + 31) & !31;
            for _ in 0..8 {
                let mut ib = Blob::<SZ_CREATE_INPUT>::new();
                ib.u32(0, CREATE_INPUT_VER);
                ib.u32(4, aligned_w);
                ib.u32(8, this.aligned_h as u32);
                ib.u32(16, this.buffer_format);
                let f: FnPtr = this.func(F_CREATE_INPUT)?;
                this.check(f(this.enc, ib.0.as_mut_ptr().cast()), "create input buffer")?;
                let input = ib.get_ptr(24);

                let mut ob = Blob::<SZ_CREATE_BITSTREAM>::new();
                ob.u32(0, CREATE_BITSTREAM_VER);
                let f: FnPtr = this.func(F_CREATE_BITSTREAM)?;
                if let Err(e) = this.check(f(this.enc, ob.0.as_mut_ptr().cast()), "create bitstream buffer") {
                    let d: FnPtr = this.func(F_DESTROY_INPUT)?;
                    d(this.enc, input);
                    return Err(e);
                }
                this.slots.push(Slot { input, output: ob.get_ptr(16) });
            }
            Ok(this)
        }
    }

    unsafe fn func<T: Copy>(&self, slot: usize) -> Result<T> {
        let raw = u64::from_le_bytes(self.funcs.0[slot..slot + 8].try_into().unwrap());
        if raw == 0 {
            return Err(Error::new(ErrorCode::Internal, format!("NVENC function table slot {slot} is empty")));
        }
        // SAFETY: caller picks `T` as the function-pointer type documented for this slot.
        Ok(unsafe { std::mem::transmute_copy::<u64, T>(&raw) })
    }

    fn bind_context(&self) {
        // SAFETY: cuCtxSetCurrent takes a context we retained and still own.
        unsafe {
            if let Ok(set) = self.cuda.get::<unsafe extern "system" fn(*mut c_void) -> i32>(b"cuCtxSetCurrent\0") {
                set(self.ctx);
            }
        }
    }

    fn check(&self, status: Status, what: &str) -> Result<()> {
        if status == NV_ENC_SUCCESS {
            return Ok(());
        }
        let detail = unsafe {
            self.func::<FnLastError>(F_LAST_ERROR)
                .ok()
                .filter(|_| !self.enc.is_null())
                .map(|f| f(self.enc))
                .filter(|p| !p.is_null())
                .map(|p| CStr::from_ptr(p).to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        Err(Error::new(ErrorCode::Internal, format!("NVENC {what} failed (status {status}{}{detail})", if detail.is_empty() { "" } else { ": " })))
    }

    /// Read finished packets in `pending` order until at most `keep` frames are still in flight.
    /// Leaving a few in flight lets the GPU encode one frame while the CPU copies the next.
    fn drain(&mut self, out: &mut Vec<Packet>, keep: usize) -> Result<()> {
        while self.pending.len() > keep {
            let slot = self.pending.pop_front().expect("checked length");
            let mut lock = Blob::<SZ_LOCK_BITSTREAM>::new();
            lock.u32(0, LOCK_BITSTREAM_VER);
            lock.ptr(8, self.slots[slot].output);
            // SAFETY: lock/unlock bracket the read; the bitstream pointer is valid between them.
            unsafe {
                let f: FnPtr = self.func(F_LOCK_BITSTREAM)?;
                self.check(f(self.enc, lock.0.as_mut_ptr().cast()), "lock bitstream")?;
                let size = lock.get_u32(36) as usize;
                let src = lock.get_ptr(56) as *const u8;
                let pts = i64::from_le_bytes(lock.0[40..48].try_into().unwrap());
                let data = std::slice::from_raw_parts(src, size).to_vec();
                let f: FnPtr = self.func(F_UNLOCK_BITSTREAM)?;
                self.check(f(self.enc, self.slots[slot].output), "unlock bitstream")?;
                out.push(Packet { stream: 0, pts, duration: 1, data });
            }
        }
        Ok(())
    }
}

impl Encoder for NvencEncoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Video(v) = frame else {
            return Err(Error::invalid_input("NVENC encoder received an audio frame"));
        };
        let want = if self.bits == 10 { PixelFormat::Yuv420p10 } else { PixelFormat::Yuv420p };
        if v.pix_fmt != want || v.width != self.width || v.height != self.height {
            return Err(Error::invalid_input(format!(
                "NVENC encoder was opened for {}x{} {} but got {}x{} {}",
                self.width,
                self.height,
                want.name(),
                v.width,
                v.height,
                v.pix_fmt.name()
            )));
        }
        if self.flushed {
            return Err(Error::new(ErrorCode::Internal, "NVENC encoder used after flush"));
        }
        let (w, h) = (self.width as usize, self.height as usize);
        let (cw, ch) = (w / 2, h / 2);
        let bps = if self.bits == 10 { 2 } else { 1 };
        let [y, u, vv] = v.planes.as_slice() else { return Err(Error::invalid_input("yuv420p frame needs 3 planes")) };
        if y.len() < w * h * bps || u.len() < cw * ch * bps || vv.len() < cw * ch * bps {
            return Err(Error::invalid_input("video frame planes are smaller than its dimensions"));
        }
        // 10-bit input goes in as P010: the value in the top ten bits of each word, U and V
        // interleaved. Build that layout on the host, then copy it row by row like 8-bit input.
        let staged: Option<(Vec<u8>, Vec<u8>)> = (bps == 2).then(|| {
            let top = |p: &[u8]| (u16::from_le_bytes([p[0], p[1]]) << 6).to_le_bytes();
            let ys: Vec<u8> = y[..w * h * 2].as_chunks::<2>().0.iter().flat_map(|c| top(c)).collect();
            let mut uv = Vec::with_capacity(cw * ch * 4);
            for (a, b) in u[..cw * ch * 2].as_chunks::<2>().0.iter().zip(vv[..cw * ch * 2].as_chunks::<2>().0) {
                uv.extend_from_slice(&top(a));
                uv.extend_from_slice(&top(b));
            }
            (ys, uv)
        });

        self.bind_context();
        if self.pending.len() >= self.slots.len() {
            return Err(Error::new(ErrorCode::Internal, "NVENC held more frames than the buffer ring"));
        }
        let slot = self.next;
        self.next = (self.next + 1) % self.slots.len();
        let (input, output) = (self.slots[slot].input, self.slots[slot].output);

        // SAFETY: the input buffer is locked for the copy and unlocked before encoding; rows are
        // copied within the pitch/aligned-height NVENC allocated for this surface.
        let pitch = unsafe {
            let mut lock = Blob::<SZ_LOCK_INPUT>::new();
            lock.u32(0, LOCK_INPUT_VER);
            lock.ptr(8, input);
            let f: FnPtr = self.func(F_LOCK_INPUT)?;
            self.check(f(self.enc, lock.0.as_mut_ptr().cast()), "lock input buffer")?;
            let base = lock.get_ptr(16) as *mut u8;
            let pitch = lock.get_u32(24) as usize;
            let copy = |dst_off: usize, dst_pitch: usize, src: &[u8], src_w: usize, rows: usize| {
                for r in 0..rows {
                    ptr::copy_nonoverlapping(src.as_ptr().add(r * src_w), base.add(dst_off + r * dst_pitch), src_w);
                }
            };
            let u_off = pitch * self.aligned_h;
            match &staged {
                Some((ys, uv)) => {
                    copy(0, pitch, ys, w * 2, h);
                    copy(u_off, pitch, uv, cw * 4, ch);
                }
                None => {
                    copy(0, pitch, y, w, h);
                    copy(u_off, pitch / 2, u, cw, ch);
                    copy(u_off + (pitch / 2) * (self.aligned_h / 2), pitch / 2, vv, cw, ch);
                }
            }
            let f: FnPtr = self.func(F_UNLOCK_INPUT)?;
            self.check(f(self.enc, input), "unlock input buffer")?;
            pitch
        };

        let mut pic = Blob::<SZ_PIC_PARAMS>::new();
        pic.u32(0, PIC_PARAMS_VER);
        pic.u32(4, self.width);
        pic.u32(8, self.height);
        pic.u32(12, pitch as u32);
        pic.u32(20, self.frame_idx);
        pic.u64(24, v.pts as u64);
        pic.u64(32, 1);
        pic.ptr(40, input);
        pic.ptr(48, output);
        pic.u32(64, self.buffer_format);
        pic.u32(68, PIC_STRUCT_FRAME);
        self.frame_idx = self.frame_idx.wrapping_add(1);

        let mut out = Vec::new();
        // SAFETY: documented call; `pic` holds valid buffer handles from this session.
        let st = unsafe {
            let f: FnPtr = self.func(F_ENCODE_PICTURE)?;
            f(self.enc, pic.0.as_mut_ptr().cast())
        };
        self.pending.push_back(slot);
        if st == NV_ENC_ERR_NEED_MORE_INPUT {
            return Ok(out); // the driver is buffering; this frame's packet arrives later
        }
        self.check(st, "encode picture")?;
        self.drain(&mut out, self.slots.len() - 2)?;
        Ok(out)
    }

    fn flush(&mut self) -> Result<Vec<Packet>> {
        if self.flushed {
            return Ok(Vec::new());
        }
        self.flushed = true;
        self.bind_context();
        let mut pic = Blob::<SZ_PIC_PARAMS>::new();
        pic.u32(0, PIC_PARAMS_VER);
        pic.u32(16, PIC_FLAG_EOS);
        // SAFETY: an EOS picture with no input tells the driver to emit everything it holds.
        let st = unsafe {
            let f: FnPtr = self.func(F_ENCODE_PICTURE)?;
            f(self.enc, pic.0.as_mut_ptr().cast())
        };
        self.check(st, "flush")?;
        let mut out = Vec::new();
        self.drain(&mut out, 0)?;
        Ok(out)
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        self.bind_context();
        // SAFETY: tears down exactly what `new` created, in reverse order, ignoring errors.
        unsafe {
            if !self.enc.is_null() {
                for s in std::mem::take(&mut self.slots) {
                    if let Ok(f) = self.func::<FnPtr>(F_DESTROY_INPUT) {
                        f(self.enc, s.input);
                    }
                    if let Ok(f) = self.func::<FnPtr>(F_DESTROY_BITSTREAM) {
                        f(self.enc, s.output);
                    }
                }
                if let Ok(f) = self.func::<FnDestroy>(F_DESTROY_ENCODER) {
                    f(self.enc);
                }
            }
            if let Ok(release) = self.cuda.get::<unsafe extern "system" fn(i32) -> i32>(b"cuDevicePrimaryCtxRelease\0") {
                release(self.dev);
            }
        }
    }
}

/// True if `params` describe a stream this provider can accept without conversion.
pub fn accepts(params: &StreamParams, codec: CodecId) -> std::result::Result<(), String> {
    match params {
        StreamParams::Video { width, height, pix_fmt, .. } => {
            if *pix_fmt == PixelFormat::Yuv420p10 && codec != CodecId::Hevc {
                Err(format!("NVENC encodes 10-bit video as HEVC only, not {}", codec.name()))
            } else if !matches!(pix_fmt, PixelFormat::Yuv420p | PixelFormat::Yuv420p10) {
                Err(format!("NVENC input must be yuv420p (or yuv420p10le for HEVC), got {}", pix_fmt.name()))
            } else if width % 2 != 0 || height % 2 != 0 {
                Err(format!("NVENC needs even dimensions, got {width}x{height}"))
            } else {
                Ok(())
            }
        }
        _ => Err("NVENC only encodes video".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Struct sizes and version words as computed by clang from nvEncodeAPI.h (SDK 13.1).
    #[test]
    fn layout_constants_match_the_header() {
        assert_eq!(std::mem::size_of::<Blob<SZ_PRESET_CONFIG>>(), 5128);
        assert_eq!(std::mem::size_of::<Blob<SZ_FUNCTION_LIST>>(), 2552);
        const _: () = assert!(PRESET_CFG + SZ_CONFIG <= SZ_PRESET_CONFIG);
        assert_eq!(FUNCTION_LIST_VER, 1895956493);
        assert_eq!(CREATE_INPUT_VER, 1895956493);
        assert_eq!(OPEN_SESSION_VER, 1895890957);
        assert_eq!(CREATE_BITSTREAM_VER, 1895890957);
        assert_eq!(LOCK_INPUT_VER, 1895890957);
        assert_eq!(CONFIG_VER as i32, -251068403);
        assert_eq!(INITIALIZE_VER as i32, -251199475);
        assert_eq!(PIC_PARAMS_VER as i32, -251199475);
        assert_eq!(PRESET_CONFIG_VER as i32, -251330547);
        assert_eq!(LOCK_BITSTREAM_VER as i32, -251527155);
        assert_eq!(API_VERSION, 16777229);
    }

    #[test]
    fn guid_bytes_are_little_endian_fields() {
        assert_eq!(&CODEC_H264[..4], &0x6bc82762u32.to_le_bytes());
        assert_eq!(CODEC_H264[15], 0xbf);
    }

    #[test]
    fn rejects_unsupported_input_before_touching_the_driver() {
        let odd = StreamParams::Video { width: 33, height: 32, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(30, 1) };
        assert!(accepts(&odd, CodecId::H264).is_err());
        let rgb = StreamParams::Video { width: 32, height: 32, pix_fmt: PixelFormat::Rgb24, frame_rate: Rational::new(30, 1) };
        assert!(accepts(&rgb, CodecId::H264).unwrap_err().contains("yuv420p"));
        let ten = StreamParams::Video { width: 32, height: 32, pix_fmt: PixelFormat::Yuv420p10, frame_rate: Rational::new(30, 1) };
        assert!(accepts(&ten, CodecId::Hevc).is_ok());
        assert!(accepts(&ten, CodecId::H264).unwrap_err().contains("HEVC only"));
        assert!(NvencEncoder::new(CodecId::H264, 32, 32, 10, Rational::new(30, 1), RateControl::ConstQp(23), false).is_err());
        assert!(NvencEncoder::new(CodecId::H264, 33, 32, 8, Rational::new(30, 1), RateControl::ConstQp(23), false).is_err());
    }

    fn synthetic(w: u32, h: u32, n: i64) -> Vec<Frame> {
        (0..n)
            .map(|t| {
                let y = (0..h).flat_map(|r| (0..w).map(move |c| ((c as i64 * 2 + r as i64 + t * 3) & 255) as u8)).collect();
                let c = vec![128u8; (w / 2 * h / 2) as usize];
                Frame::Video(ironav_core::VideoFrame { width: w, height: h, pix_fmt: PixelFormat::Yuv420p, pts: t, planes: vec![y, c.clone(), c] })
            })
            .collect()
    }

    /// Annex B NAL unit types present in `data` (H.264: low 5 bits; HEVC: bits 1..6).
    fn nal_types(data: &[u8], hevc: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 4 < data.len() {
            if data[i..i + 3] == [0, 0, 1] {
                out.push(if hevc { (data[i + 3] >> 1) & 0x3F } else { data[i + 3] & 0x1F });
                i += 3;
            } else {
                i += 1;
            }
        }
        out
    }

    /// Runs on a real GPU when one is present; otherwise reports why it skipped.
    fn roundtrip(codec: CodecId) {
        let mut enc = match NvencEncoder::new(codec, 640, 360, 8, Rational::new(30, 1), RateControl::ConstQp(26), false) {
            Ok(e) => e,
            Err(e) if e.code == ErrorCode::HardwareUnavailable => {
                eprintln!("skipping NVENC test: {}", e.message);
                return;
            }
            Err(e) => panic!("{e:?}"),
        };
        let mut packets = Vec::new();
        for f in synthetic(640, 360, 90) {
            packets.extend(enc.encode(&f).unwrap());
        }
        packets.extend(enc.flush().unwrap());
        assert_eq!(packets.len(), 90, "one packet per frame");
        assert!(packets.windows(2).all(|w| w[0].pts < w[1].pts), "output order must equal input order");
        assert!(packets.iter().all(|p| !p.data.is_empty() && p.data.starts_with(&[0, 0, 0, 1])), "Annex B start codes");
        let bytes: Vec<u8> = packets.iter().flat_map(|p| p.data.clone()).collect();
        let types = nal_types(&bytes, codec == CodecId::Hevc);
        // IDR slice, SPS and PPS must all be present (H.264: 5/7/8; HEVC: 19-20 IDR, 33 SPS, 34 PPS).
        let want: &[&[u8]] = if codec == CodecId::Hevc { &[&[19, 20], &[33], &[34]] } else { &[&[5], &[7], &[8]] };
        for alts in want {
            assert!(types.iter().any(|t| alts.contains(t)), "missing NAL {alts:?} in {:?}", &types[..types.len().min(12)]);
        }
        eprintln!("{codec:?}: {} frames -> {} bytes", packets.len(), bytes.len());
    }

    #[test]
    fn low_latency_sends_every_picture_in_order_with_a_keyframe_each_second() {
        let mut enc = match NvencEncoder::new(CodecId::H264, 640, 360, 8, Rational::new(30, 1), RateControl::Bitrate(1_000_000), true) {
            Ok(e) => e,
            Err(e) if e.code == ErrorCode::HardwareUnavailable => {
                eprintln!("skipping NVENC test: {}", e.message);
                return;
            }
            Err(e) => panic!("{e:?}"),
        };
        let mut packets = Vec::new();
        for f in synthetic(640, 360, 90) {
            packets.extend(enc.encode(&f).unwrap());
        }
        packets.extend(enc.flush().unwrap());
        assert_eq!(packets.len(), 90, "one packet per frame");
        assert!(packets.windows(2).all(|w| w[0].pts < w[1].pts), "output order must equal input order");
        let idrs = packets.iter().filter(|p| nal_types(&p.data, false).contains(&5)).count();
        assert!(idrs >= 3, "an IDR picture each second, got {idrs} in three seconds");
    }

    #[test]
    fn h264_encodes_on_the_gpu_in_order() {
        roundtrip(CodecId::H264);
    }

    #[test]
    fn hevc_encodes_on_the_gpu_in_order() {
        roundtrip(CodecId::Hevc);
    }

    #[test]
    fn hevc_main10_encodes_ten_bit_frames() {
        let mut enc = match NvencEncoder::new(CodecId::Hevc, 640, 360, 10, Rational::new(30, 1), RateControl::ConstQp(26), false) {
            Ok(e) => e,
            Err(e) if e.code == ErrorCode::HardwareUnavailable => {
                eprintln!("skipping NVENC test: {}", e.message);
                return;
            }
            Err(e) => panic!("{e:?}"),
        };
        let (w, h) = (640usize, 360usize);
        let frame = |t: i64| {
            let y: Vec<u8> = (0..w * h).flat_map(|i| ((((i % w) * 3 + (i / w) + t as usize * 7) % 1024) as u16).to_le_bytes()).collect();
            let c: Vec<u8> = (0..w * h / 4).flat_map(|_| 512u16.to_le_bytes()).collect();
            Frame::Video(ironav_core::VideoFrame { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p10, pts: t, planes: vec![y, c.clone(), c] })
        };
        let mut packets = Vec::new();
        for t in 0..30 {
            packets.extend(enc.encode(&frame(t)).unwrap());
        }
        packets.extend(enc.flush().unwrap());
        assert_eq!(packets.len(), 30);
        // The SPS says Main 10: general_profile_idc 2, after the 2-byte NAL header and 1 byte.
        let bytes: Vec<u8> = packets.iter().flat_map(|p| p.data.clone()).collect();
        let at = bytes.windows(5).position(|b| b[..3] == [0, 0, 1] && (b[3] >> 1) & 0x3F == 33).expect("an SPS");
        assert_eq!(bytes[at + 3 + 3] & 0x1F, 2, "profile_idc should be Main 10");
        // An 8-bit frame is refused by a 10-bit session, and the other way round.
        let eight = Frame::Video(ironav_core::VideoFrame { width: 640, height: 360, pix_fmt: PixelFormat::Yuv420p, pts: 0, planes: vec![vec![0; w * h], vec![0; w * h / 4], vec![0; w * h / 4]] });
        let mut enc2 = NvencEncoder::new(CodecId::Hevc, 640, 360, 10, Rational::new(30, 1), RateControl::ConstQp(26), false).unwrap();
        assert!(enc2.encode(&eight).is_err());
    }
}
