"""IronAV against the ffmpeg command-line program, on one machine.

    IRONAV_BIN=C:/cargo-target-ironav/release/ironav.exe FFMPEG=ffmpeg python -I bench/vs_ffmpeg.py [media-dir]

Both sides are command-line programs, one fresh process per run, start-up included. Each job runs the IronAV
command and the ffmpeg command in turn, five runs each, and both write their real output to a temporary directory.
Wall time is time.perf_counter around subprocess.run. The table gives the median and the minimum; the ratio is
FFmpeg median over IronAV median, so above 1 means IronAV was faster. After the timing, each IronAV output is
probed with ffprobe and fully decoded, and audio outputs are compared by MD5 of their decoded samples.

Inputs that are missing from the media directory are generated there: a.wav (180 s, stereo, 44.1 kHz, 16-bit,
numpy signal), ref.flac (ffmpeg's FLAC of a.wav), src.y4m (testsrc2, 240 frames, 1280x720, 30 fps), av.mp4
(H.264 720p, no B-frames) and bf.mp4 (H.264 720p, 3 B-frames, libx264 only). av.mp4 uses libx264 when present,
else h264_nvenc. A job whose encoder or input is unavailable is skipped, and the table says why.

IRONAV_BIN defaults to C:/cargo-target-ironav/release/ironav.exe and FFMPEG to ffmpeg (ffprobe is taken from the
same place). Figures are from one machine with a warm file cache. Encoders run with their own defaults, so output
sizes differ and are not a quality comparison.
"""
import json, os, re, shutil, statistics, subprocess, sys, tempfile, time, wave

IRONAV = os.environ.get("IRONAV_BIN", "C:/cargo-target-ironav/release/ironav.exe")
FFMPEG = os.environ.get("FFMPEG", "ffmpeg")
_d, _b = os.path.split(FFMPEG)
FFPROBE = os.path.join(_d, _b.replace("ffmpeg", "ffprobe"))
V = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else "bench-media")
REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
N = 5
TMP = tempfile.mkdtemp(prefix="ironav-vs-ffmpeg-")
rows = []


class Fail(Exception):
    pass


def sh(cmd):
    return subprocess.run(cmd, stdin=subprocess.DEVNULL, capture_output=True, text=True, errors="replace")


def ffmpeg_args(*a):
    return [FFMPEG, "-nostdin", "-hide_banner", "-nostats", "-v", "error", *a]


def J(name):
    return os.path.join(TMP, name)


def p(name):
    return os.path.join(V, name)


def size(path):
    return os.path.getsize(path) if path and os.path.exists(path) else None


def fmt_n(x):
    return "missing" if x is None else f"{x:,} B"


def encoders():
    out = sh([FFMPEG, "-hide_banner", "-encoders"]).stdout
    return {m.group(1) for m in re.finditer(r"^\s*[VAS][.A-Za-z]{5}\s+(\S+)", out, re.M)}


def ff_make(*a):
    r = sh(ffmpeg_args("-y", *a))
    if r.returncode:
        raise SystemExit(f"generating input failed: {r.stderr.strip()}")


def make_inputs(enc):
    os.makedirs(V, exist_ok=True)
    wav = p("a.wav")
    if not os.path.exists(wav):
        import numpy as np
        rng = np.random.default_rng(7); sr = 44100; n = sr * 180; t = np.arange(n) / sr
        sig = sum(a_ * np.sin(2*np.pi*f*t + ph) for a_, f, ph in [(.3,220,0),(.2,330,1),(.12,495,2),(.08,1320,.5),(.05,3100,3)])
        sig = sig * (0.4 + 0.6*np.abs(np.sin(2*np.pi*0.7*t))**3) + rng.normal(0, .01, n)
        st = (np.stack([sig, .8*sig + rng.normal(0, .008, n)], 1) * .7)
        pcm = (st * 32767).astype(np.int16)
        with wave.open(wav + ".part", "wb") as w:
            w.setnchannels(2); w.setsampwidth(2); w.setframerate(sr); w.writeframes(pcm.tobytes())
        os.replace(wav + ".part", wav)
    if not os.path.exists(p("ref.flac")):
        ff_make("-i", wav, "-c:a", "flac", p("ref.flac"))
    src = p("src.y4m")
    if not os.path.exists(src):
        ff_make("-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30", "-frames:v", "240", "-pix_fmt", "yuv420p", src)
    if not os.path.exists(p("av.mp4")) and ("libx264" in enc or "h264_nvenc" in enc):
        codec = (["-c:v", "libx264", "-preset", "medium", "-crf", "23", "-bf", "0"] if "libx264" in enc
                 else ["-c:v", "h264_nvenc", "-bf", "0"])
        ff_make("-i", src, *codec, "-pix_fmt", "yuv420p", p("av.mp4"))
    if not os.path.exists(p("bf.mp4")) and "libx264" in enc:
        ff_make("-i", src, "-c:v", "libx264", "-preset", "medium", "-crf", "23", "-bf", "3",
                "-pix_fmt", "yuv420p", p("bf.mp4"))


