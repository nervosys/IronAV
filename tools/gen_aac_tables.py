# usage: python tools/gen_aac_tables.py <ffmpeg source dir> <output tables.rs>
import re, os, sys
src = open(os.path.join(sys.argv[1], 'libavcodec', 'aactab.c'), encoding='utf-8').read()

def arr(name):
    m = re.search(r'\b' + re.escape(name) + r'\s*\[[^\]]*\]\s*=\s*\{(.*?)\};', src, re.S)
    assert m, name
    body = re.sub(r'/\*.*?\*/', '', m.group(1), flags=re.S)
    body = re.sub(r'//[^\n]*', '', body)
    toks = [t.strip() for t in body.replace('\n', ' ').split(',') if t.strip()]
    return toks

def ints(name):
    return [str(int(t, 0)) for t in arr(name)]

def floats(name):
    return [t.rstrip('f') for t in arr(name)]

def mapping(name):
    m = re.search(r'\b' + re.escape(name) + r'\s*\[[^\]]*\]\s*=\s*\{(.*?)\};', src, re.S)
    body = re.sub(r'/\*.*?\*/', '', m.group(1), flags=re.S)
    return [t.strip() for t in body.replace('\n', ' ').split(',') if t.strip()]

out = []
out.append('''//! Tables of the AAC decoder.
//!
//! The Huffman codebooks, scalefactor bands and TNS coefficient maps are generated from
//! `libavcodec/aactab.c` of FFmpeg (Copyright (c) 2005-2006 Oded Shimon, 2006-2007 Maxim Gavrilov;
//! LGPL-2.1-or-later), which transcribes the tables of ISO/IEC 14496-3.
//! Regenerate with `tools/gen_aac_tables.py`; do not edit by hand.
''')
def emit(name, ty, vals, per=16):
    lines = []
    for i in range(0, len(vals), per):
        lines.append('    ' + ', '.join(vals[i:i + per]) + ',')
    out.append(f'pub const {name}: [{ty}; {len(vals)}] = [\n' + '\n'.join(lines) + '\n];\n')

for i in range(1, 12):
    emit(f'CODES{i}', 'u16', ints(f'codes{i}'))
    emit(f'BITS{i}', 'u8', ints(f'bits{i}'))
out.append('pub const SPECTRAL_CODES: [&[u16]; 11] = [' + ', '.join(f'&CODES{i}' for i in range(1, 12)) + '];\n')
out.append('pub const SPECTRAL_BITS: [&[u8]; 11] = [' + ', '.join(f'&BITS{i}' for i in range(1, 12)) + '];\n')
emit('SCALEFACTOR_CODE', 'u32', ints('ff_aac_scalefactor_code'), 8)
emit('SCALEFACTOR_BITS', 'u8', ints('ff_aac_scalefactor_bits'))

used = set()
for kind in ('1024', '128'):
    m = mapping(f'ff_swb_offset_{kind}')
    names = [t for t in m if t.startswith('swb_offset')]
    assert len(names) == 13, (kind, len(names))
    for n in dict.fromkeys(names):
        emit(n.upper(), 'u16', ints(n), 14)
    out.append(f'pub const SWB_OFFSET_{kind}: [&[u16]; 13] = [' + ', '.join('&' + n.upper() for n in names) + '];\n')
emit('TNS_MAX_BANDS_1024', 'u8', ints('ff_tns_max_bands_1024'))
emit('TNS_MAX_BANDS_128', 'u8', ints('ff_tns_max_bands_128'))
for n in ('tns_tmp2_map_0_3', 'tns_tmp2_map_0_4', 'tns_tmp2_map_1_3', 'tns_tmp2_map_1_4'):
    emit(n.upper(), 'f32', floats(n), 4)
m = mapping('ff_tns_tmp2_map')
out.append('/// Indexed by `2 * coef_compress + coef_res`.\npub const TNS_MAP: [&[f32]; 4] = [' + ', '.join('&' + t.upper() for t in m) + '];\n')
dst = sys.argv[2]
os.makedirs(os.path.dirname(dst), exist_ok=True)
open(dst, 'w', encoding='utf-8', newline='\n').write('\n'.join(out))
print(len('\n'.join(out)), 'bytes')
