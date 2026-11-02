# Model sidecars

IronAV links no machine-learning runtime. Speech-to-text, object detection, OCR, captioning and embeddings run as
**sidecars**: separate programs that IronAV starts, feeds decoded media and reads observations from, one JSON object per
line on their standard input and output. A sidecar can use any language, runtime or GPU. If one crashes or leaks memory,
only that child process is affected, and IronAV restarts it.

## Configuring

Sidecars are defined by whoever runs IronAV, not by requests. `IRONAV_SIDECARS` names a JSON file:

```json
{
  "asr":    { "command": ["python", "/opt/sidecars/whisper_asr.py"], "start_seconds": 120 },
  "detect": { "command": ["python", "/opt/sidecars/yolo_detect.py"] }
}
```

| Field | Default | Meaning |
|---|---|---|
| `command` | (required) | the program and its arguments, run without a shell |
| `start_seconds` | 30 | how long it may take to say hello (loading a model can be slow) |
| `finish_seconds` | 120 | how long it may take to finish once the input has ended |
| `hang_seconds` | 60 | a child that takes no input for this long while inputs are waiting is killed and restarted |
| `media` | (none) | `"audio"` or `"video"`: what its hello will ask for. When given, the sidecar is not started at all for media with no stream of that kind, and a hello asking for the other kind is a failure. Without it, IronAV must start the program to learn what it wants |
| `max_memory_mb` | (no limit) | the most memory the child may use, in whole MB (at most 1048576). See "Resource limits" below |
| `max_cpu_seconds` | (no limit) | the most CPU time the child may use over its life, in seconds (at most a year). See "Resource limits" below |

A request's sidecars start side by side, so a slow model load does not hold up the others.

A request names sidecars from that list (`"sidecars": ["asr"]` on `analyze` or `stream_open`, `--sidecars asr` on the
CLI). `capabilities` lists the configured names. A request cannot start any other program. Names are up to 32 lowercase
letters, digits, `_` or `-`, and cannot be a built-in analyzer's name. Each sidecar takes the first analyzed stream of the
kind it asks for (use `streams` to choose). A live session can also gain or lose a sidecar while it runs
(`stream_configure` with `add` or `remove`): it starts in the background and joins the first stream of its kind once it
has said hello, so the media is never held up by a model loading, and a removed one gets `end` and its usual time to
finish.

### Resource limits

`max_memory_mb` and `max_cpu_seconds` bound what a child can use. Neither is set by default, so an entry without them runs
as it always has. Recommended for a CPU-only speech or detection model: `max_memory_mb` of 2048 to 8192. For a GPU runtime,
leave `max_memory_mb` unset on Linux and macOS, because CUDA and large Python runtimes reserve far more address space than
they use. For `max_cpu_seconds`, choose a few times the CPU a file of the expected length needs. A long-running live
sidecar needs a value well above a day's CPU use, or none.

- **Windows:** the child runs in a job object. `max_memory_mb` caps its committed memory and `max_cpu_seconds` its user-mode
  CPU time. The job is assigned just after the child starts, so a grandchild started in that first instant is not limited.
- **Linux and macOS:** the limits are `setrlimit` calls made before the program runs. `max_memory_mb` caps the address space
  (`RLIMIT_AS`) and `max_cpu_seconds` the CPU time (`RLIMIT_CPU`). The memory limit therefore counts reserved address space,
  not only memory in use.

A child over a limit is stopped. IronAV treats that as a crash: it reports `analyzer_failed` and restarts the child with
back-off, and the CPU time starts again from zero in the new child. If a limit cannot be applied (the job cannot be made, or
`setrlimit` refuses), the child does not start and the error says why. Invalid values are refused when the list is read.
Limits do not restrict what a child reads, writes or connects to; use the operating system's sandboxing, a cgroup or a
container for that.

## Protocol (version 1)

The child speaks first, then IronAV sends the stream and media, and the child answers with observations at any time:

