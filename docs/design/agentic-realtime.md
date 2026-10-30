# Design: agentic-first real-time streaming, parsing and understanding

Status: M1 (understanding files) and most of M2 (live bytes) are implemented; the rest is a proposal
(2026-10-29). Nothing beyond that is implemented yet unless it says so. §8 lists what M2 left for later. TLS, when the HTTPS transport comes, uses rustls with IronCrypto's `ic-rustls`
provider.

IronAV already treats agents as first-class for **files**: one tool registry feeds the CLI, the MCP server and the
GUIs; jobs are declarative JSON; `plan` validates without writing; errors name alternatives; the ontology says what
exists. This document extends that to **live media**: an agent opens a source that never ends, receives a compact,
typed stream of what is happening in it, asks about the recent past, and acts on it (snapshot, clip, record,
transcode), all within stated latency and resource budgets.

## 1. Where IronAV is today (what this design has to change)

| Area | Today | Gap for live use |
|---|---|---|
| Input | `open_demuxer(path)`; every demuxer is `Read + Seek` and most call `seek(End)` at open; Ogg, ADTS, MP3 and JPEG read the whole file | nothing can read a pipe, socket, growing file or device |
| Containers | WAV, FLAC, Y4M, MP4 (fragmented read), Matroska/WebM, Ogg, ADTS, MP3, JPEG | no MPEG-TS, RTP, RTSP, HLS, DASH, SRT or WebRTC; `mpegts` is listed only as planned in the ontology |
| Packet | `stream, pts, duration, data` | no DTS, keyframe flag, loss/corruption flag, arrival time or sequence number |
| Muxers | all seek back at `finish` (MP4 moov, MKV sizes and Cues) | no progressive output: no fragmented MP4 or TS writer, no segmenting |
| Decoders | `decode(packet) -> Vec<Frame>`, `flush()` (incremental already) | H.264 and HEVC hold frames for reordering by the SPS depth; no low-delay or keyframe-only mode |
| Encoders | software H.264 with two B pictures and look-ahead-free rate control; NVENC | the B pictures add two pictures of delay; the software encoder is far from real time at 720p (about 0.2 s of CPU per picture) |
| Execution | thread per stage, `sync_channel` depth 64, blocking back-pressure | a live source must never block on a slow consumer |
| Understanding | `probe` (stream metadata); player `snapshot`; scene-cut scoring hidden inside the H.264 encoder | no analysis tools, no event model |
| MCP | stdio JSON-RPC 2.0 (`2025-06-18`): `tools/*`, `resources/list|read`; requests are handled one at a time | no progress notifications, cancellation, subscriptions or concurrent calls; `run` reports no progress over MCP |
| Sessions | player sessions (`play_open` etc., at most 8) | no live sessions, no event log, no retention |

## 2. Principles

1. **One graph, many surfaces.** Live sessions, analyzers and observations are registered the same way tools are
   today. The CLI, the MCP server, the GUIs and the ontology are all generated from the same registry, so they cannot drift apart.
2. **Observations, not pixels.** By default an agent receives small typed events and rolling digests. Pixels and
   audio come only when asked for, as thumbnails, snapshots or clips, and within a budget. Token cost is a design
   constraint, like CPU and memory.
3. **Time is explicit.** Every packet, frame and observation carries media time and arrival (wall-clock) time.
   Every observation reports its own latency, so latency is measured on every event rather than claimed.
4. **Live never blocks.** A source is read at its own pace. Each consumer declares what happens when it falls
   behind (block, drop oldest, keep latest, decimate). A consumer that falls behind is told so with a `lagging`
   observation; it never stalls the source silently.
5. **Files and live are the same code path.** A file read at real-time pace yields the same observations as a
   live source, and so does a file read as fast as possible, apart from the latency fields. All tests and golden
   logs are built from that.
