# IronAV

Audio and video conversion in Rust, built to be driven by AI agents as well as people.
A CLI, an MCP server and a HandBrake-style GUI all sit on one tool registry, so they cannot drift apart.

> **Status: foundation (v0.1).** The architecture, agent interface, ontology, hardware model, CLI, MCP server
> and both GUIs are working and tested. The codec/container set is deliberately small so far:
> **WAV, FLAC and Y4M containers; PCM (8/16/24/32-bit, float), FLAC and raw-video codecs, an AAC-LC decoder, an Opus decoder and an Opus (CELT) encoder; `volume`, `resample`, `gray`, `hflip` filters.**
> **H.264 and HEVC can be encoded and decoded on NVIDIA GPUs (NVENC/NVDEC)**, MP4 files can be read and written, and
> Matroska/WebM files can be read.
> There is no software H.264/HEVC codec, so those need an NVIDIA GPU. MP4 can carry FLAC audio next to the video, and AAC
> audio copied from another MP4/M4A (AAC can be decoded but not encoded; Opus can be both). AV1, writing Matroska etc. are modelled in
> the ontology as `planned`.
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
ironav convert -i in.mp3 -o out.mka --acodec opus --abitrate 96   # Opus at 96 kbit/s (resampled to 48 kHz automatically)
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

### Matroska and WebM (read only)

`.mkv`, `.webm` and `.mka` files are read: the elements are walked once to find every frame (simple blocks, block groups,
Xiph/EBML/fixed lacing, clusters of unknown size, a file cut short mid-cluster), and the frames then go through the same
reader as MP4, so probing, hardware decoding, seeking, playing and copying H.264/HEVC/AAC into an MP4 all work on them.
Matroska keeps presentation times only, so for B-frame video the decode times are taken to be the sorted presentation
times. Audio whose frame length is known (AAC, MP3, Opus, anything with a `DefaultDuration`) gets a sample-rate time base, so its
times are exact despite the file's millisecond ticks.
Checked on files remuxed by FFmpeg: an x264 file with B-frames and AAC and an HEVC file copy into MP4 with the same frames
(identical pixels when decoded by FFmpeg) and audio samples; the 1080p H.264 file decodes on the GPU to the same 50 frames
as FFmpeg's decoder; AAC in a `.mka` decodes to within 1 LSB of FFmpeg's. Opus in Matroska decodes too (see below); a VP9 WebM is listed by `probe`
but cannot be decoded (there is no VP9 decoder yet).
Not handled: tracks with a content encoding (header stripping, compression, encryption) are refused; chapters, tags, cues
and attachments are ignored; subtitle tracks are skipped; Matroska is never written. A copied track starts at its own first
timestamp rounded to the output's frame grid, so a video that starts a fraction of a frame after the audio in the source
(23 ms in one test file) ends up up to half a frame (20 ms at 25 fps) away from where it was.

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

**HE-AAC v1 (SBR)** is decoded when the AudioSpecificConfig announces it (explicit signalling): the spectral band replication
tables come from FFmpeg's `aacsbrdata.h` by `tools/gen_sbr_tables.py`, and the rest is written from the specification and
FFmpeg's float decoder. It was checked on one stream, Nero's encoder (a 44.1 kHz stereo `ct_nero-heaac.mp4` from the FFmpeg
sample archive, 22.05 kHz core), which decodes to within float rounding of FFmpeg's output (131 dB SNR). **That one file is
all the testing the SBR path has had**: it is not committed, mono SBR, coupled channels, the harmonic and short-envelope
paths, and cores other than 22.05 kHz are unchecked, and the committed test only checks the filter bank on a tone.

