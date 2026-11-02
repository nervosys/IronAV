# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately, not in a public issue:

- through GitHub: **Security** tab of [nervosys/IronAV](https://github.com/nervosys/IronAV) > **Report a vulnerability**
  (a private security advisory), or
- by email to **opensource@nervosys.ai**, with "IronAV security" in the subject.

Include the IronAV version (`ironav --version`), the platform, the command or tool call, and, where you can, a file or
stream that reproduces the problem. We aim to acknowledge a report within 3 working days and to agree a fix and a
disclosure date with you within 14. Fixes are released as patch versions and described in a GitHub security advisory
with a CVE where one applies. Please give us 90 days, or until a fix is released if that is sooner, before disclosing.

## Supported versions

| Version | Supported |
|---|---|
| 0.1.x | yes |
| older | no (no earlier release) |

## Scope

In scope: every crate in this repository, the `ironav` binary, the MCP server (`ironav mcp`), the web UI
(`ironav gui --web`) and the native GUI. Particularly welcome: memory-safety or panic bugs reachable from a media file or
stream, path or permission bypasses of the operator settings, SSRF bypasses of the network allow-list, TLS validation
bypasses, and ways for media content to steer an agent.

Out of scope: bugs in GPU drivers (NVDEC, NVENC), in operator-configured sidecar programs and in yt-dlp; denial of
service by a local user who can already run IronAV; and reports that need the operator to set a permissive setting such
as `IRONAV_NET_ALLOW=*`.

## Security design

The threat model, the operator settings and the protections are described in the README ("Security and operator
settings") and in [docs/security/compliance.md](docs/security/compliance.md), which maps them to CWE, MITRE ATT&CK,
NIST FIPS 140-3 and CMMC 2.0 / NIST SP 800-171. In short:

- IronAV is written in Rust. `unsafe` code is limited to hardware interfaces (NVDEC, NVENC, Media Foundation, V4L2), two
  documented concurrency primitives in the codecs and one SIMD function.
- Files and streams are untrusted. Sizes read from headers are bounded, decoded pictures are capped
  (`IRONAV_MAX_PIXELS`), and parsers are exercised by a mutation fuzz harness
  (`crates/ironav-engine/tests/fuzz_inputs.rs`). A panic ends one tool call, not the server.
- Agent tools are limited by settings an agent cannot change: `IRONAV_ROOTS`, `IRONAV_READ_ONLY`,
  `IRONAV_ALLOW_DEVICES`, `IRONAV_ALLOW_COOKIES`, `IRONAV_NET_ALLOW`, with `IRONAV_AUDIT_LOG` as an audit trail.
- Network sources are checked against an allow-list after name resolution, and on every redirect. TLS is 1.3 only, and
  certificate verification cannot be turned off.
- There is no telemetry, no update check, and no crash upload.

## Cryptography

IronAV's TLS comes from IronSocketLayer over IronCrypto. **Neither is a CMVP-validated FIPS 140-3 module**, and IronAV
makes no claim of validated cryptography. The `fips` feature and `IRONAV_FIPS=1` restrict IronAV to approved
algorithms; see [docs/security/compliance.md](docs/security/compliance.md).
