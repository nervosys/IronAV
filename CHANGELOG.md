# Changelog

All notable changes to IronAV are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- The desktop player plays forward at 0.5×, 1×, 1.5×, 2× and 4× and in reverse at the same speeds, from double-arrow Forward and Reverse buttons or J, K and L. Forward rates resample the audio so it stays in sync, so pitch follows speed; reverse plays video without sound. The JSON status reports `rate`. New in ironav-player: `Player::play_at_rate` and `Player::playback_rate`, and `AudioOutput::write_frames`, which reports how many frames the device queue accepted.
- The desktop player has audio tools: live plots, a peak meter, a five-band equalizer (80 Hz to 12 kHz, ±12 dB), compression and echo. They show for audio files and dock beside the picture for video with sound (View → Audio tools). The plots are waterfall, cumulative spectral decay, waveform, spectrum and EQ response. Each can fill the window (the expand button or View → Full screen audio plot; Esc returns). The color scales are Jet (the default), Parula, Spectral, Turbo, Viridis, Plasma, Inferno, Magma, Cividis and Greyscale. On wgpu devices the analysis and the drawing run in compute and render shaders, with software plots as the fallback; GPU spectral decay refreshes up to 60 times a second. The player remembers the plot and colors. The effects run on the decoder worker before the device queue. New in ironav-player: `AudioEffects`, `EQ_FREQUENCIES`, `Player::set_audio_effects`, `equalizer_response` and `equalizer_coefficients`, and the spectrum, waveform and decay feeds (`audio_spectrum`, `audio_analysis`, `audio_visualization_pcm`, `audio_decay_pcm`), which return only what the playback clock has reached. The colormap tables come from Matplotlib, ColorBrewer and OpenCV (Parula); their notices are in `crates/ironav-gui/src/audio_tools/COLORMAP-LICENSES.txt`.
- The player tone maps HDR video (PQ and HLG) to an SDR BT.709 display: BT.2390 highlight roll-off, BT.2020 to BT.709 gamut mapping, in the GPU shader and in the f32 CPU fallback and snapshots (`rgb_frame_with`). The decoder's colour description now travels with each queued picture (`TimedFrame::color`), selects the matrix and range, and appears as `video.color` in the JSON status. Unit tests only; no HDR display or HDR file was viewed. SDR output is unchanged.
- The desktop player has the 16 Rodeo and Interlinked palettes (View → Theme). It starts in Monochrome and remembers the choice, written atomically.