Not decoded, and refused with an explanation rather than decoded wrongly: HE-AAC v2 (parametric stereo) and SBR that is only
signalled implicitly in the stream, AAC Main prediction, LTP,
coupling channel elements, 960-sample frames, gain control, and channel layouts given by a program config element.
AAC comes from MP4/M4A, Matroska and raw `.aac` (ADTS) files; the ADTS reader strips each frame's header, builds the AudioSpecificConfig from it, and decodes to the same samples as the MP4 copy of the same audio (checked on the one fixture, remuxed by FFmpeg). ADTS frames with several raw data blocks and channel configuration 0 are not read, and nothing is trimmed at the start (as with FFmpeg).
On a 5-minute stereo 44.1 kHz file the decoder takes roughly 0.3-0.5 s (FFmpeg's: about 0.11 s) on a noisy machine, so it
is a few times slower than FFmpeg's.


### AAC encoder

`--acodec aac` (`--abitrate 8` to `512`, default 64 per channel) writes AAC-LC into MP4/M4A, ADTS (`.aac`) or Matroska, mono or
stereo at any of AAC's sampling rates (others are refused with the list). Sine windows, with **window switching**: a block whose
first difference jumps (a 128-sample piece with 8x the energy of the four before it) becomes eight short windows, each in a group of
its own, with start and stop windows around it, which costs one frame of look-ahead. On a noise burst over a quiet tone the error in
the 900 samples before the attack fell to a third (test), and FFmpeg decodes a stereo file with four such attacks to the same samples as
ours (136 dB). The detector is a simple energy rule, not tuned on music, and short blocks do not group windows, so they cost more side
information than they might. The decoder's own Huffman
and scalefactor-band tables, the cheapest of the eleven spectral codebooks chosen per band, sections merged, and mid/side per band
where it looks cheaper. A feedback loop on the quantiser's step holds the bit rate (the first frames search for the right step), and the
step follows each band's level partway (exponent 0.6), so noise is shaped but there is no psychoacoustic model. No TNS, PNS, intensity stereo or prediction, no bit reservoir, no more than two channels.

The stream starts with one frame of priming: in MP4 an edit list says so (our reader and FFmpeg skip it), in ADTS and Matroska it
stays as 1024 leading samples, as with most encoders. MP4's edit list also ends the audio at the input's length, but players that ignore it
(FFmpeg does) show up to one padded frame at the end. FFmpeg (through PyAV) decodes all three containers; on a 4 s stereo test signal
(two tones and a little noise) 128 kb/s gives 16 dB SNR and 256 kb/s 25 dB, with white noise taking its share of the bits. A 120 s MP3 becomes
128 kb/s AAC in about 1 s.

## Opus

Opus **decoder** (RFC 6716): SILK, CELT and hybrid frames, every frame size from 2.5 to 60 ms, mono and stereo, packets of
all four framing codes, in-band redundancy frames, and the output gain and pre-skip from the `OpusHead` (the pre-skip is
dropped, so a file starts where its audio does). The band-energy, bit-allocation, pulse-vector, inverse-MDCT, linear-prediction
and excitation stages follow FFmpeg's native Opus decoder, with its tables (`tools/gen_opus_tables.py` reads them from
`libavcodec/opus/tab.c`, so none were typed in). The 8/12/16 kHz SILK output is brought to 48 kHz by a polyphase
windowed-sinc filter of our own, with the same delay (4, 8 and 11 input samples) as the reference decoder.

**Checked against FFmpeg's native Opus decoder** (through PyAV) on fifteen streams made by libopus, plus a pure sine in CELT
and in SILK: SILK narrow band at 20 ms, medium band at 10 ms and wide band at 20, 40 and 60 ms (mono and stereo); hybrid at 10
and 20 ms; CELT at 2.5, 5, 10, 20 and 40 ms, mono and stereo. **CELT frames match to float rounding (about 138 dB SNR).**
SILK and hybrid frames match at 42-48 dB for wide and medium band and **only 24 dB for narrow band**. That difference comes
from our upsampling filter (FFmpeg's resampler is longer than a filter with a fixed delay of 8 taps per phase can be), though
I did not isolate it further. Three of those streams are committed with FFmpeg's decode as regression tests (CELT to within 3
LSB, SILK and hybrid above 35 dB).
Not covered: multichannel Opus (channel-mapping families 1 and 255 are refused), a packet-loss concealment pass (a damaged or
empty frame decodes to silence), and the way the reference decoder flushes its resampler when the stream switches between SILK
and CELT, which our fixed-delay filter does not reproduce, so the few samples at such a switch can differ. The libopus encoder
only produced streams in one mode at a time, so **mode switches within a stream were not tested**, nor were redundancy frames.
Opus is read from Matroska/WebM (`.mka`, `.webm`) and from Ogg (`.opus`, `.ogg`); for writing see "Opus encoder" below. The Ogg reader (pages are checksummed; the file is read into memory, up to 1 GiB) takes only the first Opus, Vorbis or FLAC stream: Speex and Theora in Ogg are refused with a message naming the codec, and chained streams after the first are ignored. The final page's granule position shortens the last packet, so the output ends where the file says (for Opus the length is the granule minus the pre-skip). Tested on libopus files remuxed to Ogg (identical samples to the Matroska copies); multi-page packets are covered by a synthetic unit test only. FLAC in Ogg (the native-FLAC mapping, as FFmpeg's muxer writes it) decodes bit for bit to the PCM it was made from.


