//! Cameras and microphones through Media Foundation's source reader.

use super::{choose, send, wav_header, y4m_frame, y4m_header, Device, Layout, VideoWants};
use ironav_core::{Error, ErrorCode, Rational, Result};
use std::sync::mpsc::SyncSender;
use windows::core::{GUID, PWSTR};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED};

type Tx = SyncSender<std::io::Result<Vec<u8>>>;

/// COM and Media Foundation, started for this thread for as long as it is held.
struct Mf {
    com: bool,
}

impl Mf {
    fn start() -> Result<Mf> {
        // a thread already in another apartment keeps it; Media Foundation works in either
        let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        if let Err(e) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) } {
            if com {
                unsafe { CoUninitialize() };
            }
            return Err(fail("Media Foundation could not start")(e));
        }
        Ok(Mf { com })
    }
}

impl Drop for Mf {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            if self.com {
                CoUninitialize();
            }
        }
    }
}

fn fail(what: &str) -> impl Fn(windows::core::Error) -> Error + '_ {
    move |e| {
        let err = Error::new(ErrorCode::HardwareUnavailable, format!("{what}: {}", e.message()));
        // E_ACCESSDENIED: the privacy settings refuse this program the device
        if e.code().0 as u32 == 0x8007_0005 {
            err.hint("allow access to the camera or microphone in Windows Settings > Privacy & security")
        } else {
            err
        }
    }
}

/// The devices of one kind, in Media Foundation's order.
unsafe fn enumerate(kind: GUID) -> Result<Vec<IMFActivate>> {
    let mut attrs = None;
    MFCreateAttributes(&mut attrs, 1).map_err(fail("listing devices"))?;
    let attrs = attrs.unwrap();
    attrs.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &kind).map_err(fail("listing devices"))?;
    let (mut list, mut n) = (std::ptr::null_mut::<Option<IMFActivate>>(), 0u32);
    MFEnumDeviceSources(&attrs, &mut list, &mut n).map_err(fail("listing devices"))?;
    let mut out = Vec::new();
    if !list.is_null() {
        for i in 0..n as usize {
            out.extend((*list.add(i)).take());
        }
        CoTaskMemFree(Some(list as *const _));
    }
    Ok(out)
}

unsafe fn name(a: &IMFActivate) -> String {
    let (mut p, mut len) = (PWSTR::null(), 0u32);
    if a.GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut p, &mut len).is_err() {
        return String::new();
    }
    let s = p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(p.0 as *const _));
    s
}

unsafe fn listed(kind: &'static str) -> Result<(Vec<IMFActivate>, Vec<Device>)> {
    let guid = if kind == "video" { MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID } else { MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_AUDCAP_GUID };
    let acts = enumerate(guid)?;
    let devs = acts.iter().enumerate().map(|(index, a)| Device::new(kind, index, name(a))).collect();
    Ok((acts, devs))
}

pub(super) fn devices() -> Result<Vec<Device>> {
    // on a thread of its own, so the caller's COM apartment is not touched
    std::thread::spawn(|| {
        let _mf = Mf::start()?;
        let mut all = unsafe { listed("video")? }.1;
        all.extend(unsafe { listed("audio")? }.1);
        Ok(all)
    })
    .join()
    .unwrap_or_else(|_| Err(Error::new(ErrorCode::Internal, "listing capture devices failed")))
}

enum Out {
    Video { layout: Layout, w: usize, h: usize, stride: usize, rows: usize },
    Audio,
}

struct Capture {
    source: IMFMediaSource,
    reader: Option<IMFSourceReader>,
    stream: u32,
    header: Vec<u8>,
    out: Out,
}

/// The capture thread: opens the device, says so on `ok`, then sends the byte stream until the
/// source is closed or the device fails.
pub(super) fn run(kind: &str, name: Option<&str>, wants: VideoWants, ok: SyncSender<Result<()>>, tx: Tx) {
    let mf = match Mf::start() {
        Ok(m) => m,
        Err(e) => {
            let _ = ok.send(Err(e));
            return;
        }
    };
    match unsafe { if kind == "video" { open_video(name, wants) } else { open_audio(name) } } {
        Err(e) => {
            let _ = ok.send(Err(e));
        }
        Ok(mut cap) => {
            let _ = ok.send(Ok(()));
            if send(&tx, std::mem::take(&mut cap.header)) {
                if let Err(e) = unsafe { cap.pump(&tx) } {
                    let _ = tx.send(Err(std::io::Error::other(e.message)));
                }
            }
            cap.reader = None;
            unsafe {
                let _ = cap.source.Shutdown();
            }
        }
    }
    drop(mf);
}

unsafe fn activate(kind: &'static str, name: Option<&str>) -> Result<IMFMediaSource> {
    let (acts, devs) = listed(kind)?;
    let d = choose(&devs, name, kind)?;
    acts[d.index].ActivateObject::<IMFMediaSource>().map_err(fail(&format!("could not open {}", d.name)))
}

unsafe fn reader(source: &IMFMediaSource, stream: u32) -> Result<IMFSourceReader> {
    let mut attrs = None;
    MFCreateAttributes(&mut attrs, 1).map_err(fail("opening the device"))?;
    let attrs = attrs.unwrap();
    // colour conversion (and decoding of MJPEG cameras) to the layouts asked for below
    attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1).map_err(fail("opening the device"))?;
    let r = MFCreateSourceReaderFromMediaSource(source, &attrs).map_err(fail("opening the device"))?;
    let _ = r.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false);
    r.SetStreamSelection(stream, true).map_err(fail("opening the device"))?;
    Ok(r)
}

