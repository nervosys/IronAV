"""IronAV against the FFmpeg libraries (through PyAV) on one machine.

    pip install av numpy
    IRONAV_BIN=target/release/ironav python bench/vs_ffmpeg.py [media-dir]

The FFmpeg side runs in-process, so it pays no process start-up; the IronAV side is a fresh process
per run, start-up included. That leans against IronAV, and on Windows the start-up alone is about
100 ms. Each figure is the median of 5 runs. The audio part generates its own signal. The video part
needs av.mp4 (H.264, 720p), bf.mp4 (H.264 with B-frames) and src.y4m in the media directory and is
skipped when they are missing. FFmpeg's decode figures do not write the pictures anywhere, while
IronAV writes a Y4M file, so they overstate FFmpeg's advantage on decoding."""
import subprocess, json, os, time, statistics, sys
import numpy as np, av
IA = r"E:\cargo-target\release\ironav.exe"
V = (sys.argv[1] if len(sys.argv) > 1 else "bench-media").rstrip("/") + "/"
N = 5

def ironav(*a):
    t = time.perf_counter()
    r = subprocess.run([IA, *a], capture_output=True, text=True)
    dt = time.perf_counter() - t
    j = json.loads(r.stdout)
    assert j["ok"], j
    return dt

def med(f, n=N):
    xs = [f() for _ in range(n)]
    return statistics.median(xs), min(xs)

def rm(*fs):
    for f in fs:
        if os.path.exists(f): os.remove(f)

def line(name, a, b, unit_work, work):
    print(f"{name:34s} IronAV {a[0]*1000:8.0f} ms (min {a[1]*1000:6.0f})   FFmpeg {b[0]*1000:8.0f} ms (min {b[1]*1000:6.0f})   ratio {b[0]/a[0]:5.2f}x   {work/a[0]:7.0f} vs {work/b[0]:7.0f} {unit_work}")

# ---------- startup / probe latency ----------
def pyav_probe():
    t = time.perf_counter()
    c = av.open(V + "av.mp4"); _ = [(s.type, s.codec_context.name) for s in c.streams]; c.close()
    return time.perf_counter() - t
if os.path.exists(V + "av.mp4"):
  a = med(lambda: ironav("probe", V + "av.mp4"), 15)
  b = med(pyav_probe, 15)
  print(f"probe av.mp4: IronAV process {a[0]*1000:.0f} ms median (incl. process start); FFmpeg in-process open {b[0]*1000:.1f} ms (python already running)")

# ---------- audio: 180 s stereo 44.1k WAV <-> FLAC ----------
rng = np.random.default_rng(7); sr = 44100; n = sr * 180; t = np.arange(n) / sr
sig = sum(a_ * np.sin(2*np.pi*f*t + p) for a_, f, p in [(.3,220,0),(.2,330,1),(.12,495,2),(.08,1320,.5),(.05,3100,3)])
sig = sig * (0.4 + 0.6*np.abs(np.sin(2*np.pi*0.7*t))**3) + rng.normal(0, .01, n)
st = (np.stack([sig, .8*sig + rng.normal(0, .008, n)], 1) * .7)
pcm = (st * 32767).astype(np.int16)
import wave
with wave.open("b_in.wav", "wb") as w:
    w.setnchannels(2); w.setsampwidth(2); w.setframerate(sr); w.writeframes(pcm.tobytes())
raw = os.path.getsize("b_in.wav")

def ff_flac_enc():
    rm("b_ff.flac")
    t = time.perf_counter()
    ic = av.open("b_in.wav"); oc = av.open("b_ff.flac", "w")
    os_ = oc.add_stream("flac", rate=sr); os_.layout = "stereo"
    for fr in ic.decode(audio=0):
        for p in os_.encode(fr): oc.mux(p)
    for p in os_.encode(None): oc.mux(p)
    oc.close(); ic.close()
    return time.perf_counter() - t
def ia_flac_enc():
    rm("b_ia.flac")
    return ironav("convert", "-i", "b_in.wav", "-o", "b_ia.flac", "--acodec", "flac")
a = med(ia_flac_enc); b = med(ff_flac_enc)
line("WAV->FLAC encode, 180 s audio", a, b, "x realtime", 180)
print(f"   sizes: IronAV {os.path.getsize('b_ia.flac')/raw:.4f}  FFmpeg {os.path.getsize('b_ff.flac')/raw:.4f}  (fraction of WAV)")

