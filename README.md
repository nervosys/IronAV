# IronAV

Audio and video conversion in Rust, built to be driven by AI agents as well as people.
A CLI, an MCP server and a HandBrake-style GUI all sit on one tool registry, so they cannot drift apart.

> **Status: foundation (v0.1).** The architecture, agent interface, ontology, hardware model, CLI, MCP server
> and both GUIs are working and tested. The codec/container set is deliberately small so far:
> **WAV and Y4M containers; PCM and raw-video codecs; `volume`, `gray`, `hflip` filters.**
> H.264/HEVC/AV1/AAC/Opus, MP4/MKV/WebM etc. are modelled in the ontology as `planned`, not implemented.
> IronAV is not yet a drop-in replacement for FFmpeg or HandBrake. No native hardware provider is linked yet (see below).

## Interfaces

| Surface | Command | Use |
|---|---|---|
| CLI | `ironav convert -i in.wav -o out.wav --acodec pcm_f32le --filter volume:gain=0.5` | humans and scripts |
| GUI (native) | `ironav gui [file]` | HandBrake-style desktop window (egui): source, tabs, queue, drag-and-drop |
| GUI (browser) | `ironav gui --web` | same layout as a loopback-only web app, for machines without a display stack |
| MCP | `ironav mcp` | agents; resident process, no per-call startup |
| Tool schemas | `ironav tools` | JSON Schema for function-calling APIs |

Every CLI command prints one JSON envelope: `{"ok":true,"result":…}` or
`{"ok":false,"error":{"code","message","hint","alternatives"}}`, with a distinct exit code per error class.

## Agent workflow

1. `ironav capabilities` — what exists in this build, with the workflow spelled out.
2. `ironav ontology search|describe|convert` — what is implemented vs planned, and why a conversion is or is not feasible.
3. `ironav probe <file>` → `ironav convert --dry-run …` (or `plan`) → `ironav run job.json`.

Design choices for agents: declarative JSON jobs; `plan` validates without writing; outputs are atomic
(`.partial` then rename) and never overwrite unless `overwrite: true`; errors name valid alternatives.

## Ontology

Generated from the live registries, never hand-maintained in parallel. Kinds: codec, container, pixel format,
sample format, filter, hardware backend, platform, operation (each tool). Relations: `carries`, `accelerates`,
`availableOn`, `usesSampleFormat`, `acceptsPixelFormat`, `producesPixelFormat`.
Export: `ironav ontology jsonld` (also an MCP resource).

## Hardware acceleration

`ironav-hwaccel` models CUDA/NVENC, VA-API, Quick Sync, VideoToolbox, D3D11VA, AMF, Vulkan Video, MediaCodec and
OpenCL across Windows, Linux, macOS, Android and iOS: capability matrix, driver detection, and policy-based selection
(`auto`, `off`, `require`, or a named backend) that explains every rejected candidate.
A backend is *usable* only if its driver is detected **and** a native provider is linked. **This build links none,** so
`auto` falls back to software and `require` fails with `hardware-unavailable`. Providers are the next major milestone.

## Performance

Stages run on separate threads with bounded queues, packets are moved not copied, and the MCP/GUI processes stay
resident. Measured (release build, one Windows machine, 10 min of 48 kHz stereo PCM): s16→f32 with a volume filter
≈ 2,500× real time (≈ 450 MiB/s in); stream copy ≈ 5,100× real time. These are PCM numbers only; no claim is made
about compressed codecs, and no comparison against FFmpeg has been run.

## Security notes (browser GUI)

The native GUI runs in-process and opens no sockets. The `--web` variant is loopback only; every request needs a per-launch random token and a loopback `Host` header. The file browser can list
any directory the process can read, so treat the launch URL like a credential.

## Layout

`ironav-core` types/traits · `ironav-hwaccel` · `ironav-codec` · `ironav-formats` · `ironav-ontology` ·
`ironav-engine` (jobs, planner, pipeline, tool registry) · `ironav-gui` (native, egui) · `ironav-webui` (browser) ·
`ironav-cli`

Build: `cargo build --release` · Test: `cargo test`
Lean build for servers/agents without the graphics stack: `cargo build --release --no-default-features -p ironav-cli`

## License

Dual-licensed: **[AGPL-3.0-or-later](LICENSE)** for open use, or a **commercial license** from Nervosys if you
cannot comply with the AGPL (e.g. closed-source embedding or proprietary hosted services). See
[COMMERCIAL-LICENSE.md](COMMERCIAL-LICENSE.md). Contributions are accepted under the terms in
[CONTRIBUTING.md](CONTRIBUTING.md), which let Nervosys offer the commercial license.