6. **Sans-IO parsers.** Demuxers and depacketizers take bytes in and give packets out. They never own the socket
   or the file, so one parser serves files, pipes, sockets and fuzzers.
7. **No async runtime, no new heavy dependencies by default.** Thread-per-stage with bounded queues already works
   here and is easy to reason about. Network transports use `std::net`. Anything that needs TLS, a model runtime
   or a device SDK sits behind a cargo feature, and each one is a separate decision (§10).
8. **Bounded and safe by default.** Every queue, buffer, log and session has a cap. Network input is opt-in.
   Credentials in URLs are redacted everywhere they are echoed. Every parser that reads untrusted bytes gets a
   fuzz target.

## 3. Architecture

```
            ┌────────────── LiveSession (one per open source) ───────────────┐
 Source ──▶ │ Ingest ─▶ Demux (sans-IO) ─▶ Packet bus ─┬─▶ Decode ─▶ Frame bus ─┬─▶ Analyzers ─┐ │
 (bytes)    │  arrival time, seq, loss     ring buffer │   low-delay            │   (pool)     │ │
            │                                         └─▶ Sinks: record, clip, ├─▶ Sinks:      │ │
            │                                             restream, segment    │   transcode  │ │
            │                                                                  │              ▼ │
            │                                           Observation log (bounded, seq-numbered) │
            └──────────────────────────────────────────────────────────┬────────────────────────┘
                                                                       ▼
                       MCP tools / resources · `ironav watch` JSON lines · GUI/web overlays
```

New crates (the existing ones keep their roles):

- `ironav-live`: sources, the session graph, buses, the ring buffer, clocks and back-pressure policies.
- `ironav-analyze`: the `Analyzer` trait, the built-in analyzers, the observation types and the digest builder.
  It depends on `ironav-core` and `ironav-codec`, not on `ironav-live`, so the batch `analyze` tool can use it on files.
- `ironav-engine` stays the tool registry and registers the new tools.

### 3.1 Sources (L0)

```rust
pub trait Source: Send {
    fn describe(&self) -> SourceInfo;              // kind, redacted URL, live or not, seekable or not
    /// Fills `buf`; `Ok(0)` is the end of the stream; `WouldBlock`/timeout returns `Err(Timeout)` so the reader can check for cancellation.
    fn read(&mut self, buf: &mut [u8], deadline: Instant) -> Result<usize>;
    /// Datagram transports (UDP, RTP) hand over whole datagrams with their arrival time.
    fn read_datagram(&mut self, _buf: &mut [u8], _deadline: Instant) -> Result<Option<(usize, Instant)>> { Ok(None) }
}
```

Source URLs, in rough order of delivery:

| URL | Notes |
|---|---|
| `file:path`, plain path | as today, plus `?pace=realtime` (paced replay that simulates live) and `?follow=1` (growing file, like `tail -f`) |
| `-` / `pipe:` | stdin or a named pipe |
| `tcp://host:port`, `udp://host:port[?multicast]` | `std::net`; carries MPEG-TS, Annex B, ADTS and other raw byte streams |
| `rtsp://` | RTSP over TCP using `std::net`; RTP over UDP or interleaved; no TLS |
| `http://` | HLS and progressive download over plain HTTP using `std::net` |
| `https://`, `rtsps://`, `srt://` | feature-gated; each needs a dependency decision (§10) |
| `device:audio/default`, `device:video/0` | capture (WASAPI/Media Foundation, V4L2, AVFoundation); feature-gated, last |

### 3.2 Incremental demux (L1)

A second, push-based demuxer trait sits beside today's pull-based `Demuxer`. A blanket adapter turns any
`StreamDemuxer` plus a `Source` into a `Demuxer`, so the existing engine, player and tests keep working unchanged.

