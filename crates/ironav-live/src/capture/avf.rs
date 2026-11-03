//! Cameras and microphones on macOS through AVFoundation: an `AVCaptureSession` with a video or
//! audio data output, whose delegate gets each `CMSampleBuffer` on a dispatch queue of its own and
//! hands a copy to this thread through a bounded channel (a buffer is dropped when it is full).
//!
//! Cameras are asked for NV12 and written as Y4M; microphones give 16-bit little-endian PCM, which
//! is written behind a WAV header (other linear PCM layouts are converted). The picture size and
//! the sample rate are those of the first buffer, so the header goes out with it.

use super::{choose, pcm_to_s16le, pick_mode, send, wav_header, y4m_frame_nv12_planes, y4m_header, Device, Samples, VideoWants};
use block2::RcBlock;
use dispatch2::DispatchQueue;
use ironav_core::{Error, ErrorCode, Rational, Result};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureAudioDataOutput, AVCaptureAudioDataOutputSampleBufferDelegate, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceInput, AVCaptureOutput, AVCaptureSession, AVCaptureVideoDataOutput,
    AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaTypeAudio, AVMediaTypeVideo,
};
use objc2_core_audio_types::AudioStreamBasicDescription;
use objc2_core_media::{CMAudioFormatDescriptionGetStreamBasicDescription, CMSampleBuffer, CMTime, CMVideoFormatDescriptionGetDimensions};
use objc2_core_video::{
    kCVReturnSuccess, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSDictionary, NSNumber, NSObject, NSObjectProtocol, NSString};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type Tx = SyncSender<std::io::Result<Vec<u8>>>;

/// '420v' and '420f': NV12 with video-range and full-range luma.
const NV12_VIDEO: u32 = 0x3432_3076;
const NV12_FULL: u32 = 0x3432_3066;
/// 'lpcm'
const LINEAR_PCM: u32 = 0x6C70_636D;
const FLAG_FLOAT: u32 = 1;
const FLAG_BIG_ENDIAN: u32 = 2;
const FLAG_NON_INTERLEAVED: u32 = 0x20;
/// Buffers waiting for the capture thread, before the delegate drops them.
const QUEUED: usize = 4;
/// How long to wait for the person at the machine to answer the permission prompt.
const PROMPT: Duration = Duration::from_secs(60);

struct Ivars {
    video: bool,
    out: SyncSender<Vec<u8>>,
    failed: Arc<Mutex<Option<String>>>,
    rate: Rational,
    started: AtomicBool,
}

impl Ivars {
    fn fail(&self, why: String) {
        self.failed.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(why);
    }

    /// Queues a buffer, with the stream's header before the first one; a full queue drops it.
    fn push(&self, header: impl FnOnce() -> Vec<u8>, mut data: Vec<u8>) {
        let first = !self.started.load(Ordering::Relaxed);
        if first {
            data = [header(), data].concat();
        }
        if self.out.try_send(data).is_ok() && first {
            self.started.store(true, Ordering::Relaxed);
        }
    }

    fn picture(&self, sb: &CMSampleBuffer) {
        // SAFETY: the buffer is valid for the call
        let Some(img) = (unsafe { sb.image_buffer() }) else { return };
        let pb = &*img;
        let format = CVPixelBufferGetPixelFormatType(pb);
        if (format != NV12_VIDEO && format != NV12_FULL) || CVPixelBufferGetPlaneCount(pb) < 2 {
            self.fail(format!("the camera gave pictures that are not NV12 (pixel format {format:#x})"));
            return;
        }
        // SAFETY: unlocked below, on every path
        if unsafe { CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) } != kCVReturnSuccess {
            return;
        }
        // 4:2:0 needs even sizes: an odd last row or column is left out
        let (w, h) = (CVPixelBufferGetWidth(pb) & !1, CVPixelBufferGetHeight(pb) & !1);
        let (yp, ys, yrows) = (CVPixelBufferGetBaseAddressOfPlane(pb, 0) as *const u8, CVPixelBufferGetBytesPerRowOfPlane(pb, 0), CVPixelBufferGetHeightOfPlane(pb, 0));
        let (cp, cs, crows) = (CVPixelBufferGetBaseAddressOfPlane(pb, 1) as *const u8, CVPixelBufferGetBytesPerRowOfPlane(pb, 1), CVPixelBufferGetHeightOfPlane(pb, 1));
        let frame = if yp.is_null() || cp.is_null() {
            None
        } else {
            // SAFETY: the planes are locked, `rows` rows of `stride` bytes each
            let (y, uv) = unsafe { (std::slice::from_raw_parts(yp, ys * yrows), std::slice::from_raw_parts(cp, cs * crows)) };
            y4m_frame_nv12_planes(y, ys, uv, cs, w, h)
        };
        // SAFETY: locked above
        unsafe { CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) };
        if let Some(f) = frame {
            self.push(|| y4m_header(w as u32, h as u32, self.rate), f);
        }
    }

    fn sound(&self, sb: &CMSampleBuffer) {
        // SAFETY: the buffer is valid for the call
        let (Some(desc), Some(block)) = (unsafe { sb.format_description() }, unsafe { sb.data_buffer() }) else { return };
        // SAFETY: a pointer to the description inside `desc`, or null
        let Some(a) = (unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(&desc).as_ref() }).copied() else { return };
        let kind = match sample_kind(&a) {
            Ok(k) => k,
            Err(why) => {
                self.fail(why);
                return;
            }
        };
        // SAFETY: the block buffer is valid for the call
        let len = unsafe { block.data_length() };
        let mut raw = vec![0u8; len];
        let Some(dest) = NonNull::new(raw.as_mut_ptr().cast()) else { return };
        // SAFETY: `dest` has room for `len` bytes
        if len == 0 || unsafe { block.copy_data_bytes(0, len, dest) } != 0 {
            return;
        }
        let mut pcm = Vec::with_capacity(len);
        pcm_to_s16le(&raw, kind, &mut pcm);
        self.push(|| wav_header(a.mSampleRate as u32, a.mChannelsPerFrame.clamp(1, 8) as u16), pcm);
    }
}

