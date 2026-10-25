# IronAV

Audio and video conversion in Rust, built to be driven by AI agents as well as people.
A CLI, an MCP server and a HandBrake-style GUI all sit on one tool registry, so they cannot drift apart.

> **Status: foundation (v0.1).** The architecture, agent interface, ontology, hardware model, CLI, MCP server
> and both GUIs are working and tested. The codec/container set is deliberately small so far:
> **WAV, FLAC and Y4M containers; PCM (8/16/24/32-bit, float), FLAC and raw-video codecs, and an AAC-LC decoder; `volume`, `gray`, `hflip` filters.**
> **H.264 and HEVC can be encoded and decoded on NVIDIA GPUs (NVENC/NVDEC)**, and MP4 files can be read and written.
> There is no software H.264/HEVC codec, so those need an NVIDIA GPU. MP4 can carry FLAC audio next to the video, and AAC
> audio copied from another MP4/M4A (AAC can be decoded but not encoded). Opus, AV1, MKV/WebM etc. are modelled in the
> ontology as `planned`.
> IronAV is not yet a drop-in replacement for FFmpeg or HandBrake.

## Interfaces

| Surface | Command | Use |
|---|---|---|
| CLI | `ironav convert -i in.wav -o out.wav --acodec pcm_f32le --filter volume:gain=0.5` | humans and scripts |
| GUI (native) | `ironav gui [file]` | HandBrake-style desktop window (egui): source, tabs, queue, drag-and-drop |
| Player (terminal) | `ironav play file` | full-screen TUI on a terminal; JSON lines on stdin/stdout otherwise, for agents |
| Player (window) | `ironav gui --play [file]` | native player window: seek bar, volume, snapshot, drag and drop |
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
Today there is one vendor: **NVIDIA, NVENC encode and NVDEC decode of H.264 and HEVC** (Windows and Linux). Every other
backend is detected and reported but not driven, and `select` says so per candidate. Where no provider applies,
`auto` falls back to software and `require` fails with `hardware-unavailable`.

**Where this has been run.** Windows 11 (RTX 3090 Ti) is where it was developed. Linux was tried once, in Debian under
WSL 2 with the same GPU: the workspace tests pass there (without the sound and window crates, which need system packages
that were not installed), and NVENC and NVDEC both work on real video. That run found and fixed two Linux-only bugs:
the NVDEC structs used 4-byte integers where the C header's `unsigned long` is 8 bytes on Linux (decoding silently
produced no pictures), and the driver search did not look in WSL's `/usr/lib/wsl/lib` or `LD_LIBRARY_PATH`. Native
Linux (not WSL), macOS, other GPU vendors, the sound output and the windows on any platform other than Windows have
not been run. A GitHub Actions workflow for Linux, macOS and Windows is in `.github/workflows`; it has not run yet
because the account's Actions billing refused to start it.

NVENC is loaded from the driver at run time (`nvEncodeAPI64.dll` / `libnvidia-encode.so.1`), so the same binary runs on
machines without NVIDIA hardware. It needs a driver that supports NVENC API 13.1 or newer and refuses older ones with a
clear error rather than guessing struct layouts. Input must be `yuv420p` (or `yuv420p10le` for HEVC, written as Main 10) with even dimensions; there are no
B-frames and no look-ahead, so output order equals input order and each frame yields a packet immediately.

```
ironav convert -i in.y4m -o out.h264 --vcodec h264 --qp 24        # quality-targeted (0-51, default 23)
ironav convert -i in.y4m -o out.mp4  --vcodec hevc --vbitrate 4000   # ~4 Mbit/s, in an MP4
```

The MP4 writer is constant-frame-rate, and puts the index in front of the media data (fast-start) by copying the finished file once,
so writing costs one extra pass over the output. Verified by opening the files with FFmpeg's MP4 demuxer: correct codec, dimensions, 30 fps,
exact 8.000 s duration, all frames decoded, and seeking lands on the right keyframe.

### Reading MP4 and decoding on the GPU (NVDEC)

`ironav probe`, `convert` and the other tools read MP4 (regular files with the index anywhere, and fragmented files). H.264/HEVC video is decoded by NVDEC through the driver's `nvcuvid` library, loaded at run time like NVENC;
the output is cropped to the display size and returned as planar `yuv420p` in display order, so B-frame streams work.
8-bit and 10-bit 4:2:0 are accepted (10-bit comes back as `yuv420p10le`); 4:2:2/4:4:4 and 12-bit are refused with a clear error.
10-bit HEVC decoding was compared with FFmpeg's software decoder on a 60-frame 720p Main 10 clip: every sample of every
frame is identical. 10-bit H.264 (High 10) and 10-bit encoding of H.264 are not offered (consumer NVENC cannot encode it).

