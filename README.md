# IronAV

Audio and video conversion in Rust, built to be driven by AI agents as well as people.
A CLI, an MCP server and a HandBrake-style GUI all sit on one tool registry, so they cannot drift apart.

> **Status: foundation (v0.1).** The architecture, agent interface, ontology, hardware model, CLI, MCP server
> and both GUIs are working and tested. The codec/container set is deliberately small so far:
> **WAV, FLAC and Y4M containers; PCM (8/16/24/32-bit, float), FLAC and raw-video codecs; `volume`, `gray`, `hflip` filters.**
> **H.264 and HEVC can be encoded and decoded on NVIDIA GPUs (NVENC/NVDEC)**, and MP4 files can be read and written.
> There is no software H.264/HEVC codec, so those need an NVIDIA GPU. MP4 can carry FLAC audio next to the video, but not
> AAC or Opus (no encoder or decoder exists here yet); an MP4's AAC track is listed by `probe` and must be left out with
> `--acodec none`. AV1/AAC/Opus, MKV/WebM etc. are modelled in the ontology as `planned`.
> IronAV is not yet a drop-in replacement for FFmpeg or HandBrake.

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
A backend is *usable* only if its driver is detected **and** a native provider implements the codec and direction.
Today there is exactly one provider: **NVIDIA NVENC encode of H.264 and HEVC** (Windows and Linux). Every other
backend is detected and reported but not driven, and `select` says so per candidate. Where no provider applies,
`auto` falls back to software and `require` fails with `hardware-unavailable`.

NVENC is loaded from the driver at run time (`nvEncodeAPI64.dll` / `libnvidia-encode.so.1`), so the same binary runs on
machines without NVIDIA hardware. It needs a driver that supports NVENC API 13.1 or newer and refuses older ones with a
clear error rather than guessing struct layouts. Input must be 8-bit `yuv420p` with even dimensions; there are no
B-frames and no look-ahead, so output order equals input order and each frame yields a packet immediately.

```
ironav convert -i in.y4m -o out.h264 --vcodec h264 --qp 24        # quality-targeted (0-51, default 23)
ironav convert -i in.y4m -o out.mp4  --vcodec hevc --vbitrate 4000   # ~4 Mbit/s, in an MP4
```

The MP4 writer is constant-frame-rate, with the index at the end of the file (not fast-start, so it is not ideal for
progressive web playback). Verified by opening the files with FFmpeg's MP4 demuxer: correct codec, dimensions, 30 fps,
exact 8.000 s duration, all frames decoded, and seeking lands on the right keyframe.

### Reading MP4 and decoding on the GPU (NVDEC)

`ironav probe`, `convert` and the other tools read MP4 (regular files with the index anywhere; **fragmented MP4 is not
supported**). H.264/HEVC video is decoded by NVDEC through the driver's `nvcuvid` library, loaded at run time like NVENC;
the output is cropped to the display size and returned as planar `yuv420p` in display order, so B-frame streams work.
Only 8-bit 4:2:0 is accepted (10-bit and 4:2:2/4:4:4 are refused with a clear error).

```
ironav convert -i in.mp4 -o out.y4m --vcodec rawvideo                    # decode to raw frames
ironav convert -i in.mp4 -o out.mp4 --vcodec hevc --qp 24 --acodec none  # GPU decode -> GPU encode
ironav convert -i in.mp4 -o out.mp4                                      # stream copy (remux), no GPU needed
```

`--acodec none` / `--vcodec none` leave a stream out. Copying H.264/HEVC into an MP4 is refused for streams with
B-frames, because the MP4 writer does not write composition offsets yet; re-encode them instead.

Checked against FFmpeg's software decoders (via PyAV), frame by frame: NVDEC output is **identical** for 240/240 frames of
720p H.264 and of HEVC (made by IronAV), and for 50/50 frames of 1080p H.264 with B-frames made by x264 (a different
encoder, which also exercises cropping and reordering). A 1080p x264 → HEVC GPU transcode gave 49 dB PSNR at QP 20.
Timing on the RTX 3090 Ti is noisy: decoding the 8 s 720p clip to a raw Y4M took 0.9-1.7 s over several runs, much of it
spent writing 332 MB. No comparison with FFmpeg has been made.

