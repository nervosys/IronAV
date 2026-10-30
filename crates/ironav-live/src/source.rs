//! Byte sources.

use crate::url::{redact, SourceUrl};
use ironav_core::{Error, ErrorCode, Result};
use serde::Serialize;
use std::io::{ErrorKind, Read};
use crate::net::{self, Allow};
use std::net::{Ipv4Addr, TcpListener, TcpStream, UdpSocket};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// What a read gave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chunk {
    /// This many bytes were put at the start of the buffer.
    Data(usize),
    /// Nothing arrived before the deadline; the source may still send more.
    Idle,
    /// The source has ended and will send nothing more.
    End,
    /// The source was lost and is back: what follows starts over, as from a new connection (a
    /// new header or description, timestamps from a new origin).
    Restart,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceInfo {
    /// The URL with any password replaced.
    pub url: String,
    /// `file`, `pipe`, `tcp`, `udp`, `rtsp`, `hls`, `http`, `device` or `test`.
    pub kind: &'static str,
    /// Whether the bytes arrive at the pace of the media (a socket, a pipe, a followed file)
    /// rather than as fast as they can be read.
    pub live: bool,
    /// Made up for testing (`test://`), not captured: never real footage.
    pub synthetic: bool,
}

pub trait Source: Send {
    fn info(&self) -> &SourceInfo;
    /// Bytes into `buf`, waiting at most `wait` for some to arrive. A datagram source gives one
    /// datagram per read, so `buf` should hold 64 KiB.
    fn read(&mut self, buf: &mut [u8], wait: Duration) -> Result<Chunk>;
    /// How a source that is kept up is doing (see [`crate::reconnect`]); `None` for the others.
    fn health(&self) -> Option<crate::reconnect::Health> {
        None
    }
}

/// How long to wait for a TCP connection to be made.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How often a followed file is checked for new bytes.
const FOLLOW_POLL: Duration = Duration::from_millis(20);

/// Opens the source a URL names (see [`SourceUrl`]). An RTSP source, and a TCP connection with
/// `?reconnect`, is kept up (see [`crate::reconnect`]): `stall=S` seconds without data is a stall
/// and twice that a loss (5 by default), `retries=N` attempts after a loss before giving up
/// (unlimited by default), and `reconnect=0` turns it off.
pub fn open_source(url: &str) -> Result<Box<dyn Source>> {
    let u = SourceUrl::parse(url)?;
    let client = u.scheme == "tcp" && !u.flag("listen") && !u.target.starts_with(':');
    let keep_up = match u.option("reconnect") {
        Some(_) => u.flag("reconnect"),
        None => matches!(u.scheme.as_str(), "rtsp" | "rtsps"),
    };
    if keep_up && (client || matches!(u.scheme.as_str(), "rtsp" | "rtsps")) {
        let num = |key: &str| u.option(key).map(|s| s.parse::<f64>().ok().filter(|v| *v > 0.0 && v.is_finite()).ok_or_else(|| Error::invalid_arg(format!("{key} must be a positive number, got '{s}'")))).transpose();
        let policy = crate::reconnect::Policy {
            stall: num("stall")?.map_or(crate::reconnect::Policy::default().stall, Duration::from_secs_f64),
            retries: num("retries")?.map(|r| r as u32),
        };
        let url = url.to_string();
        return Ok(Box::new(crate::reconnect::Reconnecting::new(move || open_once(&url), policy)?));
    }
    open_once(url)
}

