# Design: agentic-first real-time streaming, parsing and understanding

Status: M1 to M5 are implemented and M6 is in part (TLS, HTTPS and RTSPS, capture devices, WebRTC ingest); the rest is a proposal
(2026-10-29). Nothing beyond that is implemented yet unless it says so. §8 lists what is still open. TLS (`https://`, `rtsps://`) uses IronSocketLayer, with
IronPKI validating certificates.

IronAV already treats agents as first-class for **files**: one tool registry feeds the CLI, the MCP server and the
GUIs; jobs are declarative JSON; `plan` validates without writing; errors name alternatives; the ontology says what
exists. This document extends that to **live media**: an agent opens a source that never ends, receives a compact,
typed stream of what is happening in it, asks about the recent past, and acts on it (snapshot, clip, record,
transcode), all within stated latency and resource budgets.

## 1. Where IronAV is today (what this design has to change)

| Area | Today | Gap for live use |
|---|---|---|
| Input | `open_demuxer(path)` for files (every demuxer is `Read + Seek`); Ogg, ADTS, MP3 and JPEG read the whole file | live sources exist (M2, M4): pipes, sockets, growing files, RTSP, HLS and devices; what is still missing is in §8 |
| Containers | WAV, FLAC, Y4M, MP4 (fragmented read), Matroska/WebM, Ogg, ADTS, MP3, JPEG | MPEG-TS, RTP, RTSP, HLS and SRT exist now (M3, M4, M6); no DASH or WebRTC |
| Packet | `stream, pts, duration, data` | the keyframe flag and arrival time are held in `StreamPacket` (M2), not in `Packet`; `Packet` has no DTS field |
| Muxers | all seek back at `finish` (MP4 moov, MKV sizes and Cues) | progressive fMP4 and TS recording and HLS segmenting exist (M3); the file muxers still seek back at `finish` |
| Decoders | `decode(packet) -> Vec<Frame>`, `flush()` | low-delay output for H.264 (HEVC outputs by its SPS) and keyframe-only decoding exist (M2); the software H.264 decoder runs pictures on threads, low-delay mode on one |
| Encoders | software H.264, HEVC, VP8 and VP9 encoders; NVENC | B pictures add delay unless the `latency: low` profile drops them (M3); the software H.264 encoder is much slower than x264 (see the README's Performance section) |
| Execution | thread per stage, `sync_channel` depth 64, blocking back-pressure | a live source must never block on a slow consumer |
| Understanding | `probe` (stream metadata); player `snapshot` | `analyze` and its analyzers, observations and digests (M1), and sidecars (M5); model-backed analysis stays in sidecars |
| MCP | stdio JSON-RPC 2.0 (`2025-06-18`): `tools/*`, `resources/list|read` | concurrent calls, progress and cancellation (M1); resource subscriptions (`resources/subscribe`) are not built yet |
| Sessions | player sessions (`play_open` etc., at most 8) | live sessions (`stream_*`, M2) with a ring buffer, idle timeout and recording (M3); keeping logs past the process is still open (§10) |

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
| `rtsp://` | RTSP over TCP using `std::net`; RTP over UDP or interleaved (over TLS as `rtsps://`, below) |
| `http://` | HLS and progressive download over plain HTTP using `std::net` |
| `https://`, `rtsps://` | the same over TLS 1.3: IronSocketLayer, with IronPKI checking the chain and name against the platform's PEM bundle and `IRONAV_CA_FILE` (`ironav-live/src/tls.rs`) |
| `srt://host:port`, `srt://:port?mode=listener` | SRT live mode, receiver only, written here (`ironav-live/src/srt.rs`): handshake v5, ACK/NAK/retransmission, timestamp-based delivery (`?latency=` ms, 120), too-late drop, `?streamid=`, and AES-CTR encryption with `?passphrase=`; the payload is MPEG-TS, read as `udp://` is. A caller is kept up and reconnected like RTSP |
| `device://video[/N]`, `device://audio[/NAME]` | capture through each platform's own API, written here (`ironav-live/src/capture/`): Media Foundation on Windows and V4L2 on Linux (done), and AVFoundation on macOS (written and compile-checked; not yet run against a camera). A camera arrives as Y4M (NV12/YUY2 converted to 4:2:0), a microphone as open-ended 16-bit WAV, so the stream demuxers read them like any other bytes |
| `device://screen[/N]` | a display as Y4M 4:2:0 (BT.709 limited) at a constant `fps` (30, up to 60), scaled down to `size=WxH` or to 1920 wide by default, the last picture repeated while the screen is still (`ironav-live/src/capture/screen.rs`): DXGI Desktop Duplication on Windows (done, run on a desktop), X11 `XGetImage` with libX11/libXinerama/libXfixes loaded at run time on Linux and CoreGraphics on macOS (both compile-checked only); Wayland needs the PipeWire portal, not supported yet. It reads everything on the display, so it is a device for `IRONAV_ALLOW_DEVICES` |

A Rust consumer does not go through the URL to see a display's changes: `ironav_live::capture::ScreenCapture::next_frame(wait)` gives each picture as 32-bit BGRA with the dirty and moved rectangles since the previous one, and `whole` when they are unknown (the first frame, and the first after a lost duplication). Windows reads the rectangles from DXGI Desktop Duplication; X11 and macOS report every frame as whole, and their backends are compile-checked only. A remote-desktop sender sends only what changed.

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
As built (M5), media goes inline as base64 in the line rather than through temporary files, so no file outlives a crash
and the child needs no access to IronAV's directories; [docs/sidecars.md](../sidecars.md) has the protocol.

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
- **Real-time encoding profile** (built, as `video.latency: "low"` on a job and `--latency low` on `convert`):
  no B-frames, so each picture leaves as soon as it is coded and the SPS declares no reordering; an IDR picture
  every second; no look-ahead; and a bit rate held to a quarter of a second's bits in software, or CBR with a
  one-frame VBV buffer under NVENC's low-latency tuning (which a constant quantiser also gets). Naming a codec with it re-encodes even a stream of the
  same codec, since the source may hold B-frames. When this was written, the software H.264 encoder was suitable only for
  recording at a reduced resolution, or offline. A faster software preset is separate encoder work, and this
  document does not promise it. The transcode sink came later as `transcode` on `stream_open`
(`engine/src/transcode.rs`): its own decoder, the `format` and `scale` filters as needed, the software encoder with
this profile always (so no `latency` argument), and the recorder for its output, on a thread behind a bounded queue
that drops video to the next keyframe when full. `stream_configure` starts one on a running session (from the next video keyframe, its
timestamps from zero, as a late recording) or finishes it, waiting for its queued pictures.
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
- `ironav watch --commands` also accepts the player's command style on stdin (`snapshot`, `clip from=… to=…`,
  `status`, `query`, `configure`, `close`, or a JSON object with `cmd`); replies are `{"reply":…}` lines among the
  observations.

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
- Outbound connections pass an address allow-list (`ironav-live/src/net.rs`, built before M4). Agents choose source
  URLs, and a server that connects anywhere it is told is an SSRF hazard. The default allows nothing. The operator opens
  classes with `IRONAV_NET_ALLOW` (`private`, `loopback`, `public`, `any`, or addresses and CIDR ranges), which a request
  cannot set. The cloud metadata endpoints (169.254.169.254, fd00:ec2::254, 169.254.170.2 and .23) stay refused unless
  `IRONAV_NET_ALLOW_METADATA=1` is also set. Every address a
  name resolves to is checked, and only one that passed is dialled, so a name that resolves differently a moment later
  cannot slip past. Redirects go through the same check. A refusal is `invalid_argument`, with a hint naming the variable.
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
| **M2: live bytes** (done) | the `Source` trait; `pipe:`, `?follow`, `?pace=realtime`, `tcp://`, `udp://`; `StreamDemuxer` for Annex B, ADTS, Y4M, WAV, Ogg and MP3; packet metadata; `LiveSession`, the ring buffer, the `stream_*` tools and `ironav watch`; low-delay H.264 and HEVC output | none |
| **M3: broadcast** (done) | the MPEG-TS demuxer and muxer; live fMP4 and MKV reading; a progressive fMP4/TS recorder; `stream_clip`; segmenting; the `latency: low` encode profile | none |
| **M4: cameras and the web** (done) | RTSP and RTP depacketizers with a jitter buffer and RTCP; HLS over plain HTTP | none |
| **M5: models** (done) | the sidecar protocol, voice-activity gating and two reference sidecars kept out of tree (speech-to-text, detection) | none in the core; each sidecar chooses its own |
| **M6: optional transports** (done) | HTTPS and HLS over TLS, RTSPS, SRT (caller and listener, with encryption), WebRTC ingest (WHIP), and capture devices (Media Foundation, V4L2, and AVFoundation, which has not been run on a Mac) | each one by decision (§10); SRT and WHIP add no crate (`ic-cipher` and `ic-core` were already locked), AVFoundation adds the macOS-only `objc2` framework crates |