/// The samples an audio description says its buffers hold, if this can read them.
fn sample_kind(a: &AudioStreamBasicDescription) -> std::result::Result<Samples, String> {
    let (flags, bits) = (a.mFormatFlags, a.mBitsPerChannel);
    if a.mFormatID != LINEAR_PCM || flags & FLAG_BIG_ENDIAN != 0 || flags & FLAG_NON_INTERLEAVED != 0 {
        return Err(format!("the microphone gave samples that are not interleaved little-endian linear PCM (format {:#x}, flags {flags:#x})", a.mFormatID));
    }
    match (flags & FLAG_FLOAT != 0, bits) {
        (false, 16) => Ok(Samples::S16),
        (false, 32) => Ok(Samples::S32),
        (true, 32) => Ok(Samples::F32),
        _ => Err(format!("the microphone gave {bits}-bit samples, which are not read")),
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and `Sink` does not implement `Drop`.
    #[unsafe(super = NSObject)]
    #[name = "IronAVCaptureSink"]
    #[ivars = Ivars]
    struct Sink;

    // SAFETY: `NSObjectProtocol` has no safety requirements.
    unsafe impl NSObjectProtocol for Sink {}

    // SAFETY: the delegate methods are optional; the one method both protocols share (it has the
    // same selector) is implemented below, for pictures or sound as the sink was made for.
    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for Sink {
        // SAFETY: the signature is the protocol's
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn did_output(&self, _output: &AVCaptureOutput, buffer: &CMSampleBuffer, _connection: &AVCaptureConnection) {
            let iv = self.ivars();
            if iv.video {
                iv.picture(buffer);
            } else {
                iv.sound(buffer);
            }
        }
    }

    // SAFETY: as above
    unsafe impl AVCaptureAudioDataOutputSampleBufferDelegate for Sink {}
);

impl Sink {
    fn new(ivars: Ivars) -> Retained<Sink> {
        let this = Self::alloc().set_ivars(ivars);
        // SAFETY: NSObject's init
        unsafe { msg_send![super(this), init] }
    }
}

fn dict(items: &[(&str, Retained<NSNumber>)]) -> Retained<NSDictionary<NSString, AnyObject>> {
    let keys: Vec<Retained<NSString>> = items.iter().map(|(k, _)| NSString::from_str(k)).collect();
    let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let values: Vec<&AnyObject> = items.iter().map(|(_, v)| &***v as &AnyObject).collect();
    NSDictionary::from_slices(&key_refs, &values)
}

fn media_type(video: bool) -> Result<&'static NSString> {
    // SAFETY: constants of AVFoundation
    unsafe { if video { AVMediaTypeVideo } else { AVMediaTypeAudio } }.ok_or_else(|| Error::new(ErrorCode::Internal, "AVFoundation has no media type for this device"))
}

fn denied(video: bool) -> Error {
    let (what, setting) = if video { ("camera", "Camera") } else { ("microphone", "Microphone") };
    Error::new(ErrorCode::HardwareUnavailable, format!("access to the {what} is not allowed")).hint(format!("allow the terminal (or the program running this) in System Settings > Privacy & Security > {setting}"))
}

