# Changelog

All notable changes to IronAV are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - Unreleased

### Added

- **Codecs (decoders):** multichannel Opus (channel-mapping families 1 and 255: several mono and coupled-stereo streams in one packet, self-delimited as RFC 6716 Appendix B, routed by the mapping table; family 1 in WAV and FFmpeg channel order), read from Ogg, Matroska, WebM and MP4 (`dOps`); software decoders for H.264 (8- and 10-bit, 4:2:0, 4:2:2, 4:4:4 and monochrome,
  CAVLC and CABAC P and B slices, lossless, several pictures decoded at once), HEVC (Main, Main 10, 4:2:2, 4:4:4, 12-bit), VP8, VP9
  (profiles 0-3, tile columns and a multi-threaded loop filter), baseline and progressive JPEG and
  Motion-JPEG, AAC-LC with HE-AAC v1, MP3 (MPEG-1/2/2.5 Layer III), Vorbis, Opus (SILK, CELT and hybrid)
  FLAC (4 to 32 bits) and ALAC (16, 20 and 24 bits, and 32-bit mono; one to eight channels).
- **Codecs (encoders):** software HEVC (Main profile, 8-bit 4:2:0: 64x64 coding tree units, CABAC, intra and P pictures with merge and skip, AMVP motion, quarter-sample search, deblocking, rate control by QP or bit rate; MP4 and Matroska), software H.264 (Constrained Baseline, then Main with CABAC and High with the 8x8
  transform, B pictures, rate-distortion mode decisions), VP8 (key and inter frames, quarter-pixel motion, rate control), VP9 (key and inter frames with coefficient probability updates, profile 0, WebM, Matroska and MP4), AAC-LC with an ADTS writer (one to six and eight channels, 5.1 and 7.1 included), MP3 Layer III with
  an MP3 writer, Vorbis, Opus (CELT, wideband SILK, hybrid, DTX, in-band FEC, automatic mode switching,
  2.5 to 60 ms frames, and 3 to 8 channels in channel-mapping family 1 with the RFC 7845 stream layouts; the CLI and job JSON select the mode, signal, frame size, FEC and DTX), JPEG and Motion-JPEG, FLAC, and
  ALAC (16 and 24 bits, one to eight channels). `--keyint N` (or `keyint` in a job) sets the key frame interval of H.264, HEVC, VP8, VP9 and AV1.
- **Codecs (voice link):** a playout buffer, a streaming resampler, speech detection with push-to-talk
  trimming, automatic gain control, noise suppression, and Opus packet-loss concealment.
- **Codecs (ProRes):** Apple ProRes decoder and encoder in MOV: 4:2:2 at 10 bits (Proxy, LT, Standard, HQ) and 4:4:4 at
  12 bits (4444, 4444 XQ), progressive only. Decoding matches FFmpeg's bit for bit; `--vcodec prores` writes HQ, and
  `--prores-profile` (or `prores_profile` in a job) picks any of the six, keeping 4:4:4 chroma under 4444 and 4444 XQ.
- **Codecs (MPEG-4 Part 2):** software decoder for Simple and Advanced Simple profile video in MP4 (`mp4v`) and
  Matroska (`V_MPEG4/*`): I, P and B pictures, half- and quarter-sample motion, four vectors, direct mode, H.263 and
  MPEG quantisation with custom matrices, resync markers. Decoding matches FFmpeg's bit for bit. Interlaced, sprite
  and global-motion streams, and data partitioning, are refused with a clear error. About 90 frames a second at 720p.
- **Codecs (AV1 decoder):** software decoder for key, intra-only and inter frames, at 8, 10 and 12 bits in every chroma
  layout (4:2:0, 4:2:2, 4:4:4 and monochrome). It covers every intra prediction tool, palettes, all transforms, lossless
  coding, tiles, intra block copy, the deblocking filter, CDEF, loop restoration (Wiener, self-guided and switchable),
  quantiser matrices and film grain (added to the output only), and every inter tool: compound and skip-mode prediction,
  the motion vector stack and projected motion field, dual filters, global and local warped motion, OBMC, wedge,
  difference- and distance-weighted compound, interintra, variable inter transforms, hidden frames, `show_existing_frame`
  and film grain loaded from a reference. It matches libaom bit for bit on key-frame-only, P-only and alt-ref streams at
  `-cpu-used` 1 to 8, and is used when no NVIDIA GPU decodes AV1. Superres is decoded (upscaling after CDEF and before loop
  restoration), and matches libaom bit for bit on IronAV-encoded superres streams at 1/9 to 1/16 of the width.
