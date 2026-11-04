# IronAV

Audio and video conversion in Rust, built to be driven by AI agents as well as people.
A CLI, an MCP server and a HandBrake-style GUI all sit on one tool registry, so they cannot drift apart.

> **Status: foundation (v0.1.2).** The architecture, agent interface, ontology, hardware model, CLI, MCP server
> and both GUIs are working and tested. Containers: **WAV, FLAC, Y4M, MP4, QuickTime MOV (read and write), Matroska/WebM (read and write), Ogg, ADTS, MP3, JPEG and MPEG transport streams (read and write); raw H.264 and HEVC (Annex B: read and written as files, and read as live sources).**
> Audio codecs, each decoded and encoded: **PCM, FLAC, ALAC, AAC-LC (plus HE-AAC v1 decode), MP3, MP2, AC-3, Opus and Vorbis.**
> Video: **software decoders for H.264, HEVC (including 4:2:2 and 4:4:4), VP8 and VP9; a software H.264 encoder (High profile: CABAC and the 8x8 transform), a software HEVC encoder (Main profile, 8-bit 4:2:0, merge and skip, SAO), a software VP8 encoder, and a software VP9 encoder (key and inter frames), and a software AV1 encoder (key and inter frames, profile 0, 8-bit 4:2:0); JPEG/Motion-JPEG and Apple ProRes decoded and encoded in software; MPEG-4 Part 2 (Simple and Advanced Simple profiles) decoded in software, and Simple profile I and P pictures encoded in software, with Advanced Simple B pictures as an option (MP4 and Matroska); MPEG-1 and MPEG-2 video (frame and field pictures, 4:2:0 and 4:2:2) decoded in software, and MPEG-2 video (Main profile, 4:2:0, progressive frames, optional B pictures) encoded in software.**
> NVIDIA GPUs encode H.264 and HEVC (NVENC, preferred when usable) and decode H.264, HEVC and AV1 (NVDEC).
> AV1 is decoded on NVIDIA GPUs (NVDEC) or in software (key, intra-only and inter frames, superres included). VP9 and AV1 copy between WebM, Matroska and MP4.
> Live sources: **files as they grow, pipes, TCP, UDP, SRT (with encryption), RTSP/RTSPS, HLS over HTTP(S), WebRTC ingest (WHIP, over TLS with `whips://`), YouTube, cameras and microphones (Media Foundation, Video4Linux2, AVFoundation), and screens (DXGI Desktop Duplication, X11, CoreGraphics).**
> IronAV is not yet a drop-in replacement for FFmpeg or HandBrake.

## Install