/// Opens the source a URL names, once.
fn open_once(url: &str) -> Result<Box<dyn Source>> {
    let u = SourceUrl::parse(url)?;
    let shown = redact(url);
    let unreachable = |e: std::io::Error, what: &str| Error::new(ErrorCode::Io, format!("{what} {shown}: {e}")).hint("check the address, and that the sender is running");
    Ok(match u.scheme.as_str() {
        "file" => {
            let f = std::fs::File::open(&u.target).map_err(|e| {
                if e.kind() == ErrorKind::NotFound { Error::new(ErrorCode::NotFound, format!("no such file: {}", u.target)) } else { e.into() }
            })?;
            let follow = u.flag("follow");
            let idle = u.option("idle").map(|s| s.parse::<f64>().map_err(|_| Error::invalid_arg(format!("idle must be a number of seconds, got '{s}'")))).transpose()?;
            Box::new(FileSource { info: SourceInfo { url: shown, kind: "file", live: follow, synthetic: false }, file: f, follow, idle: idle.map(Duration::from_secs_f64), last_data: Instant::now() })
        }
        "pipe" => {
            let reader: Box<dyn Read + Send> = if u.target.is_empty() {
                Box::new(std::io::stdin())
            } else {
                Box::new(std::fs::File::open(&u.target).map_err(|e| unreachable(e, "could not open the pipe"))?)
            };
            Box::new(ThreadSource::new(SourceInfo { url: shown, kind: "pipe", live: true, synthetic: false }, reader))
        }
        "tcp" => {
            let (host, port) = host_port(&u.target)?;
            let info = SourceInfo { url: shown.clone(), kind: "tcp", live: true, synthetic: false };
            if host.is_empty() || u.flag("listen") {
                // a listener binds to loopback unless an address is given
                let host = if host.is_empty() { "127.0.0.1" } else { host };
                let l = TcpListener::bind((host, port)).map_err(|e| unreachable(e, "could not listen on"))?;
                l.set_nonblocking(true)?;
                Box::new(TcpSource { info, listener: Some(l), stream: None })
            } else {
                // only to an address the allow-list permits (see `net`)
                let s = net::connect(host, port, CONNECT_TIMEOUT, &Allow::from_env()?, &shown)?;
                Box::new(TcpSource { info, listener: None, stream: Some(s) })
            }
        }
        "udp" => {
            let (host, port) = host_port(&u.target)?;
            let ip: Option<Ipv4Addr> = host.parse().ok();
            let sock = match ip {
                Some(ip) if ip.is_multicast() => {
                    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).map_err(|e| unreachable(e, "could not bind"))?;
                    s.join_multicast_v4(&ip, &Ipv4Addr::UNSPECIFIED).map_err(|e| unreachable(e, "could not join the multicast group of"))?;
                    s
                }
                _ => UdpSocket::bind((if host.is_empty() { "127.0.0.1" } else { host }, port)).map_err(|e| unreachable(e, "could not bind"))?,
            };
            Box::new(UdpSource { info: SourceInfo { url: shown, kind: "udp", live: true, synthetic: false }, sock })
        }
        "rtsp" | "rtsps" => Box::new(crate::rtsp::RtspSource::open(&u, shown)?),
        "http" | "https" => crate::hls::open(&u, shown)?,
        "device" => Box::new(crate::capture::open(&u, shown)?),
        "test" => Box::new(crate::synthetic::open(&u, shown)?),
        _ => unreachable!("SourceUrl::parse accepts only these schemes"),
    })
}

fn host_port(target: &str) -> Result<(&str, u16)> {
    let (host, port) = target.rsplit_once(':').ok_or_else(|| Error::invalid_arg(format!("expected host:port, got '{target}'")))?;
    let port = port.trim_end_matches('/').parse().map_err(|_| Error::invalid_arg(format!("'{port}' is not a port number")))?;
    Ok((host.trim_start_matches('[').trim_end_matches(']'), port))
}

struct FileSource {
    info: SourceInfo,
    file: std::fs::File,
    follow: bool,
    idle: Option<Duration>,
    last_data: Instant,
}

impl Source for FileSource {
    fn info(&self) -> &SourceInfo {
        &self.info
    }
    fn read(&mut self, buf: &mut [u8], wait: Duration) -> Result<Chunk> {
        let deadline = Instant::now() + wait;
        loop {
            let n = self.file.read(buf)?;
            if n > 0 {
                self.last_data = Instant::now();
                return Ok(Chunk::Data(n));
            }
            if !self.follow || self.idle.is_some_and(|i| self.last_data.elapsed() >= i) {
                return Ok(Chunk::End);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(Chunk::Idle);
            }
            std::thread::sleep(FOLLOW_POLL.min(deadline - now));
        }
    }
}

/// A blocking reader (standard input, a pipe) read on a thread of its own, so reads can time out.
pub(crate) struct ThreadSource {
    info: SourceInfo,
    rx: Receiver<std::io::Result<Vec<u8>>>,
    held: Vec<u8>,
    at: usize,
    ended: bool,
    /// How long the source may send nothing before it counts as failed, and when it last sent.
    stall: Option<(Duration, Instant)>,
}