### Ogg Opus, Vorbis and FLAC output

`.opus` (and `.ogg`/`.oga`) is written for Opus, copied from any file that holds it or encoded: `OpusHead`, `OpusTags`, then pages of about
half a second whose granule positions follow RFC 7845 (the packets' durations, pre-skip included, add up to the last one's, so
the stream ends where it should). FLAC is written in Ogg too (`--acodec flac -o out.oga`, native mapping): STREAMINFO in the first
page is rewritten when the stream ends, so it carries the real length and frame sizes. Encoded and re-decoded it is bit-exact,
and FFmpeg reads the same file bit-exact. Vorbis is written in Ogg, copied from Ogg or Matroska or encoded (see "Vorbis encoder"): pages are stamped from each packet's
window size, an Ogg source keeps its exact length (FFmpeg decodes the copy sample for sample), and a Matroska source ends up to a
window (48 samples in the test file) long, because Matroska does not record where the audio stops.

### Opus encoder

`--acodec opus` writes CELT-mode Opus: 48 kHz mono or stereo, 20 ms frames, constant bit rate (64 kb/s per channel; the
planner refuses other sample rates, naming the way out), into `.opus` (Ogg), `.mka`, `.mkv` and `.webm` with the `OpusHead` and the
pre-skip. It is written against the decoder: the bit allocation and the band quantiser are the decoder's own code, made
generic over a `Coder` trait so that one body of code reads or writes a frame and the two cannot disagree, and what the
encoder adds is the forward MDCT (checked to invert the decoder's synthesis), the band energies and their coarse/fine
quantisation, the pulse search, and the stereo angle (mid/side with a coded angle, or one channel when the bit budget is
small; a flipped right channel when the channels are mostly out of phase). Digital silence costs three bytes a frame.
`--abitrate KBPS` sets the rate (6 to 510; the default is 64 per channel). The encoder takes 48 kHz only; the `resample`
filter (a Kaiser-windowed-sinc polyphase filter for any rate ratio, phase-exact, with the cut-off at the lower Nyquist
frequency so nothing aliases; checked on tones across 44.1/48/22.05/96/8/16 kHz) converts, and the planner adds it
by itself (with a warning) when the source is at another rate, so `--filter resample` is only needed to choose the rate for other codecs. A 120 s, 44.1 kHz stereo MP3 becomes Opus at 96 kb/s in 0.58 s here (about 200 times real time, decode and
resampling included); FFmpeg decodes it, at 17 dB SNR against the resampled source.
The range encoder is checked symbol by symbol against the decoder's, including its bit counts.

Checked: FFmpeg (through PyAV) opens the Matroska files and decodes them; our decoder's output and FFmpeg's agree to
83 dB, and both are 20.4 dB SNR against the source on a 6 s, 128 kb/s stereo test signal. Quality grows with the bit
rate as it should (a tone-and-noise test signal, mono: 10 dB at 16 kb/s, 22 dB at 64, 29 at 128, 39 at 256). This is a first encoder, not libopus: there
is only a simple transient detector (a frame with a sudden attack is coded as eight short blocks; on a test of decaying hits that is +1.3 dB SNR), no pitch pre-filter,
no band boosts or tuned allocation trim, and no variable bit rate or SILK/hybrid modes, so at the same size it will sound
worse than libopus. The end of a stream is exact in Ogg (the last page's granule position) and in Matroska/WebM (the last block carries a
`DiscardPadding`, which our reader honours); FFmpeg decodes both to exactly the source's length. Not compared with libopus for quality: I have no libopus encoder to run.

## Vorbis

Vorbis I is decoded (floor type 1; residue types 0, 1 and 2; any channel count up to 8 in the standard layouts, reordered to
the usual WAV/FFmpeg order) from Ogg (`.ogg`, `.oga`) and from Matroska/WebM. The setup-header, floor, residue and overlap
logic follow FFmpeg's decoder and the floor 1 gain table comes from it (`tools/gen_vorbis_tables.py`); the inverse MDCT reuses
the FFT code of the Opus decoder, and the window is computed from its definition.

Checked against FFmpeg's decoder on twelve libvorbis files (mono, stereo and 3 to 7 channels; 8, 22.05, 44.1 and 48 kHz;
several qualities; long and short blocks and the windows between them): **136 dB SNR**, largest sample difference 2.4e-7 of
full scale (single-precision rounding). The same packets in Matroska decode identically. Fuzzed with 400 damaged and truncated
files: no panics. Output is limited to [-1, 1] like every other IronAV decoder, so the overshoot a lossy stream can have
is clipped where FFmpeg's float output keeps it.