```rust
pub trait StreamDemuxer: Send {
    fn format_name(&self) -> &'static str;
    /// Bytes as they arrive; the demuxer buffers what it cannot use yet, up to a stated cap.
    fn feed(&mut self, bytes: &[u8], arrival: Instant) -> Result<()>;
    /// The next complete packet, or `None` until more bytes arrive.
    fn next_packet(&mut self) -> Result<Option<Packet>>;
    /// Streams may appear, change or end mid-stream (new PMT, new SPS, an HLS variant switch).
    fn take_stream_events(&mut self) -> Vec<StreamEvent>;
    fn end_of_input(&mut self) -> Result<()>;
}
```

Order of work:

1. Annex B (H.264/HEVC), ADTS, Y4M, WAV (with an unknown or zero data size), Ogg and MP3. These framings are
   already self-delimiting; the change is to stop reading to the end and stop seeking.
2. **MPEG-TS** (new): PAT and PMT, PES reassembly, PCR, and continuity counters, which become loss flags. Carries H.264, HEVC,
   AAC (ADTS and LATM), Opus and MP3. This is the most useful live container.
3. Live fragmented MP4 (`moof`/`mdat` pairs as they arrive; reading already parses `moof`) and live Matroska
   (clusters of unknown size).
4. RTP depacketizers: H.264 (RFC 6184: single NAL, STAP-A, FU-A), HEVC (RFC 7798), AAC (RFC 3640), Opus (RFC 7587),
   and PCM, with a jitter buffer, sequence-gap loss detection and RTCP sender reports for wall-clock mapping.

### 3.3 Packet and frame metadata

Fields are added with defaults so existing code constructs packets unchanged (`Packet::new(stream, pts, duration, data)`):

```rust
pub struct Packet {
    pub stream: usize,
    pub pts: i64,
    pub dts: Option<i64>,
    pub duration: i64,
    pub flags: PacketFlags,          // KEY, DISCARDABLE, CORRUPT, AFTER_GAP
    pub arrival: Option<Instant>,    // when the last byte arrived (live sources only)
    pub data: Vec<u8>,
}
```

Frames gain `arrival` (copied from the packet) and `flags` (`KEY`, `CORRUPT`, and `CONCEALED` when a decoder
hid an error). Latency is then simply `now - arrival` at any stage.

### 3.4 Decoding for low delay (L2)

`DecoderOptions { low_delay: bool, keyframes_only: bool, threads: usize }`, passed to the codec registry's
constructors:

- `low_delay`: H.264 and HEVC output a picture as soon as nothing earlier can still arrive. They honour
  `max_num_reorder_frames`/`sps_max_num_reorder_pics` when they are present, and output immediately when they are
  0. HEVC's parsed `low_delay` flag is used.
- `keyframes_only`: decode only IDR/IRAP pictures. Analyzers that need one picture every few seconds (thumbnails,
  coarse scene description) then cost almost nothing.
- Loss: a decoder that meets `AFTER_GAP` or `CORRUPT` either conceals the damage and marks the frame `CONCEALED`,
  or drops pictures until the next keyframe and emits a `decode_gap` observation. It never fails the session.
- NVDEC is preferred for live video when it is present, exactly as `hwaccel: auto` works today.

### 3.5 Analyzers: the understanding layer (L3)

```rust
pub trait Analyzer: Send {
    fn name(&self) -> &'static str;
    /// What it consumes: packets or frames, of which media type, at what decimation (every frame, N fps, keyframes).
    fn wants(&self) -> Wants;
    fn on_packet(&mut self, _p: &Packet, _s: &StreamInfo, _out: &mut Vec<Observation>) {}
    fn on_frame(&mut self, _f: &Frame, _s: &StreamInfo, _out: &mut Vec<Observation>) {}
    /// Called every digest interval; contributes its part of the rolling summary.
    fn digest(&mut self, _window: TimeRange) -> Option<serde_json::Value> { None }
    fn flush(&mut self, _out: &mut Vec<Observation>) {}
}
```