def timed(cmd, outs, is_ironav):
    for o in outs:
        if os.path.exists(o):
            os.remove(o)
    t = time.perf_counter()
    r = sh(cmd)
    dt = time.perf_counter() - t
    if is_ironav:
        try:
            j = json.loads(r.stdout)
        except ValueError:
            j = None
        if r.returncode or not (j and j.get("ok")):
            raise Fail(f"exit {r.returncode}, output: " + (r.stdout.strip() or r.stderr.strip())[:800])
    elif r.returncode:
        raise Fail(f"exit {r.returncode}: " + r.stderr.strip()[:800])
    for o in outs:
        if not os.path.exists(o) or os.path.getsize(o) == 0:
            raise Fail(f"no output written: {o}")
    return dt


def job(name, ia_cmd, ff_cmd, ia_out=None, ff_out=None, extra="", md5_ref=None):
    row = {"name": name, "ia": [], "ff": [], "err": {}, "ia_out": ia_out, "ff_out": ff_out,
           "extra": extra, "md5_ref": md5_ref}
    rows.append(row)
    print(f"running {name} ...", flush=True)
    for _ in range(N):
        for side, cmd, out in (("ia", ia_cmd, ia_out), ("ff", ff_cmd, ff_out)):
            if side in row["err"]:
                continue
            try:
                row[side].append(timed(cmd, [out] if out else [], side == "ia"))
            except Fail as e:
                row["err"][side] = str(e)


def skip(name, why):
    rows.append({"name": name, "skip": why})
    print(f"skipped {name}: {why}", flush=True)


def machine():
    cpu = sh(["powershell", "-NoProfile", "-Command", "(Get-CimInstance Win32_Processor).Name"]).stdout.strip().splitlines()
    gpu = sh(["nvidia-smi", "--query-gpu=name,driver_version", "--format=csv,noheader"]).stdout.strip().splitlines()
    ffv = (sh([FFMPEG, "-version"]).stdout.splitlines() or ["unknown"])[0]
    git = sh(["git", "-C", REPO, "rev-parse", "--short", "HEAD"]).stdout.strip()
    return [
        f"CPU: {cpu[0].strip() if cpu else 'unknown'}",
        f"logical cores: {os.cpu_count()}",
        f"GPU: {'; '.join(g.strip() for g in gpu) or 'nvidia-smi unavailable'}",
        f"FFmpeg: {ffv}",
        f"IronAV binary: {IRONAV}; repo HEAD {git or 'unknown'}",
    ]


def fmt_t(ts):
    if not ts:
        return "FAIL"
    return f"{statistics.median(ts)*1000:,.0f} ms (min {min(ts)*1000:,.0f})"


def notes(r):
    parts = [r["extra"]] if r["extra"] else []
    if size(r["ia_out"]) is not None or size(r["ff_out"]) is not None:
        parts.append(f"output bytes: IronAV {fmt_n(size(r['ia_out']))}, FFmpeg {fmt_n(size(r['ff_out']))}")
    for side, label in (("ia", "IronAV"), ("ff", "FFmpeg")):
        if side in r["err"]:
            parts.append(f"{label} FAILED, see below")
    return "; ".join(parts)


