#!/usr/bin/env python3
"""Writes small MPEG-1 and MPEG-2 video streams made of random but valid syntax, to test decoders.

FFmpeg's encoders write only frame pictures, so field pictures, 16x8 and dual prime motion, concealment
vectors, escape codes, custom matrices and the rest of the interlaced tools are made here: every
macroblock gets a random type, random vectors (kept inside the picture, which decoders need) and random
coefficients, written with the VLC tables of ISO/IEC 11172-2 and 13818-2 (the tables as in FFmpeg's
libavcodec/mpeg12data.c, LGPL-2.1-or-later). With an output directory, it also runs `ffmpeg` to decode
each stream with `-idct simple` into the `.ffmpeg.yuv` file that the tests compare IronAV's decode with.
`-cpuflags 0` makes FFmpeg use its C transform: the random coefficients go outside the range of real
streams, where the SIMD transforms saturate and the C one wraps (IronAV follows the C one).

    python tools/gen_mpeg2_synth.py crates/ironav-engine/tests/data
"""
import random
import sys

# Tables of MPEG-1/2 video (ISO/IEC 11172-2, 13818-2), as in FFmpeg libavcodec/mpeg12data.c (LGPL-2.1-or-later).
DC_LUM_CODE = [
    4, 0, 1, 5, 6, 14, 30, 62, 126, 254, 510, 511,
]
DC_LUM_BITS = [
    3, 2, 2, 3, 3, 4, 5, 6, 7, 8, 9, 9,
]
DC_CHROMA_CODE = [
    0, 1, 2, 6, 14, 30, 62, 126, 254, 510, 1022, 1023,
]
DC_CHROMA_BITS = [
    2, 2, 2, 3, 4, 5, 6, 7, 8, 9, 10, 10,
]
MPEG1_VLC = [
    3, 2, 4, 4, 5, 5, 6, 7, 38, 8, 33, 8, 10, 10, 29, 12,
    24, 12, 19, 12, 16, 12, 26, 13, 25, 13, 24, 13, 23, 13, 31, 14,
    30, 14, 29, 14, 28, 14, 27, 14, 26, 14, 25, 14, 24, 14, 23, 14,
    22, 14, 21, 14, 20, 14, 19, 14, 18, 14, 17, 14, 16, 14, 24, 15,
    23, 15, 22, 15, 21, 15, 20, 15, 19, 15, 18, 15, 17, 15, 16, 15,
    3, 3, 6, 6, 37, 8, 12, 10, 27, 12, 22, 13, 21, 13, 31, 15,
    30, 15, 29, 15, 28, 15, 27, 15, 26, 15, 25, 15, 19, 16, 18, 16,
    17, 16, 16, 16, 5, 4, 4, 7, 11, 10, 20, 12, 20, 13, 7, 5,
    36, 8, 28, 12, 19, 13, 6, 5, 15, 10, 18, 12, 7, 6, 9, 10,
    18, 13, 5, 6, 30, 12, 20, 16, 4, 6, 21, 12, 7, 7, 17, 12,
    5, 7, 17, 13, 39, 8, 16, 13, 35, 8, 26, 16, 34, 8, 25, 16,
    32, 8, 24, 16, 14, 10, 23, 16, 13, 10, 22, 16, 8, 10, 21, 16,
    31, 12, 26, 12, 25, 12, 23, 12, 22, 12, 31, 13, 30, 13, 29, 13,
    28, 13, 27, 13, 31, 16, 30, 16, 29, 16, 28, 16, 27, 16, 1, 6,
    2, 2,
]
MPEG2_VLC = [
    2, 2, 6, 3, 7, 4, 28, 5, 29, 5, 5, 6, 4, 6, 123, 7,
    124, 7, 35, 8, 34, 8, 250, 8, 251, 8, 254, 8, 255, 8, 31, 14,
    30, 14, 29, 14, 28, 14, 27, 14, 26, 14, 25, 14, 24, 14, 23, 14,
    22, 14, 21, 14, 20, 14, 19, 14, 18, 14, 17, 14, 16, 14, 24, 15,
    23, 15, 22, 15, 21, 15, 20, 15, 19, 15, 18, 15, 17, 15, 16, 15,
    2, 3, 6, 5, 121, 7, 39, 8, 32, 8, 22, 13, 21, 13, 31, 15,
    30, 15, 29, 15, 28, 15, 27, 15, 26, 15, 25, 15, 19, 16, 18, 16,
    17, 16, 16, 16, 5, 5, 7, 7, 252, 8, 12, 10, 20, 13, 7, 5,
    38, 8, 28, 12, 19, 13, 6, 6, 253, 8, 18, 12, 7, 6, 4, 9,
    18, 13, 6, 7, 30, 12, 20, 16, 4, 7, 21, 12, 5, 7, 17, 12,
    120, 7, 17, 13, 122, 7, 16, 13, 33, 8, 26, 16, 37, 8, 25, 16,
    36, 8, 24, 16, 5, 9, 23, 16, 7, 9, 22, 16, 13, 10, 21, 16,
    31, 12, 26, 12, 25, 12, 23, 12, 22, 12, 31, 13, 30, 13, 29, 13,
    28, 13, 27, 13, 31, 16, 30, 16, 29, 16, 28, 16, 27, 16, 1, 6,
    6, 4,
]
LEVEL = [
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
    17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32,
    33, 34, 35, 36, 37, 38, 39, 40, 1, 2, 3, 4, 5, 6, 7, 8,
    9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 1, 2, 3, 4, 5, 1,
    2, 3, 4, 1, 2, 3, 1, 2, 3, 1, 2, 3, 1, 2, 1, 2,
    1, 2, 1, 2, 1, 2, 1, 2, 1, 2, 1, 2, 1, 2, 1, 2,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
]
RUN = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 3,
    3, 3, 3, 4, 4, 4, 5, 5, 5, 6, 6, 6, 7, 7, 8, 8,
    9, 9, 10, 10, 11, 11, 12, 12, 13, 13, 14, 14, 15, 15, 16, 16,
    17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31,
]
MBINCR = [
    1, 1, 3, 3, 2, 3, 3, 4, 2, 4, 3, 5, 2, 5, 7, 7,
    6, 7, 11, 8, 10, 8, 9, 8, 8, 8, 7, 8, 6, 8, 23, 10,
    22, 10, 21, 10, 20, 10, 19, 10, 18, 10, 35, 11, 34, 11, 33, 11,
    32, 11, 31, 11, 30, 11, 29, 11, 28, 11, 27, 11, 26, 11, 25, 11,
    24, 11, 8, 11, 15, 11, 0, 8,
]
MBPAT = [
    1, 9, 11, 5, 9, 5, 13, 6, 13, 4, 23, 7, 19, 7, 31, 8,
    12, 4, 22, 7, 18, 7, 30, 8, 19, 5, 27, 8, 23, 8, 19, 8,
    11, 4, 21, 7, 17, 7, 29, 8, 17, 5, 25, 8, 21, 8, 17, 8,
    15, 6, 15, 8, 13, 8, 3, 9, 15, 5, 11, 8, 7, 8, 7, 9,
    10, 4, 20, 7, 16, 7, 28, 8, 14, 6, 14, 8, 12, 8, 2, 9,
    16, 5, 24, 8, 20, 8, 16, 8, 14, 5, 10, 8, 6, 8, 6, 9,
    18, 5, 26, 8, 22, 8, 18, 8, 13, 5, 9, 8, 5, 8, 5, 9,
    12, 5, 8, 8, 4, 8, 4, 9, 7, 3, 10, 5, 8, 5, 12, 6,
]
MV = [
    1, 1, 1, 2, 1, 3, 1, 4, 3, 6, 5, 7, 4, 7, 3, 7,
    11, 9, 10, 9, 9, 9, 17, 10, 16, 10, 15, 10, 14, 10, 13, 10,
    12, 10,
]