impl ThreadSource {
    pub(crate) fn new(info: SourceInfo, mut r: Box<dyn Read + Send>) -> Self {
        let (tx, rx) = sync_channel(16);
        std::thread::Builder::new()
            .name("ironav-source".into())
            .spawn(move || loop {
                let mut b = vec![0u8; 64 << 10];
                match r.read(&mut b) {
                    Ok(0) => break,
                    Ok(n) => {
                        b.truncate(n);
                        if tx.send(Ok(b)).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        break;
                    }
                }
            })
            .expect("spawning a thread");
        Self::from_channel(info, rx)
    }

    /// Bytes another thread sends, in order; the source ends when it stops sending.
    pub(crate) fn from_channel(info: SourceInfo, rx: Receiver<std::io::Result<Vec<u8>>>) -> Self {
        ThreadSource { info, rx, held: Vec::new(), at: 0, ended: false, stall: None }
    }

    /// Fails reads once the source has sent nothing for `limit`: for a device, whose thread can
    /// wait forever on a driver that gives nothing (when another program holds the device, say).
    pub(crate) fn stall_after(mut self, limit: Duration) -> Self {
        self.stall = Some((limit, Instant::now()));
        self
    }
}

impl Source for ThreadSource {
    fn info(&self) -> &SourceInfo {
        &self.info
    }
    fn read(&mut self, buf: &mut [u8], wait: Duration) -> Result<Chunk> {
        if self.at == self.held.len() {
            if self.ended {
                return Ok(Chunk::End);
            }
            match self.rx.recv_timeout(wait) {
                Ok(Ok(b)) => {
                    (self.held, self.at) = (b, 0);
                    if let Some((_, last)) = &mut self.stall {
                        *last = Instant::now();
                    }
                }
                Ok(Err(e)) => return Err(e.into()),
                Err(RecvTimeoutError::Timeout) => {
                    if let Some((limit, last)) = self.stall {
                        if last.elapsed() >= limit {
                            return Err(Error::new(ErrorCode::HardwareUnavailable, format!("{} sent nothing for {:.0} s", self.info.url, limit.as_secs_f64()))
                                .hint("another program (a video call, say) may be holding the device; close it, or choose another device from `ironav devices`"));
                        }
                    }
                    return Ok(Chunk::Idle);
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.ended = true;
                    return Ok(Chunk::End);
                }
            }
        }
        let n = buf.len().min(self.held.len() - self.at);
        buf[..n].copy_from_slice(&self.held[self.at..self.at + n]);
        self.at += n;
        Ok(Chunk::Data(n))
    }
}

struct TcpSource {
    info: SourceInfo,
    /// Waiting for the sender to connect.
    listener: Option<TcpListener>,
    stream: Option<TcpStream>,
}

fn timed_out(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

impl Source for TcpSource {
    fn info(&self) -> &SourceInfo {
        &self.info
    }
    fn read(&mut self, buf: &mut [u8], wait: Duration) -> Result<Chunk> {
        let deadline = Instant::now() + wait;
        while self.stream.is_none() {
            let l = self.listener.as_ref().expect("a TCP source has a stream or a listener");
            match l.accept() {
                Ok((s, _peer)) => {
                    s.set_nonblocking(false)?;
                    self.stream = Some(s);
                    self.listener = None;
                }
                Err(e) if timed_out(&e) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Ok(Chunk::Idle);
                    }
                    std::thread::sleep(FOLLOW_POLL.min(deadline - now));
                }
                Err(e) => return Err(e.into()),
            }
        }
        let s = self.stream.as_mut().unwrap();
        let left = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
        s.set_read_timeout(Some(left))?;
        match s.read(buf) {
            Ok(0) => Ok(Chunk::End),
            Ok(n) => Ok(Chunk::Data(n)),
            Err(e) if timed_out(&e) || e.kind() == ErrorKind::Interrupted => Ok(Chunk::Idle),
            Err(e) => Err(e.into()),
        }
    }
}

struct UdpSource {
    info: SourceInfo,
    sock: UdpSocket,
}

