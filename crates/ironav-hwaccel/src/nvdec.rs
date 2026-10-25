//! NVIDIA NVDEC provider: H.264 and HEVC decoding on the GPU through the driver's `nvcuvid`
//! library, loaded at run time like the NVENC provider.
//!
//! The driver's own bitstream parser does the NAL parsing, reference management and output
//! reordering: packets (Annex B access units, as the MP4 reader produces them) go in, and
//! decoded pictures come out in display order through callbacks. Each picture is copied from
//! the GPU into a planar `yuv420p` [`VideoFrame`] with its original timestamp.
//!
//! Struct layouts below were read from NVIDIA's `cuviddec.h` / `nvcuvid.h` (nv-codec-headers
//! n13.x) with `clang -fdump-record-layouts`; the tests pin their sizes. Only 8-bit 4:2:0 is
//! accepted, and the GPU is asked first whether it can decode the stream.

use ironav_core::{CodecId, Decoder, Error, ErrorCode, Frame, Packet, PixelFormat, Result, StreamParams, VideoFrame};
use libloading::Library;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr;

/// Codecs this provider can decode.
pub const DECODE_CODECS: &[CodecId] = &[CodecId::H264, CodecId::Hevc];

const CODEC_H264: u32 = 4;
const CODEC_HEVC: u32 = 8;
const CHROMA_420: u32 = 1;
const SURFACE_NV12: u32 = 0;
const DEINTERLACE_WEAVE: u32 = 0;
const CREATE_PREFER_CUVID: u32 = 4;
const PKT_ENDOFSTREAM: u32 = 1;
const PKT_TIMESTAMP: u32 = 2;
const PKT_ENDOFPICTURE: u32 = 8;
const MEM_HOST: u32 = 1;
const MEM_DEVICE: u32 = 2;

#[repr(C)]
struct CreateInfo {
    width: u32,
    height: u32,
    num_decode_surfaces: u32,
    codec: u32,
    chroma_format: u32,
    creation_flags: u32,
    bit_depth_minus8: u32,
    intra_decode_only: u32,
    max_width: u32,
    max_height: u32,
    reserved1: u32,
    display_area: [i16; 4],
    output_format: u32,
    deinterlace: u32,
    target_width: u32,
    target_height: u32,
    num_output_surfaces: u32,
    vid_lock: *mut c_void,
    target_rect: [i16; 4],
    enable_histogram: u32,
    enable_decode_features: u32,
    reserved2: [u32; 3],
}

#[repr(C)]
struct ParserParams {
    codec: u32,
    max_num_decode_surfaces: u32,
    clock_rate: u32,
    error_threshold: u32,
    max_display_delay: u32,
    flags: u32,
    reserved1: [u32; 4],
    user_data: *mut c_void,
    on_sequence: Option<unsafe extern "system" fn(*mut c_void, *const VideoFormat) -> i32>,
    on_decode: Option<unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32>,
    on_display: Option<unsafe extern "system" fn(*mut c_void, *const DispInfo) -> i32>,
    on_operating_point: *mut c_void,
    on_sei: *mut c_void,
    reserved2: [*mut c_void; 5],
    ext_video_info: *mut c_void,
}

#[repr(C)]
struct SourcePacket {
    flags: u32,
    payload_size: u32,
    payload: *const u8,
    timestamp: i64,
}

#[repr(C)]
struct DispInfo {
    picture_index: i32,
    progressive_frame: i32,
    top_field_first: i32,
    repeat_first_field: i32,
    timestamp: i64,
}

#[repr(C)]
struct VideoFormat {
    codec: u32,
    frame_rate_num: u32,
    frame_rate_den: u32,
    progressive: u8,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
    min_num_decode_surfaces: u8,
    coded_width: u32,
    coded_height: u32,
    display_area: [i32; 4], // left, top, right, bottom
    chroma_format: u32,
    bitrate: u32,
    aspect: [i32; 2],
    signal: [u8; 4],
    seqhdr_data_length: u32,
}

#[repr(C)]
struct ProcParams {
    progressive_frame: i32,
    second_field: i32,
    top_field_first: i32,
    unpaired_field: i32,
    reserved_flags: u32,
    reserved_zero: u32,
    raw_input_dptr: u64,
    raw_input_pitch: u32,
    raw_input_format: u32,
    raw_output_dptr: u64,
    raw_output_pitch: u32,
    reserved1: u32,
    output_stream: *mut c_void,
    reserved: [u32; 46],
    histogram_dptr: *mut u64,
    proc_ext: *mut c_void,
}

