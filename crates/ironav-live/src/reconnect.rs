//! Keeping a network source up: a camera reboots, a link drops, a server restarts. The source is
//! opened again when it ends, fails, or sends nothing for a while, with a growing wait between
//! attempts, and the reader is told by a [`Chunk::Restart`] that what follows starts over. Its
//! health (connected, stalled, reconnecting or failed) can be asked at any time.
//!
//! The first open is not retried: a source that cannot be reached at all is an error at once.

use crate::source::{Chunk, Source, SourceInfo};
use ironav_core::{Error, ErrorCode, Result};
use serde::Serialize;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// The first wait before reopening, doubled after each failed attempt up to [`MAX_BACKOFF`].
const FIRST_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthState {
    Connected,
    /// Connected, but nothing has arrived for the stall time.
    Stalled,
    /// Lost, and being opened again.
    Reconnecting,
    /// Lost, and the attempts allowed have run out.
    Failed,
}

/// How a source is doing.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Health {
    pub state: HealthState,
    /// Since when, as Unix time in seconds.
    pub since: f64,
    /// Times it has come back.
    pub reconnects: u32,
    /// Failed attempts since it was lost.
    pub attempts: u32,
    /// Why it was last lost, or why the last attempt failed.
    pub last_error: Option<String>,
}

/// When to give up on a source and how often to try it again.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Policy {
    /// No bytes for this long is a stall; for twice this long, a loss.
    pub stall: Duration,
    /// Attempts after a loss before giving up; `None` keeps trying.
    pub retries: Option<u32>,
}

impl Default for Policy {
    fn default() -> Self {
        Policy { stall: Duration::from_secs(5), retries: None }
    }
}

type Opener = Arc<dyn Fn() -> Result<Box<dyn Source>> + Send + Sync>;

pub struct Reconnecting {
    info: SourceInfo,
    open: Opener,
    policy: Policy,
    inner: Option<Box<dyn Source>>,
    /// An attempt running on its own thread, so a read never waits for a connection.
    attempt: Option<Receiver<Result<Box<dyn Source>>>>,
    next_try: Instant,
    backoff: Duration,
    last_data: Instant,
    /// A new connection is up and the reader has not yet been told.
    restarted: bool,
    health: Health,
}

fn now() -> f64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

impl Reconnecting {
    /// Opens the source with `open`, failing at once if it cannot, and keeps it up after.
    pub fn new(open: impl Fn() -> Result<Box<dyn Source>> + Send + Sync + 'static, policy: Policy) -> Result<Self> {
        let open: Opener = Arc::new(open);
        let first = open()?;
        Ok(Self {
            info: first.info().clone(),
            open,
            policy,
            inner: Some(first),
            attempt: None,
            next_try: Instant::now(),
            backoff: FIRST_BACKOFF,
            last_data: Instant::now(),
            restarted: false,
            health: Health { state: HealthState::Connected, since: now(), reconnects: 0, attempts: 0, last_error: None },
        })
    }

    fn set(&mut self, state: HealthState) {
        if self.health.state != state {
            (self.health.state, self.health.since) = (state, now());
        }
    }

    /// Drops the connection and plans the first attempt.
    fn lose(&mut self, why: String) {
        self.inner = None;
        self.health.last_error = Some(why);
        self.health.attempts = 0;
        self.backoff = FIRST_BACKOFF;
        self.next_try = Instant::now() + self.backoff;
        self.set(HealthState::Reconnecting);
    }

    /// While lost: starts an attempt when one is due, and takes its result.
    fn reconnect(&mut self, wait: Duration) -> Result<Chunk> {
        if let Some(rx) = &self.attempt {
            match rx.try_recv() {
                Ok(Ok(src)) => {
                    self.attempt = None;
                    self.inner = Some(src);
                    self.health.reconnects += 1;
                    self.last_data = Instant::now();
                    self.restarted = true;
                    self.set(HealthState::Connected);
                    return Ok(Chunk::Idle);
                }
                Ok(Err(e)) => self.failed(e.message)?,
                Err(TryRecvError::Disconnected) => self.failed("the attempt ended unexpectedly".into())?,
                Err(TryRecvError::Empty) => {}
            }
            std::thread::sleep(wait.min(Duration::from_millis(20)));
            return Ok(Chunk::Idle);
        }
        let now = Instant::now();
        if now >= self.next_try {
            let (tx, rx) = channel();
            let open = self.open.clone();
            std::thread::spawn(move || {
                let _ = tx.send(open());
            });
            self.attempt = Some(rx);
        } else {
            std::thread::sleep(wait.min(self.next_try - now));
        }
        Ok(Chunk::Idle)
    }

    /// An attempt failed: try again later, or give up.
    fn failed(&mut self, why: String) -> Result<()> {
        self.attempt = None;
        self.health.attempts += 1;
        self.health.last_error = Some(why.clone());
        if self.policy.retries.is_some_and(|r| self.health.attempts >= r) {
            self.set(HealthState::Failed);
            return Err(Error::new(ErrorCode::Io, format!("{} could not be reconnected after {} attempts: {why}", self.info.url, self.health.attempts)));
        }
        self.next_try = Instant::now() + self.backoff;
        self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
        Ok(())
    }
}

impl Source for Reconnecting {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    fn health(&self) -> Option<Health> {
        Some(self.health.clone())
    }

