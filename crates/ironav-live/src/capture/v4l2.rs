//! Cameras on Linux through Video4Linux2 (memory-mapped streaming, YUYV, NV12 or I420 modes, or
//! Motion JPEG when the camera offers nothing uncompressed), and microphones through ALSA by way of
//! `arecord` (from alsa-utils), which keeps a C library out of the build. A camera that offers only
//! H.264 or another compressed mode is refused.
//!
//! The structures below are the kernel's, as laid out on 64-bit targets.

use super::{choose, mjpeg_frame, mjpeg_header, send, wav_header, y4m_frame, y4m_header, Device, Layout, VideoWants};
use ironav_core::{Error, ErrorCode, Rational, Result};
use std::ffi::CString;
use std::io::Read;
use std::os::raw::{c_int, c_ulong, c_void};
use std::process::{Command, Stdio};
use std::sync::mpsc::SyncSender;

type Tx = SyncSender<std::io::Result<Vec<u8>>>;

const CAP_VIDEO_CAPTURE: u32 = 0x1;
const CAP_STREAMING: u32 = 0x0400_0000;
const CAP_DEVICE_CAPS: u32 = 0x8000_0000;
const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const MEMORY_MMAP: u32 = 1;
const FIELD_NONE: u32 = 1;
const BUFFERS: u32 = 4;

const fn fourcc(c: &[u8; 4]) -> u32 {
    c[0] as u32 | (c[1] as u32) << 8 | (c[2] as u32) << 16 | (c[3] as u32) << 24
}
const YUYV: u32 = fourcc(b"YUYV");
const NV12: u32 = fourcc(b"NV12");
const YU12: u32 = fourcc(b"YU12");
const MJPG: u32 = fourcc(b"MJPG");
const JPEG: u32 = fourcc(b"JPEG");

#[repr(C)]
struct Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    private: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

#[repr(C)]
struct Format {
    kind: u32,
    // the union is aligned for the pointers some of its members hold
    _pad: u32,
    pix: PixFormat,
    _rest: [u8; 200 - 48],
}

#[repr(C)]
struct FmtDesc {
    index: u32,
    kind: u32,
    flags: u32,
    description: [u8; 32],
    pixelformat: u32,
    mbus_code: u32,
    reserved: [u32; 3],
}

#[repr(C)]
struct StreamParm {
    kind: u32,
    capability: u32,
    capturemode: u32,
    numerator: u32,
    denominator: u32,
    extendedmode: u32,
    readbuffers: u32,
    _rest: [u8; 200 - 24],
}

#[repr(C)]
struct RequestBuffers {
    count: u32,
    kind: u32,
    memory: u32,
    capabilities: u32,
    flags: u32,
}

#[repr(C)]
struct Buffer {
    index: u32,
    kind: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    timestamp: [i64; 2],
    timecode: [u32; 4],
    sequence: u32,
    memory: u32,
    offset: u64,
    length: u32,
    reserved2: u32,
    request_fd: u32,
}

const _: () = {
    assert!(std::mem::size_of::<Capability>() == 104);
    assert!(std::mem::size_of::<Format>() == 208);
    assert!(std::mem::size_of::<FmtDesc>() == 64);
    assert!(std::mem::size_of::<StreamParm>() == 204);
    assert!(std::mem::size_of::<RequestBuffers>() == 20);
    assert!(std::mem::size_of::<Buffer>() == 88);
};

const fn ioc(dir: c_ulong, nr: c_ulong, size: usize) -> c_ulong {
    dir << 30 | (size as c_ulong) << 16 | (b'V' as c_ulong) << 8 | nr
}
const R: c_ulong = 2;
const W: c_ulong = 1;
const QUERYCAP: c_ulong = ioc(R, 0, 104);
const ENUM_FMT: c_ulong = ioc(R | W, 2, 64);
const S_FMT: c_ulong = ioc(R | W, 5, 208);
const REQBUFS: c_ulong = ioc(R | W, 8, 20);
const QUERYBUF: c_ulong = ioc(R | W, 9, 88);
const QBUF: c_ulong = ioc(R | W, 15, 88);
const DQBUF: c_ulong = ioc(R | W, 17, 88);
const STREAMON: c_ulong = ioc(W, 18, 4);
const STREAMOFF: c_ulong = ioc(W, 19, 4);
const G_PARM: c_ulong = ioc(R | W, 21, 204);
const S_PARM: c_ulong = ioc(R | W, 22, 204);