**One deliberate difference.** FFmpeg's SSE code for inverse channel coupling, until a fix in May 2026, treated a magnitude of
exactly zero as positive; the specification, libvorbis and the C code (and FFmpeg now) do not. FFmpeg's own Vorbis encoder
produces such values often, so a decode of its streams by an older FFmpeg differs from ours in the sign of the angle channel on
those spectral lines (35 dB SNR on the test file); libvorbis streams did not hit it. `VorbisDecoder::legacy_zero_coupling(true)`
reproduces the old behaviour, and with it the FFmpeg-encoder test file matches bit for bit.

Not covered: **floor type 0** (obsolete; a stream that uses it is refused) and streams with more than 8 channels. The Ogg
granule position of the last page trims the end; a stream that begins mid-way (a first granule above zero) is not shifted. Chained or multiplexed Ogg streams: only the first Vorbis stream is
read. Mid-stream changes of the headers are not followed. Decoding 120 s of 44.1 kHz stereo to WAV takes about 0.25 to 0.29 s end to end
inside IronAV's own timing, of which the decoder alone is about 0.15 s (it was about 0.25 s: the inverse transform now runs a
radix-2 butterfly with no generic loop, the transform's output is mirrored with slice copies, and the overlap-add runs in
segments instead of deciding per sample). FFmpeg's decoder alone ran the same file in 0.08 to 0.19 s through PyAV, so the two are
now close. This decoder is scalar `f32`; codebook lookups use a 10-bit table, longer codes walk a tree.


### Vorbis encoder

`--acodec vorbis` (`--abitrate 16` to `500`, default 64 per channel and 2 channels at most) writes Vorbis I into Ogg or Matroska/WebM:
2048-sample blocks, and 256-sample ones around attacks, floor 1 with 24 posts (12 for the short blocks), residue type 1 coded with four lattice codebooks (values to +-1, 3, 7 and 31, chosen
per 16-value partition), and square-polar coupling for stereo. The headers are written by hand and parsed back by our own decoder, and
the floor curve the encoder divides by is the one it gets by decoding its own floor bits, so the two cannot drift apart. A feedback loop
on the quantiser's step holds the bit rate (the first packets search for the right step first). The step follows the local spectrum level only
partway (exponent 0.6), so noise is shaped but not hidden: there is no psychoacoustic model. **Block switching**: when 128 samples of the signal's first difference carry 8x the energy of the four before them inside the window
the next long block would have, short blocks run until the attack is clear; the decision is one block ahead, because a long block next to a
short one has a short slope on that side. On a noise burst over a quiet tone the error before the attack fell 60x (mono) and 2.4x
(stereo) (test), and FFmpeg decodes the Ogg and Matroska files to the same samples as ours (137 dB). The
detector is a simple energy rule, untuned on music. No noise substitution, no floor 0, no more than two channels, and the code books are fixed, not trained: on
Gaussian noise it lands about 4 dB under the rate-distortion bound.

