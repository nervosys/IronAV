# Changelog

All notable changes to IronAV are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - Unreleased

### Added

- **Codecs (decoders):** software decoders for H.264 (8- and 10-bit, 4:2:0, 4:2:2, 4:4:4 and monochrome,
  CAVLC and CABAC P and B slices, lossless), HEVC (Main, Main 10, 4:2:2, 4:4:4, 12-bit), VP8, VP9
  (profiles 0-1, tile columns and a multi-threaded loop filter), baseline and progressive JPEG and
  Motion-JPEG, AAC-LC with HE-AAC v1, MP3 (MPEG-1/2/2.5 Layer III), Vorbis, Opus (SILK, CELT and hybrid)
  and FLAC (4 to 32 bits).
- **Codecs (encoders):** software H.264 (Constrained Baseline, then Main with CABAC and High with the 8x8
  transform, B pictures, rate-distortion mode decisions), AAC-LC with an ADTS writer, MP3 Layer III with
  an MP3 writer, Vorbis, Opus (CELT, wideband SILK, hybrid, DTX, in-band FEC, automatic mode switching,
  2.5 to 60 ms frames; the CLI and job JSON select the mode, signal, frame size, FEC and DTX), JPEG and Motion-JPEG, and FLAC.
- **Codecs (voice link):** a playout buffer, a streaming resampler, speech detection with push-to-talk
  trimming, automatic gain control, noise suppression, and Opus packet-loss concealment.
- **Containers:** WAV, FLAC, Y4M, MP4, Matroska and WebM (read and write), Ogg (Opus, Vorbis and FLAC),
  ADTS, MP3, JPEG, and MPEG transport streams (read and write).
- **Containers (MP4):** fragmented MP4 is read as it arrives; the writer is constant-frame-rate and
  writes fast-start files; B-frames are copied with `ctts` and an edit list.
- **Containers (copy):** VP9 and AV1 are copied between WebM, Matroska and MP4; AAC keeps its encoder
  delay in MP4; Opus ends where the source did in Matroska (DiscardPadding) and Ogg.
- **Live input and streaming:** byte sources for growing files, standard input, named pipes, TCP, UDP,
  RTSP (cameras and servers, RTP over UDP), HTTP (HLS, including alternate audio renditions, and
  progressive download), and HTTPS and RTSPS through IronSocketLayer with IronPKI certificate checks.
- **Live input and streaming (devices):** cameras and microphones on Windows (Media Foundation) and
  Linux (Video4Linux2, including Motion-JPEG cameras), ONVIF discovery and media profiles, a synthetic
  test source, and an address allow-list for outbound connections.
- **Live input and streaming (YouTube):** YouTube URLs stream through their HLS playlist, resolved by yt-dlp.
- **Captions:** HLS (and YouTube) WebVTT subtitle tracks are followed and logged as `caption` events.
- **Taps at open:** `live::open_with_taps` attaches taps that see a session's first picture and sound.
- **Live sessions:** `stream_*` tools and `ironav watch`, with commands on stdin, backoff and health
  states for RTSP sources, an idle timeout, recording to a file or HLS segments while media arrives,
  and a live transcode sink.
- **Live sessions (low delay):** low-delay H.264 output (no B-frames, periodic IDR), keyframes-only
  analysis, and a ring of keyframe JPEGs.
- **Analysis and agent tools:** `analyze` turns a file into typed observations (scene cuts, silences,
  loudness, voice activity) and rolling digests, at digest, events or raw levels.
- **Sidecars:** analyzers run beside a live session and can be added or removed while it runs; a
  stopped sidecar is reaped cleanly, and taps expose decoded pictures, sound and per-stage latency.
- **Agent interface:** one tool registry feeds the CLI, the MCP server and `ironav tools` (JSON Schema).
  Jobs are declarative JSON, `plan` validates without writing, outputs are written atomically and are
  never overwritten unless asked, and errors name valid alternatives.
- **MCP server:** `ironav mcp` runs as a resident process with concurrent calls, progress notifications
  and cancellation. Playback sessions are exposed as `play_*` tools.
- **Ontology:** `ironav ontology` (search, describe, convert, and JSON-LD export) generated from the live
  codec, container, format and hardware registries.
- **Playback:** `ironav-player` decodes audio and video on separate threads, with an audio-driven clock,
  exact seeking and a JSON command interface. `ironav play` offers a terminal TUI, or JSON lines when
  not on a terminal.
- **GUI:** a native desktop window (`ironav-gui`, egui) with source, tabs, queue and drag-and-drop; the
  same window plays media with `ironav gui --play`; a loopback-only browser GUI with `ironav gui --web`.
  Both GUIs show the exact `ironav` command and job JSON behind the form.
- **Hardware acceleration (NVIDIA):** NVENC encodes H.264 and HEVC (including 10-bit Main 10); NVDEC
  decodes H.264, HEVC and AV1. Driver libraries are loaded at run time, so one binary runs without an
  NVIDIA GPU.
- **Hardware acceleration (model):** a capability matrix for CUDA/NVENC, VA-API, Quick Sync,
  VideoToolbox, D3D11VA, AMF, Vulkan Video, MediaCodec and OpenCL, driver detection, and selection by
  policy (`auto`, `off`, `require`, or a named backend) that reports each rejected candidate.
- **CLI:** `ironav` with `convert`, `probe`, `plan`, `run`, `analyze`, `watch`, `play`, `gui`, `mcp`,
  `tools`, `capabilities`, `ontology`, `devices`, `onvif` and `hw`. Every command prints one JSON envelope with a distinct exit
  code per error class. A lean build without the GUI and sound stacks is available with
  `--no-default-features`.
- **Build and test:** a workspace test suite, and a GitHub Actions workflow for Linux, macOS and Windows
  (not yet run on hosted runners).

### Known limitations

- AV1 is decoded on NVIDIA GPUs only; there is no software AV1 decoder and no AV1 encoder yet.
- HEVC has no software encoder; hardware encoding requires an NVIDIA GPU.
- Sound output through a real device has been built but not verified by listening or measurement.
- IronAV is not yet a drop-in replacement for FFmpeg or HandBrake.