impl Source for UdpSource {
    fn info(&self) -> &SourceInfo {
        &self.info
    }
    fn read(&mut self, buf: &mut [u8], wait: Duration) -> Result<Chunk> {
        self.sock.set_read_timeout(Some(wait.max(Duration::from_millis(1))))?;
        match self.sock.recv_from(buf) {
            Ok((n, _from)) => Ok(Chunk::Data(n)),
            Err(e) if timed_out(&e) || e.kind() == ErrorKind::Interrupted => Ok(Chunk::Idle),
            // Windows reports an ICMP "port unreachable" from an earlier send as a reset; a
            // receiving socket can ignore it
            Err(e) if e.kind() == ErrorKind::ConnectionReset => Ok(Chunk::Idle),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_that_goes_quiet_fails_rather_than_waiting_forever() {
        let (tx, rx) = sync_channel(4);
        let info = SourceInfo { url: "device://audio".into(), kind: "device", live: true, synthetic: false };
        let mut s = ThreadSource::from_channel(info, rx).stall_after(Duration::from_millis(150));
        tx.send(Ok(b"header".to_vec())).unwrap();
        let mut buf = [0u8; 64];
        assert!(matches!(s.read(&mut buf, Duration::from_millis(10)).unwrap(), Chunk::Data(6)));
        assert!(matches!(s.read(&mut buf, Duration::from_millis(10)).unwrap(), Chunk::Idle));
        let t0 = Instant::now();
        let e = loop {
            match s.read(&mut buf, Duration::from_millis(20)) {
                Ok(Chunk::Idle) => assert!(t0.elapsed() < Duration::from_secs(5)),
                Ok(c) => panic!("{c:?}"),
                Err(e) => break e,
            }
        };
        assert_eq!(e.code, ErrorCode::HardwareUnavailable);
        assert!(e.message.contains("device://audio"), "{}", e.message);
        drop(tx);
    }
    use std::io::Write;

    /// Everything a source gives until it ends or `limit` bytes have come.
    fn drain(s: &mut dyn Source, limit: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; 64 << 10];
        let start = Instant::now();
        while out.len() < limit && start.elapsed() < Duration::from_secs(10) {
            match s.read(&mut buf, Duration::from_millis(50)).unwrap() {
                Chunk::Data(n) => out.extend_from_slice(&buf[..n]),
                Chunk::Idle | Chunk::Restart => {}
                Chunk::End => break,
            }
        }
        out
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ironav-live-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_followed_file_is_read_as_it_grows() {
        let d = scratch("follow");
        let p = d.join("grow.bin");
        std::fs::write(&p, b"first ").unwrap();
        let mut s = open_source(&format!("{}?follow&idle=0.5", p.display())).unwrap();
        assert!(s.info().live);
        let writer = {
            let p = p.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                let mut f = std::fs::OpenOptions::new().append(true).open(p).unwrap();
                f.write_all(b"second").unwrap();
            })
        };
        let got = drain(s.as_mut(), usize::MAX);
        writer.join().unwrap();
        assert_eq!(got, b"first second");
    }

    #[test]
    fn a_plain_file_ends() {
        let d = scratch("plain");
        let p = d.join("a.bin");
        std::fs::write(&p, vec![7u8; 100_000]).unwrap();
        let mut s = open_source(&p.display().to_string()).unwrap();
        assert_eq!(drain(s.as_mut(), usize::MAX).len(), 100_000);
    }

    #[test]
    fn tcp_listen_and_connect() {
        // find a free port, then listen on it as a source and send to it
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut s = open_source(&format!("tcp://:{port}")).unwrap();
        let sender = std::thread::spawn(move || {
            let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
            c.write_all(&vec![1u8; 300_000]).unwrap();
        });
        let got = drain(s.as_mut(), usize::MAX);
        sender.join().unwrap();
        assert_eq!(got.len(), 300_000);

        // and connecting out to a listener
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            c.write_all(b"hello").unwrap();
        });
        let mut s = open_source(&format!("tcp://127.0.0.1:{port}")).unwrap();
        server.join().unwrap();
        assert_eq!(drain(s.as_mut(), usize::MAX), b"hello");
    }

    #[test]
    fn udp_datagrams_arrive_whole_and_the_source_waits_for_more() {
        let port = UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut s = open_source(&format!("udp://:{port}")).unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        for i in 0..5u8 {
            tx.send_to(&[i; 1316], ("127.0.0.1", port)).unwrap();
        }
        let got = drain(s.as_mut(), 5 * 1316);
        assert_eq!(got.len(), 5 * 1316);
        let mut buf = vec![0u8; 65536];
        assert_eq!(s.read(&mut buf, Duration::from_millis(20)).unwrap(), Chunk::Idle);
    }

    #[test]
    fn a_missing_file_is_not_found() {
        let e = open_source("no/such/file.y4m").err().unwrap();
        assert_eq!(e.code, ErrorCode::NotFound);
    }
}