### Video and audio together

A job can read from more than one file, as with repeated `-i` in ffmpeg: `extra_inputs` in a job, or a second `-i` on
the command line. The first stream of each media type that the output format can hold is used and later ones are
dropped with a warning in the plan. Inputs are merged by timestamp, and the MP4 writer groups each track into
half-second chunks, so audio and video for the same moment sit close together in the file.

```
ironav convert -i in.y4m -i in.wav -o out.mp4 --vcodec h264 --acodec flac
```

The only audio codec MP4 can hold here is FLAC (the standard "FLAC in ISO BMFF" mapping). It plays in FFmpeg-based
players, VLC, Chrome and Firefox, but **not in QuickTime or most hardware players**; AAC is the codec those expect and
it is not implemented. Checked with FFmpeg's decoders: a Y4M + WAV conversion gives an 8.000 s file whose 240 video
frames decode and whose 352,800 audio samples are identical to the WAV, with the two tracks never more than 0.46 s
apart in file order.

Validated by decoding the output with FFmpeg's decoders (via PyAV) and comparing every frame to the source: 240/240
frames, correct order, High/Main profile, PSNR rising monotonically as QP falls, chroma bit-exact at QP 0. The struct
layouts were generated from NVIDIA's `nvEncodeAPI.h` (SDK 13.1) with clang, and the tests pin their sizes. Tests that
need a GPU skip themselves when none is usable. Not done: hardware scaling, 10-bit decode or encode, AV1, rate-control
tuning beyond QP/average bitrate, and the other vendors' providers.

## FLAC

Lossless audio, implemented from RFC 9639 (no code from libFLAC or FFmpeg): all subframe types, every stereo mode,
wasted bits, 4-32 bit decode, 8/12/16/20/24-bit encode, 1-8 channels, CRC-checked frames (corruption is reported,
never decoded into garbage). Limits: the encoder does not write the STREAMINFO MD5 (zeros = "not computed", which is
valid), the `.flac` demuxer skips metadata blocks such as tags and cover art (they are not carried over), and 32-bit
sources are written as 24-bit FLAC.

**Conformance** was checked against the reference libFLAC (through libsndfile): 26 files across 8/16/24-bit, 1/2/6
channels, 1-100,003 samples, and three libFLAC compression levels, in both directions (IronAV→libFLAC and
libFLAC→IronAV), all bit-exact. Small libFLAC-produced fixtures are committed so `cargo test` keeps checking this.
On one synthetic 3-minute stereo signal the output size equals libFLAC's mid compression level (0.652 of the WAV).

## Performance

Stages run on separate threads with bounded queues, packets are moved not copied, and the MCP/GUI processes stay
resident. Measured (release build, one Windows machine, 10 min of 48 kHz stereo PCM): s16→f32 with a volume filter
≈ 2,500× real time (≈ 450 MiB/s in); stream copy ≈ 5,100× real time. These are PCM numbers.
FLAC, same machine (heavily loaded, so treat as rough): decode ≈ 270-400× real time; **encode ≈ 55-90× real time,
which is several times slower than libFLAC at a comparable setting** (about 0.4 s vs 2 s for the same 3-minute file).
Per-block multithreading was tried and reverted because it was not reliably faster.
NVENC on an RTX 3090 Ti: 240 frames of 1280x720 (a 332 MB Y4M) convert to H.264 in 1.3-3.8 s wall (median about 2 s,
roughly 120 fps) over nine runs on a busy machine, including process start. Copying the same Y4M with no encoding takes
0.75-1.3 s, so uncompressed disk I/O is a large share of that time; the encoder itself was not profiled in isolation.
(An earlier quieter run took 0.8 s, so expect wide variance.) No comparison against FFmpeg has been run.

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
