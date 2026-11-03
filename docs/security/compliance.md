# IronAV security assessment and compliance mapping

This document records the pre-release security review of IronAV 0.1.0, updated for 0.1.1 (MD5 compiled out of FIPS builds, sidecar resource limits) and 0.1.2 (SRT and WHIP ingest, macOS capture). It covers the weaknesses found (by CWE) and how
each was fixed, the threat model (MITRE ATT&CK), the cryptography (NIST FIPS 140-3), and how IronAV supports the
practices of CMMC 2.0 Level 2 (NIST SP 800-171 Rev. 2) and the NIST Secure Software Development Framework (SP 800-218).

IronAV is a component, not a system. CMMC and SP 800-171 apply to an organization's systems. A mapping here says how IronAV
helps an operator meet a practice, or what the operator must do, not that IronAV is "CMMC compliant".

## 1. Method

- A source review of every crate, with a focus on code that reads untrusted input (demuxers, decoders, network
  clients), FFI (`unsafe`), the agent tool surface (MCP server, web UI) and secrets handling.
- An inventory of every cryptographic algorithm and random-number source in the tree and the dependency graph.
- A scan of the repository and its history for credentials, personal data and local paths.
- `cargo audit` (RustSec advisory database) and `cargo deny` (advisories, licences, bans, sources) over the 529
  locked dependencies.
- A mutation fuzz harness over the demuxers and decoders (`crates/ironav-engine/tests/fuzz_inputs.rs`). The crashes it
  found are fixed and kept as regression tests.

Known CVEs: `cargo audit` reports **no known vulnerabilities** in the dependency graph. Four crates are reported as
unmaintained (RUSTSEC-2026-0192 `ttf-parser`, -0370 `zvariant`, -0371 `zbus_names`, -0372 `zvariant_derive`). All four
are reached only through the native GUI's windowing and accessibility stack on Linux, never on a path that reads media or
the network. They are listed with reasons in `deny.toml`. Upgrading `ratatui` to 0.30 removed `lru` 0.12 (unsound,
RUSTSEC-2026-0002 and RUSTSEC-2026-0253; `lru` 0.18.5 is now used) and `paste` (unmaintained).

## 2. Findings and fixes