- The desktop player groups its commands into File, View, and Subtitles menus. File dialogs, decoder teardown, and snapshot saving run in the background. The newest selection wins when files are opened in quick succession.
- The desktop player's Agent view can connect a local MCP client to the open window. Enable MCP and copy the configuration to inspect playback, send transport commands, or request inline video snapshots. Disable MCP to revoke the window's connection key.
- The desktop player displays timed subtitles over the video. Choose an embedded Matroska or WebM text track or open an SRT, WebVTT, ASS or SSA file from the Subtitles menu. Tracks load in the background and display as plain text, including overlapping cues and cues already in progress at a seek target.
- The player automatically deinterlaces planar video flagged by the decoder. It fills the missing lines of the first field with cubic interpolation and keeps one picture per timestamp for playback, seek previews and snapshots. NVDEC now reports field order for its woven output.
- ironav-codec, ironav-core, ironav-engine: a `deinterlace` video filter (`--filter deinterlace:mode=field`): `method` bwdif (motion-adaptive, from the BWDIF description) or linear, `mode` frame or field (field mode doubles the frame rate and halves the time base through the new `Filter::time_scale`), `parity` auto, tff or bff, and `deint` all or interlaced; 8 to 12-bit planar YUV in every layout, and gray; large pictures are split across threads. The MPEG-2 and H.264 decoders report each picture's `FieldOrder` (new, in ironav-core) through `Decoder::field_orders`, and the engine passes it to filters through `Filter::process_interlaced`, so `deint=interlaced` and `parity=auto` follow the stream. `ironav_codec::Deinterlacer` and `deinterlace_frame` are there for the player. Against FFmpeg's `bwdif` on an interlaced `testsrc2` clip: 84 dB luma PSNR. The GUI lists the filter with its options as numbers.
- ironav-gui: the player window shows video on the GPU. The decoder's planes (8-bit, or 10/12-bit as 16-bit integers) are uploaded to textures that stay alive across frames and are updated in place; a shader converts YUV to RGB with the picture's matrix (BT.601 below 720 lines, BT.709 from 720 up; BT.2020 and full range are in the shared constants) and scales with bilinear filtering, as an `egui_wgpu` paint callback on eframe's default wgpu backend. The CPU converter remains the fallback (packed RGB pictures, a window without a wgpu render state) and is what snapshots use.
- ironav-player: `Colorimetry`, `Matrix` and `YuvToRgb`, the YUV to RGB matrix and offsets shared by the CPU converter and the shader, with tests that check the shader's formula against the CPU path; `Player::set_waker` (called when a picture is queued or playback fails), `Player::next_frame_due`, and cheap `volume`, `muted` and `looping` accessors.
- ironav-player: `examples/bench_decode.rs`, a headless benchmark (`cargo run --release -p ironav-player --example bench_decode -- [FILE]`). It decodes a file, or a 1080p H.264 clip it encodes in-process, and prints JSON with the time to the first decoded frame, decode frames per second, and the CPU YUV to RGB time per frame for the old and new converters.
- `chunk`, a read-only tool (CLI `ironav chunk`, MCP `readOnlyHint: true`) that cuts a file into stable, addressable chunks for agents: fixed windows (`mode: "time"`, `seconds`) or scenes (`mode: "scene"`, cut to cut, with `min_seconds` merging and `max_seconds` splitting). Each chunk has an `id` that is the same for the same file and arguments on every run (the file's fingerprint, a hash of the mode and parameters, the index and the range), `t`, `t_end`, `index`, `scene_id`, a `uri` (`ironav://chunks/{id}`), and what `include` asks for: a small keyframe `thumbnail`, `audio` figures (levels, loudness, voice activity, silence), `stats` and the `transcript` of the named sidecars (marked `untrusted`). `limit` and `cursor` page it, and `budget_tokens` bounds a page by dropping thumbnails, stats, audio and transcript in that order, then chunks. It reuses the analyze pipeline (one decoder loop) and keeps the chunk table in an in-memory LRU of 8, keyed by fingerprint and parameters, so paging does not analyze again.
- `chunk_media`, a read-only tool (CLI `ironav chunk-media`) that returns, inline and without writing anything, evenly spaced `frames` of a chunk (JPEG or PNG, limited by `max_side`), a short `audio` WAV (capped in seconds and bytes) or a `clip` (an MP4 of the copied packets, made in memory with `FragmentedMp4Writer`, under a byte cap).
- MCP: `resources/templates/list` offers `ironav://chunks/{id}`, and `resources/read` of a chunk URI returns the cached descriptor, or an error whose hint says to call `chunk` first. Audio in a tool result goes out as an audio block, and an MP4 clip as an embedded resource.
- ironav-formats, ironav-engine: playlists. `ironav_formats::playlist` parses M3U/M3U8 (extended or plain, with `#EXTINF` durations and titles) and PLS files into entries (path or URL, title, duration), resolving relative paths against the playlist's directory; HLS playlists (`#EXT-X-` tags) are recognised and left to the stream sources. `probe playlist.m3u` lists the entries (`format`, `count`, `entries`, marked `untrusted`) and opens nothing: no file, no URL, no network; probing an HLS playlist says to use `stream_open`. File size, entry count and entry lengths are capped.
- ironav-core, ironav-formats, ironav-engine: Matroska chapters and attachments. `Demuxer::chapters()` and `Demuxer::attachments()` (default methods, empty for other formats) give each chapter's start, end, title, language and nesting depth (hidden and disabled chapters left out, the English or undetermined title preferred, nesting and counts capped) and each attachment's name, MIME type and data size (the data is not read). `probe` lists both as `chapters` and `attachments`; their text is the file's own, so the answer is cleaned and marked `untrusted`. Damaged chapters or attachments are left out and do not stop the file from opening.
- ironav-core, ironav-formats: subtitles as streams. `MediaType::Subtitle`, `StreamParams::Subtitle` and the codecs `subrip`, `ass` and `webvtt`; a subtitle stream's packets are cues (time and duration in milliseconds, the cue's text as the data; ASS events in Matroska's `ReadOrder,Layer,Style,...,Text` form with the script header as the stream's extradata). Matroska `S_TEXT/UTF8`, `S_TEXT/ASS`, `S_TEXT/SSA` and `S_TEXT/WEBVTT` tracks are read as subtitle streams (timed by BlockDuration), and standalone `.srt`, `.vtt` and `.ass`/`.ssa` files open as demuxers, recognised by their content (a byte order mark, UTF-16 and Latin-1 are handled; CRLF, a period before the milliseconds and missing blank lines or indexes are tolerated). `ironav_formats::subtitle` has `Cue { start, end, text, raw }` and `strip_markup`; the WebVTT parser moved from ironav-live to `ironav_formats::webvtt` (ironav-live re-exports it). Cues are capped in length and count and binary input is refused. A Matroska track of an unknown codec is listed in `unsupported_streams`. Stream-kind matches across the workspace (GUI, policy, sidecars, ontology) gained the subtitle arm. A zlib-compressed Matroska frame that inflates past 256 MiB is refused.
- ironav-formats: an AVI reader (`avi`, `.avi`): RIFF AVI and OpenDML (AVIX RIFFs, `indx` super indexes and `ix##` standard indexes, so files over 1 GB read), seeking by the OpenDML or `idx1` index and, with neither, by a walk over the `movi` lists (key frames then come from the bitstream). Video: MPEG-4 Part 2 (XVID, DIVX, DX50, FMP4, MP4V; DivX packed bitstream, whose padding VOPs are dropped), H.264 (Annex B, or length-prefixed with an avcC record) and Motion-JPEG; audio: PCM, MP2, MP3 (CBR by bytes, VBR by chunk), AC-3 and AAC. Streams of any other codec or kind are left out and listed by the new `Demuxer::unsupported_streams()` (a default method, empty for other formats), which `probe` shows as `unsupported_streams`; a file with nothing playable is refused with an error that names them. An index that does not match the file is not believed. Chunk sizes are checked against the file before anything is allocated.
- ironav-codec, ironav-core, ironav-formats, ironav-ontology: E-AC-3 (Dolby Digital Plus, ATSC A/52 annex E) is decoded, as the new `CodecId::Eac3` (`eac3`). `ac3::Ac3Decoder` now decodes both codecs, told apart by the bit stream id. Decoded: syncframes of 1, 2, 3 and 6 blocks, the frame-based exponent strategy, standard coupling, spectral extension, and the adaptive hybrid transform (6-point transform, gain adaptive and vector quantisation); transient pre-noise data is parsed and ignored, as FFmpeg does. Dependent substreams (`strmtyp` 1, the 7.1 channels beyond the 5.1 core) and substreams above 0 are skipped, so only the independent substream 0 (at most 5.1 channels) comes out. Enhanced coupling and the reduced sample rates (`fscod` 3) are refused with an `unsupported_codec` error that names them (FFmpeg does not decode them either). Against FFmpeg's decoder the SNR is 102 to 112 dB on streams from FFmpeg's E-AC-3 encoder and 124 to 134 dB on synthetic streams that use spectral extension, the adaptive hybrid transform in all its modes, and coupling with spectral extension. Read from raw `.eac3` and `.ec3` files (the new `eac3` format; an independent frame and its dependent frames are one packet), Matroska (`A_EAC3`, also written when copying), MP4 (`ec-3` with `dec3`, whose channel count is the independent substream's) and MPEG-TS (stream types 0x87, 0x84 and 0xA1, or 0x06 with the enhanced AC-3 descriptor or the `EAC3` registration descriptor). New functions `ac3::eac3_frame_info`, `ac3::any_frame_info` and `ac3::dec3_body`, and the `eac3`, `skipped` and `samples` fields of `ac3::FrameInfo`.
- Player: seeks take an optional mode. `{"cmd":"seek","to":30,"mode":"keyframe"}` shows the keyframe at or before the target at once (`"accurate"`, the default, lands on the frame at the target, as before); the engine's `play_control` tool takes the same field. `Player::seek_mode` and `SeekMode` expose it, and the GUI seek bar seeks by keyframe while dragging and accurately on release.
- `Decoder::reset` resets a decoder in place for a seek (H.264, HEVC, AV1, VP9 and NVDEC implement it; the others are rebuilt as before).
- Matroska: `open_demuxer_lazy` opens a file through its SeekHead and Cues and indexes clusters on demand instead of scanning every cluster, falling back to the full scan when the Cues are missing or corrupt. The Matroska writer now writes a SeekHead.
- GUI: files open on a background thread with a loading indicator.
- `bench_seek` example: open-to-first-frame, seek latency in both modes, and Matroska open time with and without Cues, as JSON.

- ironav-codec, ironav-formats: HE-AAC v2 is decoded. The AAC decoder handles parametric stereo (ISO/IEC 14496-3 subpart 8, the new `aac/ps.rs`): the `ps_data` in the SBR extension payload with IID (coarse and fine; 10, 20 and 34 bands), ICC and the IPD/OPD extension, the hybrid analysis and synthesis filterbanks of both layouts, the decorrelator (all-pass links with fractional delay, transient detection and ducking), and the mixing matrices (rotations A and B) interpolated across the envelopes, applied in the QMF domain before the SBR synthesis. A mono core with parametric stereo comes out as two channels. Signalled by object type 29 or by the backward-compatible extension; `aac::output_params` gives the output rate and channel count (two with parametric stereo) from the AudioSpecificConfig alone, and the MP4 and Matroska readers use it for the stream's channel count (and MP4 the doubled rate). `aac::implicit_config` finds SBR and parametric stereo in the first frames of an ADTS stream, which signals neither, and the ADTS file reader, the live ADTS stream reader and the MPEG-TS reader use it, so HE-AAC v1 and v2 in raw `.aac` files and internet radio streams decode instead of being refused. Tests: 18 synthetic streams made by a test-only writer (no FFmpeg encoder here writes parametric stereo) decode to 62 to 77 dB SNR of FFmpeg's decode, plus fixtures in MP4 and ADTS, malformed-input tests, and the hybrid filterbank's reconstruction. `tools/gen_ps_tables.py` generates the Huffman tables from FFmpeg's `aacpsdata.c`.

### Changed

- The desktop player's menu bar, transport and Agent panes each have their own height budget, so their controls stay visible without clipping at the minimum window size and when entering or leaving full-screen plots. The transport buttons were restyled.
- The software H.264 decoder batches CABAC bit reads, uses SIMD for interpolation and deblocking on x86-64, and writes unweighted single-reference predictions directly into the picture. In three alternating Windows runs on a Ryzen 9 9900X using synthetic 1080p clips, median single-thread throughput rose from 66 to 93 fps on High profile and from 100 to 154 fps on Baseline. Default-thread throughput rose about 4% on both clips. The decoded pictures matched FFmpeg byte for byte.
- ironav-gui: the player window no longer repaints every 8 ms. While playing it repaints when the next picture is due or a decoder thread queues one (20 Hz for the clock and seek bar at the most otherwise), and it stays idle when paused. The status JSON is built at most once per UI frame, and only when the Agent view is open or the file has no video; volume and mute are read directly.
- ironav-player: `rgb_frame` converts in integer fixed point over row slices, with the chroma terms worked out once per chroma sample, and no longer clones a 10-bit picture to narrow it. The output is within 1 per channel of the old scalar converter (kept as `rgb_frame_reference` for the test and the benchmark); about 5 times faster at 1080p.
- ironav-formats: a raw file of E-AC-3 frames is read as E-AC-3 instead of being refused as AC-3, whatever its extension. `ac3::frame_info` still reports an E-AC-3 frame as an `unsupported_codec` error: use `any_frame_info` for either.
- Player: the decoder and its CUDA context are kept across seeks and the probe demuxer is reused at start-up; the video and audio threads wait on condition variables instead of sleeping 5 ms; the audio ring buffer is lock-free.
- MP4 seeks find the sample by bisection instead of a linear scan, and land on the key frame by display time.

### Fixed

- ironav-codec: a fill element in an AAC frame whose escape count was 0 panicked in debug builds (subtract with overflow) and wrapped in release builds; the count is now 14, as the standard has it.
- MCP: pictures in a tool result are image blocks whatever their encoding. Only `png_base64` was turned into one, so the JPEGs of `stream_keyframes` (`jpeg_base64`) reached the model as base64 text; they are now `image/jpeg` blocks, and each entry says which block is its own.
- Player: every seek rebuilt the video decoder (NVDEC reloaded its libraries and recreated the context and parser).

## [0.1.10] - 2026-11-19

### Added

- ironav-codec: every encoder is now reachable as `ironav_codec::<codec>::<Codec>Encoder`: `av1::Av1Encoder`, `mpeg2::Mpeg2Encoder` and `mpeg4::Mpeg4Encoder` join the rest (the `<codec>::enc` paths still work). `ironav_codec::encoders` re-exports them all under one path, and the crate docs carry a table of each codec's decoder and encoder path.
- ironav-formats: `OggStreamWriter<W: Write>`, an Ogg writer (Opus, Vorbis, FLAC) for sinks that cannot seek, such as an HTTP response. The header pages go out when it is created, `flush` forces the pending packets out as a page with the right granule position, and `finish` gives the sink back. The file writer is built on the same page core, so the same packets give the same bytes; a FLAC stream keeps its initial STREAMINFO, as it cannot go back to fill in the totals.
- ironav-formats: `FragmentedMp4Writer<W: Write>`, the fragmented MP4 muxer as a streaming writer: the same boxes as `create_muxer_with(path, "mp4", Some(seconds))`, with no seeking. `flush` ends the fragment early (at once for audio, at the next key frame with video). The `moov` goes out with the first fragment, not at creation, as it describes the streams from their first packets.

### Fixed

- MCP annotations: `analyze` was marked as a writer and `play_open` as read-only, both wrong. `analyze` writes nothing now and is `readOnlyHint: true`; writing thumbnails to a directory (`thumbnail_dir`, CLI `--thumbnails DIR`) is the new `analyze_thumbnails` tool, which is a writer, and `analyze` refuses `thumbnail_dir` with a hint pointing there. `play_open` is no longer read-only: it opens a decoder and, by default, the sound device. A client that filters on `readOnlyHint` (such as Rodeo's `ironav-readonly` preset) now lists `analyze` and no longer lists `play_open`. `play_control` is now `destructiveHint: true` (a snapshot to `path` overwrites).
- ironav-engine: a `play_control` snapshot written to a `path` skipped `IRONAV_ROOTS` and `IRONAV_READ_ONLY`; it is now checked like any other write. A test classifies every registered tool as read-only or not and fails when a tool that writes or acts on a device is annotated read-only.
- ironav-formats: the MP4 muxer (file and `FragmentedMp4Writer`) read an audio packet's `duration` as samples whatever the stream's `time_base` said, so audio in a coarser or finer time base (a millisecond tick, say) came out too short or too long. Durations are now converted from the time base to the track's sample time scale; with a `time_base` of 1 / sample rate, the usual case, the bytes are unchanged. Video timing was already read through the time base (presentation times) and the frame rate (decode times). Tests also pin that the Matroska and Ogg writers follow it.

## [0.1.9] - 2026-11-10

### Added

- `ironav-live`, Windows: `capture::WindowCapture`, per-window capture through Windows.Graphics.Capture (a `GraphicsCaptureItem`
  made for the window's handle, a free-threaded `Direct3D11CaptureFramePool`, a `GraphicsCaptureSession`). It gives one window's own
  pixels, tightly packed BGRA, wherever the window is: while it is dragged, under other windows, partly off the display. This is
  what a remote desktop that shows one application window needs instead of copying the display and masking what lies outside it.
  `WindowCapture::open(hwnd, WindowCaptureOptions)` and `next_frame(timeout) -> Result<Option<WindowFrame>>`; frames carry their size,
  a sequence number, whether the size changed (`resized`), how many pool frames were skipped for a newer one (`dropped`) and the time
  the copy took. The frame pool is recreated when the window is resized, as WGC requires; the one frame made while a window grows
  is marked `truncated`. Options: `cursor`, `border` (the system's capture border is kept by default; when false it is asked to be left out, which Windows 11 allows;
  `border_hidden()` says whether it was) and `client_area` (cut to the client area when Windows' numbers for it match the picture).
  `state()` tells `Running`, `Minimized`, `Hidden` or `Closed` (a minimized window sends no frames, so `next_frame` returns
  `Ok(None)`); a closed window is an error (`NotFound`, "the window was closed"). `window_capture_supported()` and the open error
  (`HardwareUnavailable`, with a hint) let a caller fall back to `ScreenCapture`. The capture is not `Send`: it is used on the thread
  that opened it, which joins the Windows Runtime's multithreaded apartment until it is dropped.
- New example `window_check` in `ironav-live`, an owner-run check of window capture on a real window (by handle or process name):
  it takes about 120 frames and prints verdicts and timings (frame count, sizes seen, time between frames and copy time as median
  and maximum, whether 60 fps is possible at that size, minimized and closed handling), and no pixels or window titles.

## [0.1.8] - 2026-11-10

### Fixed

- `ironav-codec`: the H.264 encoder's bit rate control no longer swings the quantiser when pictures come only on change. The
  surplus or deficit is now a leaky bucket that drains at the rate in time and holds at most a window of credit or debt, and it
  nudges the next picture's quantiser instead of being added into the controller's state each picture. Before, the running total of
  bits spent against bits allowed fed back through that state (so it behaved like an integral of an integral), and every picture
  counted as one frame period whatever the time since the last, so a debt from an IDR picture was never repaid and a quiet screen
  left most of the rate unused. New `H264Encoder::with_time_base` says what unit the picture timestamps are in; with it the rate is
  followed in real time. Without it each picture still counts as one frame period, as before. The quantiser also keeps a floor
  (never above 26) at which a picture as costly as the last IDR picture stays within one window of bits, so a scroll after a quiet
  spell cannot burst. On a synthetic 640x480 typing run at 4 Mbit/s the quantiser's direction reversals fall from 15 to 0 and the
  largest non-IDR picture from 403 ms of rate to 316 ms.
- `ironav-live`: an `@` in the URL path or query is no longer taken for user information, so `rtsp://192.168.1.10:554/live/ch@1`
  connects to the host it names and is logged unchanged; an unescaped `/` in a password is still read as part of it when the text
  before the first `/` looks like `user:password`.
- `redact` no longer prints a malformed password that starts with digits and holds an unescaped `/`, `?` or `#` (such as
  `rtsp://user:123/x@host/live`), which the parser reads as a port; it masks conservatively, and leaves `cam:554/live?x=a@b` and
  `cam:554/live/ch@1` alone.

## [0.1.7] - 2026-11-09

### Added

- `ScreenCapture::set_region(Option<Rect>)`: ask for only a rectangle of the display, which may move between frames. Frames carry
  `origin` (where the picture lies on the display) and `display` (its full size); `dirty` and `moved` are the region's own. On
  Windows only the region is copied from the GPU (`CopySubresourceRegion` into a region-sized staging texture), mapped and moved
  on the CPU, and the cursor is drawn only where it lies inside it, so the cost scales with the region, not the display; X11 and
  macOS crop the whole picture in software. Without a region, behaviour is unchanged.
- New example `region_check` in `ironav-live`, an owner-run check of region capture on a real GPU: each test rectangle is
  compared with a crop of the whole display, and it prints no screen content.

### Fixed

- Live H.264 encoding no longer stalls for hundreds of milliseconds when other programs compete for the cores: the parallel loop
  filter's rows waited for the row above by spinning and yielding for ever, so a preempted thread left every later row burning
  cores; they now spin briefly, then sleep on a condition variable. Output is byte-identical.

## [0.1.6] - 2026-11-08

### Added

- `H264Encoder::realtime()`, an opt-in speed setting for live video (no change to any other setting's output): more slices,
  a pooled thread set for the loop filter, half-sample planes and input conversion, reused picture buffers, one
  reference, a cheaper motion search and early P_Skip. With `low_latency().constrained_baseline()` a 1920x1080 picture
  of screen content codes in about 15 ms and one of gradient plus noise in about 25 ms (was 55 and 105 ms on a 24-thread
  machine), at about the same PSNR for the bytes. New example `h264_realtime_bench` in `ironav-codec`.

## [0.1.5] - 2026-11-07

### Added

- A server-side WebSocket (RFC 6455), public as `ironav_live::websocket` (`Connection`, `Message`, `Config`, `Error`,
  `Stream`, `Refused`): upgrade validation with an Origin check, the 101 response, and a strict framing layer (masking,
  fragments, control frames, UTF-8 and size limits, idle timeout). A caller gets a connection from
  `HttpReply::upgrade_websocket` and runs it on its own thread.
- **`WhepServer`, a send-only WHEP endpoint** (`whep://`, `wheps://`; library API in `ironav-live`): a viewer POSTs an offer with
  `recvonly` video, gets a `sendonly` answer, and is sent the H.264 access units the caller pushes, over the WHIP endpoint's ICE-lite,
  DTLS server and SRTP, with NACK retransmission, sender reports, a bearer-token hook and a viewer limit. Tested in process only, not yet
  against a browser; a session cannot yet publish to it (`publish` and `--publish` take no `whep://`). One origin can serve
  more than the signalling: `WhepOptions::fallback` gets every request whose path is not the WHEP path or one of its
  sessions (as an `HttpRequest` and an `HttpReply`), to answer with a page (the server adds `nosniff` and `no-store`; the caller
  gives the content type and CSP) or to upgrade to a WebSocket, over TLS too. Upgraded connections leave the HTTP server's
  small pool and have their own limit, `WhepOptions::max_websockets` (8; the 9th is answered `503`). The `authorize` hook
  covers the WHEP paths only and is shown an `AuthRequest` (`header()`, `peer_addr()`). `WhepOptions::tls_from_pem` serves
  TLS from PEM text with the hooks set, `tls_fingerprint()` gives the certificate's SHA-256, and `tls::self_signed_pem` makes
  a certificate. `WhepOptions::h264_profile` (`H264Profile`, High by default) says what the stream is: the answer takes only an
  offered format it can be sent as, else `400`. `H264Encoder::constrained_baseline` makes a stream for baseline-only viewers.
- `Rect::new`, `MoveRect::new`, `ScreenFrame::new` and `ScreenFrame::with_changes`, so that a frame can be
  made outside a capture, as a test or another source needs. `ScreenFrame::new` panics when the pixels or the stride
  are too small for the size given.

## [0.1.4] - 2026-11-04

### Fixed

- **Opus decoder, a second overflow on malformed packets:** after an empty CELT frame set the bit count to zero, the
  fraction left in the range made the fine bit count negative, which was calculated unsigned and panicked in builds with
  overflow checks on. 0.1.3 still has this. Release builds wrapped to the same value, so their output does not change.

## [0.1.3] - 2026-11-04

### Added

- **Screen capture API:** `ironav_live::capture::list_screens()` lists the displays, and `ScreenCapture::open(index, ScreenOptions { cursor })` opens one. `next_frame(wait)` gives each picture as 32-bit BGRA with the rectangles that changed since the last (`dirty`, and `moved` for a display's own moves), and a `whole` flag for a frame whose changes are not known (the first frame, and the first after a lost duplication); it returns `None` when nothing changed within `wait`. Windows reads the rectangles from DXGI Desktop Duplication, and a mouse-only update is a frame only when the cursor is drawn. X11 and macOS report every frame as whole and do not return `None`. The capture is not `Send`. `device://screen` is unchanged.
- **Screen capture:** `device://screen` (the primary display) and `device://screen/N` as a live source, listed by
  `ironav devices` and the `devices` tool with each display's name, index and native size. Y4M 4:2:0 (BT.709, limited
  range) at a constant rate: `fps=` (30, at most 60), `size=WxH` (scaled down to fit with the aspect kept; by default the
  native size, capped at 1920 wide) and `cursor=0`. A still screen repeats its last picture. Windows uses DXGI Desktop
  Duplication (recreated after `DXGI_ERROR_ACCESS_LOST`; run on a desktop), Linux X11 through `XGetImage` with libX11,
  libXinerama and libXfixes loaded at run time (Wayland is refused with an error that names the PipeWire portal), and macOS
  CoreGraphics (the error for a missing Screen Recording permission names the System Settings pane). The Linux and macOS
  backends are compile-checked only. It is a device under `IRONAV_ALLOW_DEVICES`.
- **SRT sender, and `publish`:** `ironav_live::open_sink(url)` returns a `Sink` (`write` MPEG-TS bytes, `finish`, `stats`) for
  `srt://host:port` (caller) and `srt://:port?mode=listener`, sharing the handshake, key-material and crypto code with
  the receiver. Live mode with 1316-byte payloads, a send buffer of the latency, NAK retransmission, ACK/ACKACK with an
  RTT estimate, keep-alive, shutdown, too-late drop, `latency`, `streamid`, `passphrase` with `pbkeylen` (16, 24, 32)
  and `reconnect`. `ironav watch --publish URL` and the `stream_open` tool's `publish` field send a session's MPEG-TS
  (the transcode's, else the source remuxed as a `.ts` recording) on a thread with a bounded queue; a failure is a
  `publish_lost`, `publish_restored` or `publish_failed` event and never stops the session, and a caller is retried
  with back-off. Destinations are checked by `IRONAV_NET_ALLOW`, and FIPS mode refuses a plaintext one. Tested against
  FFmpeg 9.0.1 (libsrt) receiving, in both roles, in the clear and with 16, 24 and 32 byte keys. The receiver's shutdown
  packet now carries the 4-byte body libsrt requires. The SRT receiver asks for a 4 MB socket buffer, as WHIP does,
  so a burst or a stalled thread does not overflow the default one. `publish` also takes `whip://host:port/path` and `whips://` (with
  `?token=T`) and sends the session's H.264 and Opus through the WHIP publisher below, on its own thread with the same queue,
  back-off and status: length-prefixed H.264 becomes Annex B with the parameter sets before each keyframe, other audio is not
  sent (a `publish_audio_skipped` event), other video ends the publishing with a hint to transcode, and the server's keyframe
  requests are counted (`keyframe_requests`), and with a transcode the transcoder makes an IDR picture for each, at most one
  per half second (`keyframes_forced`; `keyframes_ignored` counts a request that came while one waited, or was still
  waiting at the end). Without a transcode nothing can be forced, so `keyframes_forced` stays 0. The H.264 encoder has
  `force_idr`: the next picture is an IDR picture with the parameter sets, and the GOP count starts again from it.
  Opus goes from the first packet, not from the first keyframe. FIPS mode refuses `whip://`. Tested against IronAV's own
  WHIP endpoint with H.264 and Opus.
- **WHIP publisher (library):** `ironav_live::WhipPublisher::connect(url, token, WhipOptions)` pushes H.264 and Opus to a WHIP
  endpoint: `send_h264_access_unit` (Annex B, 90 kHz), `send_opus` (48 kHz), `keyframe_requested` (a picture loss indication or
  full intra request from the server, polled), and `close` (RTCP BYE, DTLS close_notify, DELETE of the session). It POSTs an offer
  (send-only, BUNDLE, rtcp-mux, a=setup:actpass, H.264 packetization-mode=1 and Opus) over `http://` or `https://` with an
  optional bearer token (sent only to the endpoint's own host and port); `https://` uses the TLS stack of `https://` sources,
  and FIPS mode refuses `http://` as it does `whip://`. ICE connectivity checks with USE-CANDIDATE go to the answer's host
  candidates that `IRONAV_NET_ALLOW` permits, with consent checks every 5 s and a 30 s timeout. The DTLS 1.2 client (the former
  test client, now with retransmission, fragment reassembly, a handshake timeout and bounds checks) takes only a server whose
  certificate has the answer's SHA-256 fingerprint, and offers AES-GCM SRTP only in FIPS mode. H.264 goes out as single NAL
  units and FU-A fragments of at most 1200 bytes (a keyframe without parameter sets gets the last ones seen), RTCP sender
  reports go out every second, and NACKs are answered from the last 512 packets of each stream. Tested against the in-process
  WHIP endpoint over `http://` and `https://`. Not wired into the engine, CLI or tools yet.
  The DTLS role follows the answer's `a=setup` (RFC 5763): `passive` makes the publisher the DTLS client, as above, and `active`
  (what a full ICE agent such as Pion answers, being told to prefer it) makes it the DTLS server, with the same checks
  (the peer's certificate must have the answer's SHA-256 fingerprint, GCM only in FIPS mode, the handshake bounded and its flights
  sent again) and the SRTP keys and salts swapped to the server's side; `actpass` in an answer is refused. A full ICE agent's own
  checks are answered (success with MESSAGE-INTEGRITY under the local password and FINGERPRINT, only for a request that
  authenticates), a role conflict is settled by the tiebreakers (487, or the publisher becomes the controlled agent), and
  demultiplexing is by first byte (RFC 7983) during the handshake as well. Server-reflexive candidates are taken with host ones;
  relayed, peer-reflexive and TCP ones, and the second component Pion lists for `rtcp-mux`, are skipped. `close()` now DELETEs the
  session before saying goodbye on the media path, since MediaMTX ends the session on close_notify and then answers a DELETE with 404.
  Tested against MediaMTX v1.21.3 (Windows, loopback only): plain `whip://`, `whips://` (`webrtcEncryption: yes`, throwaway CA via
  `IRONAV_CA_FILE`) and a `--features fips` build with `IRONAV_FIPS=1` over `whips://`; the path became ready with H264 and Opus,
  FFmpeg 9.0.1 read both back over RTSP without decode errors, and the DELETE was answered 200 (an `#[ignore]`d test,
  `interop_with_mediamtx`, takes the executable from `IRONAV_MEDIAMTX`).
- **`whips://`, WHIP signalling over TLS:** `whips://[host]:port[/path]?cert=CHAIN.pem&key=KEY.pem[&token=...]` is the WHIP
  endpoint with each accepted connection wrapped in a TLS 1.3 server session (IronSocketLayer) before the HTTP/1.1
  parsing, so the offer, the answer and the bearer token no longer cross the network in the clear. `cert` is a PEM
  certificate chain (server certificate first) and `key` a PEM private key (PKCS#8, or SEC1 `EC PRIVATE KEY`), both
  required and read once at open. Each connection's handshake runs on its own thread with a 3 s deadline, so a stalled or
  failed handshake drops that connection only; the request size, deadline and connection limits are unchanged. In FIPS
  mode it uses IronSocketLayer's FIPS 140-3 profile, narrowed as the client's is. `cert` and `key` are options of the
  source URL (the paths are not secret; `token` stays redacted), and the engine's file policy applies to them.
- **WHIP from a web page:** `cors=ORIGIN` (or `*`) on `whip://` and `whips://` sends the CORS headers a browser needs to
  publish from a page on another origin, its preflight included, and lets the page read `Location`. Off by default, so a
  web page elsewhere cannot publish to a local endpoint. Tested with headless Chrome over `whips://` in FIPS mode.

### Changed

- **FIPS mode refuses `whip://` (security):** its HTTP signalling is plaintext, which the SDP fingerprints and the bearer
  token should not cross. The error says to use `whips://` with `cert=` and `key=`. `whips://` is allowed, with AES-GCM SRTP
  only as before. FFmpeg 9.0.1 cannot publish in FIPS mode, since its DTLS offers only AES-128-CM SRTP.
- **WHIP, after the session ends:** for 2 s the endpoint still answers a DELETE of the session that ended, so a publisher
  that closes DTLS before it DELETEs (as a browser closing its peer connection may) gets its answer.

### Fixed

- **Opus decoder with malformed packets:** a silent CELT frame smaller than the bits already read took a negative bit
  count, which the range decoder's bit total could not hold. Release builds wrapped the arithmetic and stayed in
  bounds, with the same output; builds with overflow checks panicked. The bit count is now set to the frame's size as
  the reference decoder does it, and the decoder is fuzzed with random packets of every config.
- **WHIP with OBS Studio:** OBS's DTLS certificate (libdatachannel's) has an empty extensions list, which the strict X.509
  parser refused, so OBS could not publish. The publisher's certificate is vouched for by its offer's fingerprint and only
  its key is used, so the key is now read from it without holding it to RFC 5280. OBS publishes to `whip://`; it offers
  only AES-128-CM SRTP, so not in FIPS mode.
- **SRT with libsrt:** a caller sent its handshake conclusion to its own socket ID instead of 0, which libsrt drops, so
  `srt://host:port` could not connect to a libsrt listener, and the listener took a conclusion only if it was addressed to
  its own ID, which a libsrt caller does not do. Both now follow libsrt, and the sender's socket ID is taken from its
  conclusion. Tested against FFmpeg 9.0.1's libsrt in both roles, with and without encryption at each key length, and
  with packet loss; `interop_with_ffmpeg_libsrt` is an ignored test that needs FFmpeg with libsrt.

## [0.1.2] - 2026-11-03

### Added

- **`voice::Vad` learns steady noise:** a level that holds still for 400 ms (a standard deviation of at most 3 dB) becomes the
  noise floor, so a fan louder than the -50 dBFS starting floor stops reading as speech within half a second instead of rising
  out of it at 1 dB a second; speech over it is still found. `Vad::with_floor` starts from a known floor, such as the one
  `levels()` gave at the end of the last push-to-talk press. `Vad::measurable()` says whether a frame is speech to take a
  level from: speech is under way, the frame itself stands above the floor (not the hangover's quiet tail), and the detector
  has had those 400 ms. `Agc` relaxes a gain above 0 dB toward it over frames that are not speech (0.5% a frame). Fed
  `measurable()`, a fresh detector no longer lets `Agc` raise a steady fan at the start of a press and keep it raised.
- **macOS capture devices:** `device://video` and `device://audio` on macOS, through AVFoundation (an `AVCaptureSession` with video and audio data outputs, written with the `objc2` crates): `ironav devices` and the `devices` tool list the cameras and microphones, `size=` and `fps=` pick the nearest camera mode, pictures arrive as Y4M converted from NV12 and sound as 16-bit WAV, and a refused Camera or Microphone permission is an error that names System Settings > Privacy & Security. It is compile-checked for `aarch64-apple-darwin` and `x86_64-apple-darwin` and its conversions are unit-tested, but it has not been run against a real camera or microphone.
- **SRT live source:** `srt://host:port` calls an SRT sender and `srt://:port?mode=listener` waits for one (live mode, receiver
  only, MPEG-TS payload read as `udp://` is). Written in tree with no new dependency: handshake v5, ACK and ACKACK, NAK loss
  reports with retransmission, timestamp-based delivery (`?latency=MS`, 120 by default) with too-late packet drop,
  keep-alive and shutdown, and `?streamid=`. `?passphrase=` reads an AES-CTR encrypted stream (key material, PBKDF2-HMAC-SHA1
  and the RFC 3394 key wrap; the passphrase is redacted from everything shown). A caller reconnects with back-off like RTSP,
  addresses go through `IRONAV_NET_ALLOW`, and without a passphrase it is a plaintext transport that FIPS mode refuses.
- **WebRTC ingest (WHIP, RFC 9725):** a `whip://[host]:port[/path]` live source that OBS, FFmpeg, GStreamer or a browser
  publishes H.264 and Opus to. It is written in-tree with no async runtime: an HTTP/1.1 endpoint for the offer, answer and
  DELETE; ICE-lite (STUN with MESSAGE-INTEGRITY and FINGERPRINT); a DTLS 1.2 server (ECDHE-ECDSA P-256 with AES-128-GCM,
  `use_srtp`, cookies, retransmission and reassembly, and the publisher's certificate checked against its offer);
  SRTP and SRTCP (AES-128-GCM and AES-128-CM-HMAC-SHA1-80) with replay protection; receiver reports, NACK and PLI. The
  packets reach the session as an RTSP camera's do. It listens on loopback unless a host is given, takes other publishers
  only from addresses `IRONAV_NET_ALLOW` admits, and `?token=` requires a bearer token, compared in constant time. FIPS
  mode keeps it, with the AES-GCM SRTP profile only. Works with `ironav watch`, `stream_open` and the MCP server.

### Fixed

- **Lost UDP datagrams on Windows:** `udp://`, ONVIF discovery and the new SRT and WHIP sources waited for
  datagrams with a short socket receive timeout, and on Windows a datagram that arrives just as such a timeout runs out
  can be lost (a few in a thousand with sparse traffic). They now wait with `WSAPoll` and then read.

## [0.1.1] - 2026-11-02

### Added

- **Codec features:** `ironav-codec` puts each codec behind its own cargo feature, with `voice` and `resample` for the
  voice-link DSP and the resampler. `default-features = false` with `features = ["opus", "voice"]` builds only those,
  without serde. `ironav-core`'s serde support is an optional feature too, on by default.

### Security

- **Security (FIPS):** builds with the `fips` feature of `ironav-live` compile out the MD5 Digest code instead of only refusing it at run time; an MD5 challenge is still refused, and SHA-256 Digest is unchanged.
- **Sidecar resource limits:** a sidecar entry may set `max_memory_mb` (the job object's committed memory on Windows, `RLIMIT_AS` on Linux and macOS) and `max_cpu_seconds` (the job's user-mode CPU time on Windows, `RLIMIT_CPU` on Linux and macOS). Both are unset by default, so nothing changes until an operator sets them. Invalid values are refused when the list is read, and a child whose limits cannot be applied is not started. This addresses the CWE-400 residual risk in `docs/security/compliance.md`, section 8. See "Resource limits" in `docs/sidecars.md`.

### Fixed

- **Dependencies:** `async-executor` 1.14.0 was yanked; the lockfile uses 1.13.3 (only the GUI's Linux accessibility stack uses it).
- **Tests:** a sidecar test tells whether a process is alive on macOS, which has no `/proc`; a reconnect test no longer fails when a stall recovers on a loaded machine.

## [0.1.0] - 2026-11-02

### Added

- **Codecs (decoders):** multichannel Opus (channel-mapping families 1 and 255: several mono and coupled-stereo streams in one packet, self-delimited as RFC 6716 Appendix B, routed by the mapping table; family 1 in WAV and FFmpeg channel order), read from Ogg, Matroska, WebM and MP4 (`dOps`); software decoders for H.264 (8- and 10-bit, 4:2:0, 4:2:2, 4:4:4 and monochrome,
  CAVLC and CABAC P and B slices, lossless, interlaced MBAFF frames, several pictures decoded at once), HEVC (Main, Main 10, 4:2:2, 4:4:4, 12-bit), VP8, VP9
  (profiles 0-3, 8, 10 and 12 bits in 4:2:0, 4:2:2, 4:4:4 and 4:4:0, tile columns and a multi-threaded loop filter), baseline, progressive and lossless JPEG (lossy RGB files, with an Adobe transform 0 or `R` `G` `B` component ids, decode to RGB rather than YCbCr) and
  Motion-JPEG, AAC-LC with HE-AAC v1, MP3 (MPEG-1/2/2.5 Layer III), MP2 (MPEG-1/2 Layer II, as the `mp2` codec), Vorbis, Opus (SILK, CELT and hybrid)
  FLAC (4 to 32 bits) and ALAC (16, 20 and 24 bits, and 32-bit mono; one to eight channels).
- **Codecs (encoders):** software HEVC (Main profile, 8-bit 4:2:0: 64x64 coding tree units, CABAC, intra and P pictures with merge and skip, AMVP motion, quarter-sample search, deblocking, rate control by QP or bit rate, and B pictures as an option (`HevcEncoder::with_bframes`, off by default, with a quantiser cascade); MP4 and Matroska), software H.264 (Constrained Baseline, then Main with CABAC and High with the 8x8
  transform, B pictures, rate-distortion mode decisions, P pictures predicting from up to three references (`with_refs`, `--refs`; three by default), 10-bit `yuv420p10le` in the High 10 profile, and sub-8x8 partitions as an option (`with_sub8x8`, off by default: at most about 1 percent fewer bytes, for 8 to 30 percent more encoding time, at QP 20 and below)), VP8 (key and inter frames, quarter-pixel motion, rate control), VP9 (key and inter frames with coefficient probability updates, profile 0, WebM, Matroska and MP4), AAC-LC with an ADTS writer (one to six and eight channels, 5.1 and 7.1 included), MP3 Layer III with
  an MP3 writer, Vorbis, Opus (CELT, wideband SILK, hybrid, DTX, in-band FEC, automatic mode switching,
  2.5 to 60 ms frames, and 3 to 8 channels in channel-mapping family 1 with the RFC 7845 stream layouts; the CLI and job JSON select the mode, signal, frame size, FEC and DTX), JPEG and Motion-JPEG, FLAC, and
  ALAC (16 and 24 bits, one to eight channels). `--keyint N` (or `keyint` in a job) sets the key frame interval of H.264, HEVC, VP8, VP9, AV1 and MPEG-4.
- **Encoder options:** `--bframes N` (`bframes` in a job's `video`, 0 to 3) sets the B pictures of the software HEVC and MPEG-4 encoders; `--tiles COLSxROWS`, `--quarter-mv` and `--altref LAG` set the tile grid, quarter-sample motion search and hidden ALTREF frames of the AV1 encoder. Each is refused by name for the codecs that do not take it, and out-of-range values are refused with a hint.
- **Codecs (AC-3):** AC-3 (Dolby Digital, ATSC A/52) decoder, a translation of FFmpeg's `ac3dec.c`: all channel modes with LFE, coupling, rematrixing, delta bit allocation and block switching, at FFmpeg's default dynamic range. Read from raw `.ac3`, Matroska (`A_AC3`), MPEG-TS (stream types 0x81 and 0x06 with the AC-3 descriptor) and MP4 (`ac-3`). Mono, stereo and 5.1 streams match FFmpeg's float decoder to 2.5e-5 (SNR 102 dB or better), and the containers to 4.5e-5; tests in `crates/ironav-codec/tests/ac3_ffmpeg.rs` and `crates/ironav-formats/tests/ac3_containers.rs`.
- **Codecs (AC-3 encoder):** AC-3 encoder, a translation of FFmpeg's float encoder (`ac3enc.c`, `ac3enc_float.c`): mono to 3/2 with and without LFE, 32, 44.1 and 48 kHz, 32 to 640 kbit/s (`--acodec ac3`), with FFmpeg's defaults. Written to raw `.ac3`, Matroska (`A_AC3`), MP4 (`ac-3` with `dac3`, the same bytes as FFmpeg's) and MPEG-TS (stream type 0x81 with the AC-3 descriptor). Decoded by FFmpeg and IronAV to the same audio as FFmpeg's own stream, to within 0.1 dB of its SNR; tests in `crates/ironav-engine/tests/ac3_encode.rs` and `crates/ironav-formats/tests/ac3_containers.rs`.
- **Codecs (MP2 encoder):** MPEG-1 Layer II encoder, a translation of FFmpeg's `mpegaudioenc.c`: mono or stereo at 32, 44.1 or 48 kHz, 32 to 384 kbit/s (`--acodec mp2`, default 96 per channel), written to `.mp2`, Matroska, MP4 (`mp4a`) and MPEG-TS. The frames match FFmpeg's byte for byte for the same 16-bit input, and FFmpeg decodes them to within 0.1 dB of the SNR its own stream gives (23 dB at 192 kb/s stereo, 9 dB at 64 kb/s stereo at 32 kHz); tests in `crates/ironav-engine/tests/mp2_encode.rs`.
- **Codecs (voice link):** a playout buffer, a streaming resampler, speech detection with push-to-talk
  trimming, automatic gain control, noise suppression, and Opus packet-loss concealment.
- **Codecs (ProRes):** Apple ProRes decoder and encoder in MOV: 4:2:2 at 10 bits (Proxy, LT, Standard, HQ) and 4:4:4 at
  12 bits (4444, 4444 XQ), progressive only. Decoding matches FFmpeg's bit for bit; `--vcodec prores` writes HQ, and
  `--prores-profile` (or `prores_profile` in a job) picks any of the six, keeping 4:4:4 chroma under 4444 and 4444 XQ.
- **Codecs (MPEG-1 and MPEG-2 video):** software decoder for MPEG-1 and MPEG-2 video (`mpeg1video`, `mpeg2video`), a translation of FFmpeg's `mpeg12dec.c`: I, P and B pictures, frame and field pictures, field and frame DCT, field, 16x8 and dual-prime motion, the alternate scan, non-linear quantiser scale, custom matrices, 4:2:0 and 4:2:2, MPEG-1 full-pel vectors. Read from raw elementary streams (`.m2v`, `.mpv`, `.m1v`), MPEG program streams (`.mpg`, `.mpeg`, `.vob`, `.ps`), MPEG-TS, Matroska and MP4. The output matches FFmpeg's byte for byte; tests in `crates/ironav-engine/tests/mpeg2.rs`, with streams from `tools/gen_mpeg2_synth.py`. Not decoded: 4:4:4 and the scalable profiles. The MPEG-2 encoder is listed below; MPEG-1 video is decode only, and is not written to output containers.
- **Codecs (MPEG program streams):** every video stream (0xE0-0xEF) and the audio of a program stream is read: MP2 and MP3 (0xC0-0xDF, labelled by each frame's layer), AC-3 and DVD linear PCM from private stream 1 (0x80-0x87 and 0xA0-0xA7; LPCM at 16, 20 and 24 bits, converted to little-endian). Times are the PES presentation timestamps on the 90 kHz clock, every stream starting at the earliest, so audio and video stay aligned. Checked against FFmpeg's `mpeg`, `vob` and `pcm_dvd` output: the stream list is ffprobe's, MP2 decodes to within 1 LSB, AC-3 within 5e-5 and LPCM exactly, and a transcode to MP4 starts video and audio within a frame. FFmpeg writes LPCM at 16 and 24 bits only, so 20-bit follows its decoder source.
- **Codecs (MPEG-4 Part 2):** software decoder for Simple and Advanced Simple profile video in MP4 (`mp4v`) and
  Matroska (`V_MPEG4/*`): I, P and B pictures, half- and quarter-sample motion, four vectors, direct mode, H.263 and
  MPEG quantisation with custom matrices, resync markers. Decoding matches FFmpeg's bit for bit. Interlaced, sprite
  and global-motion streams, and data partitioning, are refused with a clear error. About 90 frames a second at 720p.
- **Codecs (MPEG-4 Part 2 encoder):** software Simple profile encoder for I and P pictures, with half-sample motion search, four vectors per macroblock, intra DC and AC prediction, H.263 quantisation and the escape codes, at a fixed quantiser (1 to 31) or a bit rate, and `--keyint` for the key picture interval. Written to MP4 (`mp4v`) and Matroska (`V_MPEG4/ISO/SP`), and decoded by FFmpeg to exactly what IronAV decodes. B pictures are an option (`with_bframes`, off by default, Advanced Simple profile: forward, backward, bidirectional and direct macroblocks, with the anchors coded ahead; B pictures at a quantiser 1.25 times the anchors' plus 1.25, as FFmpeg's default; about 3 to 9 percent fewer bytes than without them at equal PSNR on 352x288 testsrc2 and 6 to 15 percent on mandelbrot, one B picture between anchors), and quarter-sample motion and interlaced video are not encoded. The decoder's motion-vector predictor now takes the above-right candidate for the first block as FFmpeg does, which corrects decoding of some streams.
- **Codecs (MPEG-2 video encoder):** software encoder for MPEG-2 video (`mpeg2video`): Main profile, 4:2:0, progressive frame pictures, I and P pictures, with B pictures between anchors (`--bframes N`, off by default); `--qp` 1 to 31, `--vbitrate` rate control and `--keyint`. Written to raw `.m2v`, MPEG-TS (stream type 0x02), Matroska (`V_MPEG2`) and MP4 (object type 0x61). FFmpeg decodes the stream to exactly IronAV's reconstruction; tests in `crates/ironav-engine/tests/mpeg2_enc.rs`. The motion search never uses a vector that reads outside the picture, where FFmpeg and IronAV disagree.
- **Codecs (AV1 decoder):** software decoder for key, intra-only and inter frames, at 8, 10 and 12 bits in 4:2:0, 4:2:2 and 4:4:4, and at 8 bits as RGB (`rgb24`) and monochrome. It covers every intra prediction tool, palettes, all transforms, lossless
  coding, tiles, intra block copy, the deblocking filter, CDEF, loop restoration (Wiener, self-guided and switchable),
  quantiser matrices and film grain (added to the output only), and every inter tool: compound and skip-mode prediction,
  the motion vector stack and projected motion field, dual filters, global and local warped motion, OBMC, wedge,
  difference- and distance-weighted compound, interintra, variable inter transforms, hidden frames, `show_existing_frame`
  and film grain loaded from a reference. It matches libaom bit for bit on key-frame-only, P-only and alt-ref streams at
  `-cpu-used` 1 to 8, and is used when no NVIDIA GPU decodes AV1. Superres is decoded (upscaling after CDEF and before loop
  restoration), and matches libaom bit for bit on IronAV-encoded superres streams at 1/9 to 1/16 of the width. The post filters run on threads, and so do the tiles of a tile group (not frames with intra block copy): at 1080p with 4x2 tiles, 45 against 24 frames a second, bit-identical to the serial decode.
- **Codecs (AV1 encoder):** profile 0, 8-bit 4:2:0, in WebM, Matroska and MP4, `--qp` or an average bit rate (`--vbitrate`; see below). It writes inter
  frames after each key frame (every 250 frames by default, `--keyint`): prediction from LAST, LAST2 or LAST3 (the three frames
  before, each block by rate-distortion cost) with quarter-sample vectors
  searched to half samples, the reference MV stack's modes, skip and intra blocks inside inter frames, deblocking and CDEF
  on every frame, and rate-distortion mode decisions from the CDF bit costs. It runs the decoder's own reconstruction, so
  the decoder's pictures equal the encoder's references bit for bit, and libaom decodes the same pictures. About 5 frames a
  second at 720p with LAST_FRAME alone (testsrc2, 60 frames, qp 20 to 40), up from about 1.6: the open-loop planner and the motion search are
  cheaper, and the transform bases are built once per stream. Files are 1 to 3.5 % smaller. Three references against one
  (60 frames, 720p, qp 20/30/40): testsrc2 is 2.0, 3.0 and 7.5 % smaller at 0.06, 0.07 and 0.15 dB higher PSNR; mandelbrot is
  5.0 to 5.1 % smaller at 0.05 to 0.08 dB higher. Encoding takes about 40 % longer (about 3 frames a second against 5).
- **Codecs (AV1 encoder, compound):** a block may average LAST with LAST2 or LAST3 (`with_compound`, on by default; the frame
  header sets reference_select). Its modes are NEAREST_NEARESTMV, NEAR_NEARMV and NEW_NEWMV, by cost with the others. Against
  the three references alone (60 frames, 720p, qp 20/30/40): testsrc2 is 4.4, 10.6 and 17.6 % smaller at 0.04, 0.20 and 0.34 dB
  higher PSNR; mandelbrot is 8.6, 16.7 and 33.3 % smaller at 0.13, 0.34 and 0.76 dB higher. Encoding is about half the speed
  (about 1 to 2 frames a second).
- **Codecs (AV1 encoder, hidden ALTREF):** `with_altref(lag)` is an encoder option (not a command-line option), off by default.
  Every `lag` frames the last of a group is coded first as a hidden frame (`show_frame` 0, in a reference slot of its own), the
  frames before it predict from it singly or bidirectionally with LAST_FRAME (compound, one forward and one backward
  reference), and it is shown later by a show_existing_frame header in its own temporal unit. Frames carry order hints (7 bits),
  which the default stream now has too: about one byte a frame. Against no ALTREF (60 frames, 720p, qp 20/30/40): testsrc2 is
  6.6, 8.8 and 15.1 % larger at lag 8 (14.3, 17.8 and 31.0 % at lag 4) for 0.23, 0.30 and 0.38 dB more PSNR; mandelbrot is 0.3,
  3.3 and 12.2 % larger at lag 8 (1.1, 7.3 and 22.3 % at lag 4) for 0.39, 0.54 and 0.49 dB more. That is not a gain at equal
  PSNR, so it stays an option. libaom decodes the streams to IronAV's pictures, in display order with the frames' times
  (`altref_streams_decode_identically_with_libaom_and_ironav` in `crates/ironav-engine/tests/av1_enc.rs`).
- **Containers:** WAV, FLAC, Y4M, MP4, QuickTime MOV, Matroska and WebM (read and write), Ogg (Opus, Vorbis and FLAC),
  ADTS, MP3, JPEG, and MPEG transport streams (read and write), and raw H.264 and HEVC (Annex B: read and written as files, and read as live sources; a file's frame rate is its SPS's VUI timing, else 25; presentation times come from picture order, so B-frame video copies into MP4 with the right times, and into Matroska from 0 (the reordering lead-in comes off); the pixel format comes from the SPS, 4:2:0, 4:2:2 and 4:4:4 at 8, 10 and 12 bits).
- **Containers (MPEG-TS Opus):** Opus is read and written in MPEG transport streams as ETSI TS 102 366 has it (as FFmpeg writes it), in mono, stereo and 3 to 8 channels, with the pre-skip and end trims in each packet's control header. What IronAV writes decodes in FFmpeg to IronAV's decode of the source, after the pre-skip, which FFmpeg's decoder keeps.
- **Containers (MP4):** fragmented MP4 is read as it arrives, and written with `fragment_seconds` in a job (`--fragment-seconds S`
  on the CLI): the index first, with no samples in it, then a `moof` and `mdat` for each fragment, cut at a key frame once
  it is at least S seconds long (at any audio packet in a file with no video). Composition offsets are signed, as in
  `trun` version 1. FFmpeg decodes a fragmented copy to the same pictures and samples as the plain copy. The writer is
  constant-frame-rate and writes fast-start files otherwise; B-frames are copied with `ctts` and an edit list; ALAC audio is read and written
  in the `alac` sample entry, in MP4 and MOV.
- **Containers (QuickTime):** MOV is read as MP4 is, including QuickTime sound entries (versions 1 and 2, `wave`
  atoms, and PCM in `twos`, `sowt`, `in24`, `in32`, `fl32`, `fl64`, `raw ` and `lpcm` form) and files with no `ftyp`;
  written with a `qt  ` brand, H.264, HEVC, ProRes, Motion-JPEG, AAC, ALAC and PCM. FFmpeg decodes the PCM bit-exact and reads the copied
  packets the same as IronAV does.
- **Containers (Matroska content encodings):** header stripping (`ContentCompAlgo` 3) and zlib (0) are undone on read, frame by frame.
  Each alone is checked against FFmpeg's reading of files written by hand (FFmpeg refuses two encodings on one track, which
  IronAV reads in the order the specification gives); no writer produces encodings.
- **Containers (Matroska PCM):** PCM is read and written in Matroska (`A_PCM/INT/LIT`, `A_PCM/INT/BIG` and `A_PCM/FLOAT/IEEE`, 8 to 64 bits):
  FFmpeg's files decode to the same bytes IronAV reads, and FFmpeg reads IronAV's copies to the same bytes.
- **Containers (Ogg streams):** every logical stream of an Ogg file is read. Multiplexed Opus, Vorbis or FLAC streams (serial numbers
  that share the file in time) are separate tracks; chained Vorbis or FLAC links with the same setup are one track on one timeline.
  Chained Opus links stay separate tracks, each with its own pre-skip. The unit tests cover multiplexed Opus and chained FLAC on
  synthetic files; joined Vorbis links have no FFmpeg-checked file yet.
- **Containers (copy):** VP9 and AV1 are copied between WebM, Matroska and MP4; AAC keeps its encoder
  delay in MP4; Opus ends where the source did in Matroska (DiscardPadding) and Ogg. MP2 and MP3 written by IronAV carry their
  encoder delay as CodecDelay in Matroska, which FFmpeg trims (its decoding lines up with the input, as IronAV's does). An MP4 with
  MPEG audio is labelled MP2 when its frames are Layer II (object type 0x69 or 0x6B), not MP3.
- **Live input and streaming (HTTP authentication):** Digest (MD5, MD5-sess, SHA-256 and SHA-256-sess, RFC 7616, and RFC 2069 forms; SHA-256 is preferred) and
  Basic credentials in http(s) URLs, answered per host so that later segments need no extra round trip.
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

### Security

- **Security (agent tools):** release builds unwind and every tool call and live session worker contains a panic as an `internal` error; `IRONAV_ROOTS` (default: the working directory in `ironav mcp`), `IRONAV_READ_ONLY`, `IRONAV_ALLOW_DEVICES` and `IRONAV_ALLOW_COOKIES` limit paths, writes, cameras and browser cookies; `IRONAV_MAX_PIXELS` and a 64-channel cap bound decoders; `IRONAV_AUDIT_LOG` writes an audit trail; text from media is cleaned, cut and marked `"untrusted"`; `<output>.partial` is created exclusively; yt-dlp and arecord resolve to absolute paths and sidecars get a minimal environment; MCP request lines are capped at 4 MiB and tool calls in flight at 4; MCP tool annotations are truthful. New error code `permission-denied`. See "Security and operator settings" in the README.
- **Security (network and FIPS):** no outbound connection is allowed by default; `IRONAV_NET_ALLOW` takes `private`, `loopback`, `public`, `any`, addresses and CIDR ranges, and cloud metadata endpoints also need `IRONAV_NET_ALLOW_METADATA=1`. FIPS mode (`IRONAV_FIPS=1` or the `fips` feature) restricts TLS to AES-GCM, P-256/P-384 and ECDSA/RSA-PSS, and refuses plaintext transports, YouTube sources and MD5 Digest; `ironav capabilities` reports it. IronAV's cryptography is not CMVP-validated. Basic credentials are refused over plaintext unless `IRONAV_ALLOW_PLAINTEXT_BASIC=1`; NVIDIA driver DLLs load from System32 only; NVDEC display rectangles are checked.
- **Security (web UI):** the token, Digest client nonce and ONVIF nonce come from a CTR_DRBG; the token is compared in constant time and no longer appears on a command line; Host, Origin and token are checked before the body is read; at most 32 connections, 4 running and 64 finished jobs; a 15 s write timeout; a CSP script nonce; the file browser lists only `IRONAV_ROOTS`.
- **Security (parsers):** a mutation fuzz harness (`crates/ironav-engine/tests/fuzz_inputs.rs`) runs over the demuxers and decoders; Y4M headers, MPEG-4 Part 2 escapes, HEVC picture sizes and WAV `fmt ` chunks it broke are now refused cleanly. See [SECURITY.md](SECURITY.md) and [docs/security/compliance.md](docs/security/compliance.md).

### Changed

- **Network default:** loopback and private-network sources (LAN cameras, local servers) now need `IRONAV_NET_ALLOW=loopback` or `private`; before, they were allowed by default.
- **MPEG-2 in MP4 and Matroska:** a 4:2:2 stream is declared `yuv422p` (it was declared 4:2:0; the decoded frames were already right). FFmpeg writes no sequence header into those containers, so the chroma format is read from the first picture's sequence header and extension.
- **Opus stereo CELT** decodes to within 1 LSB of libopus at 2.5 to 20 ms and 32 to 256 kb/s, as mono does. The decoder drew
  the anti-collapse noise channel by channel; libopus draws it band by band, both channels in turn (FFmpeg's native decoder
  still draws it channel by channel, so it differs from libopus on such streams). The stereo hybrid's CELT part follows.
- **Opus SILK and hybrid decoding** brings SILK's 8, 12 and 16 kHz output to 48 kHz with libopus's own resampler (16-bit
  fixed point, translated from libopus's `silk/resampler*.c`), so SILK and hybrid frames line up with libopus's decode sample
  for sample. The float filter it replaces ran one or two samples early and differed from libopus by 25 to 35 dB. Mono SILK
  and hybrid now differ from libopus by a mean of 1 to 7 LSB on speech; stereo SILK by 1.5 to 4.6 LSB on speech, and by up to
  3267 LSB on two tones of different pitch (wide band, 40 and 60 ms); stereo hybrid is off on speech by up to 117 LSB on the test asset (see the README). The SILK pre-skip the encoder writes follows the new delay: SILK files IronAV wrote
  before this change decode one sample later than their source now.
- **Opus SILK tests** compare with libopus (`ffmpeg -c:a libopus`), not FFmpeg's native decoder, which is itself a few
  samples late of libopus in SILK; a multichannel SILK test (4, 6 and 8 channels) is added.

### Known limitations

- **Hardware:** only NVIDIA is driven: NVENC for H.264 and HEVC, NVDEC for H.264, HEVC and AV1. The other backends
  (VA-API, Quick Sync, VideoToolbox, D3D11VA, AMF, Vulkan Video, MediaCodec and OpenCL) are planned.
- **AV1 encoder:** no joint compound (COMPOUND_DISTANCE), no motion field projection and no skip mode; hidden ALTREF frames are an
  encoder option (`with_altref`), off by default; 8-bit 4:2:0; tile columns and rows (`with_tiles`, two columns by default from 1280 pixels wide, each tile coded on its own thread; 0.2 to 2.8 % more bytes, 1.2 to 3.0 times as fast at 720p); the motion search stops at half samples unless `with_quarter_mv(true)` adds a quarter-sample step (under 1 % fewer bytes for 18 to 112 % more time at 720p, so off by default). Rate control is an average bit rate only: no maximum rate or buffer size (`--maxrate`,
  `--bufsize` are not options of any encoder).
- **AV1 decoder:** RGB and monochrome above 8 bits are refused, as there is no output format for them.
- **HEVC encoder:** Main profile, 8-bit 4:2:0 only, with no sign data hiding; B pictures are an option (`with_bframes`), off by default: with a quantiser cascade they save about 12 % on the mandelbrot clip at equal PSNR, but only about 3 % on `testsrc2` (see the HEVC section of the README); 10-bit HEVC encoding needs an NVIDIA GPU (NVENC).
- **HEVC decoder:** the range extension coding tools (RDPCM, cross-component prediction, extended precision, persistent
  Rice adaptation and the rest) are refused by name, as are unequal luma and chroma bit depths (10-bit luma with 9-bit
  chroma, for instance).
- **H.264:** the decoder refuses field pictures (PAFF; MBAFF frames are decoded), slice groups, separate colour planes,
  SP and SI slices and bit depths above 10.
- **VP9:** reference frames of another size (spatial resampling) are refused; scaled motion compensation is not implemented,
  because no available encoder makes such a stream to test it against.
- **MPEG-1 and MPEG-2 video:** 4:4:4 (High profile), the scalable profiles and data partitioning are refused; concealment
  vectors are parsed and ignored; damaged slices are skipped, not concealed, so a damaged stream can differ from FFmpeg's
  decode; in program streams, DTS, subpictures and DVD-Audio are skipped, and a stream that first appears more than a few megabytes in is not read; MPEG-1 video is decoded but not encoded; MPEG-2 video is encoded only as progressive
  4:2:0 frame pictures; coefficients outside the range of real streams follow FFmpeg's C transform, not its SIMD one.
- **MPEG-4 Part 2:** interlaced, sprite and global-motion pictures, data partitioning and scalable layers are refused;
  the encoder writes Simple profile I and P pictures, and B pictures as an option (`with_bframes`, Advanced Simple profile), but not quarter-sample motion or interlaced video.
- **AAC:** HE-AAC v2 (parametric stereo) is refused, as are Main prediction, LTP, gain control, coupling channels, 960-sample frames, SBR that is only signalled implicitly and
  program-config layouts. The encoder writes one to six and eight channels; seven has no standard layout and is refused.
- **Opus and Vorbis:** Opus is encoded in mono, stereo and 3 to 8 channels (channel-mapping family 1); 9 or more channels,
  and family 255 streams, are refused by the encoder (the decoder reads both families). Vorbis floor type 0 is refused
  (no encoder writes it, and no local file has it to test with), and streams over eight channels are refused. The Vorbis
  encoder writes one to eight channels; only a stereo pair is coupled, so three or more channels are coded one by one.
- **MP3, MP2, JPEG, ProRes:** MPEG audio Layer I is refused (no encoder makes a test stream); MP2 joint-stereo (intensity) streams
  decode but have no FFmpeg reference; JPEG arithmetic-coded and 12-bit files are refused, and so are four-component CMYK and YCCK files and progressive or lossless files over 67 megapixels (lossless JPEG decodes, but not with restart intervals);
  ProRes interlaced pictures are refused.
- **AC-3 encoder:** frames are not byte-identical to FFmpeg's (the MDCT and quantiser round a few coefficients differently), though
  the decoded quality matches FFmpeg's to within 0.1 dB. Dual mono is not written. FFmpeg's raw-file probe does not identify
  IronAV's 48 kHz 5.1 stream at 640 kb/s (it decodes when the format is given); the cause is not isolated.
- **Containers:** Matroska tracks with encryption, bzip2 or lzo content encodings are refused (header stripping and zlib are read;
  the live reader refuses any content encoding); E-AC-3 is refused; Ogg refuses files
  holding only Speex or Theora (other streams in a file are passed over).
- **Live:** servers that speak only TLS 1.2 cannot be reached; cameras and microphones are read on Windows and Linux
  only (no AVFoundation).
- Sound output through a real device has been built but not verified by listening or measurement.
- IronAV is not yet a drop-in replacement for FFmpeg or HandBrake.
