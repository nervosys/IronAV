"""IronAV sidecar: speech to text with faster-whisper (https://github.com/SYSTRAN/faster-whisper).

    pip install faster-whisper

The model is fetched on first use, or set WHISPER_MODEL to a local model directory. WHISPER_DEVICE (auto, cpu, cuda)
and WHISPER_COMPUTE (default, int8, float16, ...) pass through to faster-whisper. List it in the IRONAV_SIDECARS file:

    {"asr": {"command": ["python", "/path/to/whisper_asr.py"], "start_seconds": 300}}

IronAV sends speech only (gate "speech"), 16 kHz mono f32, in chunks of up to 10 s. An utterance is transcribed when
its final chunk arrives (or every 30 s within a long one), and each segment becomes a `transcript` observation.
Protocol: docs/sidecars.md. Not run by IronAV's tests.
"""
import base64
import json
import math
import os
import sys

import numpy as np
from faster_whisper import WhisperModel

RATE = 16000
LONGEST = 30 * RATE


def say(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def transcribe(model, audio, start):
    segments, info = model.transcribe(audio, beam_size=5, vad_filter=False, condition_on_previous_text=False)
    for seg in segments:
        text = seg.text.strip()
        if not text:
            continue
        say({
            "op": "observe",
            "kind": "transcript",
            "t": round(start + seg.start, 3),
            "t_end": round(start + max(seg.end, seg.start), 3),
            "data": {"text": text, "lang": info.language, "confidence": round(math.exp(seg.avg_logprob), 3)},
        })


def main():
    model = WhisperModel(
        os.environ.get("WHISPER_MODEL", "base"),
        device=os.environ.get("WHISPER_DEVICE", "auto"),
        compute_type=os.environ.get("WHISPER_COMPUTE", "default"),
    )
    say({"op": "hello", "protocol": 1,
         "wants": {"audio": {"rate": RATE, "format": "f32", "chunk_seconds": 10, "gate": "speech"}}})
    pending, start, held = [], None, 0
    for line in sys.stdin:
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        op = msg.get("op")
        if op == "audio":
            if start is None:
                start = msg["t"]
            samples = np.frombuffer(base64.b64decode(msg["data"]), dtype="<f4")
            pending.append(samples)
            held += len(samples)
            if msg.get("final") or held >= LONGEST:
                transcribe(model, np.concatenate(pending), start)
                pending, start, held = [], None, 0
        elif op == "end":
            break
    if pending:
        transcribe(model, np.concatenate(pending), start)


if __name__ == "__main__":
    main()
