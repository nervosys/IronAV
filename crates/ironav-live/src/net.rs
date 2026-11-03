//! Which addresses a source may connect to. Agents choose source URLs, and a server that connects
//! wherever it is told can be steered at hosts it should never reach (server-side request forgery).
//! So every outbound connection is checked against an allow-list after the name resolves, and only
//! an address that passed is connected to, so a name that resolves differently a moment later
//! (DNS rebinding) cannot slip past. Redirects, once a source follows them, go through the same
//! check.
//!
//! Nothing is reachable unless whoever runs IronAV says so, with `IRONAV_NET_ALLOW`: a
//! comma-separated list of
//! - `loopback`: this machine (127/8, ::1);
//! - `private`: the local network, where cameras and encoders usually live (10/8, 172.16/12,
//!   192.168/16, 100.64/10 carrier-grade NAT, 169.254/16 link-local, fc00::/7 and fe80::/10);
//! - `public`: any other unicast address, which is where internet hosts are;
//! - `any`: every address, the special-purpose ones (unspecified, multicast, documentation) too;
//! - addresses and CIDR ranges (`203.0.113.7, 198.51.100.0/24, 2001:db8::/32`), which admit the
//!   special-purpose addresses they name.
//!
//! Cloud metadata endpoints (169.254.169.254, fd00:ec2::254, 169.254.170.2 and .23) are refused
//! whatever is listed, `any` included, unless `IRONAV_NET_ALLOW_METADATA=1` is set as well.

use crate::fips::env_flag;
use ironav_core::{Error, ErrorCode, Result};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::time::Duration;

/// The environment variable that opens addresses to sources.
pub const ALLOW_VAR: &str = "IRONAV_NET_ALLOW";
/// The environment variable (`1`) that lets sources reach the cloud metadata endpoints at all.
pub const METADATA_VAR: &str = "IRONAV_NET_ALLOW_METADATA";

/// Cloud metadata endpoints: the instance service (AWS, Azure, GCP and others), AWS's IPv6 one, and
/// the ECS task and EKS pod identity services.
const METADATA: [IpAddr; 4] = [
    IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
    IpAddr::V6(Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254)),
    IpAddr::V4(Ipv4Addr::new(169, 254, 170, 2)),
    IpAddr::V4(Ipv4Addr::new(169, 254, 170, 23)),
];

/// The classes of address the keywords name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Loopback,
    Private,
    Public,
    /// Unspecified, multicast, reserved and documentation addresses.
    Special,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Allow {
    any: bool,
    loopback: bool,
    private: bool,
    public: bool,
    /// Configured ranges: a network address and its prefix length.
    nets: Vec<(IpAddr, u8)>,
    /// `IRONAV_NET_ALLOW_METADATA=1`.
    metadata: bool,
}

impl Allow {
    /// The allow-list the environment sets up (see the module documentation). Nothing is allowed
    /// when `IRONAV_NET_ALLOW` is unset.
    pub fn from_env() -> Result<Allow> {
        let mut a = match std::env::var(ALLOW_VAR) {
            Ok(spec) => Allow::parse(&spec)?,
            Err(_) => Allow::default(),
        };
        a.metadata = env_flag(METADATA_VAR);
        Ok(a)
    }