The current release is **0.1.2** ([release notes and SBOM](https://github.com/nervosys/IronAV/releases/latest),
[CHANGELOG.md](CHANGELOG.md)). All twelve crates are on [crates.io](https://crates.io/crates/ironav-cli).

- Command line, native GUI and player: `cargo install ironav-cli` (gives `ironav` and `ironav-gui`; on Linux the
  sound output needs the ALSA development files).
- Without the graphics or sound stack, for servers and agents: `cargo install ironav-cli --no-default-features`.
- As a library: `cargo add ironav-engine`, or only the codecs you need, as `ironav-codec` with features (see Layout).
- From source: `git clone https://github.com/nervosys/IronAV && cd IronAV && cargo build --release`.

Each release carries a CycloneDX SBOM of the locked dependency graph, made by `tools/cyclonedx_sbom.py`.

## Interfaces

| Surface | Command | Use |
|---|---|---|
| CLI | `ironav convert -i in.wav -o out.wav --acodec pcm_f32le --filter volume:gain=0.5` | humans and scripts |
| GUI (native) | `ironav gui [file]` | HandBrake-style desktop window (egui): source, tabs, queue, drag-and-drop |
| Player (terminal) | `ironav play file` | full-screen TUI on a terminal; JSON lines on stdin/stdout otherwise, for agents |
| Player (window) | `ironav gui --play [file]` | native player window: seek bar, volume, snapshot, drag and drop |
| GUI (browser) | `ironav gui --web` | same layout as a loopback-only web app, for machines without a display stack |
| Analysis | `ironav analyze file [--level digest\|events\|raw] [--jsonl]` | what is in a file, as typed observations (see below) |
| MCP | `ironav mcp` | agents; resident process, no per-call startup; concurrent calls, progress and cancellation |
| Tool schemas | `ironav tools` | JSON Schema for function-calling APIs |

Every CLI command prints one JSON envelope: `{"ok":true,"result":…}` or
`{"ok":false,"error":{"code","message","hint","alternatives"}}`, with a distinct exit code per error class.

## Agent workflow

1. `ironav capabilities` — what exists in this build, with the workflow spelled out.
2. `ironav ontology search|describe|convert` — what is implemented vs planned, and why a conversion is or is not feasible.
3. `ironav probe <file>` → `ironav convert --dry-run …` (or `plan`) → `ironav run job.json`.
4. `ironav analyze <file>` (the `analyze` tool) to learn what is in a file without looking at its pictures.

Design choices for agents: declarative JSON jobs; `plan` validates without writing; outputs are atomic
(`.partial` then rename) and never overwrite unless `overwrite: true`; errors name valid alternatives.

Over MCP every tool call runs on its own thread, so a long `run` or `analyze` does not hold up other requests.
A call that carries `_meta.progressToken` gets `notifications/progress`, and `notifications/cancelled` stops it
(error code `cancelled`; a cancelled `run` leaves no output).

## Understanding media (`analyze`)

`analyze` decodes a file and runs analyzers over it, returning small typed observations instead of pixels:

| Analyzer | Reports |
|---|---|
| `structure` | bit rate per window, timestamp gaps, jumps backwards (packets only, no decoding) |
| `levels` | sample peak and RMS (dBFS), clipping |
| `loudness` | EBU R128 / ITU-R BS.1770: momentary, short-term and integrated loudness (LUFS), loudness range (LU) |
| `silence` | spans of at least 0.5 s below -50 dBFS |
| `voice_activity` | spans of speech (energy in the speech band over the noise floor, pitch, syllabic modulation; a steady tone, hum, hiss or noise does not count). On a synthesized voice under pink noise it finds as much speech at 10 dB SNR as when clean, and at 0 dB all of it, with some of the pauses between sentences counted too (it found half before) |
| `scene` | scene cuts (the measure the H.264 encoder uses for IDR placement) and how much each window changes |
| `black`, `freeze` | black spans (0.5 s or more) and unchanging pictures (1 s or more) |
| `thumbnails` | a small picture when the content changes (64-bit perceptual hash, at most every 2 s) or every 10 s |

Every observation has `seq`, `kind`, `analyzer`, `stream`, `t` (seconds) and, for spans, `t_end`. A `digest` closes every
`digest_seconds` (10 by default) with each analyzer's figures for that window, and a `summary` comes last. `level`
chooses how much comes back: `digest` (digests and the summary: the fewest tokens), `events` (the default: plus cuts,
silences, spans, thumbnails, gaps) or `raw` (plus per-picture change scores and loudness once a second).
`--jsonl` prints each observation as a line the moment it is found. Thumbnails are written as PNG files with
`--thumbnails DIR` (`thumbnail_dir`), or returned to MCP clients as image blocks with `inline_thumbnails`.
`keyframes_only` (`--keyframes-only`, for live sessions too) decodes only the pictures that need no other (IDR and I
pictures, key frames), told from the bitstream: a fraction of the work for thumbnails, cuts and black spans at keyframe
resolution (a cut is reported at the first keyframe after it). `freeze` does not run then. A GPU decoder is not
drained after each keyframe, so with one (`hwaccel` auto on an NVIDIA machine) a picture can come out a few
keyframes late; its time is still right.
The kinds and analyzers are listed by `ironav capabilities` and are entities in the ontology (`analyzer/…`,
`observation/…`, related by `emits`).

Models run beside IronAV, not inside it. A **sidecar** is any program speaking JSON lines that whoever runs IronAV lists
in the file named by `IRONAV_SIDECARS`. A request names it (`"sidecars": ["asr"]`, `--sidecars asr`) and cannot start
anything else. A sidecar says what it wants (audio at its rate, optionally only while someone speaks, or a picture every N
seconds at a given width), receives it decoded, and answers with observations that join the others, with its text
gathered into the digests. Files wait for it; live sources drop inputs while it is behind, and a crashed or hung sidecar
is restarted with back-off. The protocol and the reference sidecars (speech-to-text with faster-whisper, object
detection with ONNX Runtime, and a model-free DTMF detector) are in [docs/sidecars.md](docs/sidecars.md).

Checked against EBU Tech 3341/3342 test signals (a -23 dBFS stereo sine reads -23.0 LUFS at 44.1 and 48 kHz; two levels
10 dB apart give a 10 LU range).

## Watching live sources (`watch`, `stream_*`)

The same analyzers run on media as it arrives. A source is a path, `path?follow` (a file that is still growing),
`path?pace=realtime` (a file played as if live), `-` or `pipe:NAME`, `tcp://host:port` (connect),
`tcp://:port` (listen, on loopback unless an address is given), or `udp://:port` / `udp://group:port` (multicast).
`srt://host:port` calls an SRT sender (an encoder, `srt-live-transmit`, ffmpeg in listener mode) and
`srt://:port?mode=listener` waits for one to call (on loopback unless an address is given); the MPEG-TS it carries is read
as `udp://` is. This is SRT's live mode, written here with no extra dependency: handshake version 5,
ACKs, NAK loss reports and retransmission, and delivery by the sender's timestamps plus a latency (`?latency=MS`, 120 by
default; a packet that has not come when a later one is due is dropped, so raise it for a lossy link or a heavily loaded
machine: at 100% CPU the default lost data in our tests where 1000 did not), `?streamid=ID`, keep-alives and shutdown.
`?passphrase=SECRET` (10 to 79 characters) reads an AES-CTR encrypted stream (AES-128, 192 or 256 by the sender's choice,
key material wrapped as RFC 3394, PBKDF2-HMAC-SHA1 key derivation); a sender that does not encrypt, or has another
passphrase, is refused, and the passphrase is hidden from every status and message. A caller is reconnected with back-off
like RTSP (`?reconnect=0` turns it off), and its address goes through `IRONAV_NET_ALLOW` like any other outbound
connection. Not supported: rendezvous mode, the message API, AES-GCM, forward error correction, groups and handshake
version 4 (libsrt before 1.3). Without a passphrase SRT is a plaintext transport, which FIPS mode refuses. It has been
tested against libsrt through FFmpeg 9.0.1, calling and listening, in the clear and with 16, 24 and 32 byte keys, and
with 3% of the sender's packets lost on the way (`cargo test -p ironav-live --lib interop_with_ffmpeg_libsrt -- --ignored`
with an FFmpeg built with libsrt on the `PATH`). It also sends, see "Sending streams out" below.
`rtsp://[user:password@]host[:port]/path` plays a camera or RTSP server: RTP interleaved on the RTSP connection
(or over UDP with `?transport=udp`, taken only from the server's address),
Basic or Digest credentials, keepalives and TEARDOWN, with H.264, HEVC, AAC, Opus, G.711 and L16 tracks (others are
skipped). The password never appears in output, and with Digest it is never sent.
An RTSP source (and `tcp://host:port?reconnect`) is kept up: when the connection drops, the server ends the stream, or
nothing arrives for twice `stall` seconds (`?stall=`, 5 by default), it is opened again on its own thread, waiting 0.5 s
and then twice as long after each failed attempt, up to 30 s, for ever or for `?retries=N` attempts (`?reconnect=0`
turns this off; a source that cannot be reached when the session opens is an error at once). The session's media time
goes on from where it stopped plus the time the source was gone, decoders wait for the next keyframe, and a source that
comes back with different tracks ends the session. `stream_status` has `source_health` (`connected`, `stalled`,
`reconnecting` or `failed`, since when, how many reconnects and failed attempts, and the last error), and the log has
`source_stalled`, `source_lost`, `source_restored` and `source_failed` as they happen.

`whip://:port[/path]` is a WebRTC ingest endpoint ([WHIP](https://www.rfc-editor.org/rfc/rfc9725)): OBS, FFmpeg,
GStreamer or a browser publishes to `http://host:port/path` and the session reads what it sends, H.264
(`packetization-mode=1`) and Opus, the same way it reads an RTSP camera (other codecs are declined in the answer). It
listens for HTTP on that port over TCP and for the media on the same port over UDP, on loopback unless a host is given
(`whip://0.0.0.0:8080/live`). One publisher at a time: it POSTs its SDP offer and gets the answer and a `Location`, and the
source ends when it DELETEs that, closes DTLS, or sends nothing for 10 s. The endpoint is ICE-lite with one host
candidate, and the media is DTLS 1.2 and SRTP (AES-128-GCM, or AES-128-CM with HMAC-SHA1-80; only AES-GCM in FIPS mode),
with the publisher's certificate checked against the fingerprint in its offer. It sends receiver reports, NACKs for
small gaps and picture loss indications until a keyframe comes. `?token=SECRET` makes every POST and DELETE carry
`Authorization: Bearer SECRET` (checked in constant time); without a token anyone who can reach the port can publish, so
set one whenever the endpoint listens beyond loopback. Publishers other than loopback ones are taken only from addresses
`IRONAV_NET_ALLOW` admits (the same list that limits outbound connections; a LAN encoder needs `private`). The HTTP
exchange of `whip://` is not encrypted (the media is), so the token and the offer cross the network in the clear.
`whips://:port[/path]?cert=CHAIN.pem&key=KEY.pem` is the same endpoint with its HTTP signalling served over TLS 1.3
(IronSocketLayer's server): `cert` is a PEM certificate chain, the server's certificate first, and `key` the private key
(PKCS#8 `PRIVATE KEY`, or SEC1 `EC PRIVATE KEY`; ECDSA P-256 or P-384, or RSA as PKCS#8), both read once when it opens.
Publishers then use `https://HOST:port/path`, and need to trust the certificate. Each connection gets its own handshake,
limited to 3 s, and one that fails or stalls is dropped without holding up the others; the request limits and the
connection limit are the same as for `whip://`. Use `whips://` beyond a trusted network. A web page publishes from
another origin only with `cors=ORIGIN` (such as `cors=https://studio.example.com`, or `*` for any), which adds the CORS
headers a browser asks for, its preflight included; without it a page elsewhere cannot publish to the endpoint. Headless
Chrome has published from a page to `whips://` in FIPS mode (AES-GCM throughout).

```sh
ironav watch "whip://0.0.0.0:8080/live?token=s3cret" --analyzers scene,loudness
# OBS 30+: Settings > Stream > Service "WHIP", Server http://HOST:8080/live, Bearer Token s3cret
ironav watch "whips://0.0.0.0:8443/live?cert=chain.pem&key=key.pem&token=s3cret" --analyzers scene,loudness
# the same over TLS: Server https://HOST:8443/live
ffmpeg -re -i in.mp4 -c:v libx264 -profile:v baseline -pix_fmt yuv420p -bf 0 -g 50 -c:a libopus -ar 48000 -ac 2 \
  -whip_flags dtls_active -authorization s3cret -f whip http://HOST:8080/live
```

FFmpeg's WHIP muxer (FFmpeg 8 and later) offers `a=setup:passive` unless given `-whip_flags dtls_active`; this endpoint is
always the DTLS server, so FFmpeg needs that flag. FFmpeg 9.0.1 logs "Failed to read response from DELETE" and "Failed
to dispose resource" when it stops, though its DELETE was answered and ended the source: its muxer does not read the
response's status line. FFmpeg 9.0.1 publishes to `whips://` (tested) only with a certificate its system trust store
accepts, since the muxer passes neither `-ca_file` nor `-tls_verify 0` to its HTTPS request. It cannot publish in FIPS
mode, whose SRTP must be AES-GCM: FFmpeg's DTLS offers only AES-128-CM with HMAC-SHA1-80. A browser offers AES-GCM.
OBS Studio's WHIP output (libdatachannel) publishes H.264 and Opus to `whip://` (tested with NVENC at 1280x720), and, like
FFmpeg, offers only AES-128-CM SRTP, so it cannot publish in FIPS mode either.

An ONVIF camera need not be known in advance. `onvif_discover` (`ironav onvif`) sends a WS-Discovery probe for video
transmitters and lists those that answer, with their device service URLs and their scopes (name, hardware, location).
`onvif_profiles` (`ironav onvif URL --user U --password P`) asks one camera for its maker, model and firmware and for
each media profile (Media service version 1, or version 2 when that is all the camera offers): the encoding, size,
frame rate and bitrate, the RTSP URI for `stream_open` (add the credentials to it) and a JPEG snapshot URI. Credentials
go as a WS-Security password digest dated by the camera's own clock, which it is asked for first. Its connections go
through the same allow-list as a source's.
`http://host[:port]/path` plays HLS when it names a playlist (the highest-bandwidth variant of those whose `CODECS` IronAV
decodes, no taller than `?height=` when given; TS or fMP4 segments with
`EXT-X-MAP` and byte ranges; a live playlist is followed from three segments before its end), and downloads anything
else as it arrives. Credentials in the URL (`http://user:password@host/...`) answer a Basic or Digest challenge (Digest
is preferred, SHA-256 over MD5, and its password is never sent; the answer is reused for the host's later requests).
A variant whose audio is a separate rendition (`EXT-X-MEDIA` with `TYPE=AUDIO`) plays with it: the
default one, or the one `?audio=` names by language or name (`?audio=fr`). The two playlists are fetched side by side
and their packets merged in time order; packed AAC with Apple's ID3 timestamp lines up with the video. Encrypted
playlists are not supported. A variant with WebVTT subtitles (`TYPE=SUBTITLES`) has them followed as well: the default
track, else the first, or the one `?captions=` names (`?captions=de`; `?captions=off` for none). Each cue becomes a
`caption` event (`t` to `t_end`, `data.text`, `data.language`) logged when the media reaches it, with tags removed,
`X-TIMESTAMP-MAP` applied, and rolling captions (each cue repeating the line before, as automatic captions do) cut to
what is new; if the track stops, a `captions_failed` event says why and the media goes on.
A YouTube URL (`youtube.com/watch?v=`, `youtu.be/`, `/shorts/`, `/live/`, `/embed/`) plays the video or live stream
through its HLS master playlist, found by [yt-dlp](https://github.com/yt-dlp/yt-dlp) (`yt-dlp -J`, from `IRONAV_YTDLP`
or the `PATH`), which keeps up with YouTube's player API; IronAV reads only its answer and fetches the playlist and its
segments itself, under its own name, a segment at a time as the session reads them. It does not imitate a browser or work
around YouTube's checks, and it keeps its requests few: one question to yt-dlp per video, kept until the URLs are about
to expire (a reconnect or a second session asks nothing), at least two seconds apart. When YouTube asks the address to
sign in ("confirm you're not a bot"), the error says so, and `?cookies=firefox` (or `chrome`, `edge`, ...) lets yt-dlp
use the caller's own signed-in browser session. `?height=480` caps the variant; AV1 variants are passed over. "Me at the
zoo" (`watch?v=jNQXAC9IVRw`) streams as H.264 320x240 and AAC stereo through every analyzer. YouTube's playlists list
the video's caption tracks (uploaded and automatic) as WebVTT subtitles, so they come as `caption` events too. A video with no HLS
playlist (only separate progressive files) is refused with a hint to download it first. Fetching from YouTube is
subject to its terms of service.
`https://` and `rtsps://` (port 322 by default) are the same over TLS 1.3, using IronSocketLayer, with IronPKI checking
the server's certificate chain and that it names the host in the URL (a DNS name or an IP address). The trusted roots
are the platform's PEM bundle (`SSL_CERT_FILE`, the usual Linux, BSD and macOS paths, or the one Git for Windows
installs; the Windows certificate store is not read) and `IRONAV_CA_FILE`, a PEM file of further roots, such as a
camera maker's or your own CA. Checking cannot be turned off. A redirect from TLS to plain HTTP or RTSP is refused, and so
is `rtsps://` with `?transport=udp`, which would send the media outside TLS. Servers that only speak TLS 1.2, as some
older cameras do, cannot be reached.
Outbound connections (and WHIP publishers from other machines) are refused unless whoever runs IronAV allows them with `IRONAV_NET_ALLOW`, checked after the name
resolves (and again at each redirect). The value is a comma-separated list of:
- `private` for cameras, encoders and servers on the local network (10/8, 172.16/12, 192.168/16, 100.64/10, 169.254/16,
  fc00::/7, fe80::/10). A LAN camera needs `IRONAV_NET_ALLOW=private`.
- `loopback` for this machine (127/8, ::1): `IRONAV_NET_ALLOW=loopback`
- `public` for internet hosts: `IRONAV_NET_ALLOW=public`
- `any` for every address, special-purpose ones included. It is the widest setting; prefer the classes above.
- addresses and CIDR ranges, such as `203.0.113.7, 198.51.100.0/24, 2001:db8::/32`

The cloud metadata endpoints (169.254.169.254, fd00:ec2::254, 169.254.170.2 and .23) are refused whatever is listed, `any`
included, unless `IRONAV_NET_ALLOW_METADATA=1` is also set. A refusal names the setting that would allow it. A request cannot
widen the list. A YouTube source is fetched by yt-dlp, which makes its own connections, outside this check.

`IRONAV_FIPS=1`, or a build with the `fips` cargo feature (`cargo build -p ironav-cli --features fips`), turns on FIPS mode. TLS then runs
IronSocketLayer's FIPS 140-3 profile, narrowed to AES-GCM suites, P-256 and P-384 key exchange, and ECDSA and RSA-PSS
signatures (RSA PKCS#1 v1.5 stays only for certificate chains). ChaCha20-Poly1305, X25519 and the hybrid groups are not
offered. Plaintext `http://`, `rtsp://`, `tcp://`, `udp://`, `whip://` and unencrypted `srt://` sources and outputs are refused, as are YouTube sources and MD5 Digest
authentication (SHA-256 Digest is still used). `whip://` is refused too, since its signalling is plaintext HTTP: use
`whips://` with `cert=` and `key=`, which serves it over the FIPS TLS profile and takes only the AES-GCM SRTP profile
(its media is DTLS-SRTP). `ironav capabilities` reports `"fips": true` and the TLS profile in force.
This restricts the algorithms and transports to FIPS-approved ones. IronCrypto, IronSocketLayer and IronPKI are not
CMVP-validated, so an IronAV build in FIPS mode is not a FIPS 140-3 validated module, and it should not be described as one.
Credentials: Basic (a password sent in base64, which any eavesdropper can read) is refused over plain `http://` and `rtsp://`, with a hint to use `https://` or `rtsps://`, or a camera that offers Digest, which does not send the password. Digest over plain HTTP or RTSP is allowed. Only when a camera or proxy on a network you trust needs Basic over plaintext, whoever runs IronAV can set `IRONAV_ALLOW_PLAINTEXT_BASIC=1`; a request cannot.
Readable as it arrives: MPEG-TS, Matroska/WebM, fragmented MP4 (CMAF), Y4M, WAV, ADTS AAC, MP3, Ogg (Opus, Vorbis, FLAC)
and raw H.264/HEVC (Annex B: the frame rate is the SPS's VUI timing, else 25, or `fps` when given; presentation times come from picture order, so B-frame streams copy right; the pixel format comes from the SPS). Ogg and MP3 are read the way internet
radio sends them: an ID3v2 tag in front (whatever follows it, MP3 or AAC, decides the reader), chained Ogg streams between
songs (the timeline carries on; a Vorbis song set up differently, even at another rate or channel count, has its
headers passed to the decoder, which starts over with them), and damaged pages stepped over. Matroska is read the way a browser's MediaRecorder, OBS or `ffmpeg -f matroska -`
sends it: Segment and Clusters of unknown size, no Cues. The packets are the ones the file reader gives, whatever pieces the
bytes arrive in. So `ironav watch tcp://:9000` can follow a webcam that a web page streams as WebM.
A camera or microphone on this machine is a source too, on Windows (Media Foundation), macOS (AVFoundation) and 64-bit Linux (Video4Linux2
cameras in YUYV, NV12 or I420 modes, or Motion JPEG when a camera offers nothing uncompressed, streamed through
memory-mapped buffers; ALSA microphones through `arecord` from alsa-utils, at 48 kHz mono; built and tested on Linux (Debian
under WSL 2) but not yet run with a real camera or microphone there; the macOS capture, an `AVCaptureSession` with NV12 pictures and PCM sound, is compile-checked for both Mac targets and has tests in the macOS CI job, but has not been run against a real camera or microphone, and asks for the Camera or Microphone permission in System Settings > Privacy & Security): `ironav devices` lists them, and `device://video` (the first camera), `device://video/1` or `device://audio/usb` (by a part of the name) open
one. `size=1280x720` and `fps=30` ask a camera for its nearest mode. A camera's pictures arrive as Y4M (converted from
the NV12 or YUY2 it gives to 4:2:0) and are timed at the mode's frame rate, or, from a Motion JPEG camera on Linux, as
live Matroska timed by the driver's clock and decoded by the JPEG decoder (which supplies the standard Huffman tables
such cameras leave out); a microphone arrives as 16-bit PCM. A device that sends nothing for 5 s once open (as when a video call
holds it) fails with `hardware-unavailable` rather than waiting forever.
A screen is a source too: `device://screen` is the primary display and `device://screen/N` display N as `ironav devices`
lists it, with its native size. It arrives as Y4M 4:2:0 (BT.709, limited range) at a constant rate: `fps=` (30 by
default, 60 at most), `size=WxH` (scaled down to fit, the aspect kept, both sides even; by default the display's own size,
but no wider than 1920 so that software encoding stays feasible) and `cursor=0` to leave the mouse cursor out. When the
screen has not changed, the last picture is sent again, so a still desktop does not look like a stalled stream. On Windows
it uses DXGI Desktop Duplication (run on a laptop's other GPU or in a remote-desktop session it may be unavailable, and
the error says so; the duplication is made again after a mode change or a UAC prompt, and the last picture is repeated
meanwhile). On Linux it reads the X server with `XGetImage`, loading libX11 (and libXinerama and libXfixes, for the
monitors and the cursor) when it is opened, so nothing is needed to build; a Wayland-only session is refused with an error
that says it needs the PipeWire portal, which is not supported yet. On macOS it uses CoreGraphics, which asks for the Screen
Recording permission in System Settings > Privacy & Security (a refusal says so) and never draws the cursor. The
Windows capture has been run on a real desktop; the Linux and macOS ones are compile-checked only. A screen shows
everything on the display, passwords and other people's messages included, so the agent tools treat it as a device
(`IRONAV_ALLOW_DEVICES`), and nothing opens it unless a source asks.
For testing without either, `test://video` and `test://audio` are made up at the pace they play: a grey ramp with a
white square crossing it each second and a new colour every 5 s, its frame number in its top row as 32 black or white
blocks of 8 pixels (`ironav_live::synthetic::frame_number` reads it back, so a test can tell which frame it has and how
late), and a 440 Hz tone on for half of each second. `size`, `fps`, `rate` and `duration` change them. The session's
`source` says `"synthetic": true` for them, so nothing downstream takes them for real footage.
Fragmented MP4 is read the same way: an initialization segment (`moov` with `mvex`), then `moof`+`mdat` pairs, as written by
LL-HLS and DASH packagers, Safari's MediaRecorder and `ffmpeg -movflags frag_keyframe+empty_moov`. Its packets also match
the file reader's. An MP4 without fragments cannot be read as it arrives (its index is at the end, or describes data not yet
sent) and is refused.

```sh
ironav watch udp://:5000 --analyzers scene,black --level events | head -n 20
some-encoder | ironav watch - --fps 30
```

`watch` prints each observation as a JSON line as it happens (with `wall`, the UTC time it was logged, and
`latency_ms`, from the packet's arrival), and then the final status and summary when the source ends or after
`--duration` seconds. With `--commands` it also reads one command per line on stdin and answers each with a line
`{"reply":"query","ok":true,"result":...}` among the observations: `status`, `query`, `configure`, `snapshot`, `clip`
and `close` run the matching `stream_*` tool on the session, written as JSON (`{"cmd":"query","kinds":["speech_end"],
"id":7}`, `id` echoed) or as `query kinds=speech_end newest_first=true max=1`. (Not with `-` as the source, since that
is stdin.)

```sh
ironav watch udp://:5000 --commands < commands.txt
```

Over MCP an agent opens a session and polls it. `stream_open` starts reading on its own thread and returns at
once. `stream_events {session, after, wait_ms}` is a long poll: it returns as soon as something new is logged, so
waiting costs nothing while the stream is quiet. Pass the returned `next_after` back as `after`. `stream_snapshot`
returns the newest picture, or the one at `at` seconds, as an image. `stream_clip` writes `from`..`to` of the
retained media (`retain_seconds`, 60 by default) to a file without re-encoding, starting at a keyframe.
`stream_status` reports the state, bytes, packets and latency percentiles, and in `stage_ms` where the time goes: for
the last 512 packets, the 50th and 95th percentile and the longest time from a packet's bytes arriving to the start of
its analysis (`demux`: demuxing, keeping it for clips, recording), decoding it (`decode`), and showing what it decodes
to to the analyzers and taps (`analyze`); each stream's `decoder` in `analyzed` says whether it is `software` or a GPU
decoder. `stream_close` returns the summary.
`stream_query` searches the log (the newest 20000 observations) by kind, analyzer, stream, media time and text, newest
first if asked ("when was speech last heard"). `stream_configure` changes a running session's level, digest interval,
analyzers and sidecars (an added sidecar starts in the background and joins once ready; a removed one finishes and
says its last). At most 8 sessions are open at once (error code `busy` past that), and one that no call has asked
about for `idle_timeout_seconds` (300 by default; 0 for never) is closed and forgotten.

A session can also record what arrives, without re-encoding (`record`, or `--record` for `watch`). The recording can be a
single file: a `.ts` stays playable even if the recording is cut off, while an `.mp4` is complete only once the session
closes. It can also be HLS: give an `.m3u8` path, and MPEG-TS segments are cut at the first keyframe after each
`segment_seconds` (6 by default) and written next to the playlist. The playlist is replaced whole after every segment, so a
player can follow the recording live. With `keep_segments: N` the playlist is a sliding window and older segments are
deleted; otherwise it keeps them all and is closed with `#EXT-X-ENDLIST` when the session ends. A source that the recording
format cannot hold, such as Y4M into `.ts`, leaves the analysis running and reports the reason under `record.error` in the
status. `stream_configure` starts a recording on a running session (`record`, with the same options), finishes the
one running (`stop_recording: true`), or both at once to go on in a new file; a single file started this way begins at
the next video keyframe, with its timestamps from zero. FFmpeg plays the playlists: a 12 s H.264/AAC stream recorded as three segments gives back all 300 pictures and 564
audio frames, with no decode errors.

A session can also encode its video again (`transcode`, or `--transcode OUT` for `watch`): the first video stream is
decoded, scaled to `width` if given, and encoded as low-latency H.264 (no B pictures, a keyframe a second; `qp` or
`bitrate_kbps`), written like a recording to a file or HLS, with the audio copied when the output holds it. It runs on
its own thread; when it falls behind, video is dropped up to the next keyframe and counted
(`transcode.dropped_packets`), so the session is never held up. The encoder is the software one, which suits reduced
sizes. `stream_configure` starts one on a running session (`transcode`, with the same options) beginning at the next
video keyframe with its timestamps from zero, finishes the one running (`stop_transcode: true`), or both.

```sh
ironav watch udp://:5000 --record live/stream.m3u8 --segment-seconds 4 --keep-segments 10
```

### Sending streams out

A session can send what it makes to another machine as MPEG-TS over SRT (`publish`, or `--publish srt://...` for `watch`):
the transcode's output when there is a `--transcode`, otherwise the source remuxed as `--record OUT.ts` would write it.
`srt://host:port` calls a receiver (ffmpeg, `srt-live-transmit`, a media server) and `srt://:port?mode=listener` waits for
one, on loopback unless an address is given. The options are `?latency=MS` (120 by default; also the size of the send
buffer, and a packet later than that is dropped, as libsrt does), `?streamid=ID`, `?passphrase=SECRET` with
`?pbkeylen=16|24|32` (AES-CTR, the sender makes the key and sends it wrapped as RFC 3394) and `?reconnect=0`. Payloads are 1316
bytes, lost packets are sent again on the receiver's reports, and the round trip time comes from ACK and ACKACK. The
destination of a caller goes through `IRONAV_NET_ALLOW` like any outbound connection, and FIPS mode refuses a destination
with no passphrase. A destination that cannot be reached, or that goes away, never stops the session: it is reported
(`publish_lost`, `publish_restored`, `publish_failed` events, and `publish` in the status) and a caller tries again with
back-off from 0.5 s to 30 s, resuming at a video keyframe. The session hands packets to a thread through a bounded queue,
so a slow receiver costs dropped packets (`publish.dropped_packets`), not a slow session. A listener has no one to send to
until a receiver comes, and what is made meanwhile is not kept. `publish` is set when the session opens; `stream_configure`
does not change it. From Rust, `ironav_live::open_sink(url)` gives a `Sink` (`write` MPEG-TS bytes, `finish`) for the same URLs.

A session can also publish to a WHIP endpoint (RFC 9725, the protocol OBS and FFmpeg publish with): `publish` takes
`whip://host:port/path?token=T` or `whips://host:port/path?token=T` (over TLS, the certificate checked against the platform's
roots and `IRONAV_CA_FILE`), and `--publish` takes the same for `watch`. The video goes as H.264 access units and the audio
as Opus packets. Length-prefixed H.264 (as MP4 holds it) is written in Annex B form, with the stream's parameter sets before
each keyframe. The video has to be H.264, so a source with other video needs `--transcode`, whose output is H.264. Other
audio is not sent, and a `publish_audio_skipped` event says so. The token goes only to the endpoint's host and port and is
redacted wherever the URL is shown. FIPS mode refuses `whip://` (its signalling is plaintext) and takes `whips://`. A caller
goes through `IRONAV_NET_ALLOW` as SRT does, and the same events, back-off and status apply. The server's requests for a
keyframe are counted as `keyframe_requests` in the status. With a transcode (`--transcode`, or `transcode` in `stream_open`),
the transcoder makes an IDR picture for each, with the parameter sets ahead of it and the GOP count starting again from
it, at most one per half second (`keyframes_forced`); a request that comes while one is waiting, or that is still waiting
when the session ends, is counted as `keyframes_ignored`. Without a transcode nothing can be forced: the requests are
counted, and `keyframes_forced` stays 0. Tested end to end against
IronAV's own WHIP endpoint with H.264 and Opus (the Opus goes from the source's first packet, not from its first keyframe);
also against MediaMTX v1.21.3 on loopback (plain `whip://`, and `whips://` with a throwaway CA), which answers `a=setup:active` and
checks the publisher with ICE of its own: the publisher is then the DTLS server (the answer's `passive` still makes it the DTLS
client) and answers MediaMTX's STUN checks. Not yet against OBS, FFmpeg or a browser as the receiver.

```sh
ironav watch rtsp://cam/stream --publish "whips://relay.example:8443/live?token=SECRET"
ironav watch rtsp://cam/stream --publish "srt://relay.example:9000?streamid=cam1&passphrase=0123456789abcdef"
ffmpeg -i "srt://:9000?mode=listener&passphrase=0123456789abcdef" -c copy out.mkv   # the receiving end
```

Code in the same process (a detector, a speech model) can take decoded pictures or sound from a session with a tap,
rather than as observations or through a sidecar's pipe:

```rust
use ironav_engine::tap::{PixelLayout, TapConfig, TapData};
let session = ironav_engine::live::open(&args)?;
let tap = session.tap(TapConfig { fps: Some(5.0), width: Some(640), height: Some(640), letterbox: true, layout: PixelLayout::Bgr, ..TapConfig::video() })?;
while let Some(f) = tap.recv_timeout(Duration::from_secs(5))? {
    if let TapData::Video { width, height, pixels, .. } = f.data { /* f.seq, f.t, f.decoded_at */ }
}
```

A tap made this way joins with the next packet, so it can miss a fast source's first frames; taps given to
`live::open_with_taps(&args, vec![config, ...])`, which returns the session and its taps, are attached before the
session reads anything and see its first picture and sound.

A video tap gives at most `fps` pictures a second (every one when not given), scaled to fit `width` x `height` (aspect
kept) and, with `letterbox`, padded to exactly that size (centred, or `placement: TopLeft` as YOLOX-style detectors
take it; padding `pad`, 0 by default, 114 for those), as RGB, BGR or grey bytes. An audio tap
(`TapConfig::audio()`) gives mono windows of `window_ms` at `rate` (16 kHz unless asked), starting every `hop_ms` (so
windows overlap when it is shorter), and at the end of the stream a last, shorter window with what is left; with `features: true` each window comes with its RMS and peak level in dBFS,
spectral centroid and 85% roll-off in Hz, spectral flatness (near 1 for noise, near 0 for a tone) and the share of its
20 ms frames that sound like speech: loud in the speech band over an adaptive noise floor, with a pitch (or few zero crossings), and among loud frames that vary as
syllables do over the last second, as the `voice_activity` analyzer takes them, each frame for itself with no hangover,
so silence, a steady tone or hum, and hiss read 0 (the spectrum from `ironav_codec::voice::SpectrumMeter`). It takes the stream given as `stream` or the first of its kind, starting its
decoder if nothing else needed one; if none suits, the first `recv_timeout` says why. The session never waits for a
tap: each keeps `queue` items (1 unless asked: the latest wins) and drops the oldest when full, and `stats()` counts
what was made, taken and dropped. Each item has a sequence number counting every item made, so a reader sees a gap; the
media time `t` in seconds from the source's timestamps (for RTSP, the RTP clock from the start of the stream);
`decoded_at`, the wall-clock time it was decoded; and `captured_at`, when it was captured by the sender's clock, for an
RTSP source whose server sends RTCP sender reports (each report ties the track's RTP clock to its NTP time; the latest
is used, and `stream_status` shows it as `sender_clock`, the Unix time of media time 0 per stream). Keyframe pictures
(below) carry the same capture time. Dropping
the tap stops it; once the session ends, `recv_timeout` returns an error after the last item.

With `keyframe_jpegs: N` a session keeps the pictures of the newest N keyframes of its first video stream as JPEG
(quality 85, scaled to `keyframe_width` if given), encoded as each keyframe is decoded, so the evidence for an alert is
at hand without decoding anything again: `stream_keyframes` gives the newest `max` (or those after media time `after`)
with their media time and when they were decoded, and `Session::keyframes` the same in Rust.

What is built and what is still planned is in [docs/design/agentic-realtime.md](docs/design/agentic-realtime.md).

## Ontology

Generated from the live registries, never hand-maintained in parallel. Kinds: codec, container, pixel format,
sample format, filter, hardware backend, platform, operation (each tool), analyzer and observation. Relations: `carries`, `accelerates`,
`availableOn`, `usesSampleFormat`, `acceptsPixelFormat`, `producesPixelFormat`, `emits`.
Export: `ironav ontology jsonld` (also an MCP resource).

## Hardware acceleration

`ironav-hwaccel` models CUDA/NVENC, VA-API, Quick Sync, VideoToolbox, D3D11VA, AMF, Vulkan Video, MediaCodec and
OpenCL across Windows, Linux, macOS, Android and iOS: capability matrix, driver detection, and policy-based selection
(`auto`, `off`, `require`, or a named backend) that explains every rejected candidate.
A backend is *usable* only if its driver is detected **and** a native provider implements the codec and direction.
Today there is one vendor: **NVIDIA, NVENC encode of H.264 and HEVC and NVDEC decode of H.264, HEVC and AV1** (Windows and Linux). Every other
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
ironav convert -i in.ts  -o out.ts   --vcodec h264 --vbitrate 2000 --latency low   # for live delivery: no B-frames, IDR each second
ironav convert -i in.mp3 -o out.mka --acodec opus --abitrate 96   # Opus at 96 kbit/s (resampled to 48 kHz automatically)
```

The MP4 writer is constant-frame-rate, and puts the index in front of the media data (fast-start) by copying the finished file once,
so writing costs one extra pass over the output. Verified by opening the files with FFmpeg's MP4 demuxer: correct codec, dimensions, 30 fps,
exact 8.000 s duration, all frames decoded, and seeking lands on the right keyframe.

### Reading MP4 and decoding on the GPU (NVDEC)

`ironav probe`, `convert` and the other tools read MP4 (regular files with the index anywhere, and fragmented files). H.264/HEVC video is decoded by NVDEC through the driver's `nvcuvid` library, loaded at run time like NVENC;
the output is cropped to the display size and returned as planar `yuv420p` in display order, so B-frame streams work.
8-bit 4:2:0 H.264 goes to the GPU (10-bit H.264 is decoded in software, as NVDEC cannot take it); for HEVC, 8-bit and 10-bit 4:2:0, 4:2:2 (`yuv422p`, `yuv422p10le`) and 4:4:4 (`yuv444p`, `yuv444p10le`; including the lossless transform-bypass mode) and monochrome (8-bit comes back as `gray`) are accepted (10-bit 4:2:0 comes back as `yuv420p10le`); 12-bit HEVC (`yuv420p12le`, `yuv422p12le`, `yuv444p12le`) goes to the software decoder; interlaced video is refused with a clear error. The GPU cannot take 4:2:2, 4:4:4 or monochrome, so those use the software decoder even with `--hwaccel auto`.
HEVC also has a software decoder (`--hwaccel off`, or automatically where the GPU cannot take a stream): Main and Main 10 (and monochrome, 4:2:0, 4:2:2 and 4:4:4 at 8, 10 and 12 bits; monochrome deeper than 8 bits comes out as 4:2:0 with flat chroma) with I, P and B pictures,
merge and AMVP motion prediction, temporal motion vectors, weighted prediction, wavefronts, multiple slices, default scaling lists, transform skip, lossless (transquant bypass),
QP deltas, constrained intra, the deblocking filter and sample adaptive offset. These are checked sample-for-sample against FFmpeg on x265 streams (the
`hevc_streams_decode_to_what_ffmpeg_decodes` test and a wider sweep of encoder settings), and on NVENC streams too (Main and Main 10,
B pictures used as references, intra refresh, weighted prediction, 4:4:4, ultra-low-latency and lossless tuning). Scaling lists sent in
the SPS (an x265 matrix file, at 8 and 10 bits and 4:4:4) match as well, and so does joining an open-GOP stream at a CRA picture: its
RASL pictures are dropped and the rest are the pictures the whole stream gives from there (the `hevc_joined_at_a_cra_picture_drops_its_rasl_pictures`
test, checked against NVDEC too). The JCT-VC conformance suite (the HEVC_v1 bitstreams) was decoded too: 146 of its 147 streams
decode to the official MD5 of every picture, covering tiles, PCM, long-term reference pictures, wavefronts with dependent slices,
entry points, bumping and `no_output_of_prior_pics_flag`, RASL pictures after an end of sequence, parameter set changes and conformance
windows (that run found and fixed long-term pictures without an MSB, a wavefront row start in a one-CTU-wide picture, and end of sequence
NAL units being skipped). The one left, `TSUNEQBD_A_MAIN10_Technicolor_2`, has 10-bit luma with 9-bit chroma, which is refused. 12-bit 4:2:0, 4:2:2, 4:4:4 and monochrome x265 streams
(with B pictures, weighted prediction and scaling lists) match FFmpeg too; checking them found SAO offsets scaled by the first
edition's `bitDepth - 10` rule, which the range extensions (the only profiles with 12 bits) replaced, and that is fixed.
4:2:2 and 4:4:4 (the range extension profiles' chroma formats) were checked the same way: 60 x265 streams, 4:2:2 and 4:4:4 at 8 and 10 bits,
covering intra-only and B-frame coding, noise, presets from ultrafast to slow, deep transform trees, AMP, weighted prediction,
transform skip, lossless and per-CU lossless coding, QP 0 and 51, chroma QP offsets, scaling lists, constrained intra, wavefronts with slices,
16x16 CTUs and odd sizes. All match on every frame, and four small clips are kept in that test. The range extension *coding tools*
(implicit and explicit RDPCM, cross-component prediction, extended precision, persistent Rice adaptation, chroma QP offset lists and the rest)
are not implemented, and x265 does not use them. An SPS or PPS that turns one on is refused with the tool's name. To find those flags the SPS is now read through its VUI.
With 16x16 CTUs and 4:2:0 or 4:2:2 chroma, sample adaptive offset after deblocking is where FFmpeg 9.0.1 and this decoder part:
FFmpeg filters one chroma block before the next block's chroma edges are deblocked, so now and then a corner or edge sample takes
a different offset (its development tree has since fixed part of this, after 9.0.1). This decoder deblocks the whole picture
first, as the specification orders, and matches NVDEC on every sample of every such 4:2:0 stream tried (the
`hevc_sao_on_16x16_blocks_decodes_to_what_hardware_decodes` test keeps one); 4:2:2, which this GPU cannot decode, was checked by
hand at the first differing sample.
10-bit HEVC GPU decoding was compared with FFmpeg's software decoder on a 60-frame 720p Main 10 clip: every sample of every
frame is identical. 10-bit H.264 (High 10) is decoded in software only, and NVENC does not encode 10-bit H.264 (consumer NVENC cannot; the software encoder does, see the H.264 section).

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
VP9 and AV1 can be copied into an MP4 as well: VP9 gets a `vpcC` box built from its first key frame (profile, level, bit
depth, chroma layout), AV1 keeps its `av1C` record and loses its temporal delimiter and padding OBUs, and the key frames are
listed in `stss` (see "Matroska output" for how the copies were checked).
Fragmented MP4 (`moof`/`traf`/`trun`, as made by streaming tools and `-movflags frag_keyframe`) is read too, with or without
`default-base-is-moof`; checked on two fragmented files made by FFmpeg, which copy to the same frames, times and audio samples.
Fragmented MP4 is written too, with `--fragment-seconds S` (or `fragment_seconds` in a job) in MP4 and MOV: the index first, then a
`moof` and `mdat` per fragment, cut at a key frame once S seconds have passed. The copy of the B-frame Matroska clip reads back
packet for packet, and FFmpeg decodes it to the same pictures and samples as the index-first copy.

### Reading Matroska and WebM

`.mkv`, `.webm` and `.mka` files are read: the elements are walked once to find every frame (simple blocks, block groups,
Xiph/EBML/fixed lacing, clusters of unknown size, a file cut short mid-cluster), and the frames then go through the same
reader as MP4, so probing, hardware decoding, seeking, playing and copying H.264/HEVC/AAC into an MP4 all work on them.
Matroska keeps presentation times only, so for B-frame video the decode times are taken to be the sorted presentation
times. Audio whose frame length is known (AAC, MP3, Opus, anything with a `DefaultDuration`) gets a sample-rate time base, so its
times are exact despite the file's millisecond ticks.
Checked on files remuxed by FFmpeg: an x264 file with B-frames and AAC and an HEVC file copy into MP4 with the same frames
(identical pixels when decoded by FFmpeg) and audio samples; the 1080p H.264 file decodes on the GPU to the same 50 frames
as FFmpeg's decoder; AAC in a `.mka` decodes to within 1 LSB of FFmpeg's. Opus in Matroska decodes too (see below), and so do VP9 and
AV1 in WebM (see "VP9" and "AV1").
Content encodings are undone frame by frame: header stripping and zlib compression are read (checked with FFmpeg, which reads
the same files to the same samples), while encryption, bzip2 and lzo are refused, and so is a live Matroska stream that has any
content encoding. Not handled: chapters, tags, cues
and attachments are ignored; subtitle tracks are skipped (for what Matroska output holds, see "Matroska output" below). A copied track starts at its own first
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

**10-bit and 12-bit.** `yuv420p10le` (16-bit little-endian words, value in the low 10 bits) is a pixel format in its own right: Y4M
(`C420p10`), `scale`, `hflip`, `format`, the MP4 reader and writer (`hvcC` carries the depth) and the player all handle it, and
so are the 4:2:2 and 4:4:4 layouts and the same three at 12 bits (`yuv420p12le`, `yuv422p12le`, `yuv444p12le`, Y4M `C420p12` and so
on). `format` changes depth within a layout directly (rounding down, replicating the top bits up, so full scale stays full scale);
anything else goes through 8-bit, so converting to RGB or another layout loses the extra precision. `gray` does not take 10-bit input. An NVENC Main 10 encode of the
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

The lossless audio MP4 can hold here is FLAC (the standard "FLAC in ISO BMFF" mapping) or ALAC (see "ALAC" below). FLAC plays in FFmpeg-based
players, VLC, Chrome and Firefox, but **not in QuickTime or most hardware players**; AAC is the codec those expect, and `--acodec aac` writes it (see the AAC encoder). Checked with FFmpeg's decoders: a Y4M + WAV conversion gives an 8.000 s file whose 240 video
frames decode and whose 352,800 audio samples are identical to the WAV, with the two tracks never more than 0.46 s
apart in file order.

Validated by decoding the output with FFmpeg's decoders (via PyAV) and comparing every frame to the source: 240/240
frames, correct order, High/Main profile, PSNR rising monotonically as QP falls, chroma bit-exact at QP 0. The struct
layouts were generated from NVIDIA's `nvEncodeAPI.h` (SDK 13.1) with clang, and the tests pin their sizes. Tests that
need a GPU skip themselves when none is usable. Not done on NVENC: hardware scaling, 10-bit H.264, 12-bit, 4:2:2/4:4:4, AV1, rate-control
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

## ALAC

Apple Lossless, implemented from the bitstream that Apple's reference decoder and FFmpeg read and write (no code from
either). Decodes one to eight channels at 16, 20 and 24 bits, and mono at 32: adaptive predictor, adaptive Rice codes with zero runs,
stereo decorrelation, the split-off low bits, and uncompressed frames. Encodes one to eight channels at 16 and 24 bits with a
second-order predictor, the best stereo mode per pair, and an uncompressed frame where compression does not help. Channels
go in the elements the layout asks for (single channels and pairs, the LFE as a single channel), in the usual order
(front left, front right, centre, LFE, back left, back right, ...), as FFmpeg reads and writes them. Written and read in MP4 (`--acodec alac -o out.m4a`, the `alac` sample entry) and in MOV.

**Conformance**: FFmpeg's `-c:a alac` output decodes to the source PCM bit-exactly at 16 and 24 bits, mono, stereo, 5.1 and
8 channels, and FFmpeg decodes IronAV's output bit-exactly in those cases; IronAV to IronAV is bit-exact too. These tests are in
`crates/ironav-engine/tests/alac.rs` and need `ffmpeg` on the PATH (they skip without it).

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

`--acodec aac` (`--abitrate 8` to `512`, default 64 per channel) writes AAC-LC into MP4/M4A, ADTS (`.aac`) or Matroska, with one to
six or eight channels at any of AAC's sampling rates (others are refused with the list). The channels are in FFmpeg's order (front
left, front right, centre, LFE, back left, back right, then side left and side right for 7.1), and they go into the standard
channel configurations: 5.1 is six channels, 7.1 is eight, and seven has no standard layout, so it is refused. Sine windows, with **window switching**: a block whose
first difference jumps (a 128-sample piece with 8x the energy of the four before it) becomes eight short windows, each in a group of
its own, with start and stop windows around it, which costs one frame of look-ahead. On a noise burst over a quiet tone the error in
the 900 samples before the attack fell to a third (test), and FFmpeg decodes a stereo file with four such attacks to the same samples as
ours (136 dB). The detector is a simple energy rule, not tuned on music, and short blocks do not group windows, so they cost more side
information than they might. The decoder's own Huffman
and scalefactor-band tables, the cheapest of the eleven spectral codebooks chosen per band, sections merged, and mid/side per band
where it looks cheaper. A feedback loop on the quantiser's step holds the bit rate (the first frames search for the right step), and the
step follows each band's level partway (exponent 0.6), so noise is shaped but there is no psychoacoustic model. No TNS, PNS, intensity stereo or prediction, no bit reservoir. Each channel pair gets its own mid/side choice, and the LFE is coded
like any other channel, without a low-pass.

The stream starts with one frame of priming: in MP4 an edit list says so (our reader and FFmpeg skip it), in ADTS and Matroska it
stays as 1024 leading samples, as with most encoders. MP4's edit list also ends the audio at the input's length, but players that ignore it
(FFmpeg does) show up to one padded frame at the end. FFmpeg (through PyAV) decodes all three containers; on a 4 s stereo test signal
(two tones and a little noise) 128 kb/s gives 16 dB SNR and 256 kb/s 25 dB, with white noise taking its share of the bits. On one tone per channel at 48 kHz
(1 s), FFmpeg decodes our 5.1 MP4 and 5.1 and 7.1 ADTS files with the matching layouts, each tone on its own channel, at 33 to 36 dB
SNR (checked by hand; the committed tests compare our decoder with FFmpeg's on a 16 kHz 5.1 and 7.1 pair). A 120 s MP3 becomes
128 kb/s AAC in about 1 s.

## Opus

Opus **decoder** (RFC 6716): SILK, CELT and hybrid frames, every frame size from 2.5 to 60 ms, mono and stereo (and multichannel, see below), packets of
all four framing codes, CELT redundancy frames, SILK in-band FEC (LBRR) copies, and the output gain and pre-skip from the `OpusHead` (the pre-skip is
dropped, so a file starts where its audio does). The band-energy, bit-allocation, pulse-vector, inverse-MDCT, linear-prediction
and excitation stages follow FFmpeg's native Opus decoder, with its tables (`tools/gen_opus_tables.py` reads them from
`libavcodec/opus/tab.c`, so none were typed in). The 8/12/16 kHz SILK output is brought to 48 kHz by libopus's own
resampler (`silk_resampler`, translated from libopus's `silk/resampler*.c`: 16-bit fixed-point arithmetic, the up2 allpass
pair and a 12-phase interpolation filter), with the delay that lines its output up with libopus's decode (0, 4 and 7 input
samples at 8, 12 and 16 kHz).

**Checked against FFmpeg's native Opus decoder** (through PyAV) on fifteen streams made by libopus, plus a pure sine in CELT
and in SILK: SILK narrow band at 20 ms, medium band at 10 ms and wide band at 20, 40 and 60 ms (mono and stereo); hybrid at 10
and 20 ms; CELT at 2.5, 5, 10, 20 and 40 ms, mono and stereo. **Mono CELT frames match to float rounding (about 138 dB SNR).** On stereo streams with collapsed bands FFmpeg's native decoder orders the anti-collapse noise differently from libopus, so the stereo CELT figures below are against libopus.
Those SILK figures are from before the resampler was libopus's, and FFmpeg's native decoder is itself a few samples late of
libopus in SILK, so the libopus comparison below is the one to read.

**SILK and hybrid against libopus** (`ffmpeg -c:a libopus`, the reference implementation), on 64 streams made by libopus from a
speech-like signal: narrow, medium and wide band SILK at 10 to 60 ms, hybrid at 10 and 20 ms (longer packets hold several frames), mono and stereo, at 8 to 64 kb/s.
The output lines up with libopus's sample for sample in every mode. Mono SILK and hybrid differ from libopus by a mean of 1 to
7 LSB (SNR 45 to 63 dB), with the largest differences, up to 90 LSB, in loud voiced passages: SILK's synthesis is done in
floating point here and in fixed point in libopus, and those passages are where the difference shows. In quiet passages of the 8 kHz
stream about two thirds of the samples are exact. Stereo SILK has a mean of 1.5 to 4.6 LSB. **Stereo CELT** (2.5 to 20 ms, 32 to 256 kb/s, tones, correlated channels and noise bursts with tremolo) decodes to within 1 LSB of libopus, as mono does. The fault was the order of the anti-collapse noise: libopus draws it band by band, both channels in turn, and this decoder drew it channel by channel (as FFmpeg's native decoder still does). **Stereo hybrid** then matches on its CELT bands: on tonal input it is exact (about 106 dB), and on speech-like input it peaks at 41 LSB (117 LSB on the test asset), a broadband error 35 to 45 dB below the signal that is most likely SILK's (not isolated). On a harmonic-tone signal, where
each channel has its own pitch (`opus_silk_and_hybrid_in_every_bandwidth_decode_like_libopus`, which runs every mode), tones are
harder for SILK: mono SILK is 38 to 50 dB against libopus, and stereo SILK 27 to 53 dB, with peaks of 3267 LSB at wide band
and 40 or 60 ms; the stereo hybrid is exact there (about 100 dB, worst 1 LSB). The stereo SILK case is not isolated (the
stereo prediction is the suspect). Every mode lines up with libopus at lag 0. The regression tests are libopus's decode of the CELT streams (mono and stereo, within 2 LSB; stereo CELT across frame sizes, bitrates and signals in `opus_multichannel.rs`), and libopus's decode of the SILK and hybrid ones (above 35 dB, 25 dB for stereo).
**Multichannel Opus** (channel-mapping families 1 and 255, in Ogg, Matroska, WebM and MP4 `dOps`): each stream is decoded as
above and routed by the mapping table; family 1 comes out in WAV and FFmpeg channel order, as the Vorbis decoder does. FFmpeg's
libopus files of 4, 6 and 8 channels (family 1) decode to within one LSB of libopus where their streams are CELT. A family 255 file
(6 channels) matches its source tones at about 40 dB; FFmpeg's libopus wrapper reorders family 255 channels, which RFC 7845 does
not ask for, so its decode is not the reference there. Its SILK streams (voip mode, 4 to 8 channels) decode to libopus's within 60 LSB at worst, above 50 dB on every channel
(`ffmpeg_silk_surround_opus_decodes_like_libopus`).

Not covered: the way libopus clears its SILK state and resampler when a stream switches from CELT to SILK; ours starts
from cleared state, so the few samples at such a switch can differ. The libopus encoder
only produced streams in one mode at a time; mode switches and their redundant CELT frames are covered by IronAV's own
automatic-mode streams instead, which this decoder and libopus both decode (see the encoder's automatic mode below).
Opus is read from Matroska/WebM (`.mka`, `.webm`), from Ogg (`.opus`, `.ogg`) and from MPEG-TS (see "MPEG transport streams"); for writing see "Opus encoder" below. The Ogg reader (pages are checksummed; the file is read into memory, up to 1 GiB) reads every logical stream: multiplexed Opus, Vorbis or FLAC streams are separate tracks, and chained Vorbis or FLAC links with the same setup are one track (the timeline carries on); chained Opus links are separate tracks, each with its own pre-skip. Speex and Theora in Ogg are refused with a message naming the codec when a file holds nothing else, and passed over otherwise. The final page's granule position shortens the last packet, so the output ends where the file says (for Opus the length is the granule minus the pre-skip). Tested on libopus files remuxed to Ogg (identical samples to the Matroska copies); multi-page packets are covered by a synthetic unit test only. FLAC in Ogg (the native-FLAC mapping, as FFmpeg's muxer writes it) decodes bit for bit to the PCM it was made from.


### Ogg Opus, Vorbis and FLAC output

`.opus` (and `.ogg`/`.oga`) is written for Opus, copied from any file that holds it or encoded: `OpusHead`, `OpusTags`, then pages of about
half a second whose granule positions follow RFC 7845 (the packets' durations, pre-skip included, add up to the last one's, so
the stream ends where it should). FLAC is written in Ogg too (`--acodec flac -o out.oga`, native mapping): STREAMINFO in the first
page is rewritten when the stream ends, so it carries the real length and frame sizes. Encoded and re-decoded it is bit-exact,
and FFmpeg reads the same file bit-exact. Vorbis is written in Ogg, copied from Ogg or Matroska or encoded (see "Vorbis encoder"): pages are stamped from each packet's
window size, an Ogg source keeps its exact length (FFmpeg decodes the copy sample for sample), and a Matroska source ends up to a
window (48 samples in the test file) long, because Matroska does not record where the audio stops.

### Opus encoder

`--acodec opus` writes CELT-mode Opus by default: 48 kHz mono, stereo or 3 to 8 channels (channel-mapping family 1, with the RFC 7845 stream layouts and the bit rate shared among the streams by channel count; other rates are resampled first, with a warning), 20 ms
frames, constant bit rate (64 kb/s per channel). `--opus-mode celt|silk|hybrid|auto`, `--opus-signal auto|voice|music`
(for auto), `--opus-frame-ms N` (2.5 to 60), `--opus-fec PCT` and `--opus-dtx` select the other modes, frame lengths,
in-band FEC and DTX (the job JSON takes them as `audio.opus`, with the same names as `mode`, `signal`, `frame_ms`,
`fec_loss_percent` and `dtx`; `audio.opus` is refused for any codec but opus). For example
`ironav convert -i in.wav -o out.opus --acodec opus --abitrate 24 --opus-mode silk --opus-frame-ms 60 --opus-fec 10 --opus-dtx`.
It goes into `.opus` (Ogg), `.mka`, `.mkv` and `.webm` with the `OpusHead` and the
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
is only a simple transient detector (a frame with a sudden attack is coded as eight short blocks; on a test of decaying hits that is +1.3 dB SNR),
no band boosts or tuned allocation trim, and CELT mode is constant bit rate, so at the same size it will sound
worse than libopus. The end of a stream is exact in Ogg (the last page's granule position) and in Matroska/WebM (the last block carries a
`DiscardPadding`, which our reader honours); FFmpeg decodes both to exactly the source's length.

For a voice link, frames can be 2.5, 5 or 10 ms instead of 20 (`OpusEncoder::with_frame_ms`), and a caller with its own
framing codes one frame at a time into its own buffer (`encode_frame_into(pcm, out) -> len`), which allocates nothing
after the first frame (a counting allocator checks it). `delay_ms()` gives the delay the encoder adds: a frame plus
the 2.5 ms look-ahead, so 12.5 ms at 10 ms frames and 7.5 ms at 5 ms. On one second of a speech-like test signal at
32 kb/s, libopus (through PyAV) and FFmpeg's own decoder both decode every frame length to within 2e-5 of ours, at
21.7, 22.7, 20.4 and 3.7 dB SNR for 20, 10, 5 and 2.5 ms. libopus's own encoder (CELT, constant 32 kb/s) gets
28.3, 26.5, 25.3 and 16.2 dB on the same signal: this encoder is 4 to 12 dB behind it, most of all at 2.5 ms.

The pitch pre-filter takes a voice's (or an instrument's) pitch out of the signal before the transform, with a comb
filter the decoder's post-filter undoes, so the bands are not spent on its harmonics. The period is found by
correlation on a half-rate copy, refined at the full rate and checked against its fractions so a multiple of the true
period is not taken; the gain and the thresholds for turning it on are libopus's. It is on in frames of more than 12
bytes per channel, and lifted the speech-like test from 18.9 to 21.7 dB at 20 ms and from 20.5 to 22.7 dB at 10 ms (at 5
and 2.5 ms the frames are too small for it to pay its side information). libopus's allocation trim, which follows the
spectrum's tilt, was tried and measured 1 to 4 dB worse here, so the trim stays even. `cargo test -p ironav-engine --test opus_interop -- --ignored` with
`IRONAV_OPUS_OUT=dir` writes the streams for such a check.

For speech at low rates there is SILK mode (`OpusEncoder::with_mode(OpusMode::Silk)`): wideband (audio to 8 kHz),
mono or stereo, frames of 10, 20, 40 or 60 ms (see the packets of 40 and 60 ms below), at a variable rate that averages the one asked for
(about 8 to 30 kb/s). It is written from the decoder, like the CELT side: LPC by autocorrelation and Levinson, turned
into NLSFs and quantized with the two-stage codebook and its backward prediction; an open-loop pitch search on the LPC
residual, checked against fractions of the lag, with the per-subframe contour chosen jointly; five-tap long-term
predictors from the three codebooks; subframe gains from the prediction residual and a step size the rate control moves
(and raises until a frame fits its packet). The pulses are chosen in closed loop by running the decoder's own synthesis,
so the two never drift apart, with the coding noise fed back so that it follows the spectral envelope (`1/A(z/0.94)`)
and, for 70 % of the long-term prediction, the pitch harmonics; without that feedback the pulses spend their bits
cancelling the predictors' own noise and the rate stops falling as the steps grow. The delay is 4.3 ms of look-ahead
and resampling plus the frame (`delay_ms()`), and `head()` carries the matching pre-skip. On one second of the
speech-like test signal at 16 kb/s, libopus decodes the stream at 24.3 dB SNR (20 ms frames) and 21.9 dB (10 ms), and
FFmpeg's decoder at 25.4 and 22.0; the CELT stream at 32 kb/s gets 21.7.

Stereo SILK is coded as the decoder takes it apart: mid (the mean) and side (half the difference), with the side's
prediction from the mid taken out. The decoder applies two weights, on the mid's low-passed part and on the mid itself,
interpolated from the last frame's over 8 ms; the encoder fits each band on its own (a fit of both at once ran away where
the mid has little treble and left more side than there was) and keeps the weights at zero when that leaves less. The
side gets a share of the bits by how much of it is left, and a side with nothing left (a mono or panned source) is not
coded at all, so a panned signal costs what mono does (24.9 dB in both channels at 32 kb/s, against 24.9 for mono). On
a signal with a delayed copy and a tone on the right, libopus decodes our stereo SILK at 19.6 and 18.2 dB (32 kb/s) and
our stereo hybrid at 19.4 and 17.9 dB (40 kb/s). Hybrid's CELT side is stereo with it.

Packets of 40 and 60 ms (`with_frame_ms(40.0)`, `60.0`) hold two or three 20 ms frames. In SILK mode they are one
SILK frame of 40 or 60 ms (configurations 10 and 11), as libopus codes them: the frames after the first code their
first gain and their pitch lag as steps from the frame before, which saves a few bytes a packet. Hybrid and CELT
packets are code 3 packets of 20 ms frames, with the frame sizes stated only when they differ. libopus decodes them
at the 20 ms streams' SNR (SILK: 22.2, 24.7, 25.3 and 25.8 dB at 10, 20, 40 and 60 ms).

In-band forward error correction (`with_fec(loss_percent)`, SILK and hybrid): each packet also carries a coarser copy
(LBRR) of the speech of the packet before it, coded as libopus codes it, a few quantizer steps coarser by the expected
loss, and kept only where it is speech and fits in 60 % of the frame's bits. A receiver that has lost a packet and holds
the next one rebuilds the lost one from the copy (`OpusDecoder::decode_fec_into(next, out)`, then `decode_into(next,
out)` as usual), as libopus's decoder does with FEC on. In a 40 or 60 ms SILK packet every frame has its copy; in a
hybrid packet of several frames, only the last. The copy costs bits, so a stream with FEC is a little worse when
nothing is lost. With one packet in ten lost (speech-like test, mono, 32 kb/s), the lost packets come back at 11.1 dB
SNR instead of 2.9 concealed (20 ms SILK), 5.3 instead of 2.7 (10 ms), 10.1 instead of 6.4 (60 ms), and 10.9 instead
of 3.1 (20 ms hybrid, below 8 kHz), and stereo SILK at 3.8 instead of 2.7; the whole stream without losses decodes at
15.2 dB against 20.4 without FEC (20 ms SILK). libopus decodes all of these streams, at 22 to 27.5 dB for mono.

Automatic mode (`OpusMode::Auto`) picks SILK, hybrid or CELT for each packet as libopus does: SILK below 16 kb/s
(24 for stereo), hybrid above that, and CELT above a threshold that runs from 16 kb/s for music to 64 kb/s for speech
(44 for stereo), with hysteresis, and a mode kept for three packets at least. Whether the input is speech comes from
`with_signal(OpusSignal::Voice)` or `Music`, or is guessed from how its level moves (the spread of its 10 ms levels
over two seconds: speech dips between syllables and words, music mostly holds its level), which takes a second or two to
settle and is a far simpler guess than libopus's analysis. `set_bitrate` changes the rate between packets (a link
adapting to the network), and the mode follows. A change between CELT and the others carries a redundant 5 ms CELT
frame, as libopus sends: a change to CELT waits a packet, which ends with one that the CELT frames after it carry on
from; a change from CELT starts with one, coded from CELT's state, and SILK starts again. Our decoder now follows
libopus's rules at a change of mode (CELT starts again unless a redundant frame came before, the tail of a hybrid
frame's last window is played out into a SILK frame, and a SILK frame's redundancy size counts the position bit).
Speech, music and speech at 40 kb/s goes hybrid, CELT, hybrid, and speech at 12, 24, 80 and 12 kb/s goes SILK, hybrid,
CELT, SILK; every 20 ms around a change decodes above 5 dB, most at the SNR of the speech around it, and the worst is
the first frame after a change from CELT to hybrid, where SILK starts from nothing (7 to 10 dB against 18). libopus
decodes such a stream (10, 20 and 60 ms) at 18.6, 21.6 and 22.3 dB.

Discontinuous transmission (`with_dtx(true)`): after 200 ms without activity (a frame's level against a background level
that follows the quietest frames down at once and rises 3 dB a second, never above -45 dBFS), frames are coded but only
their TOC byte is sent, with one real frame every 400 ms. A decoder conceals the empty frames as lost ones (ours does,
and so does libopus, which fills a SILK pause with its comfort noise). The first frame after a pause codes its energies
without prediction, and the SILK coder goes back to the state it had after the last frame it sent, which is the
state a decoder keeps through concealment. On speech with a 1.5 s
pause of faint noise, 61 of the pause's 75 frames go as one byte, the stream is about 35 % smaller, and the speech on
either side decodes at the same SNR as without DTX, in every mode.

Hybrid mode (`OpusMode::Hybrid`) adds the band above 8 kHz: each frame is SILK as above, then CELT from band 17 (8 kHz)
to full band in the same range coder, without the pitch post-filter, as the decoder expects there. CELT takes a steady
share (3 kb/s and a quarter of the rate above 16), SILK the rest, and the frame ends after both, so the rate is variable
(a fixed size starved SILK in voiced frames and cost 3 to 12 dB below 7 kHz); the CELT input is delayed to the SILK
side's delay, so the two meet in step and the stream has SILK's pre-skip. Mono or stereo, like SILK mode (configurations 14 and
15: full band, 10 or 20 ms). On the speech-like test with hiss added from 10 to 14 kHz, hybrid gives back 73 % of the
energy above 9 kHz, which SILK alone drops entirely. libopus decodes the hybrid streams at 28.5 dB (20 ms, 24 kb/s) and
26.3 dB (10 ms), FFmpeg's decoder at 31.9 and 26.4. Not yet: NLSF interpolation, SILK's narrow and medium bands
in the encoder, and libopus's rate-distortion pulse search, so silence still costs about 15 bytes a frame without DTX.

### Voice link

`ironav_codec::voice` holds the pieces of a live voice link (a push-to-talk intercom, say), each working on one frame
per call with buffers sized when it is made: no threads, no I/O, no allocation per frame, and each says what delay it
adds. `StreamResampler::new(in_rate, out_rate, channels, max_push)` converts as the samples come (`push`/`pull`, in
`f32` or `i16`, then `flush` at the end), with the `resample` filter's windowed sinc: 1 ms of look-ahead at 16 to 48 kHz
and back, or 16 to 24, and no offset to correct. A 16 kHz 16-bit input taken up to 48 kHz and coded as 10 ms Opus
frames (`encode_frame_into_i16`) adds 13.5 ms in all, and a counting allocator checks that nothing is allocated per frame.
`Playout::new(frame_ms, target_ms, max_ms, max_bytes)` is the receiving side's jitter buffer: `insert(seq, packet)` as
packets arrive (wrapping `u32` sequence numbers; late ones and duplicates are counted and dropped), and `pop()` once per
frame period, which gives the next packet in order, `Missing` for a gap that later packets have passed (conceal it),
`Underrun` when it runs dry (it then fills to the target again before playing), or `Buffering`. After a stall and a late
burst that leaves more than `max_ms` waiting, the oldest packets are dropped down to the target, so the delay comes back
down. `depth_ms()` is the delay it adds at that moment, and `stats()` counts what was played, missing, late and dropped.
`OpusDecoder::decode_into(packet, out)` decodes into the caller's buffer (interleaved, 48 kHz, after the gain and the
pre-skip), allocating nothing once the stream's bandwidth has been seen, and gives the same samples as the `Decoder`
path. For a `Missing` or `Underrun` it conceals (`conceal_into(out)`, or `decode_into` with an empty packet, as libopus
takes a null one). In voiced speech (the latest 15 ms of output matches itself a pitch period of 2 to 10 ms earlier
with a correlation of 0.6 or more) it repeats the last period, as libopus does, but on the decoded output, so after any
mode: at full level for 10 ms, then 3 dB quieter each 10 ms, gone after 100 ms, and the next frame that arrives is
cross-faded in from it over 2.5 ms. Otherwise, after CELT, a frame of the last frame's length made of noise shaped like
the last frame's bands, 9 dB quieter for the first frame lost and 3 dB for each one after, through the same synthesis,
so it joins the frames on either side without a click and the next real frame overlaps it as it would its own
predecessor; after SILK or hybrid, silence. On a speech-like test with one 10 ms packet in 17 lost and once three in a
row, a single lost frame comes out within 10 dB of the clean decode (2 dB with the noise alone), no frame jumps at its
join, and a few frames on the decode is back within 31 dB of the lossless one.

Before the encoder, mono `f32` frames at any rate can go through `Vad::new(frame_ms, hangover_ms)` (a frame is speech
when it stands 10 dB above a noise floor that follows the quietest frames down and creeps up 1 dB a second and is not
hiss; speech starts after two such frames and ends after the hangover; a level that holds still for 400 ms, such as a fan, becomes
the floor, `Vad::with_floor` starts from a known one, and `measurable()` says a frame is speech to take a level from: it
stands above the floor itself and the detector has had those 400 ms), `Gate::new(frame, frame_ms, preroll,
hangover_ms)` (trims a push-to-talk press: `push(frame)` says how many frames to send now and `next()` gives them, the
`preroll` frames before the start of speech going out with it so a soft first sound is not cut; `reset()` for a new
press), `Agc::new(target_dbfs, max_gain_db)` (`process(frame, speech)` measures only speech frames, best
`vad.measurable()`, so pauses are not pumped up, and a raised gain relaxes toward 0 dB over other frames; the gain falls fast, rises about 6 dB a second at 20 ms frames, ramps within a frame and is held down so no
sample clips) and `Denoiser::new(rate, hop, floor_db)` (each `hop` samples, a square-root-Hann spectrum of the last two
hops is scaled bin by bin by the square root of a decision-directed Wiener gain against each bin's smoothed minimum,
never below `floor_db`, and overlap-added; it delays by one hop, 5 ms at `hop` 80 at 16 kHz). On a speech-like test at
16 kHz with 5 ms hops and a -12 dB floor, white noise 10 dB below the speech comes out about 14 dB below it, and clean
speech comes out within 26 dB of itself. None of them allocates per frame.

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
granule position of the last page trims the end; a stream that begins mid-way (a first granule above zero) is not shifted. Chained Vorbis links are joined into one track only when their setup matches; a link that differs is a track of its own, and the joined case is not yet checked against FFmpeg. Mid-stream changes of the headers are not followed. Decoding 120 s of 44.1 kHz stereo to WAV takes about 0.25 to 0.29 s end to end
inside IronAV's own timing, of which the decoder alone is about 0.15 s (it was about 0.25 s: the inverse transform now runs a
radix-2 butterfly with no generic loop, the transform's output is mirrored with slice copies, and the overlap-add runs in
segments instead of deciding per sample). FFmpeg's decoder alone ran the same file in 0.08 to 0.19 s through PyAV, so the two are
now close. This decoder is scalar `f32`; codebook lookups use a 10-bit table, longer codes walk a tree.


### Vorbis encoder

`--acodec vorbis` (`--abitrate 16` to `500`, default 64 per channel; one to eight channels) writes Vorbis I into Ogg or Matroska/WebM:
2048-sample blocks, and 256-sample ones around attacks, floor 1 with 24 posts (12 for the short blocks), residue type 1 coded with four lattice codebooks (values to +-1, 3, 7 and 31, chosen
per 16-value partition), and square-polar coupling for a stereo pair. A stream of three or more channels has no coupling steps, so each channel is coded on its own, in the Vorbis channel order (front left, front centre, front right, ...); the input is in the WAV/FFmpeg order (front left, front right, front centre, ...) and is reordered on the way in. The headers are written by hand and parsed back by our own decoder, and
the floor curve the encoder divides by is the one it gets by decoding its own floor bits, so the two cannot drift apart. A feedback loop
on the quantiser's step holds the bit rate (the first packets search for the right step first). The step follows the local spectrum level only
partway (exponent 0.6), so noise is shaped but not hidden: there is no psychoacoustic model. **Block switching**: when 128 samples of the signal's first difference carry 8x the energy of the four before them inside the window
the next long block would have, short blocks run until the attack is clear; the decision is one block ahead, because a long block next to a
short one has a short slope on that side. On a noise burst over a quiet tone the error before the attack fell 60x (mono) and 2.4x
(stereo) (test), and FFmpeg decodes the Ogg and Matroska files to the same samples as ours (137 dB). The
detector is a simple energy rule, untuned on music. No noise substitution, no floor 0, and the code books are fixed, not trained: on
Gaussian noise it lands about 4 dB under the rate-distortion bound.

With three to eight channels (Ogg, Matroska and WebM; each channel carries its own tone) both our decoder and FFmpeg put every tone in its own output channel: 23 to 34 dB SNR on its own channel and -3 dB on the others (test `vorbis_multichannel_keeps_each_tone_in_its_channel`). FFmpeg (through PyAV) decodes the output and our decoder agrees with it to one least-significant bit on stereo and mono streams. Lengths
are exact in Ogg (the last packet's duration is trimmed); in Matroska the end is up to a block (1024 samples) late, since the container holds no
end position. On a 4 s stereo test signal (two tones and a little noise) 128 kb/s gives 19 dB SNR, 320 kb/s 28 dB. A 120 s MP3 becomes 128 kb/s
Vorbis in about 2 s (roughly 55 times real time). One trap found on the way: a coupled pair whose magnitude is exactly zero and whose angle
is not is decoded differently by the specification and by some FFmpeg builds, so the encoder never sends one.

## H.264 (software decoder)

`ironav-codec` has a software H.264 decoder written from the specification, with the constant tables read out of FFmpeg's source by `tools/gen_h264_tables.py`. It decodes **I, P and B slices** with CAVLC or CABAC: Intra 4x4, 8x8 and 16x16 prediction, I_PCM, the 8x8 transform, scaling matrices, several slices per picture, every inter partition size, quarter-sample motion compensation, several reference frames with list modification and MMCO marking, spatial and temporal direct prediction, bi-prediction with explicit and implicit weights, output reordering, cropping constrained intra prediction and the loop filter. It handles 8-bit and 10-bit (High 10) video in monochrome, 4:2:0, 4:2:2 and 4:4:4, including the lossless transform-bypass mode, progressive or interlaced as MBAFF frames (macroblock pairs coded as fields or as frames: field scans and contexts, prediction from the fields of reference frames, direct prediction and implicit weights between fields and frames, and the MBAFF loop filter, which follows FFmpeg's). Output is `gray` (8-bit monochrome; 10-bit monochrome comes out as `yuv420p10le`), `yuv420p` or `yuv420p10le`, `yuv422p` or `yuv422p10le`, and `yuv444p` or `yuv444p10le`. NVDEC cannot decode 10-bit H.264 and cannot take 4:2:2, 4:4:4 or monochrome, so those streams always take the software path. Field pictures (PAFF: each field coded as its own picture), slice groups, separate colour planes, SP/SI slices and bit depths above 10 are refused with a clear error. Pictures are decoded on several threads: `H264Decoder::new` keeps as many pictures in flight as there are cores, at most 12, one thread each, and `with_threads(n)` sets the count (`with_threads(1)` decodes on the caller's thread). Low-delay output (`H264Decoder::low_delay`, used by live sessions) stays on one thread, so its pictures never wait for others. The CABAC engine and the interpolation are not optimised.

When a GPU decoder is usable (`--hwaccel auto`) it is still chosen for 4:2:0 H.264; the software decoder runs with `--hwaccel off` or when no GPU backend works. About 50 small streams (a further 75 8-bit and 50 10-bit random libx264 configurations also matched) (libx264 in many configurations, with B-frames, pyramids, weighted prediction, several slices and reference frames, plus hand-built all-I_PCM streams) decode bit-for-bit like FFmpeg's decoder, and so do MBAFF streams from libx264 (CABAC and CAVLC, spatial and temporal direct, weighted prediction, with and without the 8x8 transform, four slices, either field first, 4:2:2, 4:4:4 and lossless, up to 1920x1080; three are checked in, the rest are made by `crates/ironav-codec/tests/h264_mbaff.rs` when ffmpeg is on the PATH); hundreds of corrupted streams made it return errors, never panic.

### H.264 encoder

H.264 can be written without a GPU. The software encoder (`h264/enc/`) produces **High profile** streams with
CABAC and the 8x8 transform (it can also write Main profile, and Constrained Baseline with CAVLC, which the tests use): I and P pictures, where a P picture predicts from up to three reference pictures (`with_refs`; each macroblock uses one of them, chosen by searching each, and three is the default. Against one, on two 720p clips of 60 frames at QP 20, 26, 32 and 38, three references give 0.7 to 1.8 percent fewer bytes and 0.00 to 0.11 dB more PSNR (FFmpeg's average) at QP 20 to 32, and at QP 38 testsrc2 is 1.3 percent smaller while mandelbrot is 7 percent larger for 0.28 dB more; single runs were about 5 percent (testsrc2) and 12 percent (mandelbrot) slower; `--refs N` or `refs` in a job's `video` sets the count from 1 to 16 for the software encoder, and NVENC ignores it), and with CABAC, two B pictures (not used for reference) between each pair of P pictures, 8-bit `yuv420p`, or 10-bit `yuv420p10le` in the High 10 profile (the SPS says 10 bits; the output decodes identically with FFmpeg, and at 720p 60 frames it is 49.6 dB average PSNR at QP 20 on testsrc2 and 46.5 dB on mandelbrot, close to the 8-bit encodes, since the sources are 8-bit content), with even dimensions (other sizes are padded to whole
macroblocks and cropped in the SPS). Each macroblock is coded as Intra 4x4, Intra 8x8, Intra 16x16, P_Skip or a P macroblock of one 16x16, two 16x8, two 8x16 or four 8x8
partitions (with the `with_sub8x8` option, off by default, each 8x8 may be split into 8x4, 4x8 or 4x4 sub-partitions, chosen by RD, at QP 20 and below); in a B picture it is B_Skip, B_Direct_16x16 (spatial direct), or one 16x16, two 16x8 or two 8x16 partitions each predicted
from list 0, list 1 or the average of both, or four 8x8 blocks that may also be direct. The partitioning and its vectors are chosen by SATD plus an estimate of the bits (16x8 and 8x16 are tried when 8x8 beats 16x16). The `with_sub8x8` option, measured against none, with three references on 720p, 60 frames, they make testsrc2 0.2 to 1.1 percent smaller at QP 20 to 38 (0.01 to 0.02 dB more PSNR), but mandelbrot 0.2 percent larger at QP 20 and 1.0 to 1.5 percent larger at QP 26 to 38 (0.02 to 0.05 dB more), and single runs were about 8 percent (testsrc2) and 30 percent (mandelbrot) slower at QP 20, which is why the option is off by default.
With CABAC, P_Skip, that inter candidate (with the 4x4 and with the 8x8 transform), Intra 16x16, Intra 4x4 and Intra 8x8 are then each coded and the one of least squared error plus
lambda times its bits is kept, the bits counted from a copy of the CABAC contexts (CAVLC still decides by SATD). Motion is found by a diamond search from the neighbours' vectors and the co-located vector of the other reference, then refined
to half and quarter samples, using half-sample planes of the reference picture computed once per picture. Each picture
is cut into slices of about eight macroblock rows, coded on separate threads (up to the number of CPUs), then loop
filtered as a whole. Reconstruction reuses the decoder's prediction, inverse transform and loop filter, so the
reference the encoder keeps is the one every decoder builds (a unit test decodes each picture and compares). `--qp N`
holds a quantiser (23 by default; IDR pictures get 3 less, as in x264, and B pictures 5 more); `--vbitrate KBPS` steers the quantiser picture by picture to an average rate.
An IDR picture with SPS and PPS starts every 250 pictures (`--keyint N`, or `keyint` in a job's `video`, sets the interval for H.264, HEVC, VP8, VP9 and AV1 alike, and NVENC uses it too), and at a scene cut: a picture that a quarter-size motion search predicts from the last one little better than from its own block means (this saves 1.9 % on a clip of three 20-picture scenes). NVENC is still chosen when a usable GPU is present;
`--hwaccel off` selects the software encoder. `--latency low` (`"latency": "low"` in a job's `video`) sets either encoder up for live delivery: no B pictures, so every picture leaves as soon as it is coded and decoders show it at once; an IDR picture every second; and a bit rate held to a quarter of a second's bits (NVENC uses its low-latency tuning, and with a bit rate CBR and a one-frame buffer). MJPEG and raw video send each picture at once either way.

Checked by decoding with FFmpeg (through PyAV): 24 cases from 16x16 to 1280x720, QP 0 to 51 and four bit rates all decode
bit-for-bit like IronAV's own decoder, with CABAC and with CAVLC. CABAC codes the same pictures in 37 % fewer bits than
CAVLC on a noisy 720p clip (BD-rate, QP 22-38; noise favours CABAC, 10-15 % is more usual); at QP 23 that clip takes
1.07 MB at 46.1 dB, where x264's Main profile without B-frames takes 0.96 MB at 46.5 dB with `veryfast` and 0.83 MB at
47.0 dB with `medium`. The CAVLC figures, for comparison with x264's Baseline profile: on that clip (60 frames) it makes 2.21 MB at 46.6 dB luma PSNR at QP 23,
where libx264's baseline profile makes 2.23 MB at 46.5 dB with `veryfast` and 1.57 MB at 47.2 dB with `medium`, so it compresses
about as well as x264 `veryfast` (x264 also uses smaller partitions, several references and RD decisions). Those
figures are for one slice; the five slices a 720p picture now gets cost 1.9 % more bits at equal quality (BD-rate over
QP 22-38). 60 frames of that 720p clip take 1.6 s (about 38 frames per second, best of three on a fully loaded 24-thread
machine), against 7.0 s on one thread before the slices and half-sample planes; x264 is still much faster. A 240-frame 720p run at 5000 kbit/s came out at 5013 kbit/s.
The partitions below 16x16 save 17.6 % (BD-rate, QP 22-38) on a synthetic 720p clip of textured shapes moving
in different directions over a panning background, and 0.3 % on the noisy clip, for about a fifth more encoding time.
The rate-distortion decisions and the finer IDR pictures together save 10.2 % on the moving-shapes clip and 35 % on the
noisy clip (BD-rate, 60 frames, QP 22-38, against the build before them), for about 1.8 times the encoding time. Against
x264 with the Main profile, no B-frames and `tune psnr`, measured the same way (both against that earlier build), IronAV now needs about 4 % fewer bits than
`veryfast` and 3.5 % more than `medium` on the shapes clip, and about 35 % fewer than `veryfast` and 4 % fewer than `medium` on
the noisy clip. The figures earlier in this section predate these decisions.
The 8x8 transform (High profile) then saves 3.5 % on a smooth 720p clip of soft shapes over a slowly varying background
(x264 `medium` gains 4.1 % going from Main to High on it), 1.9 % on the shapes clip and nothing on the noisy one, for about a
fifth more encoding time. With it IronAV needs about 6 % fewer bits than x264 `veryfast` and 5 % more than `medium`
(both High profile) on the shapes clip.
The B pictures save 0.7 % on the shapes clip, 2.7 % on the smooth clip and 14 % on the noisy one (BD-rate, 60 frames,
QP 22-38, against the build before them; x264 `medium` gains 0.5 % and 4.3 % from one B picture on the first two) and make encoding
faster, since a B macroblock tries fewer candidates. They are written with picture order counts, so an MP4 gets
composition offsets. The B partitions below 16x16 then save 6.1 % on the shapes clip and 4.7 % on the smooth clip,
cost 0.5 % on the noisy clip, and take about a fifth more encoding time. A second B picture in a row then saves 2.9 %, 1.0 % and 6.6 % and is faster again (a third
loses most of that). Not done: sub-8x8 partitions on by default, or above QP 20.

## HEVC encoder (software)

`--vcodec hevc` is encoded in software when no GPU encoder is usable, or with `--hwaccel off`. The encoder (`ironav-codec/src/hevc/enc`) makes Main profile pictures of any even size, 8-bit 4:2:0. Coding tree units are 64x64, split down to 8x8 by a rate-distortion decision on the reconstruction. Intra units choose one of 35 modes and a transform size; inter units are merged or skipped, or take one motion vector with quarter-sample refinement (AMVP). Residuals are quantised and coded with CABAC, and the deblocking filter runs, then SAO (per CTB, band or edge offsets chosen on the deblocked picture). Without B pictures, every picture is an IDR picture or a P picture that refers to the one before, so there is no reordering delay. The encoder can also code B pictures (`HevcEncoder::with_bframes(n)`: an anchor every n + 1 frames, and each B picture between two anchors predicted from both, averaged, and not used as a reference; the stream declares one picture of reordering, and the muxers carry the presentation times). They are off by default. The anchors take a quantiser 2 below the P pictures' and the B pictures 2 above (`QP_OFFSET_BFRAMES`), and with B pictures on the motion search reaches 48 pixels, since anchors four frames apart move further than the picture before (P-only streams keep the narrow search and their speed). At 720p, 60 frames, QP 22 to 37, three B pictures between anchors then come out 7 to 11 % smaller than P pictures alone on FFmpeg's `testsrc2`, with Y PSNR 0.1 to 0.9 dB lower (about 3 % smaller at equal PSNR on average), and 11 to 22 % smaller on a mandelbrot zoom, with 0.1 to 0.4 dB lower (about 12 % at equal PSNR). That is not enough on the synthetic clip to make them the default. The report is `report_720p_hevc_p_only_against_b_pictures` in `crates/ironav-engine/tests/hevc.rs`. Hierarchical B pictures are not implemented. Slices are coded on threads. `--qp` or `--vbitrate` sets the quality or the rate. Output goes to MP4 or Matroska, or to a raw `.h265` Annex B stream. FFmpeg decodes the output to the same pictures as IronAV's decoder (`crates/ironav-engine/tests/hevc.rs`).

The software HEVC and MPEG-4 encoders take `--bframes N` (or `bframes` in a job's `video`, 0 to 3) for B pictures between anchor pictures, which `with_bframes` sets (the MPEG-4 encoder also writes the Advanced Simple profile then, and its container configuration says so). The GPU encoder codes no B pictures and ignores it, as it ignores `refs`, and `--latency low` refuses it, since low latency codes none.

## VP8

VP8 video is decoded in software (`ironav-codec/src/vp8`, a port of FFmpeg's `vp8.c` and `vp8dsp.c`; the probability, quantiser and
motion tables are generated by `tools/gen_vp8_tables.py`). All four profiles work: six-tap and bilinear motion compensation, the
normal and simple loop filters, segmentation, loop-filter deltas, one to eight token partitions, golden and altref references
(with sign bias), split motion vectors, and frames that are not shown. Output is 4:2:0 8-bit, cropped to the coded size. Being
software, it lets `ironav convert -i clip.webm -o out.y4m --vcodec rawvideo` work on a machine without a GPU.

Checked bit for bit against FFmpeg's decoder (through PyAV) on libvpx output: twelve clips from 54x38 to 176x144 (odd and
non-multiple-of-16 sizes, profiles 0 to 3, one to eight partitions, constant-quality extremes, error-resilient streams with
segmentation and unsaved probabilities) all match on every frame, and three small clips are kept as tests with FFmpeg's per-frame
hashes. Sixty randomly corrupted files produced no panic. Not covered by any test file: hidden altref frames and golden/altref
sign bias (libvpx through PyAV never produced them) and keyframe size changes in mid-stream. Speed has not been tuned; the decoder
does not use more than one thread.

VP8 is also encoded in software (`ironav-codec/src/vp8/enc`): key and inter frames, intra 16x16 and 4x4 modes, whole-macroblock
motion with quarter-pixel vectors (no SPLITMV), one token partition and the default probabilities, with the loop filter level taken
from the quantiser. The encoder reconstructs with the decoder's own code, so the decoder's pictures are the encoder's, and FFmpeg
decodes what it writes bit for bit. `ironav convert -i clip.y4m -o out.webm --vcodec vp8` writes WebM (Matroska too); `--qp` (0 to
51, mapped to VP8's 0 to 127) sets the quality, and `--vbitrate` moves the quantiser toward a bit rate (within about 15% on a 720p
clip at 1 Mbit/s; a rate below what the coarsest quantiser gives is not reached). About ten frames a second at 720p in a release build, on one thread. Not done: SPLITMV, probability updates from
counted statistics, and golden or altref references other than the key frame.

## VP9

VP9 video is decoded in software (`ironav-codec/src/vp9`, a port of FFmpeg's `vp9*.c` files; the tables are generated from FFmpeg's sources).
Profiles 0 to 3 are decoded; output is `yuv420p`, `yuv422p`, `yuv440p` or `yuv444p` at 8, 10 or 12 bits (`yuv420p12le` and so on), cropped to the coded size. It covers key, inter and
intra-only frames, all block sizes and transforms (including the lossless Walsh-Hadamard mode), compound prediction, switchable interpolation filters,
segmentation, loop-filter deltas, tiles, superframes with hidden frames and "show existing frame", and backward probability adaptation.
Reference frames of a different size than the frame being decoded (spatial resampling) are refused with an error. No stream could be made to test
that path: the libvpx encoder reachable from FFmpeg keeps its size, so the scaled motion compensation is not implemented.

Checked bit for bit against FFmpeg's decoder (through PyAV) on libvpx output: about 30 clips (key frames at several quantisers, 10 to 20 frame inter
sequences with alt-ref and compound prediction, odd sizes down to 33x17, 640x360, noise, segmentation modes 1 to 3, error-resilient and frame-parallel streams,
tile columns and rows, 4:2:2, 4:4:4, lossless) all match on every frame, and six small clips are kept as the `vp9_streams_decode_to_what_ffmpeg_decodes` test.
The 10-bit 4:2:0, 12-bit 4:2:0, 12-bit 4:4:4, 4:4:0 at 8, 10 and 12 bits, and odd-sized 12-bit 4:2:0 and 4:4:0 clips (FFmpeg's libvpx-vp9 encoder made them)
match FFmpeg's decoder on every frame in `vp9_formats.rs`.
**Not tested against any stream:** intra-only frames, and corrupted-stream robustness beyond what is noted in the
changelog. VP9 is also encoded in software (`--vcodec vp9`, profile 0, 8-bit 4:2:0): a key frame every 250 frames by default, and inter frames between them that predict from the frame before, with quarter-pixel motion vectors, one reference frame and coefficient probabilities sent as per-frame updates. It writes WebM, Matroska or MP4, and its output is checked against FFmpeg's decoder in the tests (`vp9_encoded_files_decode_to_what_ffmpeg_decodes`). VP9 can also be copied between WebM, Matroska and MP4 (see "Matroska output").

Tile columns are decoded on separate threads (one per core, at most one per tile column), each into its own copy of the picture, and merged
afterwards; streams with one tile column (every stream 448 pixels wide or narrower, and any IronAV's encoder made with `with_tiles(0, 0)` or below 1280 pixels wide) still decode on one thread.
The loop filter then runs on several threads too, one superblock row each, as a wavefront: each row follows two superblocks behind the row
above it, the distance at which the filters' reach no longer overlaps, so the result is the same as filtering in raster order. Decode alone
(no output file; `IRONAV_PERF_WEBM=clip.webm cargo test --release -p ironav-engine --test vp9_perf -- --ignored --nocapture`, best of 7, on a
24-thread machine whose cores were all busy with other work, so the decoder ran at high priority), against `IRONAV_VP9_THREADS=1`:

| 1080p clip, four tile columns | one thread | four threads | one per core |
|---|---|---|---|
| synthetic picture, 30 frames | 1,067-1,186 ms | 633-707 ms | 545-617 ms |
| random noise, 8 frames (the encoder turned the loop filter off) | 827-861 ms | 343-345 ms | 364-388 ms |

Threaded output was checked against FFmpeg in both modes on 22 clips: sizes from 33x17 and 64x64 to 1080p, odd sizes, one to four tile columns,
tile rows, 4:2:2, 4:4:4, lossless, loop-filter sharpness 2, 3 and 7, strong filtering (crf 55-60), adaptive quantisation, error-resilient and frame-parallel
streams. All match on every frame. 90 corrupted copies caused no panic (and 120 more before the loop filter was threaded). `IRONAV_VP9_THREADS=1` turns the threading off.

## AV1

AV1 video in `.webm`, `.mkv` and `.mp4` files is decoded on NVIDIA GPUs through NVDEC (the same provider as H.264 and HEVC). The demuxers read the bit depth and chroma
layout from the `av1C` configuration record, and its configuration OBUs (the sequence header) are handed to the driver's parser before the first packet, as FFmpeg's
`cuviddec` does. 8-bit and 10-bit 4:2:0 come out as `yuv420p` and `yuv420p10le`; anything else is refused before the GPU is touched. The GPU says which sizes it
takes, and IronAV checks them: the RTX 3090 Ti used here decodes 128x128 to 8192x8192, and a smaller stream is refused with that range in the message.

Without a usable GPU, or with `--hwaccel off`, the software AV1 decoder takes over. It decodes key, intra-only and inter frames, with
every intra tool (partitions to 128x128, directional, smooth, Paeth, CFL, palette and filter-intra prediction, all transforms, lossless), every inter tool
(single and compound references, skip mode, the motion vector stack and the projected motion field, eight-tap and dual filters, global and local warped
motion, OBMC, wedge, difference-weighted and distance-weighted compound, smooth and wedge interintra, variable inter transform sizes, segmentation and
delta q), hidden frames, overlays and `show_existing_frame`, tiles, the deblocking filter, CDEF, loop restoration (Wiener, self-guided and switchable),
quantiser matrices, film grain (added to the output only), 8, 10 and 12 bits in 4:2:0, 4:2:2 and 4:4:4 (RGB as `rgb24` at 8 bits) and 8-bit monochrome, with intra block copy (IntraBC, for screen content), and superres (spec 7.16: the frame is decoded at its downscaled width, upscaled after CDEF, and loop restoration runs on the upscaled frame). Superres streams come from IronAV's own encoder, since FFmpeg's libaom cannot encode superres; libaom decodes them to the same pictures, at 1/9, 1/12 and 1/16 of the width, 352x288 and 177x99, key-only and key and inter, with and without loop restoration (`superres_decodes_bit_exactly` and `superres_with_loop_restoration_decodes_bit_exactly` in tests/av1.rs). Superres with warped or global motion is not covered by a test. On libaom's output (`ffmpeg -c:v libaom-av1`) the pictures equal libaom's bit for bit: key-frame-only,
P-only and alt-ref streams, `-cpu-used` 1 to 8, 128x72 to 1280x720, odd sizes, crf 0 to 63, 4x2 tiles, 32-frame streams, the bit depths and samplings
above, palettes, intra block copy (`tune-content=screen`, `enable-intrabc=1`), lossless coding, CDEF, loop restoration, quantiser matrices and film grain, in WebM, Matroska and MP4 (tests/av1.rs). Reference scaling
and switch frames are implemented but untested (FFmpeg's libaom wrapper cannot make them). About 30 frames a second at 720p with CDEF and loop
restoration off, about 9 with CDEF, quantiser matrices and film grain on (release build, `convert` to Y4M). The software encoder is below;
consumer NVENC before the RTX 40 series cannot encode AV1, and AV1 can be copied between WebM, Matroska and MP4 with no GPU (see "Matroska output").

The software encoder writes key frames every 250 frames by default (`--keyint N` changes it) and inter frames between them. An inter frame
predicts from one of the three frames before it (LAST_FRAME, LAST2_FRAME or LAST3_FRAME; each block takes the one with the lowest rate-distortion cost, LAST_FRAME searched fully and the others from its vector), or from the average of LAST_FRAME and LAST2_FRAME or LAST3_FRAME (compound, `with_compound`, on by default), with quarter-sample vectors: a full-pel search on the source picks the partitions, and a sub-pel
search on the reconstructed reference refines each NEWMV, which competes with NEARESTMV, NEARMV and GLOBALMV from the reference MV stack. Each
block is intra or inter by squared error plus lambda times the bits its syntax costs under the current CDFs, and intra blocks choose among the 13
modes and their angle deltas the same way. Deblocking and CDEF run on every frame, at levels that follow the quantiser. The encoder reconstructs
through the decoder's own state, so every picture it keeps as a reference is the one the decoder makes: the decoder's output equals the encoder's
reconstruction bit for bit, and libaom decodes the same pictures (`inter_frames_decode_identically_and_are_smaller_than_key_frames` in
`tests/av1_enc.rs`). The motion search reaches half samples; `with_quarter_mv(true)` adds a quarter-sample step, off by default because it costs more time than it saves bytes. At 720p, 60 frames, qp 20, 30 and 40, testsrc2 goes from 1,395,143, 818,341 and 228,451 bytes to 1,394,896, 823,026 and 227,180 (0.0 % smaller, 0.6 % larger, 0.6 % smaller) at luma PSNR within 0.04 dB (48.22, 40.93 and 35.88 dB in one search, 48.21, 40.90 and 35.87 dB in the other), and mandelbrot from 2,261,938, 1,112,836 and 273,512 to 2,250,943, 1,103,598 and 273,417 (0.5, 0.8 and 0.0 % smaller) at up to 0.02 dB more (44.60, 38.43 and 33.02 dB against 44.61, 38.45 and 33.03 dB); the encode takes 18 to 112 % longer. The encoder writes tile columns and rows (`with_tiles(cols_log2, rows_log2)`), each tile coded on its own thread (`with_threads`, by default the machine's; the stream does not depend on it) and decodable on its own: a picture 1280 pixels wide or more gets two tile columns by default, and `with_tiles(0, 0)` gives one tile. At 720p, 60 frames, qp 20, 30 and 40, against one tile, two and four tile columns cost 0.2, 0.8 and 1.2 % (two) and 1.4, 1.7 and 2.8 % (four) more bytes on testsrc2 (1,395,143, 818,341 and 228,451 bytes in one tile), and 0.3, 0.4 and 1.8 % (two) and 0.4, 0.7 and 2.7 % (four) on mandelbrot (2,261,938, 1,112,836 and 273,512), with luma PSNR within 0.04 dB of one tile (PSNR here compares frame n of the decode with frame n of the source); with 24 hardware threads the encode takes 1.2 to 1.8 times (two columns) and 1.9 to 3.0 times (four) as fast, and on one thread the same time as one tile. Two columns and two rows (four tiles) cost 0.6 to 3.2 % and run 2.0 to 3.0 times as fast. Superres cuts the tile columns so that each is 128 samples wide once upscaled, which libaom requires. libaom and the IronAV decoder decode the tiled streams to the same pictures (`tiled_streams_decode_identically_with_libaom_and_ironav`, `tiles_work_with_altref_superres_restoration_and_one_reference` and `wide_pictures_are_coded_in_tile_columns_by_default` in `tests/av1_enc.rs`). Rate control by bit rate (`--vbitrate`, or `bitrate_kbps` in a job) moves the quantiser the way the H.264 encoder's does: a feedback loop on the frame size against a share of the rate, with key frames at a finer quantiser (3 steps lower, expected to cost four frames' share), a window of one second's bits, and at most 4 steps a frame; the first frame's quantiser comes from the bits per pixel the rate allows. The average is an average only: there is no maximum rate or buffer size. A 60-frame 352x288 testsrc2 clip lands within 3% of 300, 800 and 2000 kbit/s, with luma PSNR rising at each (`bit_rate_is_followed_and_quality_rises_with_it` in `tests/av1_enc.rs`). Three references (`with_refs`, three by default; one for LAST_FRAME alone) against one, at 720p, 60 frames, qp 20, 30 and 40: testsrc2 is 1,513,061, 937,448 and 282,974 bytes against 1,544,586, 966,005 and 305,997 (2.0, 3.0 and 7.5 % smaller), at 47.44, 40.19 and 34.62 dB against 47.38, 40.12 and 34.48; mandelbrot is 2,475,762, 1,336,651 and 409,964 bytes against 2,607,616, 1,407,039 and 431,578 (5.0 to 5.1 % smaller), at 44.30, 38.09 and 32.44 dB against 44.25, 38.03 and 32.35. Encoding is about 40 % slower. The IronAV decode of the 720p testsrc2 three-reference stream at qp 30 is byte-identical to libaom's, and `inter_frames_decode_to_the_encoders_reconstruction` (in the encoder's tests) checks both settings. Compound prediction (`with_compound`, on by default; the frame header sets reference_select) averages LAST with LAST2 or LAST3, with the modes NEAREST_NEARESTMV, NEAR_NEARMV and NEW_NEWMV chosen by cost. At the same 720p settings, against the three references alone: testsrc2 is 1,445,891, 837,645 and 233,083 bytes against 1,513,061, 937,448 and 282,974 (4.4, 10.6 and 17.6 % smaller), at 47.48, 40.39 and 34.96 dB against 47.44, 40.19 and 34.62; mandelbrot is 2,261,878, 1,112,776 and 273,452 bytes against 2,475,762, 1,336,651 and 409,964 (8.6, 16.7 and 33.3 % smaller), at 44.44, 38.42 and 33.19 dB against 44.30, 38.09 and 32.44. Encoding is about half the speed (1 to 2 frames a second). The IronAV decode of the 720p mandelbrot compound stream at qp 40 is byte-identical to libaom's. Hidden ALTREF frames are an encoder option, `with_altref(lag)`, off by default (`--altref LAG` sets it). Every `lag` frames the last of a group is coded first as a hidden frame, the frames before it predict from it singly or bidirectionally with LAST_FRAME (compound, one forward and one backward reference), and it is shown later by a show_existing_frame header; frames carry order hints (7 bits, about one byte a frame in the default stream). Against no ALTREF (720p, 60 frames, qp 20/30/40): testsrc2 is 6.6, 8.8 and 15.1 % larger at lag 8 (14.3, 17.8 and 31.0 % at lag 4), at 0.23, 0.30 and 0.38 dB higher PSNR; mandelbrot is 0.3, 3.3 and 12.2 % larger at lag 8 (1.1, 7.3 and 22.3 % at lag 4), at 0.39, 0.54 and 0.49 dB higher. The rate-distortion curve of the encoder moves about 8 % of the bytes per dB on testsrc2 and 12 % on mandelbrot, so the gain at equal PSNR is at best a few percent, and the option stays off. The hidden frame is coded 4 steps finer than the frames after it. libaom decodes these streams to IronAV's pictures (`altref_streams_decode_identically_with_libaom_and_ironav` in `tests/av1_enc.rs`). Not yet: the joint compound mode (COMPOUND_DISTANCE), the motion field projection (`use_ref_frame_mvs`) and skip mode, which the encoder does not code. Superres and loop restoration are options of the encoder's API (`with_superres`, `with_restoration`), which the tests use; the command line does not set them.

Speed: at 720p (FFmpeg's testsrc2, 60 frames, the default key interval) the encoder ran at about 5 frames a second at qp 20, 30 and 40 with LAST_FRAME alone, against about 1.6 before the open-loop planner, the motion search and the transform bases were made cheaper. With the defaults (three references and compound prediction) it runs at about 1 to 2 frames a second. Its files are 1 to 3.5 % smaller than before at the same or a slightly higher PSNR.

The GPU decoder is checked bit for bit against FFmpeg's libdav1d (through PyAV) on SVT-AV1 output: 27 clips from 128x128 to 3840x2160, including odd sizes, 10-bit, film grain
(8-bit and 10-bit; the grain is applied, and matches libdav1d's), several key frames, hierarchical prediction with overlay frames, screen-content tools,
loop restoration and CDEF, superres, and WebM, Matroska and MP4 files. All match on every frame. Three small clips are kept as the
`av1_decodes_on_the_gpu_to_what_dav1d_decodes_or_refuses_precisely` test, which on a machine without a GPU checks the software decoder against the same pictures.

The AV1 encoder takes three more options from the command line and from a job's `video`: `--tiles COLSxROWS` (`tiles`, for example `2x1` for two tile columns and one row; each count a power of two from 1 to 64; it sets `with_tiles`), `--quarter-mv` (`quarter_mv`, which sets `with_quarter_mv(true)`) and `--altref LAG` (`altref`, 2 to 16, which sets `with_altref`). A bad count is refused by name, and `--altref` refuses `--latency low`, which holds no frames back.

## JPEG

Baseline, extended-sequential and progressive 8-bit Huffman JPEG (`.jpg`, `.jpeg`, codec `mjpeg`) is decoded in software, and the encoder writes baseline. Decoding handles grey, 4:2:0, 4:2:2 and 4:4:4, restart intervals and non-interleaved scans. A lossy three-component file is RGB, decoded with no colour conversion, when it has an Adobe marker with transform 0 (and no JFIF marker), or when its component ids are `R` `G` `B` with no JFIF or Adobe marker, as libjpeg reads them; RGB must be unsubsampled. Otherwise it is YCbCr. Four-component CMYK and YCCK files are refused with a message that says so. Lossless (SOF3) files decode too, with predictors 1 to 7 and the point transform, in grey, 4:2:0, 4:2:2, 4:4:4 and RGB (8-bit, or the 9-bit RGB that FFmpeg writes, with its colour transform); lossless files with restart intervals are refused. Lossless files FFmpeg writes (predictors 1 to 3) decode exactly as FFmpeg's decoder does; predictors 4 to 7 and the point transform are checked against a test encoder, and on 8-bit RGB with a point transform and predictors 5 to 7 FFmpeg's decoder gives different samples (not investigated further; this decoder keeps 8-bit samples modulo 256 throughout). Arithmetic-coded and 12-bit files are refused with a clear error (FFmpeg's encoders write neither, and no 12-bit encoder was available to make a test file), as are progressive pictures over 67 megapixels (all their coefficients are held in memory). The encoder writes baseline JFIF with the standard tables at quality 90; `--qp N` (0-51, as for the other video codecs) sets the quality to 100 minus twice N, so `--qp 5` is 90 and `--qp 23` is 54.

Motion-JPEG can also be written into Matroska (`V_MJPEG`, `-o clip.mkv --vcodec mjpeg`); FFmpeg opens and decodes those files. A `.jpg` holds one picture; `ironav convert -i in.y4m -o out.jpg` picks `mjpeg` by itself. Decoded output differs from FFmpeg's by at most 1 LSB (a different IDCT), checked on 4:2:0, 4:2:2, 4:4:4 and grey images, sequential and progressive (libjpeg's default progressive script, with successive approximation). Encoded files decode in FFmpeg at 40+ dB PSNR. 450 corrupted or truncated files (150 sequential, 300 progressive) made it return errors or pictures, never panic.

## ProRes

Apple ProRes (codec `prores`, in MOV) is decoded and encoded in software: 4:2:2 at 10 bits for Proxy, LT, Standard and HQ, and 4:4:4 at 12 bits for 4444 and 4444 XQ, all progressive (interlaced pictures are refused with an error). The decoder reads the quantisation matrices and skips a 4444 alpha plane. The encoder writes HQ unless `--prores-profile proxy|lt|standard|hq|4444|4444xq` (or `prores_profile` in a job) picks another; the 4444 profiles keep a 4:4:4 picture's chroma (10 or 12 bits) and other pictures are converted to 4:2:2 at 10 bits (4:4:4 at 10 bits for 4444). It uses a fixed quantiser per profile and flat matrices, and the Rust API takes any profile. Decoding is bit-exact with FFmpeg's in both directions, and HQ encodes at over 45 dB PSNR (`crates/ironav-engine/tests/prores.rs`, skipped without ffmpeg on the PATH).

## MPEG-4 Part 2 (software decoder and encoder)

Simple and Advanced Simple profile video (`mp4v` in MP4, `V_MPEG4/*` in Matroska) is decoded in software: I, P and B pictures, half- and quarter-sample motion, four vectors per macroblock, direct mode, H.263 and MPEG quantisation with custom matrices, and resync markers. Decoding matches FFmpeg's decoder bit for bit (`crates/ironav-engine/tests/mpeg4.rs`, which skips without ffmpeg on the PATH). Interlaced pictures, sprites and global motion, data partitioning and scalable layers are refused with an error. The encoder writes Simple profile I and P pictures: half-sample motion, four vectors per macroblock, intra DC and AC prediction, H.263 quantisation and the escape codes. With `with_bframes(n)` (off by default; the engine does not expose it yet) it also writes B pictures as Advanced Simple profile, with forward, backward, bidirectional and direct macroblocks, and the anchors coded ahead of the pictures between them. It runs at a fixed quantiser (`--qp`, 1 to 31) or to a bit rate (`--vbitrate`), with a key picture interval (`--keyint`), and writes MP4 (`mp4v`) and Matroska (`V_MPEG4/ISO/SP`). Select it with `--vcodec mpeg4`. Quarter-sample motion and interlaced video are not encoded.

B pictures save bytes at equal PSNR. The B pictures' quantiser is 1.25 times the anchors' plus 1.25 (FFmpeg's default `b_qfactor` and `b_qoffset`; FFmpeg applies them only when rate-controlled, not at a fixed `-qscale:v`). On 352x288, 60 frames, with IronAV's key picture every 250 frames (one key picture in the clip), against FFmpeg `-c:v mpeg4 -bf 1 -g 250 -qscale:v N` (the same interval). Bytes and luma PSNR at qscale 5, 10 and 20:

| source | qscale | IronAV no B | IronAV 1 B | IronAV 2 B | FFmpeg `-bf 1 -g 250` |
|---|---|---|---|---|---|
| testsrc2 | 5 | 284656 / 43.43 | 226900 / 41.87 | 222743 / 41.11 | 273390 / 42.93 |
| testsrc2 | 10 | 155766 / 37.90 | 126095 / 37.09 | 121644 / 36.78 | 148056 / 37.27 |
| testsrc2 | 20 | 61095 / 34.00 | 52132 / 33.61 | 56038 / 33.17 | 55161 / 33.11 |
| mandelbrot | 5 | 460628 / 40.12 | 343215 / 38.58 | 310430 / 38.13 | 431593 / 39.83 |
| mandelbrot | 10 | 198102 / 35.00 | 148635 / 34.20 | 135164 / 33.98 | 176211 / 34.70 |
| mandelbrot | 20 | 74703 / 30.98 | 59812 / 30.77 | 56024 / 30.68 | 56393 / 30.57 |

The B pictures lose PSNR as well as bytes, so the comparison needs equal PSNR. Taking the no-B curve (qscale 3 to 28, log bytes against PSNR, interpolated at each point's PSNR), one B picture saves 3.5 to 8.7 percent on testsrc2 and 6.0 to 14.5 percent on mandelbrot; two B pictures save 0.4 to 4.2 percent on testsrc2 at qscale 5 and 10 and lose 14 percent at qscale 20, and 9 to 18 percent on mandelbrot. Without the B quantiser offset (the same quantiser for B pictures as for anchors) one B picture saved 0.1 to 1.6 percent on testsrc2 and 4.7 to 7.1 percent on mandelbrot, and two B pictures lost on testsrc2. At the same equal-PSNR reading, FFmpeg's `-bf 1 -g 250` streams are 0.3 to 14 percent larger than the no-B curve on testsrc2 and 2 to 14 percent smaller on mandelbrot, so IronAV's one-B-picture stream is ahead of FFmpeg's on testsrc2 and ahead or level on mandelbrot. These are interpolations between qscale points, not encodes at the same PSNR. The table is `report_bframes_table` in `crates/ironav-engine/tests/mpeg4_enc.rs`.

## MPEG-1 and MPEG-2 video (software decoder)

MPEG-1 video and MPEG-2 video (H.262) are decoded in software as the `mpeg1video` and `mpeg2video` codecs, a translation of the parts of FFmpeg's `mpeg12dec.c` the stream syntax needs. Supported: I, P and B pictures; frame pictures and field pictures, mixed in one stream; field and frame DCT, field, 16x8 and dual-prime motion; the alternate scan, both intra VLC tables, non-linear quantiser scale, custom and per-chroma quantiser matrices, intra DC precision up to 11 bits; 4:2:0 and 4:2:2 (Main and 4:2:2 profiles), sizes that are not a multiple of 16, and MPEG-1 including its full-pel vectors. Interlaced frames come out woven, as FFmpeg's do. Input: raw elementary streams (`.m2v`, `.mpv`, `.m1v`), MPEG program streams (`.mpg`, `.mpeg`, `.vob`, `.ps`: every video stream (0xE0-0xEF), MP2 and MP3 audio (0xC0-0xDF, labelled by each frame's layer), AC-3 (private stream 1, 0x80-0x87) and DVD linear PCM (0xA0-0xA7, 16, 20 or 24 bits, converted to little-endian); DTS, subpictures and DVD-Audio are skipped, and so is a stream that first shows up more than a few megabytes into the file; timestamps are the PES presentation times, every stream starting at the earliest, so audio and video stay aligned), MPEG-TS (stream types 0x01 and 0x02), Matroska (`V_MPEG2`, `V_MPEG1`) and MP4 (object types 0x60 to 0x65 and 0x6A). It decodes only; there is no MPEG-1 or MPEG-2 video encoder, and the video is not copied into other containers. In MP4 and Matroska a 4:2:2 MPEG-2 stream is declared 4:2:2: FFmpeg writes no sequence header into those containers, so the first picture's sequence header and extension give the chroma format (`crates/ironav-engine/tests/mpeg2_containers.rs`, which also checks the decode against FFmpeg's).

What is verified: the output is FFmpeg's, byte for byte, frame for frame (`-flags +bitexact -idct simple`; IronAV uses FFmpeg's simple integer transform). `crates/ironav-engine/tests/mpeg2.rs` has FFmpeg's `mpeg2video` and `mpeg1video` encoders write progressive, interlaced (top field first), 4:2:2 and B-picture streams into elementary streams, MPEG-TS, MPEG-PS, VOB, MP4 and Matroska, and compares (skipped without ffmpeg on the PATH). FFmpeg's encoders write only frame pictures, so the field pictures, 16x8 and dual-prime motion, concealment vectors, escape codes, the alternate scan, custom matrices, 4:2:2 and cropped sizes are covered by 13 streams of random but valid syntax (`tools/gen_mpeg2_synth.py`; the streams and FFmpeg's decode of them are in `crates/ironav-engine/tests/data/mpeg2_*`, and the tests for them need no ffmpeg). All 13 and the encoder streams match. Program streams made by FFmpeg (`mpeg` and `vob` output with two video and two audio streams, MP2, AC-3 and DVD LPCM at 16 and 24 bits) are checked in `crates/ironav-engine/tests/mpeg_ps_audio.rs`: the streams are the ones ffprobe lists, MP2 decodes to within 1 LSB of FFmpeg's samples, AC-3 within 5e-5 and LPCM exactly, and a transcode to MP4 starts the video and audio within a frame of each other. FFmpeg's encoder writes LPCM at 16 and 24 bits only, so 20-bit LPCM follows FFmpeg's decoder source and is not compared with its output. Those streams hold coefficients outside the range real streams have, where FFmpeg's SIMD transforms saturate and its C transform wraps; the references are made with `-cpuflags 0`, and IronAV follows the C transform.

Speed, release build, one thread, 250 frames written to a Y4M file: 720x576 interlaced at 6 Mbit/s decodes in 1.6 s (157 frames/s), 1920x1080 progressive at 20 Mbit/s in 3.0 s (83 frames/s). FFmpeg's decoder, one thread, discarding the frames, takes 0.67 s and 2.6 s.

Refused or not done: 4:4:4 (the High profile), the scalable profiles (SNR, spatial, temporal), data partitioning; concealment vectors are parsed and ignored (FFmpeg ignores them too); damaged streams are not concealed beyond skipping the slice, so a stream with errors can differ from FFmpeg's decode; a B picture before the first anchor of an open GOP is skipped as FFmpeg skips it (not tested against FFmpeg); in program streams, DTS, subpictures and DVD-Audio are skipped and a stream that first appears late in the file is not read; there is no MPEG-1 video encoder or output, and no MPEG-2 field-picture or 4:2:2 encoder.

**MPEG-2 video encoder.** `mpeg2video` is encoded in software as Main profile, 4:2:0, progressive frame pictures: I and P pictures, with B pictures between anchors when `--bframes N` is set (off by default), `--qp` 1 to 31 for a fixed quantiser scale, `--vbitrate` for rate control, and `--keyint` for the key-picture interval. It writes raw `.m2v`, MPEG-TS (stream type 0x02), Matroska (`V_MPEG2`) and MP4 (object type 0x61). Field pictures, 4:2:2 and interlaced output are not encoded. FFmpeg decodes IronAV's stream to exactly IronAV's reconstruction (`-flags +bitexact -idct simple -cpuflags 0`), and IronAV decodes it the same way; `crates/ironav-engine/tests/mpeg2_enc.rs` checks both (skipped without ffmpeg). The encoder never chooses a motion vector whose block reads outside the picture, because FFmpeg and IronAV disagree on those reads (FFmpeg does not clamp them to the edge). The table compares 60 frames of each source at `--qp` against FFmpeg's `-c:v mpeg2video -qscale:v N` (with `-bf 2` for the B rows), by IronAV's file size and luma PSNR per frame index:

| source | size | qscale | B pictures | IronAV bytes | IronAV luma PSNR (dB) | FFmpeg bytes | FFmpeg luma PSNR (dB) |
|---|---|---|---|---|---|---|---|
| testsrc2 | 352x288 | 3 | 0 | 525378 | 49.35 | 502204 | 48.48 |
| testsrc2 | 352x288 | 5 | 0 | 375881 | 45.07 | 343599 | 44.05 |
| testsrc2 | 352x288 | 10 | 0 | 228728 | 39.23 | 187408 | 38.54 |
| testsrc2 | 352x288 | 3 | 2 | 440681 | 47.31 | 506667 | 48.55 |
| testsrc2 | 352x288 | 5 | 2 | 315854 | 43.17 | 351486 | 44.14 |
| testsrc2 | 352x288 | 10 | 2 | 197522 | 38.33 | 197378 | 38.68 |
| mandelbrot | 352x288 | 3 | 0 | 953795 | 45.83 | 880765 | 44.88 |
| mandelbrot | 352x288 | 5 | 0 | 643986 | 42.16 | 568634 | 41.14 |
| mandelbrot | 352x288 | 10 | 0 | 368761 | 36.94 | 284565 | 35.92 |
| mandelbrot | 352x288 | 3 | 2 | 707600 | 43.75 | 838229 | 44.82 |
| mandelbrot | 352x288 | 5 | 2 | 472425 | 40.15 | 536368 | 41.10 |
| mandelbrot | 352x288 | 10 | 2 | 271283 | 35.72 | 268818 | 35.96 |
| testsrc2 | 720x576 | 3 | 0 | 1591611 | 51.26 | 1479213 | 50.55 |
| testsrc2 | 720x576 | 5 | 0 | 1125099 | 46.98 | 1001833 | 46.20 |
| testsrc2 | 720x576 | 10 | 0 | 666991 | 41.47 | 530788 | 40.72 |
| testsrc2 | 720x576 | 3 | 2 | 1341894 | 49.04 | 1510014 | 50.58 |
| testsrc2 | 720x576 | 5 | 2 | 954555 | 44.99 | 1028409 | 46.26 |
| testsrc2 | 720x576 | 10 | 2 | 592704 | 40.32 | 562758 | 40.83 |
| mandelbrot | 720x576 | 3 | 0 | 2714541 | 47.46 | 2534236 | 46.43 |
| mandelbrot | 720x576 | 5 | 0 | 1842925 | 43.87 | 1649790 | 42.77 |
| mandelbrot | 720x576 | 10 | 0 | 1064491 | 38.69 | 852421 | 37.57 |
| mandelbrot | 720x576 | 3 | 2 | 2042322 | 45.41 | 2421593 | 46.42 |
| mandelbrot | 720x576 | 5 | 2 | 1380274 | 41.89 | 1572981 | 42.77 |
| mandelbrot | 720x576 | 10 | 2 | 814674 | 37.44 | 812831 | 37.65 |


## MP3

MPEG-1, MPEG-2 and MPEG-2.5 Layer III are decoded (mono, stereo, joint stereo with mid/side), from `.mp3` files (ID3v2 tags
skipped, a leading Xing/Info frame not played) and from Matroska. The Huffman and band tables and the scale-factor, stereo and
short-block handling follow FFmpeg's decoder; the filterbank is written from the standard's definitions in `f64`.

MPEG-1 and MPEG-2 Layer II (MP2) is decoded as the `mp2` codec, from raw `.mp2` files, Matroska `A_MPEG/L2`, and MP4 (an `mp4a` entry, object type 0x69 or 0x6B, is labelled by its first frame's layer, so FFmpeg's MP2 in MP4 reads as MP2 and its MP3 as MP3).
Any other stream labelled MP3 that carries Layer II frames is decoded by the same code. Checked against
FFmpeg's `mp2float` decoder on eleven encoded streams (16 to 48 kHz, mono and stereo, each of the four MPEG-1 allocation
tables, the MPEG-2 tables) and a full-scale noise stream: they agree to float32 rounding. Joint-stereo (intensity) Layer II
is decoded from the standard but has no FFmpeg reference (its encoder writes none, and patched streams do not decode to
FFmpeg's samples), so it is unverified. Layer I has no encoder to make a test stream with and is refused. The MP2 encoder is described below.

Checked against FFmpeg's `mp3float` decoder on twelve LAME-encoded files (44.1, 48, 32, 22.05, 24, 16, 11.025 and 8 kHz;
mono and stereo; 16 to 320 kb/s, CBR and VBR; long, start, short and stop blocks): **116 to 124 dB SNR**, largest sample
difference about 3e-6 of full scale. The same frames in Matroska decode identically. Fuzzed with 120 damaged and truncated
files: no panics.

Not covered, and not tested because the encoder used never produced them: **mixed blocks, intensity stereo, CRC-protected
frames and the 8 kHz mixed-block tables** are ported from FFmpeg but unverified. Layer I is refused. A frame whose
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

### MP2 encoder

`--acodec mp2` (`--abitrate 32` to `384`, snapped to the nearest Layer II rate; default 96 per channel) writes MPEG-1 Layer II to `.mp2`,
Matroska, MP4 (`mp4a`, object type 0x69) or MPEG transport streams, mono or stereo at 32, 44.1 or 48 kHz (other rates are resampled, with a
warning). It is a translation of FFmpeg's `mpegaudioenc.c` (LGPL-2.1+): the same integer polyphase analysis, the same fixed masking table
and greedy bit allocation, and the same scale factor codes. For the same 16-bit input the frames are byte for byte FFmpeg's
(`crates/ironav-engine/tests/mp2_encode.rs`, which skips without `ffmpeg` on the PATH, checks this for five configurations and a full-scale
sweep). The stream carries one more frame than FFmpeg's, so that the decoder's 481-sample delay is flushed. On a 3 s sweep, FFmpeg decodes
our 192 kb/s 44.1 kHz stereo stream to 23.1 dB SNR (its own stream: 23.1 dB), 128 kb/s 48 kHz stereo to 16.3 dB, 64 kb/s 32 kHz stereo to
9.2 dB, and 96 kb/s 48 kHz mono to 21.9 dB. Layer II has no psychoacoustic model here, so the SNR is what the bit rate buys. MP4 has an
edit list that skips the delay; `.mp2` and MPEG-TS files do not, so FFmpeg plays them 481 samples late, as it does MP3 in those. Matroska
carries the delay as CodecDelay (for MP2 and MP3 as for AAC and Opus), which FFmpeg trims: its decoding lines up with the input, and so does IronAV's reading of the file.

## AC-3

AC-3 (Dolby Digital, ATSC A/52) is decoded as the `ac3` codec, a translation of FFmpeg's `ac3dec.c` and `ac3dec_float.c`. Every
channel mode from mono to 3/2 with the LFE channel decodes, with coupling, rematrixing, delta bit allocation, block switching and
dynamic range at FFmpeg's default (`drc_scale` 1). The output is float, in FFmpeg's channel order. It is read from raw `.ac3`
files, Matroska (`A_AC3`), MPEG-TS (stream type 0x81, or 0x06 with the AC-3 descriptor) and MP4 (`ac-3` with `dac3`). The encoder
writes the same containers (see "AC-3 encoder" below). E-AC-3 is refused with an error.

Measured against `ffmpeg -c:a ac3 -f f32le` on FFmpeg-encoded streams (3 seconds each, tones with noise bursts):

| Streams | Worst absolute difference | SNR |
|---|---|---|
| mono and stereo, 44.1 and 48 kHz, 96 to 192 kb/s | 1.0e-5 to 2.5e-5 | 102 to 108 dB |
| 5.1 with LFE, 48 and 44.1 kHz, 160 to 640 kb/s | 8e-6 to 1.6e-5 | 106 to 110 dB |
| the same streams through Matroska, MPEG-TS, MP4 and raw files | up to 4.5e-5 | |

The worst difference is about one 16-bit step (3.05e-5), and is larger than the 1e-5 aimed for; the cause is not isolated.
Block switching follows FFmpeg's `do_imdct`, but it is unverified: FFmpeg's encoder writes no short blocks, so no test stream
has them. The tests are `crates/ironav-codec/tests/ac3_ffmpeg.rs` (the decoder) and `crates/ironav-formats/tests/ac3_containers.rs`
(the containers, and the E-AC-3 refusal); both skip without `ffmpeg` on the PATH.

### AC-3 encoder

`--acodec ac3` (`--abitrate` 32 to 640 kb/s, snapped to an AC-3 rate; the default is 96, 192, 320, 384 or 448 kb/s for one to five
full-bandwidth channels, as FFmpeg picks it) writes AC-3 to raw `.ac3`, Matroska (`A_AC3`), MP4 (`ac-3` with `dac3`) and MPEG-TS
(stream type 0x81 with the `AC-3` registration descriptor). It is a translation of FFmpeg's float encoder (`ac3enc.c` and
`ac3enc_float.c`) with its defaults: dialnorm -31, coupling and rematrixing where FFmpeg uses them, and the same exponent and
bit allocation search. It takes 32, 44.1 and 48 kHz, every channel mode from mono to 3/2 with or without LFE, and not dual mono.

Measured against `ffmpeg -c:a ac3` on the same float input (a 3-second sweep with clicks, 14 channel modes at 48 kHz and five other
rate and bit rate settings). The SNR is of each stream's decode against the input:

| Streams | Identical frames | SNR, FFmpeg's stream | SNR, IronAV's stream |
|---|---|---|---|
| mono, 48 kHz, 32 kb/s | 74 of 94 | 12.7 dB | 12.7 dB |
| stereo, 48 kHz, 192 kb/s | 0 of 94 | 50.9 dB | 50.8 dB |
| stereo, 44.1 kHz, 128 kb/s | 2 of 87 | 19.7 dB | 19.7 dB |
| stereo, 32 kHz, 96 kb/s | 0 of 63 | 9.8 dB | 9.8 dB |
| 5.1, 48 kHz, 640 kb/s | 0 of 94 | 48.6 dB | 48.6 dB |
| 5.1, 44.1 kHz, 448 kb/s | 0 of 87 | 47.7 dB | 47.7 dB |
| 4.0, quad, 5.0 and 5.1 at 192 kb/s | 0 to 3 of 94 | 6.7 to 13.7 dB | 6.7 to 13.7 dB |

The other mono, stereo and surround modes at 192 kb/s fall between these, with the same parity. The sweep is a hard signal:
FFmpeg's own stream reaches only -2.6 dB on quad with LFE, and IronAV's reaches 13.7 dB there. IronAV's decode and FFmpeg's decode of
IronAV's stream agree to 104 to 113 dB. Frames are not byte-identical: the MDCT and the quantiser round a few coefficients
differently, and in the first stereo frame the header, the bit allocation and the exponents match FFmpeg's exactly, and 15 bytes of
mantissas differ (the CRC-1 fields match FFmpeg's). The test is `crates/ironav-engine/tests/ac3_encode.rs`, which skips without
`ffmpeg` on the PATH. The containers are checked in `crates/ironav-formats/tests/ac3_containers.rs`: FFmpeg's probe names each
written file AC-3 with the right channel count, the packets read back byte for byte, and the MP4 `dac3` body matches FFmpeg's.

## Matroska output

`.mkv` and `.mka` can be written: H.264 or HEVC video (from the software or hardware encoder, or copied), MPEG-4 Part 2 video (copied, or encoded by IronAV), or VP8, VP9, AV1 or Motion-JPEG video (copied, or encoded by IronAV for VP8, VP9, AV1 and Motion-JPEG), with AAC, FLAC, MP3, Opus, Vorbis or PCM
audio, copied or encoded, and any number of tracks of each kind (two audio tracks, or two video tracks). Timestamps are in milliseconds; clusters are gathered in memory and
written whole, so there are no unknown-size elements, and the file ends with Cues for the video key frames (or the clusters, for
audio only). The header is held back until every stream has delivered a packet and the video's SPS/PPS have been seen.

Checked with FFmpeg (through PyAV) on AAC, Opus, Vorbis, FLAC and MP3 copied into `.mka`, and on H.264 and HEVC from the NVENC
encoder, with and without the AAC audio in `.mkv`: FFmpeg opens every file, the audio packet counts equal the sources', and
FFmpeg decodes all 50 frames of each video. IronAV's own tests copy audio through and compare every packet's bytes and the codec
configuration, and decode FLAC through Matroska to the PCM it was made from.

Several tracks of one kind are written as separate tracks, in input order: ffprobe lists them all, and IronAV's reader returns each
track's packets. They come from one input: with `extra_inputs`, the first file that has a kind supplies all of that kind,
so a separate audio file still replaces, not joins, the audio of a file that has some. Other formats keep the first stream of each kind. The `mkv_multi` test copies a file with two audio tracks and one
video track, and one with two video tracks, and checks the result with ffprobe and with IronAV's reader.

`.webm` is written with VP8, VP9 or AV1 video and Opus or Vorbis audio (document type `webm`; FFmpeg opens the files and decodes them).
VP8, VP9 and AV1 video can be encoded by IronAV (see "VP8", "VP9" and "AV1"); otherwise the video is copied, from WebM, Matroska or (VP9 and AV1) MP4. Key frames come from
the bitstream: VP8's frame tag, the first frame header of a VP9 packet (or superframe), and for AV1 the first frame header of the temporal unit,
which must be a shown key frame. So the Cues are right. VP8 and VP9 have no CodecPrivate. AV1's is its `av1C` record (a stream without one is
refused), and temporal delimiter and padding OBUs are dropped, as the Matroska AV1 mapping asks. FFmpeg (through PyAV) checked the copies:
the six VP9 and three AV1 test clips, plus VP9 and AV1 clips with a key frame every 8 to 12 frames, alt-ref superframes, 4:4:4 VP9 and 10-bit AV1,
were each copied into MP4, Matroska and WebM and back out of MP4. FFmpeg finds the same packets, the same key frames and the same decoded pixels
as in the source. The `vp9_and_av1_copy_between_webm_matroska_and_mp4` test makes these copies of the kept clips, compares every packet
and decodes the VP9 copies in MP4.

PCM in Matroska is read (`A_PCM/INT/LIT`, `A_PCM/INT/BIG` and `A_PCM/FLOAT/IEEE`, 8 to 64 bits) and written (little-endian, with the sample size as BitDepth); FFmpeg's files decode to the same bytes and FFmpeg reads IronAV's copies to the same bytes. Not done: no SeekHead, tags, chapters or attachments; more than one stream per kind is refused; an MP3's gapless trim (from its LAME tag) is not
carried over and the Opus pre-skip is carried only as the OpusHead plus CodecDelay. B-frame video in Matroska: a raw H.264 or HEVC file with B pictures copies with its presentation times running from 0 (the reordering depth, which the file's frames are ahead of their decoding by, comes off), and FFmpeg decodes the copy to the same pictures; the MPEG-TS section covers B-frames in transport streams.

## MPEG transport streams

`.ts`, `.m2ts` and `.mts` are read and written (format `mpegts`): H.264 or HEVC video with AAC, MP3 or Opus audio. Reading takes
188-byte packets and 192-byte M2TS ones and follows the first program. The parameters of each stream come from its first
units (the SPS, the ADTS or MP3 header), and the frame rate comes from the spacing of the timestamps. 33-bit timestamps are
unwrapped. A gap in the continuity counter drops only the unit it breaks. Video starts at a keyframe. The same parser reads
files and live sources, so `ironav watch udp://:5000` takes a transport stream directly. Files report their duration
(from the timestamps near the end) and can be seeked to a keyframe at or before the requested time.

Writing puts the video on PID 0x100 with the PCR, and repeats the PAT and PMT before every keyframe and at least twice a second.
Access units get an access unit delimiter, AAC gets ADTS headers, and video decode times are derived from the
presentation times, so B-frames from the encoder or a copy come out right.

Opus follows ETSI TS 102 366 as FFmpeg writes it: stream type 0x06 with the `Opus` registration descriptor and the 0x7F extension
descriptor that names the channel configuration, and each packet behind a control header. Mono, stereo and 3 to 8 channels
(channel-mapping family 1, RFC 7845 layout) are written. The pre-skip goes in the start trims of the first packets, and a packet
the stream shortens at its end carries an end trim; the reader puts the pre-skip back in the OpusHead and shortens the packet. Checked
with FFmpeg's libopus (`crates/ironav-engine/tests/opus_ts.rs`): what IronAV writes decodes in FFmpeg to IronAV's decode of the
source, after the pre-skip, to 107 dB or more (within 1 LSB) at mono, stereo and 6 channels; and the packets FFmpeg writes into a
`.ts` (several to a PES) are the packets of its Ogg and Matroska copies. FFmpeg's decoder does not apply the trims, so its decode of an
IronAV transport stream keeps the pre-skip samples at the front and the end trim is not applied to the last packet.

Checked with FFmpeg (through PyAV): files that FFmpeg wrote (libx264 or libx265 with B-frames, AAC or MP3) read with the same
packet counts and times, and FFmpeg reads what IronAV writes without a decode error. That covers copies of those files,
IronAV's own H.264 encoder with B-frames, a Matroska file remuxed to TS, and HEVC+MP3 transcoded to H.264+AAC. In all of
them every frame decodes, the decode times always increase, and no presentation time is before its decode time.

Not handled: subtitles (skipped when reading, refused when writing), more than one program,
and PMT changes during a stream.

## Performance

Stages run on separate threads with bounded queues, packets are moved not copied, and the MCP/GUI processes stay
resident. Measured (release build, one Windows machine, 10 min of 48 kHz stereo PCM): s16→f32 with a volume filter
≈ 2,500× real time (≈ 450 MiB/s in); stream copy ≈ 5,100× real time. These are PCM numbers.

**Against FFmpeg** (`bench/vs_ffmpeg.py`, which makes its own inputs; Ryzen 9 9900X, 24 threads, RTX 3090 Ti, the
`ffmpeg` 9.0.1 program; median of 5, both sides a new process each run and both writing their output files):

| Job | IronAV | FFmpeg | |
|---|---|---|---|
| WAV to FLAC, 180 s stereo | 254 ms | 185 ms | FFmpeg 1.4x faster; IronAV's file 0.2 % smaller |
| FLAC to WAV, 180 s | 339 ms (min 208) | 85 ms | FFmpeg about 3x faster |
| WAV to Opus, 128 kb/s stereo | 1.87 s | 1.19 s (libopus) | FFmpeg 1.6x faster |
| NVENC H.264, 240 frames 720p from Y4M | 465 ms | 511 ms (`h264_nvenc`) | about level (runs vary 10-20 %); default rates differ, so sizes do |
| NVDEC H.264 to Y4M, 240 frames 720p | 406 ms | 456 ms (`-hwaccel cuda`) | about level |
| Software H.264 to Y4M, 240 frames 720p | 1.33 s (min 1.05) | 0.59 s (min 0.43) | FFmpeg about 2.3x faster; this decoder runs pictures on up to 12 threads |
| Software H.264 to Y4M, 240 frames 720p, with B-frames | 1.59 s (min 1.16) | 0.97 s (min 0.41) | FFmpeg about 1.6x faster |
| Process start-up | 18 ms | 28 ms | IronAV 1.6x faster |

**So IronAV is level with FFmpeg where the GPU does the work and slower everywhere else** (most of all in software
H.264 decoding, where FFmpeg is still about 2.3 times faster; the decoder now runs pictures on several threads). The "lower latency, higher throughput" goal is a goal, not a result. The FLAC encoder alone (no file, no process) takes 0.18 s for the 3-minute file on this
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
starts in 18 ms (the start-up row of the table above). Both programs are built by default and must stay in the same directory.
`cargo build --release --no-default-features -p ironav-cli` builds only the small one. Timings on this machine vary by a factor of two from run to run when other work is going on.
NVENC in earlier runs: 240 frames of 1280x720 (a 332 MB Y4M) in 0.6-3.8 s wall depending on load.


## Security and operator settings

The agent tools (MCP server, web UI, CLI) read and write files, open sockets and, if allowed, cameras. What they may do is set by the operator in
environment variables, which a tool call cannot change. `ironav mcp` starts with the strict defaults; the command line and the GUI keep their old
behaviour unless a variable is set.

| Variable | Default | Effect |
|---|---|---|
| `IRONAV_ROOTS` | unset: no limit for the CLI and GUI; the working directory in `ironav mcp` | Directories (separated like `PATH`: `;` on Windows, `:` elsewhere) that tools may read from and write to: inputs, outputs, `thumbnail_dir`, recordings, clips, `file:` and `pipe:` sources, job files. Symbolic links and `..` are resolved first (for an output that does not exist yet, the nearest directory that does); a path outside is refused with a `permission-denied` error naming `IRONAV_ROOTS`. The check is made when a tool starts, so it guards against an agent's mistakes and injected instructions, not against a hostile local process that swaps paths meanwhile. |
| `IRONAV_READ_ONLY` | off | `1` refuses every call that would write (`run`, `stream_clip`, `analyze` with `thumbnail_dir`, `stream_open` and `stream_configure` with `record` or `transcode`). `plan`, `probe` and reading tools work. |
| `IRONAV_ALLOW_DEVICES` | off in `ironav mcp`, on elsewhere | `1` lets `device://video`, `device://audio` and `device://screen` sources (cameras, microphones, the screen) be opened. Opens are logged to stderr. |
| `IRONAV_ALLOW_COOKIES` | off in `ironav mcp`, on elsewhere | `1` allows the `?cookies=BROWSER` option, which lets yt-dlp read that browser's cookie store. |
| `IRONAV_MAX_PIXELS` | 268435456 (2^28; at most 16384 on a side) | The most pixels in one decoded picture; a stream that asks for more is refused before a decoder is made. A value replaces both limits. Audio streams above 64 channels are refused. |
| `IRONAV_YTDLP` | unset: `yt-dlp` from `PATH` | Absolute path of yt-dlp (a relative one is refused). `capabilities` reports which program is used. |
| `IRONAV_AUDIT_LOG` | off | A file to append one JSON line to for each tool call start and end (tool, caller `cli`/`mcp`/`webui`, outcome or error code, duration), file write, network source and device open, and each refusal by the settings above. Times are UTC RFC 3339; URLs are redacted; no credentials or media are written. |
| `IRONAV_NET_ALLOW` | unset: no outbound connections | Which addresses sources may reach: `private`, `loopback`, `public`, `any`, addresses and CIDR ranges, comma-separated, checked after the name resolves and at each redirect. A `whip://` endpoint takes publishers only from loopback and these addresses. See "Watching live sources". |
| `IRONAV_NET_ALLOW_METADATA` | off | `1` lets the cloud metadata endpoints (169.254.169.254 and the like) be reached when `IRONAV_NET_ALLOW` covers them. |
| `IRONAV_ALLOW_PLAINTEXT_BASIC` | off | `1` lets a Basic password be sent over plain `http://` or `rtsp://`. |
| `IRONAV_FIPS` | off (on in a `fips` build) | `1` restricts TLS to FIPS-approved algorithms and refuses plaintext transports, YouTube sources and MD5 Digest. The cryptography is not CMVP-validated. |
| `IRONAV_SIDECARS`, `IRONAV_CA_FILE` | unset | See the sections on sidecars and live sources. |

Other protections:

- **Panics.** Release builds unwind instead of aborting, and every tool call and each live session worker catches a panic and returns it as an `internal` error naming the tool, so a decoder bug on a hostile file ends that call, not the server. The release binary is larger for the unwind tables (about 29 percent larger `ironav.exe`, 10.0 to 12.9 MB, measured on Windows; speed was not measured).
- **Text from media is data.** Captions, sidecar transcripts, device, camera and rendition names and the error output of external programs are stripped of control characters, cut to 512 characters, and carry `"untrusted": true` (and `"truncated": true` when cut). The MCP `instructions` and the tool descriptions say not to follow requests found in them.
- **Temporary outputs.** `<output>.partial` is created exclusively (`O_EXCL`): an existing file or link there is refused, never written through or deleted. After a crash, remove the leftover and run again.
- **External programs.** yt-dlp and arecord are resolved to absolute paths. Sidecar children get only `PATH` and a few system variables (`HOME`, `LANG`, `TMPDIR` or `SystemRoot`, `TEMP`...) plus the `env` (name to value) and `inherit_env` (names to copy) their entry in the sidecar file lists. An entry can also set `max_memory_mb` and `max_cpu_seconds` to bound the child's memory and CPU time (no limit by default; see [docs/sidecars.md](docs/sidecars.md)).
- **MCP limits.** A request line over 4 MiB is refused (and skipped); at most 4 tool calls run at once, the next gets a JSON-RPC error (-32000) until one finishes. The tool annotations are truthful: `readOnlyHint`, `destructiveHint` for tools that can overwrite, `openWorldHint` for tools that reach the network. Run the server under a supervisor that restarts it.

## Security notes (browser GUI)

The native GUI runs in-process and opens no sockets. The `--web` variant is loopback only; every request needs a per-launch random token (compared in constant time), a loopback `Host` header and,
when the browser sends one, a loopback `Origin`. At most 32 connections and 4 running jobs are served. The file browser lists only the directories `IRONAV_ROOTS` allows (any readable directory when it is
unset), so treat the launch URL like a credential. The page carries a Content Security Policy with a per-response script nonce.

See [SECURITY.md](SECURITY.md) to report a vulnerability, and [docs/security/compliance.md](docs/security/compliance.md) for the security review and its CWE, MITRE ATT&CK, FIPS 140-3 and CMMC 2.0 mapping.

## Layout

`ironav-core` types/traits · `ironav-hwaccel` · `ironav-codec` · `ironav-formats` · `ironav-ontology` ·
`ironav-analyze` (analyzers and observations) · `ironav-live` (live sources) ·
`ironav-player` (playback engine) · `ironav-engine` (jobs, planner, pipeline, tool registry) · `ironav-gui` (native, egui) · `ironav-webui` (browser) ·
`ironav-cli`

Build: `cargo build --release` · Test: `cargo test` · Rust 1.95 or newer for `ironav-cli` and `ironav-gui` (egui sets that floor), 1.88 for the library crates; IronAV is built and tested with the current stable release
Lean build for servers/agents without the graphics or sound stack: `cargo build --release --no-default-features -p ironav-cli`

Codec features: `ironav-codec` puts each codec behind a cargo feature: `aac`, `ac3`, `alac`, `av1`, `flac`, `h264`,
`hevc`, `jpeg`, `mp3`, `mpeg2`, `mpeg4`, `opus`, `prores`, `vorbis`, `vp8` and `vp9`. Three more name shared parts:
`voice` (the voice-link DSP, which needs `resample`, the windowed-sinc resampler), `filters` (the filter registry)
and `order` (H.264 and HEVC picture order). `serde` gives the registries and core types their serialisation. All are
on by default, so the workspace crates are unchanged. A small build turns the defaults off and names what it needs,
for example `ironav-codec = { version = "0.1", default-features = false, features = ["opus", "voice"] }`, which
compiles Opus, the voice-link DSP and the resampler, with no serde. The feature list, and which features each one
pulls in, are commented in `crates/ironav-codec/Cargo.toml`.

## License

Dual-licensed: **[AGPL-3.0-or-later](LICENSE)** for open use, or a **commercial license** from Nervosys if you
cannot comply with the AGPL (e.g. closed-source embedding or proprietary hosted services). See
[COMMERCIAL-LICENSE.md](COMMERCIAL-LICENSE.md). Contributions are accepted under the terms in
[CONTRIBUTING.md](CONTRIBUTING.md), which let Nervosys offer the commercial license.