def ffprobe_line(path):
    r = sh([FFPROBE, "-v", "error", "-count_frames", "-show_entries",
            "format=format_name,duration:stream=codec_type,codec_name,width,height,sample_rate,channels,nb_read_frames",
            "-of", "json", path])
    if r.returncode:
        return f"ffprobe FAILED: {r.stderr.strip()[:300]}"
    j = json.loads(r.stdout)
    parts = []
    for s in j.get("streams", []):
        t = s.get("codec_type")
        if t == "video":
            parts.append(f"video {s.get('codec_name')} {s.get('width')}x{s.get('height')} frames={s.get('nb_read_frames')}")
        elif t == "audio":
            parts.append(f"audio {s.get('codec_name')} {s.get('sample_rate')} Hz {s.get('channels')} ch")
        else:
            parts.append(str(t))
    fmt = j.get("format", {})
    return f"{fmt.get('format_name')}, {fmt.get('duration')} s; " + ", ".join(parts)


def decodes_clean(path):
    r = sh(ffmpeg_args("-i", path, "-map", "0", "-f", "null", "-"))
    if r.returncode == 0 and not r.stderr.strip():
        return "decodes clean"
    return "decode problems: " + (r.stderr.strip() or f"exit {r.returncode}")[:300]


def md5(path):
    m = re.search(r"MD5=([0-9a-f]+)", sh(ffmpeg_args("-i", path, "-map", "0:a:0", "-f", "md5", "-")).stdout)
    return m.group(1) if m else None


def frame_types(path):
    return sh([FFPROBE, "-v", "error", "-select_streams", "v:0", "-show_entries", "frame=pict_type",
               "-of", "csv=p=0", path]).stdout.split()