/// A zeroed kernel structure.
fn zeroed<T>() -> T {
    // SAFETY: every structure here is plain integers, for which all zero bits are valid
    unsafe { std::mem::zeroed() }
}

/// An open device file, closed when dropped.
struct Fd(c_int);

impl Drop for Fd {
    fn drop(&mut self) {
        // SAFETY: the descriptor is ours and closed only here
        unsafe { libc::close(self.0) };
    }
}

impl Fd {
    fn open(path: &str) -> std::io::Result<Fd> {
        let c = CString::new(path).map_err(std::io::Error::other)?;
        // SAFETY: a valid C string
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if fd < 0 { Err(std::io::Error::last_os_error()) } else { Ok(Fd(fd)) }
    }

    /// `ioctl(request, arg)`, retried when interrupted.
    fn ioctl<T>(&self, request: c_ulong, arg: &mut T) -> std::io::Result<()> {
        loop {
            // SAFETY: `arg` is the structure `request` names, sized as its number says
            let r = unsafe { libc::ioctl(self.0, request as _, arg as *mut T as *mut c_void) };
            if r >= 0 {
                return Ok(());
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]).trim().to_string()
}

/// The cameras: `/dev/video*` nodes that capture by streaming, in number order.
fn cameras() -> Vec<(String, String)> {
    let mut nodes: Vec<(u32, String)> = std::fs::read_dir("/dev")
        .map(|d| d.flatten().filter_map(|e| e.file_name().to_str()?.strip_prefix("video")?.parse::<u32>().ok().map(|n| (n, format!("/dev/video{n}")))).collect())
        .unwrap_or_default();
    nodes.sort();
    nodes
        .into_iter()
        .filter_map(|(_, path)| {
            let fd = Fd::open(&path).ok()?;
            let mut cap: Capability = zeroed();
            fd.ioctl(QUERYCAP, &mut cap).ok()?;
            let caps = if cap.capabilities & CAP_DEVICE_CAPS != 0 { cap.device_caps } else { cap.capabilities };
            (caps & CAP_VIDEO_CAPTURE != 0 && caps & CAP_STREAMING != 0).then(|| (text(&cap.card), path))
        })
        .collect()
}

/// The microphones: ALSA devices that capture, as `plughw:CARD,DEVICE`, from `/proc/asound/pcm`.
fn microphones() -> Vec<(String, String)> {
    let list = std::fs::read_to_string("/proc/asound/pcm").unwrap_or_default();
    list.lines()
        .filter(|l| l.contains("capture"))
        .filter_map(|l| {
            let (id, rest) = l.split_once(':')?;
            let (card, dev) = id.trim().split_once('-')?;
            let (card, dev) = (card.parse::<u32>().ok()?, dev.parse::<u32>().ok()?);
            let name = rest.split(':').next().unwrap_or("").trim();
            Some((format!("{name} (hw:{card},{dev})"), format!("plughw:{card},{dev}")))
        })
        .collect()
}

pub(super) fn devices() -> Result<Vec<Device>> {
    let mut out: Vec<Device> = cameras().into_iter().enumerate().map(|(index, (name, _))| Device::new("video", index, name)).collect();
    out.extend(microphones().into_iter().enumerate().map(|(index, (name, _))| Device::new("audio", index, name)));
    Ok(out)
}

pub(super) fn run(kind: &str, name: Option<&str>, wants: VideoWants, ok: SyncSender<Result<()>>, tx: Tx) {
    let r = if kind == "video" { camera(name, wants, &ok, &tx) } else { microphone(name, &ok, &tx) };
    if let Err(e) = r {
        // before the device opened, to the opener; after, to the reader
        if ok.send(Err(e.clone())).is_err() {
            let _ = tx.send(Err(std::io::Error::other(e.message)));
        }
    }
}

fn io(what: &str) -> impl Fn(std::io::Error) -> Error + '_ {
    move |e| Error::new(ErrorCode::HardwareUnavailable, format!("{what}: {e}"))
}

/// A mapped buffer, unmapped when dropped.
struct Mapped(*mut c_void, usize);

impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: mapped by `mmap` with this length, and unmapped only here
        unsafe { libc::munmap(self.0, self.1) };
    }
}