fn split(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, v as u32)
}

unsafe fn open_video(name: Option<&str>, wants: VideoWants) -> Result<Capture> {
    let source = activate("video", name)?;
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let r = reader(&source, stream)?;
    if wants != VideoWants::default() {
        // the device's mode nearest the size, then the rate, asked for
        let mut best: Option<(f64, f64, IMFMediaType)> = None;
        for i in 0.. {
            let Ok(t) = r.GetNativeMediaType(stream, i) else { break };
            let (Ok(size), Ok(rate)) = (t.GetUINT64(&MF_MT_FRAME_SIZE), t.GetUINT64(&MF_MT_FRAME_RATE)) else { continue };
            let ((w, h), (n, d)) = (split(size), split(rate));
            let ds = wants.size.map_or(0.0, |(ww, wh)| (w as f64 - ww as f64).abs() + (h as f64 - wh as f64).abs());
            let df = wants.fps.map_or(0.0, |f| (n as f64 / d.max(1) as f64 - f).abs());
            if best.as_ref().is_none_or(|(bs, bf, _)| (ds, df) < (*bs, *bf)) {
                best = Some((ds, df, t));
            }
        }
        if let Some((_, _, t)) = best {
            r.SetCurrentMediaType(stream, None, &t).map_err(fail("setting the camera's mode"))?;
        }
    }
    let mut layout = None;
    for (sub, l) in [(MFVideoFormat_I420, Layout::I420), (MFVideoFormat_IYUV, Layout::I420), (MFVideoFormat_NV12, Layout::Nv12), (MFVideoFormat_YUY2, Layout::Yuy2)] {
        let t = MFCreateMediaType().map_err(fail("opening the device"))?;
        let set = t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).and_then(|_| t.SetGUID(&MF_MT_SUBTYPE, &sub));
        if set.is_ok() && r.SetCurrentMediaType(stream, None, &t).is_ok() {
            layout = Some(l);
            break;
        }
    }
    let layout = layout.ok_or_else(|| Error::new(ErrorCode::UnsupportedFormat, "the camera gives no picture layout that can be converted to YUV 4:2:0"))?;
    let t = r.GetCurrentMediaType(stream).map_err(fail("opening the device"))?;
    let (w, h) = split(t.GetUINT64(&MF_MT_FRAME_SIZE).map_err(fail("reading the camera's mode"))?);
    let (n, d) = t.GetUINT64(&MF_MT_FRAME_RATE).map(split).unwrap_or((30, 1));
    let fps = if n == 0 || d == 0 { Rational::new(30, 1) } else { Rational::new(n as i64, d as i64) };
    let packed = if layout == Layout::Yuy2 { 2 } else { 1 };
    // a negative stride is a bottom-up picture, which these layouts never are
    let stride = t.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|s| (s as i32).unsigned_abs() as usize).unwrap_or(w as usize * packed);
    // 4:2:0 needs even sizes: an odd last row or column is left out
    let (ew, eh) = ((w & !1) as usize, (h & !1) as usize);
    if ew == 0 || eh == 0 {
        return Err(Error::new(ErrorCode::UnsupportedFormat, format!("the camera's pictures are {w}x{h}")));
    }
    Ok(Capture { source, reader: Some(r), stream, header: y4m_header(ew as u32, eh as u32, fps), out: Out::Video { layout, w: ew, h: eh, stride, rows: h as usize } })
}

unsafe fn open_audio(name: Option<&str>) -> Result<Capture> {
    let source = activate("audio", name)?;
    let stream = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
    let r = reader(&source, stream)?;
    let native = r.GetCurrentMediaType(stream).map_err(fail("opening the device"))?;
    let rate = native.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).unwrap_or(48000);
    let channels = native.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).unwrap_or(2).clamp(1, 8);
    let t = MFCreateMediaType().map_err(fail("opening the device"))?;
    let set = (|| {
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)?;
        t.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, channels * 2)?;
        t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, rate * channels * 2)?;
        r.SetCurrentMediaType(stream, None, &t)
    })();
    set.map_err(fail("asking the microphone for 16-bit PCM"))?;
    Ok(Capture { source, reader: Some(r), stream, header: wav_header(rate, channels as u16), out: Out::Audio })
}

impl Capture {
    unsafe fn pump(&mut self, tx: &Tx) -> Result<()> {
        let r = self.reader.as_ref().unwrap();
        loop {
            let (mut flags, mut sample) = (0u32, None::<IMFSample>);
            r.ReadSample(self.stream, 0, None, Some(&mut flags as *mut _), None, Some(&mut sample as *mut _)).map_err(fail("reading from the device"))?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                return Ok(());
            }
            let Some(s) = sample else { continue };
            let buf = s.ConvertToContiguousBuffer().map_err(fail("reading from the device"))?;
            let (mut p, mut len) = (std::ptr::null_mut::<u8>(), 0u32);
            buf.Lock(&mut p, None, Some(&mut len as *mut _)).map_err(fail("reading from the device"))?;
            if p.is_null() || len == 0 {
                let _ = buf.Unlock();
                continue;
            }
            let data = std::slice::from_raw_parts(p, len as usize);
            let out = match self.out {
                Out::Video { layout, w, h, stride, rows } => y4m_frame(data, layout, w, h, stride, rows),
                Out::Audio => Some(data.to_vec()),
            };
            let _ = buf.Unlock();
            let Some(out) = out else {
                return Err(Error::new(ErrorCode::InvalidInput, format!("the camera gave a picture of {len} bytes, too short for its mode or with rows narrower than it")));
            };
            if !send(tx, out) {
                return Ok(());
            }
        }
    }
}