def main():
    if not os.path.exists(IRONAV):
        raise SystemExit(f"IronAV binary not found: {IRONAV} (set IRONAV_BIN)")
    print("Machine")
    for line in machine():
        print("  " + line)
    print(f"Media dir: {V}", flush=True)
    enc = encoders()
    make_inputs(enc)
    wav, ref, src, av, bf = p("a.wav"), p("ref.flac"), p("src.y4m"), p("av.mp4"), p("bf.mp4")
    has_av, has_bf = os.path.exists(av), os.path.exists(bf)

    # 1. WAV -> FLAC
    job("1. WAV -> FLAC, 180 s stereo",
        [IRONAV, "convert", "-i", wav, "-o", J("j1_ia.flac"), "-y"],
        ffmpeg_args("-y", "-i", wav, "-c:a", "flac", J("j1_ff.flac")),
        J("j1_ia.flac"), J("j1_ff.flac"), extra=f"input {fmt_n(size(wav))}", md5_ref=wav)

    # 2. FLAC -> WAV (the FLAC is ffmpeg's)
    job("2. FLAC -> WAV, 180 s stereo",
        [IRONAV, "convert", "-i", ref, "-o", J("j2_ia.wav"), "-y"],
        ffmpeg_args("-y", "-i", ref, J("j2_ff.wav")),
        J("j2_ia.wav"), J("j2_ff.wav"), extra=f"input {fmt_n(size(ref))}", md5_ref=ref)
    job("2b. FLAC -> WAV, IronAV with --acodec pcm_s16le",
        [IRONAV, "convert", "-i", ref, "-o", J("j2b_ia.wav"), "--acodec", "pcm_s16le", "-y"],
        ffmpeg_args("-y", "-i", ref, J("j2b_ff.wav")),
        J("j2b_ia.wav"), J("j2b_ff.wav"), extra="same as 2 with the PCM codec named", md5_ref=ref)

    # 3. WAV -> Opus
    if "libopus" in enc:
        job("3. WAV -> Opus 128 kb/s (stereo)",
            [IRONAV, "convert", "-i", wav, "-o", J("j3_ia.opus"), "--acodec", "opus", "--abitrate", "128", "-y"],
            ffmpeg_args("-y", "-i", wav, "-c:a", "libopus", "-b:a", "128k", J("j3_ff.opus")),
            J("j3_ia.opus"), J("j3_ff.opus"), extra="lossy: no sample comparison")
    else:
        skip("3. WAV -> Opus", "no libopus encoder in this FFmpeg")

    # 4. Y4M -> H.264 on NVENC
    if not os.path.exists(src):
        skip("4. Y4M -> H.264 NVENC", "src.y4m missing")
    elif "h264_nvenc" not in enc:
        skip("4. Y4M -> H.264 NVENC", "no h264_nvenc encoder in this FFmpeg")
    else:
        job("4. Y4M -> H.264 NVENC, 240 frames 720p",
            [IRONAV, "convert", "-i", src, "-o", J("j4_ia.mp4"), "--vcodec", "h264", "--hwaccel", "require", "-y"],
            ffmpeg_args("-y", "-i", src, "-c:v", "h264_nvenc", J("j4_ff.mp4")),
            J("j4_ia.mp4"), J("j4_ff.mp4"), extra="each with its own default bitrate")

    # 5. H.264 MP4 -> Y4M, GPU decode
    if not has_av:
        skip("5. H.264 -> Y4M, GPU decode", "av.mp4 missing")
    else:
        job("5. H.264 -> Y4M, GPU decode, 240 frames",
            [IRONAV, "convert", "-i", av, "-o", J("j5_ia.y4m"), "--hwaccel", "auto", "-y"],
            ffmpeg_args("-y", "-hwaccel", "cuda", "-i", av, "-f", "yuv4mpegpipe", "-pix_fmt", "yuv420p", J("j5_ff.y4m")),
            J("j5_ia.y4m"), J("j5_ff.y4m"), extra="both write the decoded frames")

    # 6. H.264 MP4 -> Y4M, software decode
    if not has_av:
        skip("6. H.264 -> Y4M, software decode", "av.mp4 missing")
    else:
        job("6a. H.264 -> Y4M, software, av.mp4",
            [IRONAV, "convert", "-i", av, "-o", J("j6a_ia.y4m"), "--hwaccel", "off", "-y"],
            ffmpeg_args("-y", "-threads", "0", "-i", av, "-f", "yuv4mpegpipe", "-pix_fmt", "yuv420p", J("j6a_ff.y4m")),
            J("j6a_ia.y4m"), J("j6a_ff.y4m"), extra="no hwaccel on either side")
    if has_bf:
        job("6b. H.264 -> Y4M, software, bf.mp4 (B-frames)",
            [IRONAV, "convert", "-i", bf, "-o", J("j6b_ia.y4m"), "--hwaccel", "off", "-y"],
            ffmpeg_args("-y", "-threads", "0", "-i", bf, "-f", "yuv4mpegpipe", "-pix_fmt", "yuv420p", J("j6b_ff.y4m")),
            J("j6b_ia.y4m"), J("j6b_ff.y4m"), extra="no hwaccel on either side")
    else:
        skip("6b. H.264 -> Y4M, software, bf.mp4", "bf.mp4 missing (needs libx264)")

    # 7. Start-up
    job("7. Process start-up",
        [IRONAV, "capabilities"], [FFMPEG, "-version"], extra="no output file; fixed cost per process")

    print("\n## Results (median, min over 5 runs; ratio = FFmpeg / IronAV, above 1 means IronAV faster)\n")
    print("| Job | IronAV median (min) | FFmpeg median (min) | FFmpeg / IronAV | Notes |")
    print("|---|---|---|---|---|")
    for r in rows:
        if "skip" in r:
            print(f"| {r['name']} | skipped | skipped | | {r['skip']} |")
            continue
        ratio = (f"{statistics.median(r['ff'])/statistics.median(r['ia']):.2f}x"
                 if r["ia"] and r["ff"] else "n/a")
        print(f"| {r['name']} | {fmt_t(r['ia'])} | {fmt_t(r['ff'])} | {ratio} | {notes(r)} |")

    errs = [(r["name"], side, e) for r in rows if "skip" not in r for side, e in r["err"].items()]
    if errs:
        print("\n## Failures")
        for name, side, e in errs:
            print(f"- {name} [{'IronAV' if side == 'ia' else 'FFmpeg'}]: {e}")

    print("\n## Verification of IronAV outputs (after timing)")
    if has_av:
        print(f"- av.mp4 B-frames: {frame_types(av).count('B')} of {len(frame_types(av))} frames")
    if has_bf:
        print(f"- bf.mp4 B-frames: {frame_types(bf).count('B')} of {len(frame_types(bf))} frames")
    for r in rows:
        if "skip" in r or "ia" in r["err"] or not r["ia"] or not r["ia_out"]:
            continue
        out = r["ia_out"]
        print(f"- {r['name']}: {ffprobe_line(out)}; {decodes_clean(out)}")
        if r["md5_ref"]:
            got, want = md5(out), md5(r["md5_ref"])
            print(f"    audio MD5 IronAV {got} vs source {want}: {'match' if got == want else 'DIFFERENT'}")

    print("\n## Machine")
    for line in machine():
        print("- " + line)
    shutil.rmtree(TMP, ignore_errors=True)


if __name__ == "__main__":
    main()