fn camera(name: Option<&str>, wants: VideoWants, ok: &SyncSender<Result<()>>, tx: &Tx) -> Result<()> {
    let list = cameras();
    let devs: Vec<Device> = list.iter().enumerate().map(|(index, (n, _))| Device::new("video", index, n.clone())).collect();
    let d = choose(&devs, name, "video")?;
    let path = &list[d.index].1;
    let fd = Fd::open(path).map_err(io(&format!("could not open {}", d.name)))?;
    // the first layout the camera offers that becomes 4:2:0 without decoding, else Motion JPEG
    let mut chosen = None;
    for index in 0..64 {
        let mut f: FmtDesc = zeroed();
        (f.index, f.kind) = (index, BUF_TYPE_VIDEO_CAPTURE);
        if fd.ioctl(ENUM_FMT, &mut f).is_err() {
            break;
        }
        let rank = [YUYV, NV12, YU12, MJPG, JPEG].iter().position(|&p| p == f.pixelformat);
        if let Some(r) = rank {
            if chosen.is_none_or(|(c, _)| r < c) {
                chosen = Some((r, f.pixelformat));
            }
        }
    }
    let Some((_, pixelformat)) = chosen else {
        return Err(Error::new(ErrorCode::UnsupportedFormat, format!("{} offers no YUYV, NV12, I420 or Motion JPEG mode", d.name)).hint("cameras that send only H.264 are not supported on Linux yet"));
    };
    let (w, h) = wants.size.unwrap_or((640, 480));
    let mut fmt: Format = zeroed();
    fmt.kind = BUF_TYPE_VIDEO_CAPTURE;
    fmt.pix = PixFormat { width: w, height: h, pixelformat, field: FIELD_NONE, ..zeroed() };
    fd.ioctl(S_FMT, &mut fmt).map_err(io("could not set the camera's mode"))?;
    let pix = fmt.pix;
    // None for Motion JPEG
    let layout = match pix.pixelformat {
        YUYV => Some(Layout::Yuy2),
        NV12 => Some(Layout::Nv12),
        YU12 => Some(Layout::I420),
        MJPG | JPEG => None,
        _ => return Err(Error::new(ErrorCode::UnsupportedFormat, format!("{} chose a mode that is not YUYV, NV12, I420 or Motion JPEG", d.name))),
    };
    let mut parm: StreamParm = zeroed();
    parm.kind = BUF_TYPE_VIDEO_CAPTURE;
    if let Some(fps) = wants.fps {
        (parm.numerator, parm.denominator) = (1000, (fps * 1000.0).round() as u32);
        let _ = fd.ioctl(S_PARM, &mut parm);
    }
    let _ = fd.ioctl(G_PARM, &mut parm);
    let rate = if parm.numerator > 0 && parm.denominator > 0 { Rational::new(parm.denominator as i64, parm.numerator as i64) } else { Rational::new(30, 1) };

    let mut req = RequestBuffers { count: BUFFERS, kind: BUF_TYPE_VIDEO_CAPTURE, memory: MEMORY_MMAP, capabilities: 0, flags: 0 };
    fd.ioctl(REQBUFS, &mut req).map_err(io("the camera does not stream"))?;
    let mut maps = Vec::new();
    for index in 0..req.count {
        let mut b: Buffer = zeroed();
        (b.index, b.kind, b.memory) = (index, BUF_TYPE_VIDEO_CAPTURE, MEMORY_MMAP);
        fd.ioctl(QUERYBUF, &mut b).map_err(io("could not set up the camera's buffers"))?;
        // SAFETY: the offset and length the driver gave for this buffer
        let p = unsafe { libc::mmap(std::ptr::null_mut(), b.length as usize, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.0, b.offset as libc::off_t) };
        if p == libc::MAP_FAILED {
            return Err(io("could not map the camera's buffers")(std::io::Error::last_os_error()));
        }
        maps.push(Mapped(p, b.length as usize));
        fd.ioctl(QBUF, &mut b).map_err(io("could not queue the camera's buffers"))?;
    }
    let mut kind = BUF_TYPE_VIDEO_CAPTURE as c_int;
    fd.ioctl(STREAMON, &mut kind).map_err(io("the camera would not start"))?;
    let _ = ok.send(Ok(()));
    let (ew, eh) = (pix.width as usize & !1, pix.height as usize & !1);
    let r = (|| {
        let header = match layout {
            Some(_) => y4m_header(ew as u32, eh as u32, rate),
            None => mjpeg_header(pix.width, pix.height, rate),
        };
        if !send(tx, header) {
            return Ok(());
        }
        // Motion JPEG is timed by the driver's clock from the first picture, or by the frame rate
        // when the driver gives no time
        let (mut first, mut count) = (None, 0u64);
        loop {
            let mut pfd = libc::pollfd { fd: fd.0, events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd
            let n = unsafe { libc::poll(&mut pfd, 1, 2000) };
            if n == 0 {
                return Err(Error::new(ErrorCode::Io, "the camera sent nothing for 2 s"));
            }
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(io("waiting for the camera")(e));
            }
            let mut b: Buffer = zeroed();
            (b.kind, b.memory) = (BUF_TYPE_VIDEO_CAPTURE, MEMORY_MMAP);
            match fd.ioctl(DQBUF, &mut b) {
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                r => r.map_err(io("reading the camera"))?,
            }
            let m = &maps[b.index as usize];
            // SAFETY: the driver filled `bytesused` bytes of this mapped buffer and hands it to us until it is queued again
            let data = unsafe { std::slice::from_raw_parts(m.0 as *const u8, (b.bytesused as usize).min(m.1)) };
            let frame = match layout {
                Some(l) => y4m_frame(data, l, ew, eh, pix.bytesperline as usize, pix.height as usize),
                // a picture cut short or left empty (some cameras send these after a glitch) is dropped
                None if data.len() < 4 || data[..2] != [0xFF, 0xD8] => None,
                None => {
                    let us = b.timestamp[0].saturating_mul(1_000_000).saturating_add(b.timestamp[1]);
                    let ms = if us > 0 {
                        (us - *first.get_or_insert(us)).max(0) as u64 / 1000
                    } else {
                        count * 1000 * rate.den as u64 / rate.num.max(1) as u64
                    };
                    count += 1;
                    Some(mjpeg_frame(data, ms))
                }
            };
            fd.ioctl(QBUF, &mut b).map_err(io("reading the camera"))?;
            if let Some(f) = frame {
                if !send(tx, f) {
                    return Ok(());
                }
            }
        }
    })();
    let _ = fd.ioctl(STREAMOFF, &mut kind);
    drop(maps);
    r
}