- **Codecs (AV1 encoder):** profile 0, 8-bit 4:2:0, in WebM, Matroska and MP4, `--qp` only (no bit rate). It writes inter
  frames after each key frame (every 250 frames by default, `--keyint`): LAST_FRAME prediction with quarter-sample vectors
  searched to half samples, the reference MV stack's modes, skip and intra blocks inside inter frames, deblocking and CDEF
  on every frame, and rate-distortion mode decisions from the CDF bit costs. It runs the decoder's own reconstruction, so
  the decoder's pictures equal the encoder's references bit for bit, and libaom decodes the same pictures. About 5 frames a
  second at 720p (testsrc2, 60 frames, qp 20 to 40), up from about 1.6: the open-loop planner and the motion search are
  cheaper, and the transform bases are built once per stream. Files are 1 to 3.5 % smaller.
- **Containers:** WAV, FLAC, Y4M, MP4, QuickTime MOV, Matroska and WebM (read and write), Ogg (Opus, Vorbis and FLAC),
  ADTS, MP3, JPEG, and MPEG transport streams (read and write), and raw H.264 and HEVC (Annex B, write only).
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
  delay in MP4; Opus ends where the source did in Matroska (DiscardPadding) and Ogg.
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

### Known limitations

- **Hardware:** only NVIDIA is driven: NVENC for H.264 and HEVC, NVDEC for H.264, HEVC and AV1. The other backends
  (VA-API, Quick Sync, VideoToolbox, D3D11VA, AMF, Vulkan Video, MediaCodec and OpenCL) are planned.
- **AV1 encoder:** one reference (the previous frame), no compound prediction, no rate control by bit rate (`--qp`
  only), 8-bit 4:2:0 and one tile; the motion search stops at half samples.
- **HEVC encoder:** Main profile, 8-bit 4:2:0 only, with no sign data hiding and no B pictures; wider HEVC encoding needs
  an NVIDIA GPU.
- **HEVC decoder:** the range extension coding tools (RDPCM, cross-component prediction, extended precision, persistent
  Rice adaptation and the rest) are refused by name, as are unequal luma and chroma bit depths (10-bit luma with 9-bit
  chroma, for instance).
- **H.264:** the decoder refuses interlaced coding (field pictures and MBAFF), slice groups, separate colour planes,
  SP and SI slices and bit depths above 10. The encoder has one reference picture, no partitions below 8x8 and no
  10-bit output.
- **VP9:** reference frames of another size (spatial resampling) are refused; scaled motion compensation is not implemented,
  because no available encoder makes such a stream to test it against. 12-bit and 4:4:0 streams decode (output `yuv420p12le`,
  `yuv440p`, `yuv440p10le` and so on), and 10-bit and 12-bit output is checked against FFmpeg's decoder.
- **MPEG-4 Part 2:** interlaced, sprite and global-motion pictures, data partitioning and scalable layers are refused;
  there is no encoder.
- **AAC:** HE-AAC v2 (parametric stereo) is refused, as are Main prediction, LTP, gain control, coupling channels and
  program-config layouts. The encoder writes one to six and eight channels; seven has no standard layout and is refused.
- **Opus and Vorbis:** Opus is encoded in mono, stereo and 3 to 8 channels (channel-mapping family 1); 9 or more channels,
  and family 255 streams, are refused by the encoder (the decoder reads both families). Vorbis floor type 0 and
  streams over eight channels are refused, and the Vorbis encoder writes at most two channels.
- **MP3, JPEG, ProRes:** MP3 layers I and II are refused; JPEG arithmetic-coded, 12-bit and lossless files are refused;
  ProRes interlaced pictures are refused.
- **Containers:** Matroska tracks with encryption, bzip2 or lzo content encodings are refused (header stripping and zlib are read;
  the live reader refuses any content encoding); MPEG-TS does not carry Opus,
  AC-3 or MPEG-2 video; raw H.264 and HEVC are write-only; Ogg refuses files
  holding only Speex or Theora (other streams in a file are passed over).
- **Live:** servers that speak only TLS 1.2 cannot be reached; cameras and microphones are read on Windows and Linux
  only (no AVFoundation).
- Sound output through a real device has been built but not verified by listening or measurement.
- IronAV is not yet a drop-in replacement for FFmpeg or HandBrake.
