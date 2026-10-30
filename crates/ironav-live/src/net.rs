//! Which addresses a source may connect to. Agents choose source URLs, and a server that connects
//! wherever it is told can be steered at hosts it should never reach (server-side request forgery).
//! So every outbound connection is checked against an allow-list after the name resolves, and only
//! an address that passed is connected to, so a name that resolves differently a moment later
//! (DNS rebinding) cannot slip past. Redirects, once a source follows them, go through the same
//! check.
//!
//! Allowed by default: loopback, private ranges (10/8, 172.16/12, 192.168/16, fc00::/7) and
//! link-local ranges (169.254/16, fe80::/10), where cameras and encoders usually live, except the
//! cloud metadata endpoints (169.254.169.254, fd00:ec2::254), the classic target of such requests.
//! Anything wider comes from `IRONAV_NET_ALLOW`, set by whoever runs IronAV and never by the request:
//! a comma-separated list of addresses and CIDR ranges (`203.0.113.7, 198.51.100.0/24, 2001:db8::/32`),
//! or `any`. An address listed there is allowed even if it is a metadata endpoint.

use ironav_core::{Error, ErrorCode, Result};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// The environment variable that widens the allow-list.
pub const ALLOW_VAR: &str = "IRONAV_NET_ALLOW";

const METADATA: [IpAddr; 2] = [IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)), IpAddr::V6(Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254))];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Allow {
    any: bool,
    /// Configured ranges: a network address and its prefix length.
    nets: Vec<(IpAddr, u8)>,
}

impl Allow {
    /// The allow-list `IRONAV_NET_ALLOW` sets up, or the default one when it is unset.
    pub fn from_env() -> Result<Allow> {
        match std::env::var(ALLOW_VAR) {
            Ok(spec) => Allow::parse(&spec),
            Err(_) => Ok(Allow::default()),
        }
    }

    /// The default ranges plus those `spec` lists (see the module documentation).
    pub fn parse(spec: &str) -> Result<Allow> {
        let mut a = Allow::default();
        for item in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if item.eq_ignore_ascii_case("any") {
                a.any = true;
                continue;
            }
            let bad = || Error::invalid_arg(format!("{ALLOW_VAR}: '{item}' is not an address or CIDR range")).hint("for example 203.0.113.7, 198.51.100.0/24, 2001:db8::/32 or any");
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
            a.nets.push((ip, len));
        }
        Ok(a)
    }

    /// Whether a connection to `ip` is allowed.
    pub fn permits(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        if self.any || self.nets.iter().any(|&(net, len)| contains(net, len, ip)) {
            return true;
        }
        if METADATA.contains(&ip) {
            return false;
        }
        match ip {
            IpAddr::V4(v) => v.is_loopback() || v.is_private() || v.is_link_local(),
            IpAddr::V6(v) => v.is_loopback() || (v.segments()[0] & 0xfe00) == 0xfc00 || (v.segments()[0] & 0xffc0) == 0xfe80,
        }
    }

    /// Fails unless a connection to `ip` is allowed; `shown` names the source in the error.
    pub fn check(&self, ip: IpAddr, shown: &str) -> Result<()> {
        if self.permits(ip) {
            return Ok(());
        }
        Err(refused(&[ip], shown))
    }
}

fn refused(ips: &[IpAddr], shown: &str) -> Error {
    let list = ips.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
    Error::new(ErrorCode::InvalidArgument, format!("{shown} resolves to {list}, outside the addresses sources may connect to"))
        .hint(format!("loopback, private and link-local addresses are allowed; whoever runs IronAV can allow more with {ALLOW_VAR} (addresses, CIDR ranges or 'any')"))
}

/// An IPv4 address carried in IPv6 (`::ffff:a.b.c.d`) is judged as the IPv4 address it is.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v) => v.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

fn contains(net: IpAddr, len: u8, ip: IpAddr) -> bool {
    let (net, ip, bits) = match (canonical(net), ip) {
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
        return Err(refused(&addrs.iter().map(|a| a.ip()).collect::<Vec<_>>(), shown));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn the_default_allows_local_networks_only() {
        let a = Allow::default();
        for ok in ["127.0.0.1", "10.1.2.3", "172.16.0.1", "172.31.255.255", "192.168.1.64", "169.254.10.20", "::1", "fd12::1", "fe80::1", "::ffff:192.168.0.9"] {
            assert!(a.permits(ip(ok)), "{ok}");
        }
        for no in ["8.8.8.8", "172.32.0.1", "100.64.0.1", "0.0.0.0", "169.254.169.254", "fd00:ec2::254", "2001:db8::1", "::ffff:8.8.8.8", "::", "224.0.0.1"] {
            assert!(!a.permits(ip(no)), "{no}");
        }
    }

    #[test]
    fn the_environment_widens_it() {
        let a = Allow::parse(" 203.0.113.7, 198.51.100.0/24 ,2001:db8::/32,169.254.169.254").unwrap();
        assert!(a.permits(ip("203.0.113.7")) && !a.permits(ip("203.0.113.8")));
        assert!(a.permits(ip("198.51.100.200")) && !a.permits(ip("198.51.101.1")));
        assert!(a.permits(ip("2001:db8:ffff::1")) && !a.permits(ip("2001:db9::1")));
        assert!(a.permits(ip("169.254.169.254")), "listed explicitly");
        assert!(a.permits(ip("10.0.0.1")), "the default ranges stay");
        assert!(Allow::parse("any").unwrap().permits(ip("8.8.8.8")));
        assert!(Allow::parse("0.0.0.0/0").unwrap().permits(ip("8.8.8.8")));
        for bad in ["8.8.8.8/33", "example.com", "10.0.0.0/x", "::/129"] {
            assert_eq!(Allow::parse(bad).unwrap_err().code, ErrorCode::InvalidArgument, "{bad}");
        }
    }

    #[test]
    fn connect_refuses_before_dialling() {
        let e = connect("8.8.8.8", 554, Duration::from_millis(1), &Allow::default(), "rtsp://8.8.8.8/x").unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        assert!(e.message.contains("8.8.8.8") && e.hint.as_deref().is_some_and(|h| h.contains(ALLOW_VAR)), "{e:?}");
        // loopback is allowed and is dialled
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(connect("127.0.0.1", port, Duration::from_secs(2), &Allow::default(), "tcp://127.0.0.1").is_ok());
    }
}