Analyzers run on a small worker pool, each with its own bounded input queue and a back-pressure policy. When one
falls behind it is decimated, not allowed to stall the source, and it emits a `lagging` observation saying how
many inputs it skipped.

**Built-in analyzers (pure Rust, no new dependencies)**:

| Analyzer | Input | Observations |
|---|---|---|
| `structure` | packets | `stream_added/changed/ended`, `gop` (keyframe interval), `bitrate` (windowed), `timestamp_gap`, `discontinuity`, `loss` (TS continuity, RTP sequence) |
| `levels` | audio frames | peak, RMS, `clipping` |
| `loudness` | audio frames | EBU R128 / ITU-R BS.1770: momentary, short-term and integrated loudness, and loudness range |
| `silence` | audio frames | `silence_start/end` (threshold and minimum duration) |
| `voice_activity` | audio frames | `speech_start/end` from energy, spectral flatness and zero crossings; a cheap gate for a transcription model |
| `scene` | video frames (low-res) | `scene_cut` with a score. This moves the encoder's `low_res`/`scene_cut` code into a shared module so the encoder and the analyzer use one implementation |
| `motion` | video frames (low-res) | motion energy per window, `still_start/end` |
| `black` / `freeze` | video frames | `black_start/end`, `freeze_start/end` |
| `quality` | video frames | sharpness (Laplacian variance), luma histogram, blockiness |
| `thumbnails` | keyframes or N per minute | `thumbnail` with a resource reference (JPEG through the existing encoder) and a perceptual hash (dHash) for de-duplication |

**Model-backed analyzers** (speech-to-text, OCR, object detection, captioning, embeddings) are **not built into
the core**. They attach through an **analyzer sidecar protocol**: a child process speaking JSON lines on
stdin/stdout, the same shape as the player's protocol. IronAV sends it decimated inputs, as audio chunks (PCM
s16 or f32) or pictures (JPEG/PNG or raw planes through a temporary file), only when gating observations allow
(for example, transcription only between `speech_start` and `speech_end`). The sidecar answers with observations.
This keeps IronAV free of model runtimes, lets users bring any model, and isolates crashes and memory use.

```json
{"op":"hello","analyzer":"asr","wants":{"audio":{"rate":16000,"channels":1,"format":"s16"},"gate":"voice_activity"}}
{"op":"audio","t":12.48,"dur":2.0,"path":"…/chunk-0042.pcm"}
{"op":"observe","kind":"transcript","t":12.48,"t_end":14.40,"data":{"text":"…","lang":"en","confidence":0.91}}
```

### 3.6 Observations and digests

Each observation is one JSON object with a stable envelope; the ontology lists each `kind` and the JSON Schema of its `data`.

```json
{"session":"s1","seq":1042,"kind":"scene_cut","stream":0,"t":12.533,"t_end":null,
 "wall":"2026-10-29T18:04:11.402Z","latency_ms":38,"analyzer":"scene",
 "data":{"score":1.53},"refs":{"thumbnail":"ironav://sessions/s1/frames/12.533.jpg"}}
```

- `seq` increases strictly within a session. Every query and subscription is a cursor on it, so a client can
  reconnect and never misses or repeats an event.
- Interval events come as start/end pairs and are also folded into the digest.
- **Digests** are rolling summaries emitted every `digest_seconds`, 10 s by default. Each one carries the window,
  loudness, speech fraction, motion level, cuts, black or frozen spans, bitrate, losses, transcript text and the
  thumbnails worth looking at. An agent with a small token budget subscribes to digests only.
- Three subscription levels: `digest` (default), `events` (every observation except per-frame metrics) and `raw`
  (everything, for tools rather than models).
- The observation log is a bounded ring (by count and by age) and can optionally be persisted as JSON lines next to a recording.

### 3.7 Live sessions, retention and actions (L4)

- `LiveSession` owns the graph for one source: its analyzers, its sinks, a media clock and a map from media time
  to wall-clock time (PCR, RTCP or arrival times).