/// Whether this program may use the camera or microphone: asks the person at the machine when
/// that has not been decided.
fn authorize(video: bool) -> Result<()> {
    let media = media_type(video)?;
    // SAFETY: a class method taking a media type
    let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media) };
    if status == AVAuthorizationStatus::Authorized {
        return Ok(());
    }
    if status != AVAuthorizationStatus::NotDetermined {
        return Err(denied(video));
    }
    let (tx, rx) = sync_channel(1);
    let handler = RcBlock::new(move |granted: Bool| {
        let _ = tx.try_send(granted.as_bool());
    });
    // SAFETY: the block lives as long as the call needs it
    unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(media, &handler) };
    match rx.recv_timeout(PROMPT) {
        Ok(true) => Ok(()),
        Ok(false) => Err(denied(video)),
        Err(_) => Err(Error::new(ErrorCode::HardwareUnavailable, "the permission prompt was not answered").hint("answer it, or allow access in System Settings > Privacy & Security")),
    }
}

fn handles(video: bool) -> Vec<Retained<AVCaptureDevice>> {
    let Ok(media) = media_type(video) else { return Vec::new() };
    // `devicesWithMediaType:` lists every device without the discovery session's device-type
    // lists, which differ between macOS versions
    #[allow(deprecated)]
    // SAFETY: a class method taking a media type
    unsafe {
        AVCaptureDevice::devicesWithMediaType(media)
    }
    .to_vec()
}

fn named(video: bool, list: &[Retained<AVCaptureDevice>]) -> Vec<Device> {
    // SAFETY: a getter
    list.iter().enumerate().map(|(index, d)| Device { kind: if video { "video" } else { "audio" }, index, name: unsafe { d.localizedName() }.to_string() }).collect()
}

pub(super) fn devices() -> Result<Vec<Device>> {
    // listing needs no permission: only opening does
    let mut all = named(true, &handles(true));
    all.extend(named(false, &handles(false)));
    Ok(all)
}

pub(super) fn run(kind: &str, name: Option<&str>, wants: VideoWants, ok: SyncSender<Result<()>>, tx: Tx) {
    if let Err(e) = capture(kind == "video", name, wants, &ok, &tx) {
        // before the device opened, to the opener; after, to the reader
        if ok.send(Err(e.clone())).is_err() {
            let _ = tx.send(Err(std::io::Error::other(e.message)));
        }
    }
}

fn fail(what: &str) -> impl Fn(String) -> Error + '_ {
    move |e| Error::new(ErrorCode::HardwareUnavailable, format!("{what}: {e}"))
}

/// Points the camera at the mode nearest what is asked, and gives the rate it will run at.
fn configure(device: &AVCaptureDevice, wants: VideoWants) -> Result<Rational> {
    // SAFETY: getters on a device we hold
    unsafe {
        if wants != VideoWants::default() {
            let formats = device.formats().to_vec();
            let (mut modes, mut owners) = (Vec::new(), Vec::new());
            for (i, f) in formats.iter().enumerate() {
                let d = CMVideoFormatDescriptionGetDimensions(&f.formatDescription());
                for r in f.videoSupportedFrameRateRanges().to_vec() {
                    modes.push((d.width.max(0) as u32, d.height.max(0) as u32, r.minFrameRate(), r.maxFrameRate()));
                    owners.push(i);
                }
            }
            if let Some((m, fps)) = pick_mode(&modes, wants) {
                device.lockForConfiguration().map_err(|e| fail("could not set the camera's mode")(e.localizedDescription().to_string()))?;
                device.setActiveFormat(&formats[owners[m]]);
                if let Some(fps) = fps.filter(|f| *f > 0.0) {
                    let t = CMTime::new(1000, (fps * 1000.0).round().clamp(1.0, i32::MAX as f64) as i32);
                    device.setActiveVideoMinFrameDuration(t);
                    device.setActiveVideoMaxFrameDuration(t);
                }
                device.unlockForConfiguration();
            }
        }
        let t = device.activeVideoMinFrameDuration();
        let (value, scale) = ({ t.value }, { t.timescale });
        Ok(if value > 0 && scale > 0 { Rational::new(scale as i64, value) } else { Rational::new(30, 1) })
    }
}