#[repr(C)]
struct Caps {
    codec: u32,
    chroma_format: u32,
    bit_depth_minus8: u32,
    reserved1: [u32; 3],
    is_supported: u8,
    num_nvdecs: u8,
    output_format_mask: u16,
    max_width: u32,
    max_height: u32,
    max_mb_count: u32,
    min_width: u16,
    min_height: u16,
    rest: [u8; 44],
}

#[repr(C)]
struct Memcpy2D {
    src_x_bytes: u64,
    src_y: u64,
    src_type: u32,
    src_host: *const c_void,
    src_device: u64,
    src_array: *mut c_void,
    src_pitch: u64,
    dst_x_bytes: u64,
    dst_y: u64,
    dst_type: u32,
    dst_host: *mut c_void,
    dst_device: u64,
    dst_array: *mut c_void,
    dst_pitch: u64,
    width_bytes: u64,
    height: u64,
}

type CuResult = i32;
type FnCreateParser = unsafe extern "system" fn(*mut *mut c_void, *mut ParserParams) -> CuResult;
type FnParse = unsafe extern "system" fn(*mut c_void, *mut SourcePacket) -> CuResult;
type FnDestroy = unsafe extern "system" fn(*mut c_void) -> CuResult;
type FnCreateDecoder = unsafe extern "system" fn(*mut *mut c_void, *mut CreateInfo) -> CuResult;
type FnDecodePicture = unsafe extern "system" fn(*mut c_void, *mut c_void) -> CuResult;
type FnMap = unsafe extern "system" fn(*mut c_void, i32, *mut u64, *mut u32, *mut ProcParams) -> CuResult;
type FnUnmap = unsafe extern "system" fn(*mut c_void, u64) -> CuResult;
type FnCaps = unsafe extern "system" fn(*mut Caps) -> CuResult;
type FnMemcpy2D = unsafe extern "system" fn(*const Memcpy2D) -> CuResult;
type FnSetCtx = unsafe extern "system" fn(*mut c_void) -> CuResult;

/// Entry points copied out of the libraries, which [`NvdecDecoder`] keeps loaded.
#[derive(Clone, Copy)]
struct Api {
    create_parser: FnCreateParser,
    parse: FnParse,
    destroy_parser: FnDestroy,
    create_decoder: FnCreateDecoder,
    decode_picture: FnDecodePicture,
    map: FnMap,
    unmap: FnUnmap,
    destroy_decoder: FnDestroy,
    caps: FnCaps,
    memcpy2d: FnMemcpy2D,
}