M2 as built: the `Source` trait and every source above; `StreamDemuxer` for Annex B H.264/HEVC, ADTS, Y4M and WAV;
live sessions with the ring buffer and log; `stream_open`, `stream_events`, `stream_status`, `stream_snapshot`,
`stream_clip` and `stream_close`; and `ironav watch`. The session limit uses a new error code, `busy` (exit 75),
and an unreachable source is `io`, so no further codes were added. Ogg (Opus, Vorbis, FLAC) and MP3 stream demuxers
followed later (`ironav-formats/src/audio_stream.rs`); their packets match the file readers'. Low-delay output
came later as `DecoderOptions { low_delay }` (`ironav_codec::new_decoder_with`), which live sessions use: HEVC already
outputs by its mandatory `sps_max_num_reorder_pics`, and H.264 honours VUI `max_num_reorder_frames`; when H.264
does not declare it (common from cameras), low delay starts at no reordering instead of the level's DPB size and grows
the depth each time a picture arrives that should have been shown before one already shown (that picture is not
output, so a stream with two B pictures loses two pictures once). This is the software decoder's: when NVDEC
decodes a live H.264 stream (`hwaccel` auto on an NVIDIA machine), its own reordering applies. Keyframes-only decoding came
later as an analysis option (`keyframes_only`, `--keyframes-only`) rather than a decoder option: the engine passes
the decoder only pictures that need no other, told from the bitstream (`ironav_formats::is_keyframe`: H.264 IDR and
all-intra pictures, so open-GOP I pictures count; HEVC IRAP; VP8, VP9 and AV1 key frames; MPEG-4 I-VOPs), drains a
software decoder after each, and leaves out `freeze`, which compares neighbouring pictures. Stdin commands for `watch` (`--commands`), `stream_query`, `stream_configure` (level, digest
interval, adding and removing analyzers and sidecars, starting and stopping recordings) and the idle timeout (`idle_timeout_seconds`,
300 by default, counted from the last call that names the session) came later. Packet metadata (keyframe flag and arrival time) is held in the live layer (`StreamPacket`) instead of
as new `Packet` fields, so §9's migration was not needed.