FFmpeg (through PyAV) decodes the output and our decoder agrees with it to one least-significant bit on stereo and mono streams. Lengths
are exact in Ogg (the last packet's duration is trimmed); in Matroska the end is up to a block (1024 samples) late, since the container holds no
end position. On a 4 s stereo test signal (two tones and a little noise) 128 kb/s gives 19 dB SNR, 320 kb/s 28 dB. A 120 s MP3 becomes 128 kb/s
Vorbis in about 2 s (roughly 55 times real time). One trap found on the way: a coupled pair whose magnitude is exactly zero and whose angle
is not is decoded differently by the specification and by some FFmpeg builds, so the encoder never sends one.

## VP8

VP8 video is decoded in software (`ironav-codec/src/vp8`, a port of FFmpeg's `vp8.c` and `vp8dsp.c`; the probability, quantiser and
motion tables are generated by `tools/gen_vp8_tables.py`). All four profiles work: six-tap and bilinear motion compensation, the
normal and simple loop filters, segmentation, loop-filter deltas, one to eight token partitions, golden and altref references
(with sign bias), split motion vectors, and frames that are not shown. Output is 4:2:0 8-bit, cropped to the coded size. It is the
first software video codec, so `ironav convert -i clip.webm -o out.y4m --vcodec rawvideo` now works on a machine without a GPU.

Checked bit for bit against FFmpeg's decoder (through PyAV) on libvpx output: twelve clips from 54x38 to 176x144 (odd and
non-multiple-of-16 sizes, profiles 0 to 3, one to eight partitions, constant-quality extremes, error-resilient streams with
segmentation and unsaved probabilities) all match on every frame, and three small clips are kept as tests with FFmpeg's per-frame
hashes. Sixty randomly corrupted files produced no panic. Not covered by any test file: hidden altref frames and golden/altref
sign bias (libvpx through PyAV never produced them) and keyframe size changes in mid-stream. Speed has not been tuned; the decoder
does not use more than one thread. VP8 has no encoder here; VP8 can be copied into WebM or Matroska.

## JPEG

Baseline and extended-sequential 8-bit Huffman JPEG (`.jpg`, `.jpeg`, codec `mjpeg`) is decoded and encoded in software. Decoding handles grey, 4:2:0, 4:2:2 and 4:4:4, restart intervals and non-interleaved scans. Progressive, arithmetic-coded, 12-bit and lossless files are refused with a clear error. The encoder writes JFIF with the standard tables at quality 90.

Motion-JPEG can also be written into Matroska (`V_MJPEG`, `-o clip.mkv --vcodec mjpeg`); FFmpeg opens and decodes those files. A `.jpg` holds one picture; `ironav convert -i in.y4m -o out.jpg` picks `mjpeg` by itself. Decoded output differs from FFmpeg's by at most 1 LSB (a different IDCT), checked on 4:2:0, 4:2:2 and 4:4:4 images. Encoded files decode in FFmpeg at 40+ dB PSNR. 150 corrupted or truncated files made it return errors, never panic.

## MP3

MPEG-1, MPEG-2 and MPEG-2.5 Layer III are decoded (mono, stereo, joint stereo with mid/side), from `.mp3` files (ID3v2 tags
skipped, a leading Xing/Info frame not played) and from Matroska. The Huffman and band tables and the scale-factor, stereo and
short-block handling follow FFmpeg's decoder; the filterbank is written from the standard's definitions in `f64`.

Checked against FFmpeg's `mp3float` decoder on twelve LAME-encoded files (44.1, 48, 32, 22.05, 24, 16, 11.025 and 8 kHz;
mono and stereo; 16 to 320 kb/s, CBR and VBR; long, start, short and stop blocks): **116 to 124 dB SNR**, largest sample
difference about 3e-6 of full scale. The same frames in Matroska decode identically. Fuzzed with 120 damaged and truncated
files: no panics.

Not covered, and not tested because the encoder used never produced them: **mixed blocks, intensity stereo, CRC-protected
frames and the 8 kHz mixed-block tables** are ported from FFmpeg but unverified. Layers I and II are refused. A frame whose
bit-reservoir data is missing (the first frames after a seek) decodes to silence. A LAME (or Lavf) tag's encoder delay and padding
are applied: the reader hands the decoder where the audio starts and ends (delay plus the decoder's 529 samples at the start,
padding less 529 at the end), and on the three LAME fixtures the output has exactly FFmpeg's length and samples. An MP3 in
Matroska has no tag and is not trimmed. Decoding 120 s of 44.1 kHz stereo (a dense, noisy test signal) to WAV
takes 0.17 to 0.18 s end to end by IronAV's own timing (about 700x real time; it was 0.65 to 0.8 s before the frame index stopped
seeking per frame, the synthesis filterbank used a fast DCT and the IMDCT its symmetry, and both ran in `f32` compiled for AVX2 and
FMA, chosen at run time when the CPU has them). Of that, the decoder alone is about 0.10 s; FFmpeg's decoder alone ran the same
file in 0.065 s through PyAV, so it is still about 1.6x faster. The Huffman decoding is the largest remaining part. The noisy
machine makes these figures good to perhaps 20 %.