/// 16-bit mono at 48 kHz from `arecord`, behind a WAV header.
fn microphone(name: Option<&str>, ok: &SyncSender<Result<()>>, tx: &Tx) -> Result<()> {
    const RATE: u32 = 48_000;
    let list = microphones();
    let devs: Vec<Device> = list.iter().enumerate().map(|(index, (n, _))| Device::new("audio", index, n.clone())).collect();
    let d = choose(&devs, name, "audio")?;
    let arecord = crate::exe::locate("arecord").ok_or_else(|| Error::new(ErrorCode::HardwareUnavailable, "could not find arecord on PATH").hint("install alsa-utils"))?;
    // the device is attached to -D, so a name that starts with '-' is not read as an option
    let mut child = Command::new(arecord)
        .args(["-q", &format!("-D{}", list[d.index].1), "-t", "raw", "-f", "S16_LE", "-c", "1", "-r", &RATE.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::new(ErrorCode::HardwareUnavailable, format!("could not run arecord: {e}")).hint("install alsa-utils"))?;
    let mut out = child.stdout.take().unwrap();
    let _ = ok.send(Ok(()));
    let mut r = Ok(());
    if send(tx, wav_header(RATE, 1)) {
        let mut buf = vec![0u8; 3840];
        loop {
            match out.read(&mut buf) {
                Ok(0) => {
                    r = Err(Error::new(ErrorCode::Io, format!("arecord stopped reading {}", d.name)));
                    break;
                }
                Ok(n) => {
                    if !send(tx, buf[..n].to_vec()) {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    r = Err(io("reading the microphone")(e));
                    break;
                }
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_requests_are_the_kernels() {
        assert_eq!([QUERYCAP, ENUM_FMT, S_FMT, REQBUFS, QUERYBUF], [0x8068_5600, 0xc040_5602, 0xc0d0_5605, 0xc014_5608, 0xc058_5609]);
        assert_eq!([QBUF, DQBUF, STREAMON, STREAMOFF, G_PARM, S_PARM], [0xc058_560f, 0xc058_5611, 0x4004_5612, 0x4004_5613, 0xc0cc_5615, 0xc0cc_5616]);
        assert_eq!(YUYV, 0x5659_5559);
    }

    #[test]
    fn listing_devices_never_fails() {
        // whatever this machine has, if anything
        assert!(devices().is_ok());
    }
}