M3 so far: the MPEG-TS reader and writer (`ironav-formats/src/ts.rs`). One push parser serves files and live sources,
and the writer's output has been checked against FFmpeg. Recording (`record` on `stream_open` and `--record` on `watch`,
in `engine/src/record.rs`) writes one file, or HLS segments cut at keyframes plus a playlist that is replaced whole after
each segment (an EVENT playlist, or a sliding window with `keep_segments`). Matroska and WebM are read as they arrive
(`ironav-formats/src/mkv/live.rs`), and the packets match the file reader's. Fragmented MP4 is read as it arrives
(`ironav-formats/src/mp4demux/live.rs`), reusing the file reader's `parse_trak` and `parse_moof`. The `latency: low` encode profile is a
video option of jobs (`StreamOpts::latency`), carried by the plan's step as `low_latency` to the software H.264
encoder (`H264Encoder::low_latency`) and NVENC; live sessions will take it when they gain a transcode sink.

M4 so far: the address allow-list (`ironav-live/src/net.rs`), and RTSP (`ironav-live/src/rtsp.rs`, with MD5 for Digest
in `md5.rs`) feeding the RTP demuxer (`ironav-formats/src/rtp.rs`). A session still has one byte stream: the RTSP source
asks for RTP interleaved on its connection and passes it on in that framing, after a description frame (channel 255:
the SDP, each track's channel, and RTP-Info's `rtptime` and `seq`). `stream_demuxer` recognises the framing as `rtp`.
The demuxer takes H.264 (single, STAP-A, FU-A), HEVC (single, AP, FU), AAC (RFC 3640 hbr/lbr), Opus, and G.711 and L16
(decoded to 16-bit PCM). Its reorder buffer holds up to 64 packets per track and costs nothing in order. A gap it gives
up on drops the damaged picture, and the next one when the lost packet may have begun it. Tracks are aligned by
RTP-Info's `rtptime`; RTCP sender reports give each track's wall-clock time (`captured_at`, `sender_clock`) but do not align tracks, so a server without RTP-Info leaves each track counting
from its first packet. Checked against FFmpeg's RTP muxer: captured, played through a stand-in camera, recorded, and
decoded by FFmpeg without an error. RTP over UDP (`?transport=udp`) uses an even/odd local port pair per track, takes
datagrams only from the server's address (or a `source=` it names that the allow-list permits), frames them the same
way, and sends an RTCP receiver report every 5 s; the order between tracks is not kept, and the demuxer does not need
it. RTCP sender reports are not used for alignment: nearly every server sends RTP-Info, and rebasing a track once its
packets have gone out would put a jump in recordings, so a server without RTP-Info leaves each track counting from its
first packet. HLS (`ironav-live/src/hls.rs`, over the HTTP/1.1 client in `http.rs`) plays a master playlist's
highest-bandwidth variant and gives its segments' bytes in order, with the `EXT-X-MAP` section in front of the first
segment that uses it: concatenated TS segments are a transport stream and fragments after their initialization section
are a fragmented MP4, so the existing stream demuxers read the result unchanged. Live playlists start three segments
from the end, reload every target duration (half when nothing was added), and use media sequence numbers so no segment
is given twice. Every connection and redirect passes the allow-list, and credentials go only to the URL's own server.
Encrypted playlists are refused at open; HTTPS came later, with TLS. Separate audio renditions came later: the HLS source
follows the variant's playlist and its audio rendition's side by side and writes them as tagged pieces of one byte
stream (`ironav_formats::renditions`), which a merging demuxer splits, reads with a demuxer per part and puts back in
time order, so the session and everything after it are unchanged. Checked with
FFmpeg's HLS muxer writing TS and fMP4 segments: both played through `ironav watch`, recorded, and decoded by FFmpeg
with every frame. M4 is done.