```
ironav convert -i in.mp4 -o out.y4m --vcodec rawvideo                    # decode to raw frames
ironav convert -i in.mp4 -o out.mp4 --vcodec hevc --qp 24 --acodec none  # GPU decode -> GPU encode
ironav convert -i in.mp4 -o out.mp4                                      # stream copy (remux), no GPU needed
```

`--acodec none` / `--vcodec none` leave a stream out. Copying H.264/HEVC with B-frames into an MP4 works: the writer
records composition offsets (`ctts`) and an edit list, and a copy of an x264 file with B-frames decodes to the same
50 frames, bit for bit, at the same times (checked with FFmpeg's decoder through PyAV).
AAC audio can be copied into an MP4 too (see the AAC decoder below for decoding). The writer builds the `esds`
box from the stream's AudioSpecificConfig and keeps the encoder-delay edit list, so a copy of an x264+AAC file decodes
to the identical 50 frames and 88,064 audio samples as the original.
Fragmented MP4 (`moof`/`traf`/`trun`, as made by streaming tools and `-movflags frag_keyframe`) is read too, with or without
`default-base-is-moof`; checked on two fragmented files made by FFmpeg, which copy to the same frames, times and audio samples.
Fragmented MP4 is not written.

Checked against FFmpeg's software decoders (via PyAV), frame by frame: NVDEC output is **identical** for 240/240 frames of
720p H.264 and of HEVC (made by IronAV), and for 50/50 frames of 1080p H.264 with B-frames made by x264 (a different
encoder, which also exercises cropping and reordering). A 1080p x264 → HEVC GPU transcode gave 49 dB PSNR at QP 20.
Timing on the RTX 3090 Ti is noisy: decoding the 8 s 720p clip to a raw Y4M took 0.9-1.7 s over several runs, much of it
spent writing 332 MB. No comparison with FFmpeg has been made.

### Resizing

`--filter scale:width=1280` (or `height=`, or both) resizes with a separable triangle filter that widens when shrinking,
so downscaling averages the pixels it covers. A missing side keeps the aspect ratio, rounded to an even number so the
result still suits the GPU encoder. It runs on the CPU, on every pixel format. Against FFmpeg's own scaler on a 1080p
to 540p downscale, luma PSNR was 54 dB versus its bilinear mode and 48 dB (41 dB worst frame) versus its area mode, so it
behaves like a bilinear scaler rather than a bicubic or Lanczos one. A 50-frame 1080p to 720p GPU transcode with scaling
took 1.5-2.0 s on the RTX 3090 Ti; I have not measured how much of that is the scaler.

### Pixel formats

`--filter format:pix_fmt=yuv420p` converts between gray8, yuv420p, yuv422p, yuv444p and rgb24, so sources the GPU encoder
cannot take directly (4:4:4, 4:2:2, grey) can be fed to it. RGB uses limited-range BT.601 (`matrix=709` for BT.709);
chroma is resampled with the same triangle filter as `scale`. Tested for known colour values, round trips and plane
layouts; not compared against FFmpeg's converter.

**10-bit.** `yuv420p10le` (16-bit little-endian words, value in the low 10 bits) is a pixel format in its own right: Y4M
(`C420p10`), `scale`, `hflip`, `format`, the MP4 reader and writer (`hvcC` carries the depth) and the player all handle it.
`format` converts to or from 10-bit by going through 8-bit 4:2:0, so only 10-bit to 10-bit keeps the extra precision,
and converting to RGB or another layout loses it. `gray` does not take 10-bit input. An NVENC Main 10 encode of the
decoded test clip came back with luma PSNR 58.9 dB against the source (a smooth synthetic gradient, so a high figure
says the bit alignment is right, not how the encoder performs on real footage).

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
need a GPU skip themselves when none is usable. Not done: hardware scaling, 10-bit H.264, 12-bit, 4:2:2/4:4:4, AV1, rate-control
tuning beyond QP/average bitrate, and the other vendors' providers.

## Playing media

`ironav play FILE` opens a player. On a terminal it is a full-screen TUI (truecolor half-block video, or a level meter for
audio; space, arrows, `j`/`l`, `+`/`-`, `m`, `r`, `0`-`9`, `s` for a PNG, mouse click on the bar to seek). Anywhere else it
reads one JSON command per line on stdin and answers with one envelope per line:

```
$ printf '%s\n' '{"cmd":"seek","to":5}' '{"cmd":"wait","position":6}' '{"cmd":"snapshot","path":"a.png"}' | ironav play movie.mp4 --audio silent
```

Commands: `status play pause toggle seek{to|by|fraction} volume mute loop snapshot wait help`. Every reply is the full status
(state, position, duration, streams, decoder, frames shown and skipped, the audio `level`), so one call both acts and
shows the effect. Over MCP the same player is `play_open`, `play_control`, `play_status` and `play_close`; snapshots come
back as image blocks. `ironav gui --play` is the same player in a window, with an "Agent view" panel showing its live JSON.

How it works: audio and video are decoded on separate threads, each with its own demuxer; the master clock is the number
of audio frames the output device has taken (so a stalled device stalls the clock and video follows), with the wall clock
used for video-only files or once the audio runs out. Seeking is exact: audio is trimmed to the target and video decodes
forward from the previous keyframe and shows the last picture at or before it. Late pictures are dropped, not queued.

What was and was not checked, on one machine (Windows, RTX 3090 Ti):
- Real-time pacing, pause, exact seeks, ordering, looping, end detection and the JSON commands are covered by tests on
  generated WAV and Y4M files, against the silent clock (`--audio silent`).
- A 720p H.264 MP4 with FLAC audio plays through NVDEC at 1.0x with no dropped pictures, seeks, and gives snapshots over
  MCP and in the window; snapshot colours match FFmpeg's BT.709 conversion to within 3 levels (mean 0.7).
- **Sound through a real device was not heard or measured**: no one was listening and the sound card path is untested
  beyond building. The `level` meter reports what was handed to the device, not what came out of the speakers.
  Audio/video sync was not measured against a reference.
- The TUI is tested by rendering to a test buffer; it has not been exercised in a real terminal emulator.
- Players keep their position only in memory; there is no subtitle,
  playlist or network-stream support, and no GPU rendering (pictures are converted to RGB on the CPU).

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

## AAC

AAC-LC **decoder** (ISO/IEC 14496-3), written from the specification with the Huffman codebooks, scalefactor-band
and TNS tables taken from FFmpeg's `aactab.c` by a script (`tools/gen_aac_tables.py`) so none were typed in by hand. It
handles single and paired channel elements (mono, stereo, 3.0 to 7.1), long, start, short and stop windows with sine and
KBD shapes, mid/side and intensity stereo, perceptual noise substitution, TNS and the pulse tool. Output is float, so
converting to FLAC or 16-bit PCM clips whatever the codec overshot, as FFmpeg's output would too.

**Checked against FFmpeg's AAC decoder** (through PyAV) on eight streams made by FFmpeg's encoder: mono, stereo at 8, 22.05,
44.1, 48 and 96 kHz, 5.1, a low-bitrate PNS stream, and one with TNS, PNS, intensity and mid/side all enabled. All decode
to within float rounding of FFmpeg's output (about 136 dB SNR). Short windows, TNS, PNS, intensity and mid/side were all
confirmed to occur in those streams; **the pulse tool did not occur in any of them, so that path is untested**, and
every stream came from one encoder. One of them is committed with FFmpeg's decode as a regression test.

Not decoded, and refused with an explanation rather than decoded wrongly: HE-AAC (SBR/PS), AAC Main prediction, LTP,
coupling channel elements, 960-sample frames, gain control, and channel layouts given by a program config element.
There is no AAC encoder. Raw `.aac` (ADTS) files are not read yet; AAC comes from MP4/M4A.
On a 5-minute stereo 44.1 kHz file the decoder takes roughly 0.3-0.5 s (FFmpeg's: about 0.11 s) on a noisy machine, so it
is a few times slower than FFmpeg's.

## Performance

Stages run on separate threads with bounded queues, packets are moved not copied, and the MCP/GUI processes stay
resident. Measured (release build, one Windows machine, 10 min of 48 kHz stereo PCM): s16→f32 with a volume filter
≈ 2,500× real time (≈ 450 MiB/s in); stream copy ≈ 5,100× real time. These are PCM numbers.

**Against FFmpeg** (`bench/vs_ffmpeg.py`; one Windows machine, RTX 3090 Ti, 24 threads; FFmpeg 8.x libraries through PyAV;
median of 5). The FFmpeg side runs in-process and pays no process start; the IronAV side is a new process each time
(about 13 ms now; it was 79 ms before the windows moved into their own program, see below), so the table leans against IronAV:

| Job | IronAV | FFmpeg | |
|---|---|---|---|
| WAV to FLAC, 180 s stereo | 0.38-0.45 s | 0.21-0.22 s | FFmpeg about 2x faster; IronAV output 0.1 % smaller |
| FLAC to WAV, 180 s | 0.33 s | 0.12 s (decode only) | FFmpeg about 2.5x faster |
| NVENC H.264, 240 frames 720p from Y4M | 0.56-0.74 s | 0.42-0.68 s (`h264_nvenc`) | FFmpeg faster in quiet runs (1.3-1.6x), about level in the noisiest; sizes differ because rate control differs |
| NVDEC H.264 to a Y4M file, 240 frames 720p | 0.84 s | 0.42 s (`cuda` hwaccel, no file written) | not like for like; FFmpeg's figure writes nothing |

**So IronAV is currently slower than FFmpeg on every job measured here.** The "lower latency, higher throughput"
goal is a goal, not a result. The FLAC encoder alone (no file, no process) takes 0.18 s for the 3-minute file on this
machine (`cargo test --release -p ironav-codec --test flac_perf -- --ignored --nocapture`); it was 1.1 s before it was
tuned (cheaper Rice-parameter search, one residual per predictor family instead of eight, a cached window, batched
four-byte bit writes, and blocks encoded in parallel). FFmpeg decodes its output bit-exactly (checked on the 3-minute file), and the committed libFLAC fixtures still pass.
NVENC used to wait for each frame before copying the next; it now keeps six in flight, which took the in-process
conversion from about 0.65 s to 0.42 s (copying the same Y4M with no encoding takes 0.23-0.32 s of that). FLAC decoding was
tuned too (four-byte CRC-16 steps, 64-bit bit-reader loads, unary codes found with a leading-zero count): 20-30 %
faster in alternating same-machine runs of the in-process test, about 0.15 s for the 3-minute file. NVDEC: decode alone (no output file; `IRONAV_PERF_MP4=clip.mp4 cargo test --release -p ironav-engine --test nvdec_perf
-- --ignored --nocapture`) takes 0.36-0.67 s for the 240-frame 720p clip on this busy machine, of which 0.10-0.25 s is
creating the decoder and CUDA context before the first picture; reusing one chroma buffer per picture made it
15-25 % faster in alternating runs. The Y4M row above is mostly the 332 MB file being written. A like-for-like FFmpeg
figure needs the `ffmpeg` command-line program, which is not installed here (PyAV's Y4M writer took 7.6 s).

**Start-up.** `ironav` with no work to do took 79 ms to start, against 11 ms for a trivial program, because the window
libraries were linked into it. They now live in a second program, `ironav-gui`, which `ironav gui` launches; `ironav`
starts in 13 ms (median of 25 on this machine). Both programs are built by default and must stay in the same directory.
`cargo build --release --no-default-features -p ironav-cli` builds only the small one. Timings on this machine vary by a factor of two from run to run when other work is going on.
NVENC in earlier runs: 240 frames of 1280x720 (a 332 MB Y4M) in 0.6-3.8 s wall depending on load.


## Security notes (browser GUI)

The native GUI runs in-process and opens no sockets. The `--web` variant is loopback only; every request needs a per-launch random token and a loopback `Host` header. The file browser can list
any directory the process can read, so treat the launch URL like a credential.

## Layout

`ironav-core` types/traits · `ironav-hwaccel` · `ironav-codec` · `ironav-formats` · `ironav-ontology` ·
`ironav-player` (playback engine) · `ironav-engine` (jobs, planner, pipeline, tool registry) · `ironav-gui` (native, egui) · `ironav-webui` (browser) ·
`ironav-cli`

Build: `cargo build --release` · Test: `cargo test`
Lean build for servers/agents without the graphics or sound stack: `cargo build --release --no-default-features -p ironav-cli`

## License

Dual-licensed: **[AGPL-3.0-or-later](LICENSE)** for open use, or a **commercial license** from Nervosys if you
cannot comply with the AGPL (e.g. closed-source embedding or proprietary hosted services). See
[COMMERCIAL-LICENSE.md](COMMERCIAL-LICENSE.md). Contributions are accepted under the terms in
[CONTRIBUTING.md](CONTRIBUTING.md), which let Nervosys offer the commercial license.