fn capture(video: bool, name: Option<&str>, wants: VideoWants, ok: &SyncSender<Result<()>>, tx: &Tx) -> Result<()> {
    let kind = if video { "video" } else { "audio" };
    let list = handles(video);
    let devs = named(video, &list);
    // before asking for permission, so that a device that is not there does not raise the prompt
    let d = choose(&devs, name, kind)?;
    authorize(video)?;
    let device = &list[d.index];
    // SAFETY: AVFoundation objects used from this thread; the session is stopped before they drop
    let input = unsafe { AVCaptureDeviceInput::deviceInputWithDevice_error(device) }.map_err(|e| fail(&format!("could not open {}", d.name))(e.localizedDescription().to_string()))?;
    let session = unsafe { AVCaptureSession::new() };
    if !unsafe { session.canAddInput(&input) } {
        return Err(Error::new(ErrorCode::HardwareUnavailable, format!("{} cannot be used for capture", d.name)));
    }
    unsafe { session.addInput(&input) };
    // after the input is added, so that the session keeps the mode
    let rate = if video { configure(device, wants)? } else { Rational::new(1, 1) };

    let (out, rx) = sync_channel::<Vec<u8>>(QUEUED);
    let failed = Arc::new(Mutex::new(None));
    let sink = Sink::new(Ivars { video, out, failed: failed.clone(), rate, started: AtomicBool::new(false) });
    let queue = DispatchQueue::new("ironav.capture", None);
    let delegate_output: Retained<AVCaptureOutput> = unsafe {
        if video {
            let o = AVCaptureVideoDataOutput::new();
            // NV12 whatever the camera sends; "PixelFormatType" is kCVPixelBufferPixelFormatTypeKey
            o.setVideoSettings(Some(&dict(&[("PixelFormatType", NSNumber::new_u32(NV12_VIDEO))])));
            o.setSampleBufferDelegate_queue(Some(ProtocolObject::from_ref(&*sink)), Some(&queue));
            Retained::into_super(o)
        } else {
            let o = AVCaptureAudioDataOutput::new();
            // 16-bit little-endian interleaved integers, at the microphone's own rate
            o.setAudioSettings(Some(&dict(&[
                ("AVFormatIDKey", NSNumber::new_u32(LINEAR_PCM)),
                ("AVLinearPCMBitDepthKey", NSNumber::new_i32(16)),
                ("AVLinearPCMIsFloatKey", NSNumber::new_bool(false)),
                ("AVLinearPCMIsBigEndianKey", NSNumber::new_bool(false)),
                ("AVLinearPCMIsNonInterleaved", NSNumber::new_bool(false)),
            ])));
            o.setSampleBufferDelegate_queue(Some(ProtocolObject::from_ref(&*sink)), Some(&queue));
            Retained::into_super(o)
        }
    };
    if !unsafe { session.canAddOutput(&delegate_output) } {
        return Err(Error::new(ErrorCode::HardwareUnavailable, format!("{} cannot be read by this program", d.name)));
    }
    unsafe { session.addOutput(&delegate_output) };
    unsafe { session.startRunning() };
    if !unsafe { session.isRunning() } {
        unsafe { session.removeOutput(&delegate_output) };
        return Err(Error::new(ErrorCode::HardwareUnavailable, format!("{} did not start", d.name)).hint("another program may be using it"));
    }
    let _ = ok.send(Ok(()));
    let r = loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(b) => {
                if !send(tx, b) {
                    break Ok(());
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break Ok(()),
        }
        if let Some(why) = failed.lock().unwrap_or_else(|e| e.into_inner()).take() {
            break Err(Error::new(ErrorCode::Io, why));
        }
    };
    // stops the delegate being called before the sink and the queue go
    unsafe {
        session.stopRunning();
        session.removeOutput(&delegate_output);
        session.removeInput(&input);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_devices_never_fails() {
        // whatever this machine has, if anything: a runner without a camera lists none
        assert!(devices().is_ok());
    }

    #[test]
    fn opening_without_a_device_or_permission_is_an_error_not_a_panic() {
        // on a machine with no camera, or where access is refused, opening fails with an error;
        // a machine with a camera and access may open it, which is no failure either
        let (ok_tx, ok_rx) = sync_channel(1);
        let (tx, _rx) = sync_channel(1);
        // an index no machine has, so that no device opens (and no permission prompt is raised
        // for a machine that has not decided: that answers at once or after the prompt timeout)
        std::thread::spawn(move || run("video", Some("9999"), VideoWants::default(), ok_tx, tx));
        let e = ok_rx.recv_timeout(PROMPT + Duration::from_secs(10)).expect("the capture thread says whether the device opened").unwrap_err();
        assert!(matches!(e.code, ErrorCode::NotFound | ErrorCode::HardwareUnavailable), "{e:?}");
    }

    #[test]
    fn audio_descriptions_are_read() {
        let mut a = AudioStreamBasicDescription { mSampleRate: 48000.0, mFormatID: LINEAR_PCM, mFormatFlags: 0xC, mBytesPerPacket: 2, mFramesPerPacket: 1, mBytesPerFrame: 2, mChannelsPerFrame: 1, mBitsPerChannel: 16, mReserved: 0 };
        assert_eq!(sample_kind(&a), Ok(Samples::S16));
        (a.mFormatFlags, a.mBitsPerChannel) = (FLAG_FLOAT | 8, 32);
        assert_eq!(sample_kind(&a), Ok(Samples::F32));
        a.mFormatFlags |= FLAG_NON_INTERLEAVED;
        assert!(sample_kind(&a).is_err());
        a.mFormatFlags = 0xC;
        a.mBitsPerChannel = 24;
        assert!(sample_kind(&a).is_err());
    }
}