    /// The allow-list `spec` names (see the module documentation). Metadata endpoints stay refused.
    pub fn parse(spec: &str) -> Result<Allow> {
        let mut a = Allow::default();
        for item in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match item.to_ascii_lowercase().as_str() {
                "any" => a.any = true,
                "loopback" => a.loopback = true,
                "private" => a.private = true,
                "public" => a.public = true,
                _ => {
                    let bad = || Error::invalid_arg(format!("{ALLOW_VAR}: '{item}' is not a keyword or an address or CIDR range")).hint("for example private, loopback, public, any, 203.0.113.7, 198.51.100.0/24 or 2001:db8::/32");
                    let (ip, len) = match item.split_once('/') {
                        Some((ip, len)) => (ip.parse::<IpAddr>().map_err(|_| bad())?, len.parse::<u8>().map_err(|_| bad())?),
                        None => {
                            let ip = item.parse::<IpAddr>().map_err(|_| bad())?;
                            (ip, if ip.is_ipv4() { 32 } else { 128 })
                        }
                    };
                    if len > if ip.is_ipv4() { 32 } else { 128 } {
                        return Err(bad());
                    }
                    // an IPv4 range written as IPv4-mapped IPv6 is the IPv4 range
                    let (ip, len) = match ip {
                        IpAddr::V6(v) if len >= 96 && v.to_ipv4_mapped().is_some() => (IpAddr::V4(v.to_ipv4_mapped().unwrap()), len - 96),
                        _ => (ip, len),
                    };
                    a.nets.push((ip, len));
                }
            }
        }
        Ok(a)
    }

    /// Whether a connection to `ip` is allowed.
    pub fn permits(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        if METADATA.contains(&ip) && !self.metadata {
            return false;
        }
        if self.any || self.nets.iter().any(|&(net, len)| contains(net, len, ip)) {
            return true;
        }
        match class(ip) {
            Class::Loopback => self.loopback,
            Class::Private => self.private,
            Class::Public => self.public,
            Class::Special => false,
        }
    }

    /// The refusal for addresses none of which is allowed, saying how to allow the first kind of one.
    fn refused(&self, ips: &[IpAddr], shown: &str) -> Error {
        let list = ips.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
        let ips: Vec<IpAddr> = ips.iter().map(|&i| canonical(i)).collect();
        let hint = if ips.iter().any(|ip| METADATA.contains(ip)) {
            format!("{shown} is a cloud metadata endpoint: to allow it, set {METADATA_VAR}=1 as well as {ALLOW_VAR}, and only on a machine that needs it")
        } else {
            let (kw, what) = match ips.first().copied().map(class) {
                Some(Class::Private) => ("private", "a camera or server on the local network"),
                Some(Class::Loopback) => ("loopback", "this machine"),
                Some(Class::Public) => ("public", "an internet host"),
                _ => ("any", "an unspecified, multicast or documentation address"),
            };
            format!("to allow it, set {ALLOW_VAR}={kw} ({what}); {ALLOW_VAR} also takes addresses and CIDR ranges, or any")
        };
        Error::new(ErrorCode::InvalidArgument, format!("{shown} resolves to {list}, which IronAV does not allow by default")).hint(hint)
    }
}

/// The class of an address (after [`canonical`]).
fn class(ip: IpAddr) -> Class {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            if v.is_loopback() {
                Class::Loopback
            } else if v.is_private() || v.is_link_local() || (o[0] == 100 && o[1] & 0xc0 == 64) {
                Class::Private
            } else if v.is_unspecified() || v.is_multicast() || v.is_broadcast() || o[0] == 0 || o[0] >= 240 || (o[0] == 198 && o[1] & 0xfe == 18) || v.is_documentation() {
                Class::Special
            } else {
                Class::Public
            }
        }
        IpAddr::V6(v) => {
            let s = v.segments();
            if v.is_loopback() {
                Class::Loopback
            } else if s[0] & 0xfe00 == 0xfc00 || s[0] & 0xffc0 == 0xfe80 {
                Class::Private
            } else if v.is_unspecified() || v.is_multicast() || (s[0] == 0x2001 && s[1] == 0x0db8) {
                Class::Special
            } else {
                Class::Public
            }
        }
    }
}

/// An IPv4 address carried in IPv6 (`::ffff:a.b.c.d`) is judged as the IPv4 address it is.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v) => v.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

fn contains(net: IpAddr, len: u8, ip: IpAddr) -> bool {
    let (net, ip, bits) = match (net, ip) {
        (IpAddr::V4(n), IpAddr::V4(i)) => (u32::from(n) as u128, u32::from(i) as u128, 32u32),
        (IpAddr::V6(n), IpAddr::V6(i)) => (u128::from(n), u128::from(i), 128u32),
        _ => return false,
    };
    let len = len as u32;
    if len == 0 {
        return true;
    }
    let mask = u128::MAX << (bits - len) & if bits == 32 { u32::MAX as u128 } else { u128::MAX };
    net & mask == ip & mask
}

/// For tests that connect to a server they started on this machine: sets `IRONAV_NET_ALLOW=loopback`
/// for the process, once. Sources read the environment when they connect, so a test server on
/// 127.0.0.1 needs it. Not for use by IronAV itself.
#[doc(hidden)]
pub fn allow_loopback_for_tests() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| std::env::set_var(ALLOW_VAR, "loopback"));
}