- **Ring buffer** of compressed packets, the last `retain_seconds` (default 60), cut at keyframes. It makes the
  past cheap to query without decoding everything:
  - `snapshot at t` decodes from the keyframe before `t`;
  - `clip from..to` copies packets into any existing muxer with no re-encode, the way stream copy works today;
  - analyzers added mid-session can catch up over the retained window.
- **Sinks**: record (fragmented MP4 or TS, written progressively so a crash leaves a playable file),
  segment (HLS-style TS/fMP4 segments and a playlist), transcode (through the existing encoder registry), and
  restream (TS over UDP/TCP).
- **Real-time encoding profile**: `latency: "low"` sets `bframes = 0`, no look-ahead and VBV-constrained rate
  control, and prefers NVENC's low-latency preset. Today the software H.264 encoder is suitable only for
  recording at a reduced resolution, or offline. A faster software preset is separate encoder work, and this
  document does not promise it.
- Limits: `MAX_LIVE_SESSIONS` like the player's 8; per-session memory caps for the ring buffer, the log and the
  queues; an idle timeout that closes sessions nobody has polled.

## 4. Agent surfaces (L5)

### 4.1 MCP tools (new; registered in `tools.rs`, so they also appear in the CLI and `ironav tools`)

| Tool | Read-only | Purpose |
|---|---|---|
| `analyze` | yes | run analyzers over a **file** (or a time range of it) and return observations or a digest. Delivered first, because files already work |
| `stream_open` | no | `{source, analyzers[], retain_seconds, digest_seconds, latency, record?}` → `{session, streams, …}` |
| `stream_events` | yes | `{session, after_seq, kinds?, level, max, wait_ms}`. **Long poll**: returns as soon as anything matches, or after `wait_ms`. Works with every MCP client, including ones without notification support |
| `stream_status` | yes | health: state, latency p50/p95, per-analyzer lag, losses, buffer use |
| `stream_snapshot` | yes | `{session, at? | latest, max_width}` → an image content block (as `play_control snapshot` does now) |
| `stream_clip` | no | `{session, from, to, output, format?}` → a file, written atomically like `run` |
| `stream_query` | yes | `{session, kinds?, from?, to?, text?}` over the observation log (for example, "when was speech last heard", "every cut in the last 5 minutes") |
| `stream_configure` | no | add or remove analyzers, change decimation or the digest interval, start or stop sinks |
| `stream_close` | no | stop the session, finish its sinks and return a final digest |

Every tool returns the existing envelope and error codes. New codes: `source-unreachable`, `source-ended`,
`session-limit` and `lagging` (a warning, not an error).

### 4.2 MCP protocol work

- **Concurrent requests**: a reader thread, a dispatcher and one writer that serializes responses. A
  `stream_events` long poll or a long `run` then no longer blocks `ping` or other calls.
- **Progress**: honour `_meta.progressToken` on `run` and `analyze`, sending `notifications/progress` built from
  the existing `on_progress` callback (which MCP currently drops).
- **Cancellation**: `notifications/cancelled` sets a cancellation token that the reader threads check at every
  `Source::read` deadline.
- **Resources**: `ironav://sessions/{id}/status`, `…/digest/latest` and `…/frames/{t}.jpg`, with
  `resources/subscribe`, plus `notifications/resources/updated` when a new digest lands, for clients that support it.
  Long polling stays the baseline.
- **Prompts** (optional): `watch-stream` and `summarize-recording`, ready-made templates that call the tools above.

### 4.3 CLI

- `ironav analyze <file> [--analyzers scene,loudness,…] [--level digest|events|raw]` prints JSON lines.
- `ironav watch <source> [same options] [--record out.mp4] [--retain 60]` streams observations as JSON lines on
  stdout until the source ends or the user presses Ctrl-C, then prints a final digest. This is the shell-tool path
  for agents without MCP: `ironav watch udp://:5000 --level events | head -n 50`.