fn unavailable(msg: impl Into<String>) -> Error {
    Error::new(ErrorCode::HardwareUnavailable, msg)
        .hint("run `ironav hw probe` to see what is detected; NVDEC needs an NVIDIA GPU and driver")
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

fn cu(r: CuResult, what: &str) -> Result<()> {
    if r == 0 {
        Ok(())
    } else {
        Err(Error::new(ErrorCode::Internal, format!("NVDEC: {what} failed with CUDA error {r}")))
    }
}

/// Everything the parser callbacks touch. It lives on the heap so its address, handed to the
/// parser as the user pointer, never changes.
struct State {
    api: Api,
    codec: CodecId,
    /// Display size the stream declared; the decoded pictures must match it.
    width: u32,
    height: u32,
    decoder: *mut c_void,
    /// Coded height of the current decoder, for locating the chroma plane in a mapped frame.
    coded_height: u32,
    display: [i32; 4],
    out: VecDeque<Frame>,
    /// Host copy of the interleaved chroma plane, kept between pictures.
    uv_scratch: Vec<u8>,
    error: Option<Error>,
}

impl State {
    fn fail(&mut self, e: Error) -> i32 {
        self.error.get_or_insert(e);
        0
    }

    fn on_sequence(&mut self, f: &VideoFormat) -> i32 {
        let want = if self.codec == CodecId::Hevc { CODEC_HEVC } else { CODEC_H264 };
        if f.codec != want {
            return self.fail(Error::invalid_input(format!("the bitstream is not {}", self.codec.name())));
        }
        if f.chroma_format != CHROMA_420 || f.bit_depth_luma_minus8 != 0 || f.bit_depth_chroma_minus8 != 0 {
            return self.fail(
                Error::new(
                    ErrorCode::UnsupportedCodec,
                    format!(
                        "NVDEC provider handles 8-bit 4:2:0 only; the stream is {}-bit with chroma format {}",
                        f.bit_depth_luma_minus8 as u32 + 8,
                        f.chroma_format
                    ),
                )
                .hint("only 8-bit yuv420p video can be decoded so far"),
            );
        }
        let (l, t, r, b) = (f.display_area[0], f.display_area[1], f.display_area[2], f.display_area[3]);
        let (dw, dh) = ((r - l).max(0) as u32, (b - t).max(0) as u32);
        if (dw, dh) != (self.width, self.height) {
            return self.fail(
                Error::new(ErrorCode::UnsupportedFormat, format!("the stream is {dw}x{dh} but was declared {}x{}; changing resolution mid-stream is not supported", self.width, self.height)),
            );
        }
        let mut caps = Caps {
            codec: f.codec,
            chroma_format: f.chroma_format,
            bit_depth_minus8: 0,
            reserved1: [0; 3],
            is_supported: 0,
            num_nvdecs: 0,
            output_format_mask: 0,
            max_width: 0,
            max_height: 0,
            max_mb_count: 0,
            min_width: 0,
            min_height: 0,
            rest: [0; 44],
        };
        // SAFETY: `caps` is a correctly laid-out CUVIDDECODECAPS the driver fills in.
        let r = unsafe { (self.api.caps)(&mut caps) };
        if r != 0 {
            return self.fail(Error::new(ErrorCode::Internal, format!("NVDEC: cuvidGetDecoderCaps failed with CUDA error {r}")));
        }
        if caps.is_supported == 0 || f.coded_width > caps.max_width || f.coded_height > caps.max_height {
            return self.fail(
                Error::new(
                    ErrorCode::HardwareUnavailable,
                    format!("this GPU's NVDEC cannot decode {} at {}x{} (it supports up to {}x{})", self.codec.name(), f.coded_width, f.coded_height, caps.max_width, caps.max_height),
                )
                .hint("set hwaccel to \"off\" only if a software decoder exists; none does for this codec yet"),
            );
        }

        // A new sequence replaces the decoder (the old one may differ in size or DPB depth).
        if !self.decoder.is_null() {
            // SAFETY: the decoder was created by us and is not in use during a sequence change.
            unsafe { (self.api.destroy_decoder)(self.decoder) };
            self.decoder = ptr::null_mut();
        }
        let surfaces = (f.min_num_decode_surfaces as u32).max(1);
        let mut info = CreateInfo {
            width: f.coded_width,
            height: f.coded_height,
            num_decode_surfaces: surfaces,
            codec: f.codec,
            chroma_format: f.chroma_format,
            creation_flags: CREATE_PREFER_CUVID,
            bit_depth_minus8: 0,
            intra_decode_only: 0,
            max_width: f.coded_width,
            max_height: f.coded_height,
            reserved1: 0,
            display_area: [0; 4],
            output_format: SURFACE_NV12,
            deinterlace: DEINTERLACE_WEAVE,
            target_width: f.coded_width,
            target_height: f.coded_height,
            num_output_surfaces: 2,
            vid_lock: ptr::null_mut(),
            target_rect: [0; 4],
            enable_histogram: 0,
            enable_decode_features: 0,
            reserved2: [0; 3],
        };
        // SAFETY: `info` is a fully initialised CUVIDDECODECREATEINFO.
        let r = unsafe { (self.api.create_decoder)(&mut self.decoder, &mut info) };
        if r != 0 {
            self.decoder = ptr::null_mut();
            return self.fail(Error::new(ErrorCode::HardwareUnavailable, format!("NVDEC could not create a decoder (CUDA error {r})")));
        }
        self.coded_height = f.coded_height;
        self.display = f.display_area;
        surfaces as i32
    }

    fn on_decode(&mut self, pic: *mut c_void) -> i32 {
        if self.decoder.is_null() {
            return self.fail(Error::new(ErrorCode::Internal, "NVDEC: picture arrived before the sequence header"));
        }
        // SAFETY: `pic` is the parser's CUVIDPICPARAMS, passed straight through.
        let r = unsafe { (self.api.decode_picture)(self.decoder, pic) };
        if r != 0 {
            return self.fail(Error::invalid_input(format!("NVDEC could not decode a picture (CUDA error {r}); the stream may be corrupt")));
        }
        1
    }

    fn on_display(&mut self, d: &DispInfo) -> i32 {
        match self.fetch(d) {
            Ok(f) => {
                self.out.push_back(f);
                1
            }
            Err(e) => self.fail(e),
        }
    }

    /// Copy one decoded picture out of GPU memory as planar yuv420p, cropped to the display area.
    fn fetch(&mut self, d: &DispInfo) -> Result<Frame> {
        let mut proc: ProcParams = ProcParams {
            progressive_frame: d.progressive_frame,
            second_field: 0,
            top_field_first: d.top_field_first,
            unpaired_field: 0,
            reserved_flags: 0,
            reserved_zero: 0,
            raw_input_dptr: 0,
            raw_input_pitch: 0,
            raw_input_format: 0,
            raw_output_dptr: 0,
            raw_output_pitch: 0,
            reserved1: 0,
            output_stream: ptr::null_mut(),
            reserved: [0; 46],
            histogram_dptr: ptr::null_mut(),
            proc_ext: ptr::null_mut(),
        };
        let (mut dptr, mut pitch) = (0u64, 0u32);
        // SAFETY: maps a picture index the parser just reported; unmapped below on every path.
        cu(unsafe { (self.api.map)(self.decoder, d.picture_index, &mut dptr, &mut pitch, &mut proc) }, "cuvidMapVideoFrame")?;
        let mut uv = std::mem::take(&mut self.uv_scratch);
        let copied = self.copy_planes(dptr, pitch, &mut uv);
        // SAFETY: `dptr` came from the successful map above.
        unsafe { (self.api.unmap)(self.decoder, dptr) };
        let (w, h) = (self.width as usize, self.height as usize);
        let y = match copied {
            Ok(y) => y,
            Err(e) => {
                self.uv_scratch = uv;
                return Err(e);
            }
        };
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let (mut u, mut v) = (vec![0u8; cw * ch], vec![0u8; cw * ch]);
        for ((u, v), p) in u.iter_mut().zip(v.iter_mut()).zip(uv.chunks_exact(2)) {
            *u = p[0];
            *v = p[1];
        }
        self.uv_scratch = uv;
        Ok(Frame::Video(VideoFrame { width: self.width, height: self.height, pix_fmt: PixelFormat::Yuv420p, pts: d.timestamp, planes: vec![y, u, v] }))
    }

    /// NV12 luma and interleaved chroma of the display area, as tightly packed host buffers.
    /// The chroma goes into `uv`, which is reused between pictures so it is not reallocated
    /// (and page-faulted in) for every one.
    fn copy_planes(&self, dptr: u64, pitch: u32, uv: &mut Vec<u8>) -> Result<Vec<u8>> {
        let (w, h) = (self.width as usize, self.height as usize);
        let (left, top) = (self.display[0].max(0) as u64 & !1, self.display[1].max(0) as u64 & !1);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let mut y = vec![0u8; w * h];
        uv.resize(cw * 2 * ch, 0);
        // Mapped frames keep the chroma plane right after the luma plane, whose height is the
        // coded height rounded up to even.
        let chroma_base = dptr + pitch as u64 * ((self.coded_height as u64 + 1) & !1);
        let copy = |src: u64, src_x: u64, src_y: u64, dst: &mut [u8], width: usize, rows: usize| -> Result<()> {
            let m = Memcpy2D {
                src_x_bytes: src_x,
                src_y,
                src_type: MEM_DEVICE,
                src_host: ptr::null(),
                src_device: src,
                src_array: ptr::null_mut(),
                src_pitch: pitch as u64,
                dst_x_bytes: 0,
                dst_y: 0,
                dst_type: MEM_HOST,
                dst_host: dst.as_mut_ptr().cast(),
                dst_device: 0,
                dst_array: ptr::null_mut(),
                dst_pitch: width as u64,
                width_bytes: width as u64,
                height: rows as u64,
            };
            // SAFETY: the destination holds `width * rows` bytes and the source is a mapped frame.
            cu(unsafe { (self.api.memcpy2d)(&m) }, "cuMemcpy2D")
        };
        copy(dptr, left, top, &mut y, w, h)?;
        copy(chroma_base, left, top / 2, uv, cw * 2, ch)?;
        Ok(y)
    }
}

unsafe extern "system" fn sequence_cb(user: *mut c_void, f: *const VideoFormat) -> i32 {
    // SAFETY: `user` is the State pointer given to the parser; `f` is valid for the call.
    unsafe { (*user.cast::<State>()).on_sequence(&*f) }
}
unsafe extern "system" fn decode_cb(user: *mut c_void, pic: *mut c_void) -> i32 {
    // SAFETY: as above.
    unsafe { (*user.cast::<State>()).on_decode(pic) }
}
unsafe extern "system" fn display_cb(user: *mut c_void, d: *const DispInfo) -> i32 {
    // SAFETY: as above. A null `d` marks end of stream in some driver versions.
    unsafe {
        if d.is_null() {
            return 1;
        }
        (*user.cast::<State>()).on_display(&*d)
    }
}

pub struct NvdecDecoder {
    _cuvid: Library,
    cuda: Library,
    ctx: *mut c_void,
    dev: i32,
    state: *mut State,
    parser: *mut c_void,
    set_ctx: FnSetCtx,
    flushed: bool,
}

// SAFETY: the decoder is moved between threads, never shared, and the CUDA context is bound to
// the calling thread before every driver call.
unsafe impl Send for NvdecDecoder {}

impl NvdecDecoder {
    /// Open a decoder for `codec` on the first NVIDIA GPU. `width` and `height` are the
    /// stream's display size; frames of any other size are rejected.
    pub fn new(codec: CodecId, width: u32, height: u32) -> Result<Self> {
        let parser_codec = match codec {
            CodecId::H264 => CODEC_H264,
            CodecId::Hevc => CODEC_HEVC,
            other => return Err(Error::invalid_arg(format!("NVDEC provider cannot decode {}", other.name())).alternatives(["h264", "hevc"])),
        };
        let cuvid = load(&if cfg!(windows) { vec!["nvcuvid.dll"] } else { vec!["libnvcuvid.so.1", "libnvcuvid.so"] })
            .map_err(|e| unavailable(format!("NVDEC driver library not found ({e})")))?;
        let cuda = load(&if cfg!(windows) { vec!["nvcuda.dll"] } else { vec!["libcuda.so.1"] })
            .map_err(|e| unavailable(format!("CUDA driver library not found ({e})")))?;

        // SAFETY: symbols are looked up by name and cast to the prototypes in NVIDIA's headers.
        unsafe {
            macro_rules! sym {
                ($lib:expr, $name:literal, $ty:ty) => {
                    *$lib.get::<$ty>(concat!($name, "\0").as_bytes()).map_err(|e| unavailable(format!("{} entry point missing: {e}", $name)))?
                };
            }
            let api = Api {
                create_parser: sym!(cuvid, "cuvidCreateVideoParser", FnCreateParser),
                parse: sym!(cuvid, "cuvidParseVideoData", FnParse),
                destroy_parser: sym!(cuvid, "cuvidDestroyVideoParser", FnDestroy),
                create_decoder: sym!(cuvid, "cuvidCreateDecoder", FnCreateDecoder),
                decode_picture: sym!(cuvid, "cuvidDecodePicture", FnDecodePicture),
                map: sym!(cuvid, "cuvidMapVideoFrame64", FnMap),
                unmap: sym!(cuvid, "cuvidUnmapVideoFrame64", FnUnmap),
                destroy_decoder: sym!(cuvid, "cuvidDestroyDecoder", FnDestroy),
                caps: sym!(cuvid, "cuvidGetDecoderCaps", FnCaps),
                memcpy2d: sym!(cuda, "cuMemcpy2D_v2", FnMemcpy2D),
            };
            let cu_init = sym!(cuda, "cuInit", unsafe extern "system" fn(u32) -> i32);
            let cu_dev = sym!(cuda, "cuDeviceGet", unsafe extern "system" fn(*mut i32, i32) -> i32);
            let cu_retain = sym!(cuda, "cuDevicePrimaryCtxRetain", unsafe extern "system" fn(*mut *mut c_void, i32) -> i32);
            let set_ctx = sym!(cuda, "cuCtxSetCurrent", FnSetCtx);
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
            set_ctx(ctx);

            let state = Box::into_raw(Box::new(State {
                api,
                codec,
                width,
                height,
                decoder: ptr::null_mut(),
                coded_height: 0,
                display: [0; 4],
                out: VecDeque::new(),
                uv_scratch: Vec::new(),
                error: None,
            }));
            // From here on the struct owns everything, so Drop cleans up on any error path.
            let mut this = NvdecDecoder { _cuvid: cuvid, cuda, ctx, dev, state, parser: ptr::null_mut(), set_ctx, flushed: false };
            let mut params = ParserParams {
                codec: parser_codec,
                max_num_decode_surfaces: 1,
                clock_rate: 0,
                error_threshold: 100,
                // Wait a few pictures before output so B-frame streams come out in display order.
                max_display_delay: 4,
                flags: 0,
                reserved1: [0; 4],
                user_data: state.cast(),
                on_sequence: Some(sequence_cb),
                on_decode: Some(decode_cb),
                on_display: Some(display_cb),
                on_operating_point: ptr::null_mut(),
                on_sei: ptr::null_mut(),
                reserved2: [ptr::null_mut(); 5],
                ext_video_info: ptr::null_mut(),
            };
            cu((api.create_parser)(&mut this.parser, &mut params), "cuvidCreateVideoParser")?;
            Ok(this)
        }
    }

    fn state(&mut self) -> &mut State {
        // SAFETY: allocated in `new`, freed only in Drop, and callbacks run only inside `feed`.
        unsafe { &mut *self.state }
    }

    fn feed(&mut self, data: &[u8], pts: i64, flags: u32) -> Result<Vec<Frame>> {
        // SAFETY: binds the context we retained to this thread.
        unsafe { (self.set_ctx)(self.ctx) };
        let mut pkt = SourcePacket { flags, payload_size: data.len() as u32, payload: if data.is_empty() { ptr::null() } else { data.as_ptr() }, timestamp: pts };
        // SAFETY: the parser calls back into `self.state` synchronously before returning.
        let r = unsafe { (self.state().api.parse)(self.parser, &mut pkt) };
        if let Some(e) = self.state().error.take() {
            return Err(e);
        }
        cu(r, "cuvidParseVideoData")?;
        Ok(self.state().out.drain(..).collect())
    }
}

impl Decoder for NvdecDecoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>> {
        if packet.data.len() > u32::MAX as usize {
            return Err(Error::invalid_input("a single video packet exceeds 4 GiB"));
        }
        self.feed(&packet.data, packet.pts, PKT_TIMESTAMP | PKT_ENDOFPICTURE)
    }

    fn flush(&mut self) -> Result<Vec<Frame>> {
        if self.flushed {
            return Ok(Vec::new());
        }
        self.flushed = true;
        self.feed(&[], 0, PKT_ENDOFSTREAM)
    }
}