/// Connects to `host:port` through the allow-list: every address the name resolves to is checked,
/// and the allowed ones are tried in turn. `shown` is the redacted URL, for errors.
pub fn connect(host: &str, port: u16, timeout: Duration, allow: &Allow, shown: &str) -> Result<TcpStream> {
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| Error::new(ErrorCode::Io, format!("could not resolve {shown}: {e}")).hint("check the address"))?
        .collect();
    if addrs.is_empty() {
        return Err(Error::new(ErrorCode::Io, format!("{shown} resolves to no address")));
    }
    let allowed: Vec<SocketAddr> = addrs.iter().copied().filter(|a| allow.permits(a.ip())).collect();
    if allowed.is_empty() {
        return Err(allow.refused(&addrs.iter().map(|a| a.ip()).collect::<Vec<_>>(), shown));
    }
    let mut last = None;
    for a in allowed {
        match TcpStream::connect_timeout(&a, timeout) {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    let e = last.expect("at least one address was tried");
    Err(Error::new(ErrorCode::Io, format!("could not connect to {shown}: {e}")).hint("check the address, and that the sender is running"))
}

/// Resolves `host:port` for a datagram peer (SRT) through the allow-list: the first address the
/// name resolves to that is allowed, which is the only one used. `shown` is the redacted URL.
pub fn resolve_udp(host: &str, port: u16, allow: &Allow, shown: &str) -> Result<SocketAddr> {
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| Error::new(ErrorCode::Io, format!("could not resolve {shown}: {e}")).hint("check the address"))?
        .collect();
    if addrs.is_empty() {
        return Err(Error::new(ErrorCode::Io, format!("{shown} resolves to no address")));
    }
    addrs.iter().copied().find(|a| allow.permits(a.ip())).ok_or_else(|| allow.refused(&addrs.iter().map(|a| a.ip()).collect::<Vec<_>>(), shown))
}