### MP3 encoder

`--acodec mp3` (`--abitrate 32` to `320`, snapped to the nearest rate MPEG-1 has; default 64 per channel) writes MPEG-1 Layer III to
`.mp3` or Matroska, mono or stereo at 32, 44.1 or 48 kHz (other rates are resampled to the nearest of those, with a warning). The chain is
the standard's: polyphase analysis, a 36-point MDCT, the aliasing butterflies, then a quantiser whose step follows each scalefactor band's
level partway (exponent 0.6) with a low-pass that depends on the bit rate. The Huffman table of each of the three regions, the region
split and the count1 table are chosen for the fewest bits. Constant bit rate with a real bit reservoir: a frame's data may start in the
unused tail of the frames before it, so a packet is emitted only once later frames have filled it. Stereo is joint, coded as left/right or
mid/side per frame. **Block switching**: a granule whose subband energy jumps (8x the mean of the four slots before it) is coded as three 12-point
short blocks, with start and stop windows around it, which costs one granule of look-ahead; both channels share the block type. On a noise
burst over a quiet tone the error before the attack fell by 5x (mono) and 16x (stereo) (tests), and FFmpeg decodes a file with four such
attacks to the same samples as ours (115 dB). The detector is a simple energy rule that has not been tuned on music, and short blocks use
neither mixed blocks nor subblock gain. There is no psychoacoustic model, so the reservoir only smooths; no intensity stereo, no
scalefactor reuse, no MPEG-2/2.5 rates.

An `.mp3` file starts with an Info frame holding the frame count and size and a tag with the delay and padding, so players can play it gapless
and know its length; the tag carries the `Lavc` prefix because FFmpeg only reads the delay behind `LAME`, `Lavf` or `Lavc`. FFmpeg (through
PyAV) decodes the files and trims them to exactly the input's length. On a 4 s stereo test signal (two tones and a little noise) 64 kb/s
gives 16 dB SNR, 128 kb/s 23 dB and 320 kb/s 28 dB (libmp3lame at 128 kb/s: 21 dB, though it aims at hearing, not SNR). A 120 s stereo file
becomes 160 kb/s MP3 in about 2 s. Converting an MP3 to MP3 is a copy unless a bit rate is given, which re-encodes it (with a warning); the same holds for the other lossy codecs.

## Matroska output

`.mkv` and `.mka` can be written: H.264 or HEVC video (from the hardware encoder or copied) with AAC, FLAC, MP3, Opus or Vorbis
audio, copied or encoded, at most one stream of each kind. Timestamps are in milliseconds; clusters are gathered in memory and
written whole, so there are no unknown-size elements, and the file ends with Cues for the video key frames (or the clusters, for
audio only). The header is held back until every stream has delivered a packet and the video's SPS/PPS have been seen.

Checked with FFmpeg (through PyAV) on AAC, Opus, Vorbis, FLAC and MP3 copied into `.mka`, and on H.264 and HEVC from the NVENC
encoder, with and without the AAC audio in `.mkv`: FFmpeg opens every file, the audio packet counts equal the sources', and
FFmpeg decodes all 50 frames of each video. IronAV's own tests copy audio through and compare every packet's bytes and the codec
configuration, and decode FLAC through Matroska to the PCM it was made from.

`.webm` is written for Opus or Vorbis audio only (document type `webm`; FFmpeg opens both and decodes them); WebM's video
codecs, VP8, VP9 and AV1, have no encoder here. VP8 video can be copied into `.webm` or `.mkv` (key frames are read from the frame tag, so Cues are written; no codec private data is needed) and decoded, see "VP8"; FFmpeg opens the copies and decodes the same frames. VP9 and AV1 are not written.

Not done: no SeekHead, tags, chapters or attachments; PCM, VP9/AV1 and more than one stream per kind are refused; an MP3's gapless trim (from its LAME tag) is not
carried over and the Opus pre-skip is carried only as the OpusHead plus CodecDelay. I did not test B-frame video, because the
encoder here does not produce it.

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