```text
child:  {"op":"hello","protocol":1,"wants":{"audio":{"rate":16000,"format":"s16","chunk_seconds":5,"gate":"speech"}}}
ironav: {"op":"start","protocol":1,"name":"asr","live":false,"stream":{"index":1,"media":"audio","codec":"aac","sample_rate":48000,"channels":2}}
ironav: {"op":"audio","t":12.48,"dur":5.0,"rate":16000,"format":"s16","final":false,"data":"<base64>"}
child:  {"op":"observe","kind":"transcript","t":12.48,"t_end":14.4,"data":{"text":"good evening","lang":"en"}}
ironav: {"op":"end"}
```

**`hello`** must be the child's first line. `wants` holds exactly one of these:

- `audio`:
  - `rate` (8000 to 192000, default 16000)
  - `format`: `s16` (the default) or `f32`, both little-endian
  - `chunk_seconds` (0.1 to 60, default 2)
  - `gate`: `speech` sends only speech found by the built-in `voice_activity` detector, with 0.25 s before each
    utterance. `none` (the default) sends everything.
  - Audio is always one channel: the stream's channels are averaged and resampled.
- `video`:
  - `every_seconds` (default 1). A picture is sent at the first frame at or after each due time.
  - `width` (16 to 4096, default 320). Pictures are scaled down to fit it, never up.
  - `format`: `png` (the default) or `rgb24` (raw packed RGB, row by row).

**`audio`** carries `t` (media seconds of the first sample), `dur`, and `data` in base64. Within an utterance, chunks
follow each other without gaps. `final: true` marks the last chunk of an utterance (or of the stream, without a gate),
which is the moment to finish a transcript. **`video`** carries `t`, `width`, `height`, `format` and `data`. **`end`**
means no more input will come. The child should report what it still holds and exit; its standard input is also closed.

**`observe`** carries:

- `kind`: up to 48 characters of lowercase letters, digits and `_`, starting with a letter, and not one of IronAV's own
  kinds
- `t`: media seconds, finite and not negative
- `t_end` (optional): not before `t`
- `data` (optional): any JSON

IronAV adds the sidecar's name as `analyzer`, the stream, and in live sessions `wall` and `latency_ms`. Observations that
break these rules are not logged, and are counted as `rejected` in the digest. Lines that are not JSON, and other `op`s,
are ignored. A line over 256 KiB counts as a crash. Diagnostics belong on standard error: its last lines are quoted when the
child fails.

## Behaviour

- **Files:** the analysis waits for the sidecar, so nothing is skipped. Up to 16 inputs queue for it.
- **Live sources:** inputs are dropped while the sidecar is behind, and `analyzer_lagging` reports how many, at most once
  every 10 s.
- **Crashes:** a child that exits, or takes nothing for `hang_seconds`, is restarted after 1, 2, 4, 8 and then 16 s.
  Each failure is an `analyzer_failed` observation (`data.message` and `data.restarting`). After five restarts it stays
  stopped and the analysis carries on without it. A child that ran for 5 minutes before failing gets its five restarts
  back. One that asks for different media after a restart is stopped.
- **Digests and summary:** a sidecar's part counts its observations by kind and joins the `data.text` of its observations
  (up to 4000 characters), so a transcript appears in the digest. It also includes `dropped_inputs`, `rejected` and
  `restarts` when they are not zero. Observations arrive asynchronously, so one may land in the digest after the window it
  describes.

## Reference sidecars

- `ironav-sidecar-example` (built with the engine, `crates/ironav-engine/src/bin/`): a DTMF (keypad tone) detector,
  needing no model, and the test modes used by IronAV's own tests. It is the smallest complete implementation of the
  protocol.
- [`examples/sidecars/whisper_asr.py`](../examples/sidecars/whisper_asr.py): speech-to-text with faster-whisper,
  speech-gated, one `transcript` observation per segment.
- [`examples/sidecars/yolo_detect.py`](../examples/sidecars/yolo_detect.py): object detection with a YOLOv8 model
  exported to ONNX and run by ONNX Runtime, one `objects` observation per picture with anything in it.

The two Python sidecars need their packages and model files installed, and IronAV's tests do not run them.