def ff_flac_dec():
    t = time.perf_counter()
    c = av.open("b_ia.flac"); n_ = 0
    for fr in c.decode(audio=0): n_ += fr.samples
    c.close()
    return time.perf_counter() - t
def ia_flac_dec():
    rm("b_back.wav")
    return ironav("convert", "-i", "b_ia.flac", "-o", "b_back.wav", "--acodec", "pcm_s16le")
a = med(ia_flac_dec); b = med(ff_flac_dec)
line("FLAC->WAV decode, 180 s (FFmpeg: decode only)", a, b, "x realtime", 180)
# FFmpeg decode of FLAC *produced by IronAV* also proves interop; IronAV's output WAV must be lossless
back = av.open("b_back.wav"); arr = np.concatenate([f.to_ndarray() for f in back.decode(audio=0)], axis=1)
print("   roundtrip lossless (IronAV FLAC -> IronAV WAV == source):", np.array_equal(arr.reshape(-1), pcm.reshape(-1)) or np.array_equal(arr, pcm.T))
back.close(); rm("b_in.wav", "b_ia.flac", "b_ff.flac", "b_back.wav")

# ---------- video ----------
if not all(os.path.exists(V + f) for f in ("av.mp4", "bf.mp4", "src.y4m")):
    print(f"video part skipped: need av.mp4, bf.mp4 and src.y4m in {V}")
    sys.exit(0)
# ---------- video decode: 720p H.264, 240 frames ----------
def ff_dec(hw=None, src="av.mp4"):
    t = time.perf_counter()
    kw = {}
    if hw:
        from av.codec.hwaccel import HWAccel
        kw["hwaccel"] = HWAccel(device_type=hw)
    c = av.open(V + src, **kw); c.streams.video[0].thread_type = "AUTO"; k = 0
    for fr in c.decode(video=0): k += 1
    c.close()
    return time.perf_counter() - t, k
def ia_dec(src="av.mp4", hw="auto"):
    rm("b_dec.y4m")
    d = ironav("convert", "-i", V + src, "-o", "b_dec.y4m", "-f", "y4m", "--vcodec", "rawvideo", "-y", "--hwaccel", hw)
    rm("b_dec.y4m"); return d
for src, frames in (("av.mp4", 240), ("bf.mp4", 50)):
    print(f"--- decode {src} ({frames} frames)")
    a = med(lambda: ia_dec(src)); line("IronAV NVDEC -> y4m file vs FFmpeg CPU", a, med(lambda: ff_dec(None, src)[0]), "fps", frames)
    try:
        b = med(lambda: ff_dec("cuda", src)[0]); line("IronAV NVDEC -> y4m file vs FFmpeg cuda", a, b, "fps", frames)
    except Exception as e:
        print("   FFmpeg cuda hwaccel via PyAV failed:", repr(e)[:200])

# ---------- video encode: NVENC, 240 frames 720p from y4m ----------
print("--- encode src.y4m -> H.264 NVENC")
def ia_enc():
    rm("b_enc.h264")
    return ironav("convert", "-i", V + "src.y4m", "-o", "b_enc.h264", "--vcodec", "h264", "--hwaccel", "nvenc", "--vbitrate", "4000", "-y")
def ff_enc(codec="h264_nvenc"):
    rm("b_ff.h264")
    t = time.perf_counter()
    ic = av.open(V + "src.y4m"); oc = av.open("b_ff.h264", "w", format="h264")
    s = ic.streams.video[0]
    os_ = oc.add_stream(codec, rate=s.average_rate); os_.width = s.width; os_.height = s.height; os_.pix_fmt = "yuv420p"; os_.bit_rate = 4_000_000
    for fr in ic.decode(video=0):
        for p in os_.encode(fr): oc.mux(p)
    for p in os_.encode(None): oc.mux(p)
    oc.close(); ic.close()
    return time.perf_counter() - t
a = med(ia_enc); b = med(ff_enc)
line("IronAV NVENC vs FFmpeg h264_nvenc", a, b, "fps", 240)
print(f"   sizes: IronAV {os.path.getsize('b_enc.h264')} B  FFmpeg {os.path.getsize('b_ff.h264')} B")
b2 = med(lambda: ff_enc("libx264"), 3); line("IronAV NVENC vs FFmpeg libx264 (CPU)", a, b2, "fps", 240)
rm("b_enc.h264", "b_ff.h264")