/// Receives one datagram, waiting at most `wait` (at least a millisecond) for it; a wait that
/// ends with nothing is `ErrorKind::TimedOut`. On Windows it waits with `WSAPoll` and then reads,
/// rather than through a receive timeout: there a datagram that arrives just as `SO_RCVTIMEO`
/// runs out can be lost (a few in a thousand on loopback with 5 ms timeouts and sparse traffic).
/// The socket must not have a read timeout of its own, and only one thread may read it.
pub(crate) fn recv_from_within(sock: &UdpSocket, buf: &mut [u8], wait: Duration) -> std::io::Result<(usize, SocketAddr)> {
    let wait = wait.max(Duration::from_millis(1));
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use windows::Win32::Networking::WinSock::{WSAPoll, POLLRDNORM, SOCKET, WSAPOLLFD};
        let mut fd = WSAPOLLFD { fd: SOCKET(sock.as_raw_socket() as usize), events: POLLRDNORM, ..Default::default() };
        // SAFETY: one descriptor, valid for the call
        match unsafe { WSAPoll(&mut fd, 1, wait.as_millis().min(i32::MAX as u128) as i32) } {
            0 => Err(std::io::ErrorKind::TimedOut.into()),
            n if n < 0 => Err(std::io::Error::last_os_error()),
            // readable, or an error to report (a reset from an earlier send): neither blocks
            _ => sock.recv_from(buf),
        }
    }
    #[cfg(not(windows))]
    {
        sock.set_read_timeout(Some(wait))?;
        sock.recv_from(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn the_default_allows_nothing() {
        let a = Allow::default();
        for no in ["127.0.0.1", "10.1.2.3", "172.16.0.1", "192.168.1.64", "100.64.0.1", "169.254.10.20", "::1", "fd12::1", "fe80::1", "::ffff:192.168.0.9", "8.8.8.8", "2001:db8::1", "0.0.0.0", "224.0.0.1"] {
            assert!(!a.permits(ip(no)), "{no}");
        }
    }

    #[test]
    fn each_keyword_opens_its_class() {
        let lo = Allow::parse("loopback").unwrap();
        assert!(lo.permits(ip("127.0.0.1")) && lo.permits(ip("::1")) && lo.permits(ip("::ffff:127.0.0.2")));
        assert!(!lo.permits(ip("10.0.0.1")) && !lo.permits(ip("8.8.8.8")));

        let p = Allow::parse("PRIVATE").unwrap();
        for ok in ["10.1.2.3", "172.31.255.255", "192.168.1.64", "100.64.0.1", "169.254.10.20", "fd12::1", "fe80::1", "::ffff:192.168.0.9"] {
            assert!(p.permits(ip(ok)), "{ok}");
        }
        for no in ["127.0.0.1", "8.8.8.8", "172.32.0.1", "169.254.169.254", "fd00:ec2::254", "0.0.0.0"] {
            assert!(!p.permits(ip(no)), "{no}");
        }

        let pu = Allow::parse("public").unwrap();
        for ok in ["8.8.8.8", "::ffff:8.8.8.8", "2606:4700::1"] {
            assert!(pu.permits(ip(ok)), "{ok}");
        }
        for no in ["10.0.0.1", "127.0.0.1", "0.0.0.0", "224.0.0.1", "2001:db8::1", "::"] {
            assert!(!pu.permits(ip(no)), "{no}");
        }

        let any = Allow::parse("any").unwrap();
        for ok in ["8.8.8.8", "127.0.0.1", "10.0.0.1", "224.0.0.1", "0.0.0.0", "2001:db8::1"] {
            assert!(any.permits(ip(ok)), "{ok}");
        }
        for no in ["169.254.169.254", "fd00:ec2::254", "169.254.170.2", "169.254.170.23"] {
            assert!(!any.permits(ip(no)), "metadata {no} must stay refused under any");
        }
    }

    #[test]
    fn metadata_needs_its_own_switch() {
        let mut a = Allow::parse("any").unwrap();
        assert!(!a.permits(ip("169.254.169.254")));
        a.metadata = true;
        for m in ["169.254.169.254", "fd00:ec2::254", "169.254.170.2", "169.254.170.23"] {
            assert!(a.permits(ip(m)), "{m}");
        }
        // and a listed address is no exception without the switch
        let listed = Allow::parse("169.254.169.254").unwrap();
        assert!(!listed.permits(ip("169.254.169.254")));
    }

    #[test]
    fn listed_ranges_widen_it() {
        let a = Allow::parse(" 203.0.113.7, 198.51.100.0/24 ,2001:db8::/32").unwrap();
        assert!(a.permits(ip("203.0.113.7")) && !a.permits(ip("203.0.113.8")));
        assert!(a.permits(ip("198.51.100.200")) && !a.permits(ip("198.51.101.1")));
        assert!(a.permits(ip("2001:db8:ffff::1")) && !a.permits(ip("2001:db9::1")));
        assert!(!a.permits(ip("10.0.0.1")), "the other classes stay closed");
        assert!(Allow::parse("0.0.0.0/0").unwrap().permits(ip("8.8.8.8")));
        let mapped = Allow::parse("::ffff:203.0.113.0/120,::ffff:0:0/64").unwrap();
        assert!(mapped.permits(ip("203.0.113.9")) && !mapped.permits(ip("203.0.114.9")));
        for bad in ["8.8.8.8/33", "example.com", "10.0.0.0/x", "::/129", "lan"] {
            assert_eq!(Allow::parse(bad).unwrap_err().code, ErrorCode::InvalidArgument, "{bad}");
        }
    }

    #[test]
    fn refusals_say_how_to_allow_them() {
        let e = connect("8.8.8.8", 554, Duration::from_millis(1), &Allow::default(), "rtsp://8.8.8.8/x").unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        assert!(e.message.contains("8.8.8.8") && e.hint.as_deref().is_some_and(|h| h.contains("IRONAV_NET_ALLOW=public")), "{e:?}");
        let e = connect("10.255.255.1", 554, Duration::from_millis(1), &Allow::default(), "rtsp://10.255.255.1/x").unwrap_err();
        assert!(e.hint.as_deref().is_some_and(|h| h.contains("IRONAV_NET_ALLOW=private")), "{e:?}");
        let e = connect("127.0.0.1", 554, Duration::from_millis(1), &Allow::default(), "tcp://127.0.0.1").unwrap_err();
        assert!(e.hint.as_deref().is_some_and(|h| h.contains("IRONAV_NET_ALLOW=loopback")), "{e:?}");
        let e = connect("169.254.169.254", 80, Duration::from_millis(1), &Allow::parse("any").unwrap(), "http://169.254.169.254/").unwrap_err();
        assert!(e.hint.as_deref().is_some_and(|h| h.contains(METADATA_VAR)), "{e:?}");
    }

    #[test]
    fn connect_dials_what_is_allowed() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(connect("127.0.0.1", port, Duration::from_secs(2), &Allow::parse("loopback").unwrap(), "tcp://127.0.0.1").is_ok());
        assert!(connect("127.0.0.1", port, Duration::from_secs(2), &Allow::default(), "tcp://127.0.0.1").is_err());
    }
}