M5 as built: the `voice_activity` analyzer (`ironav-analyze/src/speech.rs`): 20 ms frames, energy at least 10 dB over a
noise floor that follows the quietest frames, fewer than 0.35 zero crossings per sample, and at least 3 dB of spread
among the loud frames of the last second (syllables), with speech starting when 10 of 15 frames pass and ending after
0.5 s without one. A steady tone, white noise and silence do not trigger it. The sidecar runner
(`ironav-engine/src/sidecar.rs`) starts each requested sidecar from the operator's list (`IRONAV_SIDECARS`; a request
cannot name a command), waits for its hello, gives it the first analyzed stream of the kind it wants, and runs it as an
analyzer. A supervisor thread writes to the child and restarts it after a crash or a hang (1, 2, 4, 8 and 16 s, then it
stays stopped), and a reader thread takes its observations, checking kinds, times and line length. Audio is averaged to one
channel and resampled with the existing `resample` filter; with the speech gate the runner keeps its own detector and
sends each utterance with 0.25 s before it, in chunks that join without gaps, the last marked `final`. For files the
analysis waits for the child; live sessions drop inputs and report `analyzer_lagging`. `ironav-sidecar-example`, built with
the engine, detects DTMF without a model and provides the test modes. The tests cover keypad tones through resampling and
downmixing, speech gating, pictures at an interval, a crash and restart, configuration errors and a live session. The two
model-backed reference sidecars (faster-whisper speech-to-text, YOLOv8 detection on ONNX Runtime) are Python files in
`examples/sidecars/`, outside the build; they need their packages and models installed. M5 is done.

M1 comes first because it needs no new input path. It turns the existing decoders into an understanding tool for
files at once, and it fixes the event model, analyzer trait and MCP plumbing before anything goes live.

## 9. Compatibility

- `Demuxer`, `Muxer`, `Decoder`, `Encoder` and `Filter` are unchanged. `StreamDemuxer` and the adapter are additive.
- New `Packet` and `Frame` fields have defaults, and there is a constructor for them. Call sites that build them literally are migrated in
  one mechanical commit in M2.
- Player sessions and the play tools are unchanged. Later the player can open live sources through the same `Source` and
  adapter, so it gains live playback without a second implementation.

## 10. Decisions needed before the later milestones

1. **TLS**: decided and done. First rustls with IronCrypto's `ic-rustls`; then, at the user's direction,
   IronSocketLayer (TLS 1.3 only, over IronCrypto) with IronPKI for path validation and the root store. IronCrypto itself
   holds no roots. Not behind a feature: both are first-party crates.