# Macroblock types: flags (as in the decoder) and (code, length)
F_INTRA, F_CBP, F_QUANT, F_FWD, F_BWD, F_ZERO = 1, 2, 4, 8, 16, 32
PTYPE = [((3, 5), F_INTRA), ((1, 2), F_FWD | F_CBP | F_ZERO), ((1, 3), F_FWD), ((1, 1), F_FWD | F_CBP),
         ((1, 6), F_QUANT | F_INTRA), ((1, 5), F_QUANT | F_FWD | F_CBP | F_ZERO), ((2, 5), F_QUANT | F_FWD | F_CBP)]
BTYPE = [((3, 5), F_INTRA), ((2, 3), F_BWD), ((3, 3), F_BWD | F_CBP), ((2, 4), F_FWD), ((3, 4), F_FWD | F_CBP),
         ((2, 2), F_FWD | F_BWD), ((3, 2), F_FWD | F_BWD | F_CBP), ((1, 6), F_QUANT | F_INTRA),
         ((2, 6), F_QUANT | F_BWD | F_CBP), ((3, 6), F_QUANT | F_FWD | F_CBP), ((2, 5), F_QUANT | F_FWD | F_BWD | F_CBP)]
NONLIN = [0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 18, 20, 22, 24, 28, 32, 36, 40, 44, 48, 52, 56, 64, 72, 80, 88, 96, 104, 112]
ZIGZAG = [0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28,
          35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63]