| # | Severity | CWE | Finding | Fix |
|---|---|---|---|---|
| 1 | Medium | CWE-427 | NVIDIA driver DLLs (nvcuda, nvcuvid, nvEncodeAPI) were loaded by bare name, so the Windows search order could load a planted DLL | Loaded with `LOAD_LIBRARY_SEARCH_SYSTEM32` only |
| 2 | Medium | CWE-248, CWE-400 | `panic = "abort"` in release builds: one decoder bug on a hostile file ended the whole MCP server or web UI | Release builds unwind; each tool call and live session worker catches a panic and reports an `internal` error |
| 3 | Medium | CWE-22, CWE-73, CWE-862 | Agent tools accepted any read or write path | `IRONAV_ROOTS` (default in `ironav mcp`: the working directory), `IRONAV_READ_ONLY`; links and `..` are resolved before the check; the web UI file browser lists only the roots |
| 4 | Medium | CWE-359 | An agent could open the camera, the microphone or the screen without consent (a `device://screen` source reads everything on the display: passwords, messages and other people's windows; it is opened only when a source asks, and nothing is saved or logged but the open) | `IRONAV_ALLOW_DEVICES`, off in `ironav mcp`; device opens are logged |
| 5 | Medium | CWE-918 | The default network allow-list included loopback, private and link-local ranges (SSRF, internal scanning) | Nothing is allowed by default; `IRONAV_NET_ALLOW` names classes (`private`, `loopback`, `public`, `any`) or ranges; cloud metadata endpoints need `IRONAV_NET_ALLOW_METADATA=1` even under `any` |
| 6 | Medium | CWE-1426 (OWASP LLM01) | Text from media (captions, device and rendition names, sidecar output) reached the model unlabelled | Control characters stripped, cut to 512 characters, marked `"untrusted": true`; the tool descriptions say not to follow it |
| 7 | Low-Medium | CWE-214 | The web UI's launch URL, with its token, was passed on a browser command line visible to other local users | The browser opens a temporary redirect file instead |
| 8 | Low-Medium | CWE-319 | HTTP and RTSP Basic credentials were sent over plaintext when a server asked | Refused over plaintext unless `IRONAV_ALLOW_PLAINTEXT_BASIC=1` |
| 9 | Low-Medium | CWE-125, CWE-129 | The NVDEC display area was not checked against the decoded picture before the copy | Checked; a mismatch is an error |
| 10 | Low | CWE-377, CWE-59 | `<output>.partial` was created with default flags and followed links | Created exclusively (`create_new`); an existing file or link is refused |
| 11 | Low | CWE-426 | yt-dlp, arecord and sidecars were found on `PATH` | Resolved to absolute paths; `IRONAV_YTDLP` must be absolute |
| 12 | Low | CWE-522 | `?cookies=BROWSER` let a source URL make yt-dlp read a browser's cookie store | `IRONAV_ALLOW_COOKIES`, off in `ironav mcp` |
| 13 | Low | CWE-400, CWE-770 | Web UI: a thread per connection, no write timeout, a job table never trimmed | At most 32 connections, 4 running jobs and 64 finished jobs kept; 15 s write timeout; Host, Origin and token checked before the body is read |
| 14 | Low | CWE-330, CWE-208 | The web UI token, the Digest client nonce and the ONVIF nonce were not drawn from a DRBG; the token compare was not constant time | All three from `ic_drbg` (SP 800-90A CTR_DRBG seeded from the OS); constant-time compare |
| 15 | Low | CWE-327 | MD5 and SHA-1 are present | Protocol-mandated only (HTTP Digest, ONVIF WS-Security); never used for signatures; MD5 Digest refused in FIPS mode |
| 16 | Low | CWE-770, CWE-789 | Allocation sizes were taken from headers; no ceiling on decoded picture size | `IRONAV_MAX_PIXELS` (2^28, at most 16384 on a side), 64 audio channels; header sizes bounded; fuzz fixes below |
| 17 | Low | CWE-532, CWE-316 | Digest passwords were cached in plain form for the life of the process | Only HA1 is cached; URLs are redacted in every message and log |
| 18 | Info | CWE-1357 | CI ran no supply-chain checks; actions pinned by tag; no `permissions` | `cargo deny`, `cargo audit`, clippy with `-D warnings`; actions pinned by commit; `permissions: contents: read`; `--locked` builds |
| 19 | Info | CWE-362 | Two hand-written `Send`/`Sync` primitives in the H.264 and VP9 decoders | Reviewed; bounds are asserted on every access. Running them under Miri and ThreadSanitizer is future work |
| 20 | Info | CWE-250 | Sidecar programs inherited the whole environment | Only `PATH` and a few system variables, plus what the sidecar entry lists |
| 21 | Low | CWE-400 | MCP: unbounded request lines and calls in flight | 4 MiB per line, 4 calls in flight |
| 22 | Low | CWE-79 (defence in depth) | The web UI page allowed inline script | Content Security Policy with a per-response nonce |

Found by fuzzing and fixed (CWE-400, CWE-770, CWE-835, CWE-129):

- Y4M: header tokens split on a character boundary; picture size bounded.
- MPEG-4 Part 2: an escape code read inside an escape is refused.
- HEVC: the picture size must be a whole number of minimum coding blocks.
- WAV: an implausibly large `fmt ` chunk is refused.

## 3. Threat model (MITRE ATT&CK)

ATT&CK is used here as a threat model: which techniques an adversary could use IronAV for, or against it, and what
stops them. IronAV produces no detections.

| Technique | How it applies to IronAV | Mitigation |
|---|---|---|
| T1574.001 DLL Search Order Hijacking | Planted GPU driver DLL | System32-only loading (finding 1) |
| T1499 Endpoint Denial of Service | Hostile file or stream crashes or exhausts a server | Unwind and containment, caps, fuzzing (2, 13, 16, 21) |
| T1083 File and Directory Discovery, T1005 Data from Local System | An agent, or instructions injected into one, reads files outside its task | `IRONAV_ROOTS` (3) |
| T1565.001 Stored Data Manipulation, T1485 Data Destruction | Outputs overwrite or replace files | Roots, `IRONAV_READ_ONLY`, overwrite refused by default, exclusive temporary files (3, 10) |
| T1125 Video Capture, T1123 Audio Capture, T1113 Screen Capture | An agent turns on the camera or microphone, or records the screen | `IRONAV_ALLOW_DEVICES`, logged (4) |
| T1046 Network Service Discovery, T1090 Proxy | Source URLs used to probe or reach internal hosts (SSRF) | Default-deny allow-list checked after resolution and at each redirect; metadata endpoints refused (5) |
| T1552.005 Cloud Instance Metadata API | Credentials read from 169.254.169.254 | Refused unless `IRONAV_NET_ALLOW_METADATA=1` (5) |
| T1040 Network Sniffing, T1557 Adversary-in-the-Middle | Credentials or media read on the wire | TLS 1.3 with verification that cannot be turned off; no TLS-to-plaintext redirects; Basic refused over plaintext (8) |
| T1555.003 Credentials from Web Browsers | A URL option makes yt-dlp read browser cookies | `IRONAV_ALLOW_COOKIES` (12) |
| T1059 Command and Scripting Interpreter | A tool call runs a program | Sidecars only from the operator's file, chosen by name, no shell, minimal environment (11, 20) |
| T1204 / prompt injection (ATLAS AML.T0051) | Media text tells an agent what to do | Labelled `untrusted`, cleaned and cut (6) |
| T1195.001, T1195.002 Supply Chain Compromise | A dependency or build action is compromised | Committed lockfile, crates.io only, `cargo deny`, `cargo audit`, pinned actions, SBOM in each release (18) |
| T1078 Valid Accounts (web UI) | A local page or process drives the web UI | Loopback bind, random token, Host and Origin checks, no CORS, constant-time compare (7, 13, 14) |
| T1190 Exploit Public-Facing Application, T1499.002 Service Exhaustion Flood | A `whip://` endpoint is a listener: hostile HTTP requests, SDP offers, STUN, DTLS and SRTP datagrams, or floods of them | Loopback bind by default; other peers only from `IRONAV_NET_ALLOW`; optional bearer token compared in constant time; every input bounded (CWE-400, CWE-770: 16 KiB headers, 64 KiB offer, 8 connections with a 5 s deadline and, under `whips://`, a 3 s TLS handshake deadline on a thread per connection, 16 KiB DTLS messages, 16 SRTP streams, 4096 held frames); a stateless DTLS cookie before any handshake state; media only from an address that passed a STUN check and only once it authenticates; one publisher, a 15 s setup limit; parsers tested with random and truncated input (CWE-20, CWE-125) |
| T1557 Adversary-in-the-Middle (WHIP) | Someone on the path swaps the DTLS fingerprints in the plaintext HTTP offer and answer, or reads the bearer token (CWE-319) | The publisher's certificate must match its offer's fingerprint; `whips://` serves the HTTP signalling over TLS 1.3 (certificate and key from `cert=` and `key=`), so the offer, answer and token are protected on the path, and FIPS mode refuses plaintext `whip://`. Media is always DTLS-SRTP |

## 4. Cryptography (NIST FIPS 140-3)

**IronAV does not contain, and is not, a CMVP-validated cryptographic module.** IronAV and its dependencies make no
claim of FIPS 140-3 validation.

| Use | Algorithms | Implementation | Validated |
|---|---|---|---|
| TLS 1.3 client (`https://`, `rtsps://`) | AES-128/256-GCM, ChaCha20-Poly1305; ECDHE P-256/P-384, X25519, X25519MLKEM768; ECDSA, RSA-PSS, Ed25519, ML-DSA; SHA-256/384, HKDF | IronSocketLayer 0.7 over IronCrypto, IronPKI for X.509 | No |
| Random numbers for TLS, tokens and nonces | SP 800-90A CTR_DRBG (AES-256), seeded from BCryptGenRandom or `/dev/urandom` | `ic-drbg` | No |
| HTTP and RTSP Digest | SHA-256; MD5 (when a server offers only MD5), compiled out of builds with the `fips` feature | In-tree (`ironav-live`) | No |
| ONVIF WS-Security | SHA-1 PasswordDigest (protocol-mandated) | In-tree | No |
| SRT encryption (`srt://` with `passphrase`) | AES-128/192/256-CTR; RFC 3394 key wrap; PBKDF2-HMAC-SHA1 (2048 iterations, protocol-mandated) | `ic-cipher` for AES and the wrap; SHA-1 and PBKDF2 in-tree | No |
| WHIP: DTLS 1.2 server (`whip://`) | TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256: ECDHE P-256, ECDSA P-256 with SHA-256 (a self-signed certificate made at open), AES-128-GCM records, the TLS 1.2 PRF and exporter (HMAC-SHA-256), the extended master secret | ECDHE, ECDSA, X.509 and HMAC-SHA-256 from IronSocketLayer over IronCrypto; AES-GCM from `ic-cipher`; the PRF, record layer and handshake in-tree (`ironav-live/src/webrtc`) | No |
| WHIP publisher: DTLS 1.2 client (library `WhipPublisher`) | The same suite as the server, in the DTLS client role: it offers only TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256 and `use_srtp` with AEAD_AES_128_GCM (and AES128_CM_HMAC_SHA1_80 only outside FIPS mode); the server's certificate must have the SHA-256 fingerprint of its SDP answer; signalling over `https://` is TLS 1.3 (the FIPS profile in FIPS mode) and plain `http://` is refused in FIPS mode | As the server: IronSocketLayer over IronCrypto, `ic-cipher`, in-tree handshake | No |
| WHIP: SRTP and SRTCP | AEAD_AES_128_GCM (RFC 7714); AES128_CM_HMAC_SHA1_80 (AES-128 counter mode, HMAC-SHA-1 truncated to 80 bits; not offered in FIPS mode); the AES-CM key derivation | AES, AES-GCM and AES-CTR from `ic-cipher`; HMAC-SHA-1 over the in-tree SHA-1 | No |
| WHIP: STUN (ICE) | HMAC-SHA-1 MESSAGE-INTEGRITY, CRC-32 FINGERPRINT (protocol-mandated; integrity of connectivity checks only) | In-tree | No |
| WHIP: bearer token, ICE credentials, session ids | Constant-time compare of SHA-256 digests (`ic-core`); random values from `ic-drbg` | `ic-core`, `ic-drbg`, IronSocketLayer | No |
| Codec noise, dither, test data | LCG, xorshift | In-tree | Not security uses |

TLS is 1.3 only: the outbound client, and the `whips://` endpoint's HTTP signalling, served by IronSocketLayer's TLS 1.3
server from the certificate chain and private key named in the URL (PEM files read once at open; the key file's
permissions are the operator's to set). The one exception is the WHIP endpoint's DTLS 1.2 server, which WebRTC requires;
it offers a single AEAD suite and no renegotiation, and authenticates the publisher by the certificate fingerprint in its
offer, not by a CA (WebRTC has no certificate chain). SHA-1 signatures are refused. Certificate path and name validation
cannot be turned off for TLS. Encrypted HLS is not implemented, and such streams are refused. SRTP is implemented only
for WHIP ingest and the WHIP publisher.

SRT (`srt://`) is a UDP service in `ironav-live/src/srt.rs`. Its network attack surface: a listener (`mode=listener`) binds
a UDP port (loopback unless an address is given) and answers handshakes from anyone, though only a caller that returns a
cookie tied to its address gets a connection and then one sender is served; a caller connects only to an address
`IRONAV_NET_ALLOW` permits. All input is untrusted: packet and extension lengths are checked, the receive window is 8192
packets and 32 MiB, loss reports and the queue of unread payloads are bounded, a stream whose sequence numbers leave the
window is dropped, and the code is fuzzed with random and truncated packets (a test, not a long run). SRT's encryption is
AES-CTR with no authentication of the data, keyed by a passphrase through PBKDF2-HMAC-SHA1 (2048 iterations) and the RFC
3394 key wrap, which is what the protocol defines; it keeps the stream from being read, not from being altered. The
passphrase is redacted from statuses, errors and the audit log. A sender that does not encrypt when a passphrase is given
is refused, so the stream cannot be downgraded. Without a passphrase SRT is a plaintext transport. AES and the key wrap come
from `ic-cipher` (not CMVP-validated); the SHA-1 is in-tree.

The SRT sender (`ironav-live/src/srt/send.rs`, used by `--publish` and the `publish` field of `stream_open`) is an outbound
path. A caller sends only to an address `IRONAV_NET_ALLOW` permits, checked when the URL is opened and for every attempt;
the engine's audit log records the destination (`network_publish`) with the passphrase redacted. A listener sender binds a
UDP port (loopback unless an address is given) and serves one receiver at a time. The send buffer, the retransmission
queue and the output queue are bounded, a slow or absent receiver drops packets instead of blocking the session, and
control packets from the receiver are length-checked as on the receiving side. The sender generates its own key and salt
(AES-CTR, as above, with no authentication of the data). Without a passphrase it sends plaintext, which FIPS mode refuses.

**FIPS mode.** Turned on by `IRONAV_FIPS=1` or by building with the `fips` feature. In FIPS mode:

- TLS offers only AES-GCM suites, P-256 and P-384 ECDHE, and ECDSA and RSA-PSS signatures. RSA PKCS#1 v1.5 is accepted
  only on certificates.
- ChaCha20-Poly1305, X25519 and the hybrid groups are not offered.
- Plaintext `http://`, `rtsp://`, `tcp://`, `udp://`, `whip://` and unencrypted `srt://` (no `passphrase`) sources and `publish` destinations are refused, and so are YouTube sources, which yt-dlp
  fetches outside IronAV's control.
- MD5 Digest is refused, and in builds with the `fips` feature the MD5 code is not compiled in at all. SHA-256 Digest is
  still used. A default build with `IRONAV_FIPS=1` keeps the MD5 code and refuses it at run time.
- `whip://` is refused, since its HTTP signalling is plaintext; `whips://` (with `cert=` and `key=`) is allowed and serves it
  over the FIPS 140-3 TLS profile, narrowed as the client's is. Its media is DTLS-SRTP, with the DTLS suite above (approved algorithms) and only the
  AEAD_AES_128_GCM SRTP profile. AES128_CM_HMAC_SHA1_80 is not offered, since its HMAC-SHA-1 runs over IronAV's own SHA-1.
  STUN still uses HMAC-SHA-1 and CRC-32 as ICE requires; they authenticate connectivity checks, not media. The DTLS and
  SRTP protocol code is in-tree, over IronCrypto primitives, and like the rest is not CMVP-validated. The HTTP signalling
  is TLS under `whips://`. Tested in FIPS mode with headless Chrome publishing from a web page (`cors=`): TLS 1.3 for the
  signalling, DTLS 1.2 with ECDHE-ECDSA and AES-128-GCM, and AEAD_AES_128_GCM SRTP. FFmpeg 9.0.1 and OBS Studio
  (libdatachannel) cannot publish in FIPS mode, since their DTLS offers only AES128_CM_HMAC_SHA1_80.
- `ironav capabilities` reports `"fips": true` and the TLS profile in force.

FIPS mode limits IronAV to approved algorithms and transports, implemented by an unvalidated module. Where validated
cryptography is required (SC.L2-3.13.11), protect the connection with a validated module outside IronAV, such as an
IPsec or TLS tunnel terminated by a validated product, or wait for a validated provider.

## 5. CMMC 2.0 Level 2 / NIST SP 800-171

| Practice | How IronAV supports it | Operator responsibility |
|---|---|---|
| AC.L2-3.1.1 / 3.1.2 Limit system access and transactions | The web UI binds to loopback and needs a per-launch token; the MCP server is stdio only | Run IronAV under an account with only the access it needs |
| AC.L2-3.1.3 Control the flow of CUI | Default-deny network allow-list; metadata endpoints refused; roots limit where data is written | Set `IRONAV_NET_ALLOW` and `IRONAV_ROOTS` to the minimum |
| AC.L2-3.1.5 Least privilege | Roots, read-only mode, device and cookie opt-ins, sidecars defined only by the operator | Keep the sidecar file writable by the owner only |
| AC.L2-3.1.7 Prevent non-privileged users from running privileged functions | Agents cannot change the operator settings | Set them in the service environment, not in a user-writable file |
| AU.L2-3.3.1 / 3.3.2 Create audit records; trace actions to users | `IRONAV_AUDIT_LOG`: one JSON line for each tool call, file write, network source, device open and refusal, with the caller (`cli`, `mcp`, `webui`), outcome and UTC time; no credentials or media | Protect, keep and review the log; send it to a SIEM |
| AU.L2-3.3.8 Protect audit information | Append-only writes | Restrict the log file's permissions |
| CM.L2-3.4.6 / 3.4.7 Least functionality; restrict nonessential programs and ports | No listener except the opt-in loopback web UI and the sources that are asked to listen (`tcp://:port`, `udp://:port`, `whip://:port`, `whips://:port`), which bind loopback unless an address is given; the lean build has no GUI or sound stack; features are opt-in | Use the lean build on servers; set a `?token=` on any `whip://` endpoint that listens beyond loopback, and firewall its port |
| IA.L2-3.5.10 Store and transmit only protected passwords | Only Digest HA1 is cached; Basic refused over plaintext; passwords redacted from every message | Give credentials over TLS (`https://`, `rtsps://`) |
| SC.L2-3.13.1 Monitor and protect communications at boundaries | Allow-list checked after resolution and at each redirect | Firewall egress to match |
| SC.L2-3.13.8 Protect CUI in transit | TLS 1.3 with verification always on; no downgrade on redirect; FIPS mode refuses plaintext | Use TLS URLs, or turn on FIPS mode |
| SC.L2-3.13.11 FIPS-validated cryptography | FIPS mode limits algorithms to approved ones | **Not met by IronAV alone**: the module is not validated (see section 4) |
| SC.L2-3.13.15 Protect session authenticity | DRBG token and nonces, constant-time compare, Host and Origin checks | |
| SI.L2-3.14.1 Identify and correct flaws | `cargo audit` and `cargo deny` in CI; security advisories and patch releases (SECURITY.md) | Follow releases |
| SI.L1-3.14.2 Malicious code protection | System32-only driver loading; no plugins loaded from disk; sidecars by name only | Run endpoint protection |
| SI.L2-3.14.6 / 3.14.7 Monitor systems; detect unauthorized use | The audit log records refusals | Alert on refusals |
| SR (supply-chain risk, from SP 800-171 Rev. 3 and SP 800-161) | Lockfile, crates.io only, licence allow-list, pinned actions, CycloneDX SBOM with each release | Check the SBOM against your approved list |

## 6. NIST SSDF (SP 800-218)

| Practice | Evidence |
|---|---|
| PO.1 Security requirements | This document; the README "Security and operator settings" |
| PS.1 / PS.2 Protect and verify the code and releases | Signed-off commits; branch on GitHub; release SBOM; crates published from the tagged commit |
| PS.3 Archive and protect each release | GitHub releases and crates.io versions |
| PW.4 Reuse well-secured software | Few dependencies, all from crates.io, checked by `cargo deny` and `cargo audit` |
| PW.5 / PW.6 Secure coding and build settings | Rust; `unsafe` limited and commented; clippy with `-D warnings`; `--locked` builds |
| PW.7 / PW.8 Review and test | This review; more than 700 tests, many comparing against FFmpeg; mutation fuzzing |
| RV.1 / RV.2 / RV.3 Find, assess and fix vulnerabilities | SECURITY.md (private reporting, advisories, CVEs); root-cause fixes kept as regression tests |

## 7. Private data

The repository and its history were checked for credentials, keys, tokens, personal media and personal paths. None is
present in the released tree. Commit author addresses are the project address. IronAV sends no telemetry and has no
update checks, crash upload, or files in the home directory. Test media in the repository is synthetic.

## 8. Residual risks and future work

- No validated cryptographic module (section 4).
- The roots check runs when a tool call starts. It stops an agent's mistakes and injected instructions, not a hostile
  local process that swaps a path between the check and the open.
- yt-dlp makes its own connections, outside the allow-list. It is refused in FIPS mode.
- GPU drivers parse the bitstream for NVDEC. A driver bug can still crash the process.
- Sidecars run as the IronAV user. CPU and memory limits (`max_cpu_seconds`, `max_memory_mb`) are opt-in per sidecar and unset by default. They are not cgroups or containers: on Linux and macOS the memory limit caps address space, which can stop a GPU runtime. File, network and device access by sidecars is not limited.
- The H.264 and VP9 concurrency primitives have not yet been run under Miri, loom or ThreadSanitizer.
- The internals of IronSocketLayer, IronCrypto and IronPKI were not reviewed here.
- The WHIP endpoint's DTLS 1.2 handshake, record layer and SRTP are IronAV's own code over IronCrypto primitives. They are
  tested against RFC vectors (STUN RFC 5769, SRTP RFC 3711 and RFC 7714, the TLS 1.2 PRF), an in-process publisher with
  loss, reordering and garbage, and FFmpeg's WHIP muxer, but have had no independent review or fuzzing campaign. Its HTTP
  signalling is plaintext under `whip://` (served over TLS under `whips://`, whose server is IronSocketLayer's and was not
  reviewed here), and with no `?token=` anyone who can reach the port can publish. CORS headers are sent only with
  `cors=ORIGIN` (or `*`), so a web page elsewhere cannot publish to a local endpoint unless that is asked for.
- The MD5 code (RFC 1321, for Digest) is in the default build, and refused only at run time when `IRONAV_FIPS=1` is set.
  Build with the `fips` feature to compile it out. ONVIF WS-Security still uses SHA-1, which the `fips` feature keeps.