- `ironav watch` also accepts the player's command style on stdin (`snapshot`, `clip from=… to=…`, `status`).

### 4.4 Ontology

New kinds: `source` (transport), `analyzer` and `observation` (each `kind` with its data schema). New relations:
`ingests` (source → container), `observes` (analyzer → media type), `emits` (analyzer → observation) and `gates`
(analyzer → analyzer, for voice activity gating transcription). Generated from the registries, as everything in the
ontology is today, so `ontology_explain_conversion` can answer questions such as "can I get transcripts from an RTSP
camera with this build?".

## 5. Latency model

Each observation's `latency_ms` is the time from `arrival` of the newest input that produced it to the moment
the observation is appended to the log. `stream_status` reports p50 and p95 per analyzer. Targets to measure
against, not claims:

| Path | Target p95 |
|---|---|
| TS/UDP → `structure` observations | < 20 ms |
| audio → `levels`/`silence`/`voice_activity` | < 50 ms beyond the analysis window |
| 720p30 H.264 → `scene`/`black`/`freeze` with NVDEC | < 100 ms |
| the same with the software decoder | to be measured; it may need `keyframes_only` or decimation at 1080p |
| `stream_events` delivery after append | < 10 ms (long poll) |

## 6. Safety and robustness

- Network sources are off unless a source URL asks for them. There are no listening sockets except an explicit `udp://:port`
  or `tcp://:port` listener, which binds to loopback unless the host is given.
- URLs are redacted (`rtsp://user:***@host/…`) in every observation, status, log and error.
- Before M4 (RTSP, HTTP), outbound connections get an address allow-list. Agents choose source URLs, and a server that
  connects anywhere it is told is an SSRF hazard. The planned default allows loopback, private and link-local ranges, and
  anything wider has to be configured. The address is checked after the name resolves and again on every redirect.
- Files that readers may hold open (the HLS playlist, clips, job outputs) are replaced with one rename. On Windows a
  rename over a file that a reader holds fails, so `ironav_core::replace_file` retries it for about a second.
- Every untrusted-byte parser, which means every `StreamDemuxer`, depacketizer, decoder and the sidecar reader,
  has caps on buffered bytes, NAL/PES/packet size and stream count, and a `cargo fuzz` target. Fuzz targets are
  part of each milestone's definition of done.
- Sidecars run as child processes given only the paths they need. A sidecar that crashes or hangs is restarted
  with back-off, and the session continues with an `analyzer_failed` observation.

## 7. Testing

- **Paced replay**: `file:clip.ts?pace=realtime` reads at the media rate. The same file read unpaced must give
  the same observation log, apart from the time fields. Golden logs live beside the fixtures.
- **Impairment wrapper**: `Source` adapters that drop, reorder, duplicate and delay datagrams with a fixed seed,
  to test loss handling, concealment and recovery at the next keyframe deterministically.
- **Analyzer accuracy**: synthetic fixtures with known answers (tones at known LUFS, cuts at known frames, black
  spans, silence) plus the EBU R128 compliance values for `loudness`.
- **Latency harness**: an ignored test that reports p50 and p95 per path from §5, run in release builds as the
  current perf tests are.
- The existing gate (`cargo test --workspace --exclude ironav-gui` and `cargo check -p ironav-gui`) covers all of it.

## 8. Milestones

Each milestone is usable on its own and passes the test gate.