class BW:
    def __init__(self):
        self.v = 0
        self.n = 0

    def put(self, val, n):
        assert 0 <= val < (1 << n) or n == 0, (val, n)
        self.v = (self.v << n) | val
        self.n += n

    def align(self):
        while self.n % 8:
            self.put(0, 1)

    def bytes(self):
        assert self.n % 8 == 0
        return self.v.to_bytes(self.n // 8, 'big')


def start(code):
    return b'\x00\x00\x01' + bytes([code])


RL1 = {}
RL2 = {}
for i in range(111):
    RL1[(RUN[i], LEVEL[i])] = (MPEG1_VLC[2 * i], MPEG1_VLC[2 * i + 1])
    RL2[(RUN[i], LEVEL[i])] = (MPEG2_VLC[2 * i], MPEG2_VLC[2 * i + 1])
ESC1 = (MPEG1_VLC[222], MPEG1_VLC[223])
EOB1 = (MPEG1_VLC[224], MPEG1_VLC[225])
ESC2 = (MPEG2_VLC[222], MPEG2_VLC[223])
EOB2 = (MPEG2_VLC[224], MPEG2_VLC[225])


class Gen:
    def __init__(self, c):
        self.c = c
        self.r = random.Random(c.get('seed', 1))
        self.w, self.h = c['w'], c['h']
        self.mpeg2 = c.get('mpeg2', True)
        self.chroma = c.get('chroma', 1)
        self.prog = c.get('prog', False) if self.mpeg2 else True
        self.nblk = 6 if self.chroma == 1 else 8
        self.full_pel_k = 1
        self.mbw = (self.w + 15) // 16
        if self.mpeg2 and not self.prog:
            self.mbh = (self.h + 31) // 32 * 2
        else:
            self.mbh = (self.h + 15) // 16

    # ---- low level ----
    def vlc(self, bw, ct):
        bw.put(ct[0], ct[1])

    def mv_comp(self, bw, target, pred, fcode):
        f = 1 << (fcode - 1)
        low, high, rng = -16 * f, 16 * f - 1, 32 * f
        assert low <= target <= high, (target, fcode)
        d = target - pred
        if d < low:
            d += rng
        elif d > high:
            d -= rng
        if d == 0:
            bw.put(MV[0], MV[1])
            return
        a = abs(d)
        shift = fcode - 1
        code = ((a - 1) >> shift) + 1
        self.vlc(bw, (MV[2 * code], MV[2 * code + 1]))
        bw.put(1 if d < 0 else 0, 1)
        if shift:
            bw.put((a - 1) & ((1 << shift) - 1), shift)

    def coef_block(self, bw, intra, ivlc, mpeg2):
        r = self.r
        table = RL2 if (intra and ivlc) else RL1
        esc = ESC2 if (intra and ivlc) else ESC1
        eob = EOB2 if (intra and ivlc) else EOB1
        n = r.choice([0, 1, 1, 2, 3, 5, 8]) if intra else r.choice([1, 1, 2, 3, 5, 9])
        pos = -1 if not intra else 0
        first = not intra
        left = 63
        coefs = []
        for _ in range(n):
            maxrun = left - (pos + 1)
            if maxrun < 0:
                break
            run = r.choice([0, 0, 0, 1, 1, 2, 3, 5, 9, 20]) if r.random() < 0.9 else r.randrange(0, 40)
            run = min(run, left - (pos + 1))
            if run < 0:
                break
            lvl = r.choice([1, 1, 1, 2, 2, 3, 4, 6, 9, 15, 30]) if r.random() < 0.93 else r.randrange(1, 300 if mpeg2 else 200)
            if mpeg2:
                lvl = min(lvl, 2047)
            else:
                lvl = min(lvl, 255)
            sign = r.random() < 0.5
            coefs.append((run, lvl, sign))
            pos += run + 1
        for k, (run, lvl, sign) in enumerate(coefs):
            if first and k == 0 and run == 0 and lvl == 1:
                bw.put(1, 1)
                bw.put(int(sign), 1)
                continue
            ct = table.get((run, lvl))
            if ct:
                bw.put(*ct)
                bw.put(int(sign), 1)
            else:
                bw.put(*esc)
                bw.put(run, 6)
                if mpeg2:
                    sv = -lvl if sign else lvl
                    bw.put(sv & 0xFFF, 12)
                else:
                    sv = -lvl if sign else lvl
                    if 1 <= lvl <= 127:
                        bw.put(sv & 0xFF, 8)
                    elif lvl <= 255:
                        if sign:
                            bw.put(0x80, 8)
                            bw.put(sv + 256, 8)
                        else:
                            bw.put(0, 8)
                            bw.put(lvl, 8)
        if not intra and not coefs:
            # a coded block needs a coefficient
            bw.put(1, 1)
            bw.put(0, 1)
        bw.put(*eob)

    def intra_block(self, bw, n, ivlc):
        r = self.r
        tab = (DC_LUM_CODE, DC_LUM_BITS) if n < 4 else (DC_CHROMA_CODE, DC_CHROMA_BITS)
        size = r.choice([0, 1, 2, 3, 4, 5, 6, 7])
        bw.put(tab[0][size], tab[1][size])
        if size:
            v = r.randrange(0, 1 << size)
            bw.put(v, size)
        self.coef_block(bw, True, ivlc, self.mpeg2)

    def inter_block(self, bw):
        self.coef_block(bw, False, False, self.mpeg2)

    # ---- motion ----
    def in_bounds(self, hv, x0, y0, bh, mx, my):
        """The luma block (16 wide) and its chroma at the vector, inside a view of height hv."""
        W = self.mbw * 16
        sx, sy, fx, fy = x0 + (mx >> 1), y0 + (my >> 1), mx & 1, my & 1
        if sx < 0 or sy < 0 or sx + 16 + fx > W or sy + bh + fy > hv:
            return False
        cm = int(mx / 2)
        csx = x0 // 2 + (cm >> 1)
        cfx = cm & 1
        if self.chroma == 1:
            cmy = int(my / 2)
            csy = y0 // 2 + (cmy >> 1)
            cfy = cmy & 1
            chv, cbh = hv // 2, bh // 2
        else:
            csy = y0 + (my >> 1)
            cfy = my & 1
            chv, cbh = hv, bh
        if csx < 0 or csy < 0 or csx + 8 + cfx > W // 2 or csy + cbh + cfy > chv:
            return False
        return True

    def pick_mv(self, hv, x0, y0, bh, fcode, extra=None):
        """A random vector inside the picture (and inside the range of fcode), or zero. extra checks derived vectors."""
        r = self.r
        f = 1 << (fcode - 1)
        lim = 16 * f
        for _ in range(60):
            span = r.choice([4, 10, 24, lim]) if lim > 4 else lim
            span = min(span, lim)
            mx = r.randrange(-span, span) if r.random() < 0.8 else 0
            my = r.randrange(-span, span) if r.random() < 0.8 else 0
            mx = max(-lim, min(lim - 1, mx))
            my = max(-lim, min(lim - 1, my))
            k = self.full_pel_k
            if self.in_bounds(hv, x0, y0, bh, mx * k, my * k) and (extra is None or extra(mx, my)):
                return mx, my
        return 0, 0

    # ---- macroblocks ----
    def motion_for(self, bw, st, flags, motion_type, col, row):
        """Writes the vectors of an inter macroblock; updates st['lm'] (the decoder's last_mv)."""
        c = self.c
        P = self.pic
        frame = P['structure'] == 3
        lm = st['lm']
        H = self.mbh * 16
        fc = P['fcode']
        for i in range(2):
            if not flags & (F_FWD << i):
                continue
            self.full_pel_k = 2 if P['full_pel'][i] else 1
            if frame and motion_type == 2:
                # one frame vector
                mx, my = self.pick_mv(H, 16 * col, 16 * row, 16, min(fc[i]))
                mx, my = self._clamp_two(mx, my, fc[i])
                self.mv_comp(bw, mx, lm[i][0][0], fc[i][0])
                self.mv_comp(bw, my, lm[i][0][1], fc[i][1])
                lm[i][0] = [mx, my]
                lm[i][1] = [mx, my]
            elif frame and motion_type == 1:
                for j in range(2):
                    fs = self.r.randrange(2)
                    bw.put(fs, 1)
                    mx, my = self.pick_mv(H // 2, 16 * col, 8 * row, 8, min(fc[i]))
                    mx, my = self._clamp_two(mx, my, fc[i])
                    self.mv_comp(bw, mx, lm[i][j][0], fc[i][0])
                    self.mv_comp(bw, my, lm[i][j][1] >> 1, fc[i][1])
                    lm[i][j] = [mx, 2 * my]
            elif frame and motion_type == 3:
                self.dmv(bw, st, i, col, row, True)
            elif not frame and motion_type == 1:
                fs = self.r.randrange(2)
                bw.put(fs, 1)
                mx, my = self.pick_mv(H // 2, 16 * col, 16 * row, 16, min(fc[i]))
                mx, my = self._clamp_two(mx, my, fc[i])
                self.mv_comp(bw, mx, lm[i][0][0], fc[i][0])
                self.mv_comp(bw, my, lm[i][0][1], fc[i][1])
                lm[i][0] = [mx, my]
                lm[i][1] = [mx, my]
            elif not frame and motion_type == 2:
                for j in range(2):
                    fs = self.r.randrange(2)
                    bw.put(fs, 1)
                    mx, my = self.pick_mv(H // 2, 16 * col, 16 * row + 8 * j, 8, min(fc[i]))
                    mx, my = self._clamp_two(mx, my, fc[i])
                    self.mv_comp(bw, mx, lm[i][j][0], fc[i][0])
                    self.mv_comp(bw, my, lm[i][j][1], fc[i][1])
                    lm[i][j] = [mx, my]
            elif not frame and motion_type == 3:
                self.dmv(bw, st, i, col, row, False)
            else:
                raise AssertionError((frame, motion_type))

    def _clamp_two(self, mx, my, fc):
        # the two components can have different f_codes; keep both in range
        for k, v in ((0, mx), (1, my)):
            lim = 16 * (1 << (fc[k] - 1))
            v = max(-lim, min(lim - 1, v))
            if k == 0:
                mx = v
            else:
                my = v
        return mx, my

    def dmv(self, bw, st, i, col, row, frame):
        r = self.r
        P = self.pic
        fc = P['fcode'][i]
        lm = st['lm']
        H = self.mbh * 16
        tff = P['tff']
        structure = P['structure']

        def derived(mx, my, dmx, dmy):
            gx, gy = int(mx > 0), int(my > 0)
            if frame:
                m = 1 if tff else 3
                a = (((mx * m + gx) >> 1) + dmx, ((my * m + gy) >> 1) + dmy - 1)
                m = 4 - m
                b = (((mx * m + gx) >> 1) + dmx, ((my * m + gy) >> 1) + dmy + 1)
                return a, b
            y = ((my + gy) >> 1) + dmy
            y = y - 1 if structure == 1 else y + 1
            return ((((mx + gx) >> 1) + dmx, y),)

        self.full_pel_k = 1
        for _ in range(100):
            dmx = r.choice([-1, 0, 1])
            dmy = r.choice([-1, 0, 1])
            if frame:
                mx, my = self.pick_mv(H // 2, 16 * col, 8 * row, 8, min(fc))
            else:
                mx, my = self.pick_mv(H // 2, 16 * col, 16 * row, 16, min(fc))
            mx, my = self._clamp_two(mx, my, fc)
            ok = True
            for v in derived(mx, my, dmx, dmy):
                if frame:
                    ok = ok and self.in_bounds(H // 2, 16 * col, 8 * row, 8, v[0], v[1])
                else:
                    ok = ok and self.in_bounds(H // 2, 16 * col, 16 * row, 16, v[0], v[1])
            if ok:
                break
        else:
            mx = my = dmx = dmy = 0
            if not self.in_bounds(H // 2, 16 * col, 8 * row if frame else 16 * row, 8 if frame else 16, *derived(0, 0, 0, 0)[0]):
                raise RuntimeError('no valid dual prime vector')
        self.mv_comp(bw, mx, lm[i][0][0], fc[0])
        lm[i][0][0] = lm[i][1][0] = mx
        self.dmv_code(bw, dmx)
        sh = 1 if frame else 0
        self.mv_comp(bw, my, lm[i][0][1] >> sh, fc[1])
        self.dmv_code(bw, dmy)
        lm[i][0][1] = lm[i][1][1] = my * (1 << sh)

    def dmv_code(self, bw, d):
        if d == 0:
            bw.put(0, 1)
        else:
            bw.put(1, 1)
            bw.put(1 if d < 0 else 0, 1)

    def skip_ok(self, st, col, row):
        """Whether a skipped macroblock here predicts from inside the picture."""
        P = self.pic
        if P['ptype'] == 2:
            return True
        if P['ptype'] == 1 or st['prev'] is None or st['prev_intra']:
            return False
        frame = P['structure'] == 3
        H = self.mbh * 16
        for d in range(2):
            if st['prev_dir'] & (1 << d):
                k = 2 if P['full_pel'][d] else 1
                mx, my = st['lm'][d][0]
                if frame:
                    if not self.in_bounds(H, 16 * col, 16 * row, 16, mx, my):
                        return False
                else:
                    if not self.in_bounds(H // 2, 16 * col, 16 * row, 16, mx, my):
                        return False
        return True

    def macroblock(self, bw, st, col, row, incr):
        r = self.r
        P = self.pic
        ptype = P['ptype']
        frame = P['structure'] == 3
        # address increment
        while incr > 33:
            bw.put(MBINCR[2 * 33], MBINCR[2 * 33 + 1])
            incr -= 33
        bw.put(MBINCR[2 * (incr - 1)], MBINCR[2 * (incr - 1) + 1])
        # type
        if ptype == 1:
            quant = r.random() < 0.3
            flags = F_INTRA | (F_QUANT if quant else 0)
            if quant:
                bw.put(0b01, 2)
            else:
                bw.put(1, 1)
        else:
            tab = PTYPE if ptype == 2 else BTYPE
            idx = r.randrange(len(tab))
            if r.random() < 0.15:
                idx = 0
            ct, flags = tab[idx]
            bw.put(*ct)
        intra = bool(flags & F_INTRA)
        fpfd = P['fpfd']
        mt = None
        if intra:
            if frame and not fpfd:
                bw.put(r.randrange(2), 1)
            if flags & F_QUANT:
                self.qscale(bw)
            if P['conceal']:
                if not frame:
                    bw.put(r.randrange(2), 1)
                lm = st['lm']
                for k in range(2):
                    fcd = P['fcode'][0][k]
                    lim = 16 * (1 << (fcd - 1))
                    t = r.randrange(-lim, lim)
                    self.mv_comp(bw, t, lm[0][0][k], fcd)
                    lm[0][0][k] = lm[0][1][k] = t
                bw.put(1, 1)
            else:
                st['lm'] = [[[0, 0], [0, 0]], [[0, 0], [0, 0]]]
            for n in range(self.nblk):
                self.intra_block(bw, n, P['ivlc'])
            st['prev'] = 'intra'
            st['prev_intra'] = True
            return
        # inter
        if flags & F_ZERO:
            if frame and not fpfd:
                bw.put(r.randrange(2), 1)
            if flags & F_QUANT:
                self.qscale(bw)
            st['lm'][0] = [[0, 0], [0, 0]]
            st['prev_dir'] = 1
        else:
            if frame and fpfd:
                motion_type = 2
            else:
                choices = [1, 2, 3] if ptype == 2 else [1, 2]
                if frame:
                    choices = [c for c in choices if c in (1, 2, 3)]
                if self.prog or not self.mpeg2:
                    choices = [2]
                if not self.c.get('dmv', True) or P.get('no_dmv'):
                    choices = [c for c in choices if c != 3] or [1]
                motion_type = r.choice(choices)
                bw.put(motion_type, 2)
                if frame and flags & F_CBP:
                    bw.put(r.randrange(2), 1)
            if flags & F_QUANT:
                self.qscale(bw)
            self.motion_for(bw, st, flags, motion_type, col, row)
            st['prev_dir'] = (flags >> 3) & 3
        if flags & F_CBP:
            if self.nblk == 6:
                cbp = r.randrange(1, 64)
                bw.put(MBPAT[2 * cbp], MBPAT[2 * cbp + 1])
            else:
                cbp = r.randrange(1, 256)
                hi = cbp >> 2
                bw.put(MBPAT[2 * hi], MBPAT[2 * hi + 1])
                bw.put(cbp & 3, 2)
            for n in range(self.nblk):
                if cbp >> (self.nblk - 1 - n) & 1:
                    self.inter_block(bw)
        st['prev'] = 'inter'
        st['prev_intra'] = False

    def qscale(self, bw):
        bw.put(self.r.randrange(1, 32), 5)

    # ---- pictures ----
    def slices(self, rows):
        """The slices of a picture: (row, first column, end column) triples; MPEG-1 may span rows."""
        r = self.r
        out = []
        mbw = self.mbw
        for row in range(rows):
            if r.random() < 0.3 and mbw >= 3:
                cut = r.randrange(1, mbw)
                out += [(row, 0, cut), (row, cut, mbw)]
            else:
                out.append((row, 0, mbw))
        return out

    def picture_data(self):
        P = self.pic
        field = P['structure'] != 3
        rows = self.mbh // 2 if field else self.mbh
        out = b''
        ptype = P['ptype']
        for (row, c0, c1) in self.slices(rows):
            bw = BW()
            bw.put(self.r.randrange(1, 32), 5)
            bw.put(0, 1)
            st = {'lm': [[[0, 0], [0, 0]], [[0, 0], [0, 0]]], 'prev': None, 'prev_intra': False, 'prev_dir': 0}
            run = 0
            first = True
            col = c0
            # decide skips first: a skipped macroblock is never the first or last of a slice
            while col < c1:
                can_skip = (not first) and col < c1 - 1 and ptype != 1
                skip = False
                if can_skip and self.r.random() < 0.25 and self.skip_ok(st, col, row):
                    skip = True
                if skip:
                    run += 1
                    if ptype == 2:
                        st['lm'][0] = [[0, 0], [0, 0]]
                    col += 1
                    continue
                if first:
                    incr = c0 + 1
                else:
                    incr = run + 1
                self.macroblock(bw, st, col, row, incr)
                run = 0
                first = False
                col += 1
            bw.align()
            out += start(row + 1) + bw.bytes()
        return out

    def seq_header(self):
        c = self.c
        bw = BW()
        bw.put(self.w, 12)
        bw.put(self.h, 12)
        bw.put(1, 4)
        bw.put(3, 4)
        bw.put(0x3FFFF, 18)
        bw.put(1, 1)
        bw.put(112, 10)
        bw.put(0, 1)
        for key in ('intra_matrix', 'inter_matrix'):
            m = c.get(key)
            if m:
                bw.put(1, 1)
                for i in range(64):
                    bw.put(m[i], 8)
            else:
                bw.put(0, 1)
        bw.align()
        out = start(0xB3) + bw.bytes()
        if self.mpeg2:
            bw = BW()
            bw.put(1, 4)
            bw.put(0x48 if self.chroma == 2 else 0x48, 8)
            bw.put(int(self.prog), 1)
            bw.put(self.chroma, 2)
            bw.put(0, 2)
            bw.put(0, 2)
            bw.put(0, 12)
            bw.put(1, 1)
            bw.put(0, 8)
            bw.put(int(c.get('low_delay', False)), 1)
            bw.put(0, 2)
            bw.put(0, 5)
            bw.align()
            out += start(0xB5) + bw.bytes()
            # a quantiser matrix extension
            if c.get('qm_ext'):
                bw = BW()
                bw.put(3, 4)
                m = c['qm_ext']
                for key in ('intra', 'inter', 'chroma_intra', 'chroma_inter'):
                    if key in m:
                        bw.put(1, 1)
                        for i in range(64):
                            bw.put(m[key][i], 8)
                    else:
                        bw.put(0, 1)
                bw.align()
                out += start(0xB5) + bw.bytes()
        return out

    def gop_header(self, closed):
        bw = BW()
        bw.put(0, 25)
        bw.put(int(closed), 1)
        bw.put(0, 1)
        bw.align()
        return start(0xB8) + bw.bytes()

    def picture(self, ptype, structure, tr):
        c = self.c
        r = self.r
        P = {'ptype': ptype, 'structure': structure, 'tr': tr}
        P['tff'] = c.get('tff', True)
        P['fpfd'] = True if (not self.mpeg2 or self.prog) else (r.random() < 0.3 if c.get('fpfd') is None else c['fpfd'])
        P['conceal'] = c.get('conceal', False) and self.mpeg2
        P['ivlc'] = c.get('ivlc', False)
        P['qtype'] = c.get('qtype', False)
        P['alt'] = c.get('alt', False)
        P['dcp'] = c.get('dcp', 0)
        P['full_pel'] = [False, False]
        if not self.mpeg2 and c.get('full_pel'):
            P['full_pel'] = [r.random() < 0.5, r.random() < 0.5]
        fmax = c.get('fmax', 3)
        f0 = [r.randrange(1, fmax + 1), r.randrange(1, fmax + 1)] if self.mpeg2 else [None, None]
        fb = [r.randrange(1, fmax + 1), r.randrange(1, fmax + 1)] if self.mpeg2 else [None, None]
        if not self.mpeg2:
            a = r.randrange(1, fmax + 1)
            b = r.randrange(1, fmax + 1)
            f0, fb = [a, a], [b, b]
        P['fcode'] = [f0, fb]
        self.pic = P
        bw = BW()
        bw.put(tr, 10)
        bw.put(ptype, 3)
        bw.put(0xFFFF, 16)
        if ptype in (2, 3):
            bw.put(int(P['full_pel'][0]), 1)
            bw.put(P['fcode'][0][0] if not self.mpeg2 else 7, 3)
        if ptype == 3:
            bw.put(int(P['full_pel'][1]), 1)
            bw.put(P['fcode'][1][0] if not self.mpeg2 else 7, 3)
        bw.put(0, 1)
        bw.align()
        out = start(0) + bw.bytes()
        if self.mpeg2:
            bw = BW()
            bw.put(8, 4)
            for s in range(2):
                for t in range(2):
                    used = (s == 0 and (ptype in (2, 3) or P['conceal'])) or (s == 1 and ptype == 3)
                    bw.put(P['fcode'][s][t] if used else 15, 4)
            bw.put(P['dcp'], 2)
            bw.put(structure, 2)
            bw.put(int(P['tff']), 1)
            bw.put(int(P['fpfd']), 1)
            bw.put(int(P['conceal']), 1)
            bw.put(int(P['qtype']), 1)
            bw.put(int(P['ivlc']), 1)
            bw.put(int(P['alt']), 1)
            bw.put(0, 1)
            bw.put(0, 1)
            bw.put(int(self.prog or (structure == 3 and r.random() < 0.3 and False)), 1)
            bw.put(0, 1)
            bw.align()
            out += start(0xB5) + bw.bytes()
        # q_scale_type and the other flags travel in the extension: the slice writer maps the code
        return out + self.picture_data_mapped()

    def picture_data_mapped(self):
        return self.picture_data()


def make_stream(c):
    g = Gen(c)
    r = g.r
    out = g.seq_header()
    n_gops = c.get('gops', 2)
    mode = c.get('fields', 'frame')  # frame | field | mixed
    for gi in range(n_gops):
        out += g.gop_header(closed=(gi == 0))
        # I (tr 2) P (tr 5) B B  P B B ... in decoding order within the GOP; the B pictures before the first P come after it
        base = 0
        order = [(1, 2), (2, 5), (3, 3), (3, 4), (2, 8), (3, 6), (3, 7)] if c.get('bframes', True) else [(1, 0), (2, 1), (2, 2), (2, 3)]
        if gi == 0 and c.get('bframes', True):
            order = [(1, 0), (2, 3), (3, 1), (3, 2), (2, 6), (3, 4), (3, 5)]
        if gi > 0 and c.get('bframes', True):
            order = [(1, 2), (3, 0), (3, 1), (2, 5), (3, 3), (3, 4)]
        for ptype, tr in order:
            if mode == 'frame' or not g.mpeg2 or g.prog:
                structs = [3]
            elif mode == 'field':
                structs = [1, 2] if g.c.get('tff', True) else [2, 1]
            else:
                structs = [3] if r.random() < 0.5 else ([1, 2] if g.c.get('tff', True) else [2, 1])
            for k, s in enumerate(structs):
                pt = ptype
                if ptype == 1 and k == 1 and r.random() < 0.6:
                    pt = 2
                out += g.picture(pt, s, tr)
    out += start(0xB7)
    return out


def matrix(seed, lo=8, hi=60):
    rr = random.Random(seed)
    m = [rr.randrange(lo, hi) for _ in range(64)]
    m[0] = 8
    return m


# name: configuration. Sizes are in luma samples; 4:2:0 and 4:2:2, MPEG-1 and MPEG-2, frame and field pictures.
CONFIGS = {
    'm1': dict(w=64, h=48, mpeg2=False, seed=11, full_pel=True),
    'm1_crop': dict(w=50, h=38, mpeg2=False, seed=12),
    'p_frame': dict(w=64, h=48, prog=True, seed=21),
    'p_frame_ivlc': dict(w=64, h=48, prog=True, seed=22, ivlc=True, qtype=True, dcp=2, alt=False),
    'i_frame_il': dict(w=64, h=64, seed=31, fields='frame', fpfd=False),
    'i_field': dict(w=64, h=64, seed=32, fields='field'),
    'i_field_bff': dict(w=64, h=64, seed=33, fields='field', tff=False),
    'i_mixed_alt': dict(w=64, h=64, seed=34, fields='mixed', alt=True, ivlc=True, dcp=3),
    'i_mixed_conceal': dict(w=80, h=64, seed=35, fields='mixed', conceal=True, qtype=True, dcp=1, fmax=5),
    'i_field_422': dict(w=64, h=64, seed=36, fields='field', chroma=2),
    'i_mixed_422': dict(w=64, h=64, seed=37, fields='mixed', chroma=2, alt=True, ivlc=True),
    'i_frame_il_422': dict(w=64, h=64, seed=38, fields='frame', chroma=2, fpfd=False),
    'i_mixed_matrix': dict(w=72, h=56, seed=39, fields='mixed', intra_matrix=matrix(1), inter_matrix=matrix(2, 4, 40),
                           qm_ext=dict(chroma_intra=matrix(3), chroma_inter=matrix(4, 4, 40)), chroma=2),
}


def main(outdir):
    import subprocess, os
    os.makedirs(outdir, exist_ok=True)
    for name, c in CONFIGS.items():
        data = make_stream(c)
        base = os.path.join(outdir, 'mpeg2_' + name)
        open(base + '.m2v', 'wb').write(data)
        pix = 'yuv422p' if c.get('chroma', 1) == 2 else 'yuv420p'
        subprocess.run(['ffmpeg', '-v', 'warning', '-y', '-cpuflags', '0', '-flags', '+bitexact', '-idct', 'simple', '-threads', '1',
                        '-i', base + '.m2v', '-fps_mode', 'passthrough', '-f', 'rawvideo', '-pix_fmt', pix,
                        base + '.ffmpeg.yuv'], check=True)
        print(name, len(data), 'bytes ->', os.path.getsize(base + '.ffmpeg.yuv'), 'bytes decoded')


if __name__ == '__main__':
    main(sys.argv[1] if len(sys.argv) > 1 else '.')