    fn read(&mut self, buf: &mut [u8], wait: Duration) -> Result<Chunk> {
        if self.restarted {
            self.restarted = false;
            return Ok(Chunk::Restart);
        }
        let Some(src) = self.inner.as_mut() else { return self.reconnect(wait) };
        match src.read(buf, wait) {
            Ok(Chunk::Data(n)) => {
                self.last_data = Instant::now();
                self.set(HealthState::Connected);
                Ok(Chunk::Data(n))
            }
            Ok(Chunk::Idle) => {
                let quiet = self.last_data.elapsed();
                if quiet >= 2 * self.policy.stall {
                    self.lose(format!("nothing arrived for {:.0} s", quiet.as_secs_f64()));
                } else if quiet >= self.policy.stall {
                    self.set(HealthState::Stalled);
                }
                Ok(Chunk::Idle)
            }
            // the inner source never restarts by itself
            Ok(Chunk::End | Chunk::Restart) => {
                self.lose("the source ended".into());
                Ok(Chunk::Idle)
            }
            Err(e) => {
                self.lose(e.message);
                Ok(Chunk::Idle)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{open_source, Chunk, HealthState};
    use std::io::Write;
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    /// What a source gives for `limit` (or until 300 ms after it has given `until`), with each
    /// restart as `|`, and the health states it went through.
    fn read_for(url: &str, limit: Duration, until: Option<&str>) -> (String, Vec<HealthState>, ironav_core::Result<()>) {
        let mut s = open_source(url).unwrap();
        let (mut got, mut states) = (String::new(), Vec::new());
        let mut buf = [0u8; 64];
        let mut start = Instant::now();
        let mut limit = limit;
        while start.elapsed() < limit {
            if until.is_some_and(|u| got.ends_with(u)) && limit > Duration::from_millis(300) {
                (start, limit) = (Instant::now(), Duration::from_millis(300));
            }
            let r = s.read(&mut buf, Duration::from_millis(20));
            let h = s.health().unwrap();
            if states.last() != Some(&h.state) {
                states.push(h.state);
            }
            match r {
                Ok(Chunk::Data(n)) => got.push_str(std::str::from_utf8(&buf[..n]).unwrap()),
                Ok(Chunk::Restart) => got.push('|'),
                Ok(Chunk::Idle) => {}
                Ok(Chunk::End) => return (got, states, Ok(())),
                Err(e) => return (got, states, Err(e)),
            }
        }
        (got, states, Ok(()))
    }

    #[cfg_attr(feature = "fips", ignore = "FIPS build: exercises plaintext transports or MD5 Digest, which it refuses (tests/fips.rs covers the refusals)")]
    #[test]
    fn a_lost_or_stalled_connection_is_made_again() {
        crate::net::allow_loopback_for_tests();
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            // closed at once; then silent too long; then fine
            let (mut c, _) = l.accept().unwrap();
            c.write_all(b"AAAA").unwrap();
            drop(c);
            let (mut c, _) = l.accept().unwrap();
            c.write_all(b"BBBB").unwrap();
            let (mut d, _) = l.accept().unwrap();
            d.write_all(b"CCCC").unwrap();
            std::thread::sleep(Duration::from_secs(3));
            drop(c);
        });
        let (got, states, r) = read_for(&format!("tcp://127.0.0.1:{port}?reconnect&stall=0.2"), Duration::from_secs(10), Some("CCCC"));
        r.unwrap();
        assert_eq!(got, "AAAA|BBBB|CCCC");
        use HealthState::*;
        // on a loaded machine a chunk can come more than 0.2 s late: a stall the source recovers from
        // without reconnecting is right, so only the stalls that lead to a reconnect (or end the run) count
        let mut seen: Vec<HealthState> = Vec::new();
        for (i, s) in states.iter().enumerate() {
            if *s == Stalled && states.get(i + 1) == Some(&Connected) {
                continue;
            }
            if seen.last() != Some(s) {
                seen.push(*s);
            }
        }
        assert_eq!(seen, [Connected, Reconnecting, Connected, Stalled, Reconnecting, Connected, Stalled], "{got} {states:?}");
    }

    #[cfg_attr(feature = "fips", ignore = "FIPS build: exercises plaintext transports or MD5 Digest, which it refuses (tests/fips.rs covers the refusals)")]
    #[test]
    fn it_gives_up_after_its_retries() {
        crate::net::allow_loopback_for_tests();
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            c.write_all(b"AAAA").unwrap();
            // the listener goes too: nothing to reconnect to
        });
        let (got, states, r) = read_for(&format!("tcp://127.0.0.1:{port}?reconnect&retries=2"), Duration::from_secs(10), None);
        let e = r.unwrap_err();
        assert!(e.message.contains("after 2 attempts"), "{}", e.message);
        assert_eq!(got, "AAAA");
        assert_eq!(states.last(), Some(&HealthState::Failed));

        // without reconnect, a closed connection is the end
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            c.write_all(b"AAAA").unwrap();
        });
        let mut s = open_source(&format!("tcp://127.0.0.1:{port}")).unwrap();
        assert!(s.health().is_none());
        let mut buf = [0u8; 16];
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if s.read(&mut buf, Duration::from_millis(20)).unwrap() == Chunk::End {
                return;
            }
        }
        panic!("no end");
    }
}