| # | Delivers | New dependencies |
|---|---|---|
| **M1: understand files** (done) | `ironav-analyze` with `structure`, `levels`, `loudness`, `silence`, `scene` (shared with the encoder), `black`, `freeze`, `thumbnails`; the `analyze` tool and CLI; digests; MCP concurrency, progress and cancellation | none |
| **M2: live bytes** (done in part) | the `Source` trait; `pipe:`, `?follow`, `?pace=realtime`, `tcp://`, `udp://`; `StreamDemuxer` for Annex B, ADTS, Y4M, WAV, Ogg and MP3; packet metadata; `LiveSession`, the ring buffer, the `stream_*` tools and `ironav watch`; low-delay H.264 and HEVC output | none |
| **M3: broadcast** (done except the `latency: low` encode profile) | the MPEG-TS demuxer and muxer; live fMP4 and MKV reading; a progressive fMP4/TS recorder; `stream_clip`; segmenting; the `latency: low` encode profile | none |
| **M4: cameras and the web** | RTSP and RTP depacketizers with a jitter buffer and RTCP; HLS over plain HTTP | none |
| **M5: models** | the sidecar protocol, voice-activity gating and two reference sidecars kept out of tree (speech-to-text, detection) | none in the core; each sidecar chooses its own |
| **M6: optional transports** | HTTPS/HLS, RTSPS, SRT, capture devices, WebRTC ingest | each one by decision (§10) |

M2 as built: the `Source` trait and every source above; `StreamDemuxer` for Annex B H.264/HEVC, ADTS, Y4M and WAV;
live sessions with the ring buffer and log; `stream_open`, `stream_events`, `stream_status`, `stream_snapshot`,
`stream_clip` and `stream_close`; and `ironav watch`. The session limit uses a new error code, `busy` (exit 75),
and an unreachable source is `io`, so no further codes were added. Left for later: Ogg and MP3 stream demuxers;
low-delay H.264 and HEVC output; `stream_query` and `stream_configure`; the idle timeout; and stdin commands for
`watch`. Packet metadata (keyframe flag and arrival time) is held in the live layer (`StreamPacket`) instead of
as new `Packet` fields, so §9's migration was not needed.

M3 so far: the MPEG-TS reader and writer (`ironav-formats/src/ts.rs`). One push parser serves files and live sources,
and the writer's output has been checked against FFmpeg. Recording (`record` on `stream_open` and `--record` on `watch`,
in `engine/src/record.rs`) writes one file, or HLS segments cut at keyframes plus a playlist that is replaced whole after
each segment (an EVENT playlist, or a sliding window with `keep_segments`). Matroska and WebM are read as they arrive
(`ironav-formats/src/mkv/live.rs`), and the packets match the file reader's. Fragmented MP4 is read as it arrives
(`ironav-formats/src/mp4demux/live.rs`), reusing the file reader's `parse_trak` and `parse_moof`. Still to do in M3:
the `latency: low` encode profile.

M1 comes first because it needs no new input path. It turns the existing decoders into an understanding tool for
files at once, and it fixes the event model, analyzer trait and MCP plumbing before anything goes live.

## 9. Compatibility

- `Demuxer`, `Muxer`, `Decoder`, `Encoder` and `Filter` are unchanged. `StreamDemuxer` and the adapter are additive.
- New `Packet` and `Frame` fields have defaults, and there is a constructor for them. Call sites that build them literally are migrated in
  one mechanical commit in M2.
- Player sessions and the play tools are unchanged. Later the player can open live sources through the same `Source` and
  adapter, so it gains live playback without a second implementation.

## 10. Decisions needed before the later milestones

1. **TLS**: decided. rustls with IronCrypto (`ic-rustls`) as the crypto provider, behind a cargo feature.
2. **Async**: this design deliberately uses no async runtime. Confirm that, or choose `tokio` for M4 and later
   (needed only if session counts grow into the hundreds).
3. **Capture devices**: per-OS native APIs written here, or a crate such as `cpal`, which the player already uses for
   audio *output*, together with a camera crate?
4. **Reference sidecars**: which models and runtimes (for example whisper.cpp or ONNX Runtime), and whether they live in this
   repository or beside it.
5. **Persistence**: should observation logs and recordings outlive the process (a session directory), or stay in memory only?