2. **Async**: this design deliberately uses no async runtime. Confirm that, or choose `tokio` for M4 and later
   (needed only if session counts grow into the hundreds).
3. **Capture devices**: decided. Each platform's own API, written here: Media Foundation through the `windows` crate
   already in the lockfile (done), then V4L2 (done) and AVFoundation through the `objc2` crates (written and compile-checked on macOS targets; not yet run on a Mac). `ironav devices` and the `devices` tool list them.
4. **Reference sidecars**: decided. faster-whisper for speech-to-text and ONNX Runtime (a YOLOv8 export) for detection,
   as Python files in `examples/sidecars/`, outside the build, plus a model-free DTMF detector in Rust that the tests use.
5. **Persistence**: should observation logs and recordings outlive the process (a session directory), or stay in memory only?
6. **SRT**: decided and done, in tree, with no new dependency. The receiver (live mode, caller and listener) is written in
   `ironav-live/src/srt.rs` over `std::net::UdpSocket`; encryption (`?passphrase=`) uses AES and the RFC 3394 key wrap from
   `ic-cipher`, which IronSocketLayer already pulls in, and a small PBKDF2-HMAC-SHA1 (IronCrypto has no SHA-1). Not done:
   rendezvous mode, the message API, AES-GCM, packet filters (FEC), groups, and handshake version 4. Interop with libsrt
   (through FFmpeg 9.0.1) passes in both roles, in the clear and with each key length, and with packets lost; it is not
   run in CI (the `#[ignore]` tests need FFmpeg with libsrt, or `srt-live-transmit`, on the `PATH`).
7. **WebRTC ingest**: decided and done. WHIP (RFC 9725) only, receive-only, written in `ironav-live/src/webrtc` on threads
   and blocking sockets like the other sources, not webrtc-rs or an async runtime. The endpoint is the ICE-lite, DTLS-server
   side with one host candidate, BUNDLE and `rtcp-mux` on one UDP port beside its HTTP port; H.264 (`packetization-mode=1`)
   and Opus are taken and other codecs declined. DTLS 1.2 is one suite (ECDHE-ECDSA P-256, AES-128-GCM) built from
   IronCrypto's primitives through IronSocketLayer (key exchange, ECDSA, X.509) and `ic-cipher` (AES, AES-GCM, AES-CTR);
   SRTP takes AES-128-GCM or AES-128-CM-HMAC-SHA1-80 (AES-GCM only in FIPS mode). The decrypted packets go through the
   RTP framing RTSP uses, so depacketizing, the jitter buffer and sender-report timing are shared. Binding is loopback by
   default, other publishers must be in `IRONAV_NET_ALLOW`, and `?token=` adds a bearer token. Not done: the DTLS client
   role (a publisher that insists on `setup:passive`), TURN, trickle ICE and ICE restarts (PATCH is answered 405), RTX,
   simulcast, and more than one publisher per endpoint. The HTTP side has TLS as `whips://` (decided: plain HTTP signalling is not
   acceptable under FIPS, so FIPS mode refuses `whip://`): each accepted connection is wrapped in an IronSocketLayer TLS 1.3
   server session, on a thread of its own with a 3 s handshake deadline, from a PEM chain and key given as `cert=` and `key=`.

   The other direction exists as a library: `WhipPublisher` (`ironav-live/src/webrtc/publisher.rs`, not yet wired into the engine, CLI or
   tools) pushes H.264 and Opus to a WHIP endpoint. It POSTs an offer (send-only, BUNDLE, `rtcp-mux`, `setup:actpass`) over `http://` or
   `https://` (the TLS stack `https://` sources use; FIPS mode refuses `http://`), sends ICE checks with USE-CANDIDATE to the answer's host
   candidates (consent every 5 s, 30 s without an answer ends it), runs the DTLS 1.2 client (the server's certificate must have the
   answer's SHA-256 fingerprint; GCM only in FIPS mode), and sends SRTP: H.264 as single NAL units and FU-A (RFC 6184, 1200-byte
   payloads), Opus as is (RFC 7587), sender reports each second. Picture loss and full intra requests surface through
   `keyframe_requested()`, NACKs are answered from the last 512 packets of each stream, and `close()` says goodbye and DELETEs the
   session. It adds no crate, and shares the DTLS, SRTP, STUN and SDP code with the endpoint. Not done: redirects, RTX, STAP-A,
   trickle ICE, TURN, and a server that insists on being the DTLS client.