impl Drop for NvdecDecoder {
    fn drop(&mut self) {
        // SAFETY: tears down exactly what `new` and the callbacks created, in reverse order.
        unsafe {
            (self.set_ctx)(self.ctx);
            let api = (*self.state).api;
            if !self.parser.is_null() {
                (api.destroy_parser)(self.parser);
            }
            let st = Box::from_raw(self.state);
            if !st.decoder.is_null() {
                (api.destroy_decoder)(st.decoder);
            }
            drop(st);
            if let Ok(release) = self.cuda.get::<unsafe extern "system" fn(i32) -> i32>(b"cuDevicePrimaryCtxRelease\0") {
                release(self.dev);
            }
        }
    }
}

/// True if `params` describe a stream this provider can decode.
pub fn accepts(params: &StreamParams) -> std::result::Result<(), String> {
    match params {
        StreamParams::Video { pix_fmt, .. } if *pix_fmt == PixelFormat::Yuv420p => Ok(()),
        StreamParams::Video { pix_fmt, .. } => Err(format!("NVDEC provider decodes 8-bit yuv420p only, the stream is {}", pix_fmt.name())),
        _ => Err("NVDEC only decodes video".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    /// Struct sizes as computed by clang from the NVIDIA headers.
    #[test]
    fn layout_matches_the_headers() {
        assert_eq!(size_of::<CreateInfo>(), 112);
        assert_eq!(size_of::<ParserParams>(), 136);
        assert_eq!(size_of::<SourcePacket>(), 24);
        assert_eq!(size_of::<DispInfo>(), 24);
        assert_eq!(size_of::<VideoFormat>(), 64);
        assert_eq!(size_of::<ProcParams>(), 264);
        assert_eq!(size_of::<Caps>(), 88);
        assert_eq!(size_of::<Memcpy2D>(), 128);
        assert_eq!(std::mem::offset_of!(CreateInfo, vid_lock), 72);
        assert_eq!(std::mem::offset_of!(ParserParams, user_data), 40);
        assert_eq!(std::mem::offset_of!(VideoFormat, display_area), 24);
        assert_eq!(std::mem::offset_of!(ProcParams, output_stream), 56);
        assert_eq!(std::mem::offset_of!(Caps, is_supported), 24);
    }

    #[test]
    fn rejects_other_codecs_without_touching_the_driver() {
        let e = NvdecDecoder::new(CodecId::Flac, 64, 64).err().unwrap();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
    }
}
