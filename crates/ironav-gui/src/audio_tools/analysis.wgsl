struct AnalysisParams {
    audio: vec4<u32>, // sample count, hop, new spectrum rows, first output row
    device: vec4<u32>, // output sample rate, samples after the newest spectrum row
    filters: array<vec4<f32>, 10>,
};
@group(0) @binding(0) var<uniform> params: AnalysisParams;
@group(0) @binding(1) var<storage, read> pcm: array<vec2<f32>>;
@group(0) @binding(2) var<storage, read_write> fft: array<vec2<f32>>;
@group(0) @binding(3) var<storage, read_write> plots: array<f32>;
@group(0) @binding(4) var<storage, read> rotations: array<vec2<f32>>;

const TAU: f32 = 6.28318530718;
var<workgroup> tile: array<vec2<f32>, FFT_SIZE>;

fn rotate(value: vec2<f32>, width: u32, index: u32) -> vec2<f32> {
    let rotation = rotations[index * DECAY_SIZE / width];
    return vec2<f32>(
        value.x * rotation.x - value.y * rotation.y,
        value.x * rotation.y + value.y * rotation.x,
    );
}

fn taper(distance: u32, rise: u32) -> f32 {
    return 0.5 - 0.5 * cos(TAU * 0.5 * f32(min(distance, rise)) / f32(rise));
}

fn transform_tile(chunk: u32, row: u32, lane: u32, decay: bool) {
    let size = select(FFT_SIZE, DECAY_SIZE, decay);
    let captured = select(FFT_SIZE, min(params.audio.x, DECAY_SIZE), decay);
    let base = chunk * FFT_SIZE;
    var source = params.audio.x - captured;
    let step = captured / (DECAY_SLICES + 2u);
    var cut = 0u;
    if decay {
        cut = row * step;
    } else {
        source -= params.device.y + (params.audio.z - 1u - row) * params.audio.y;
    }
    let rise = clamp(params.device.x / 500u, 8u, step / 2u);
    let shift = 32u - firstLeadingBit(size);
    for (var i = lane; i < FFT_SIZE; i += FFT_THREADS) {
        let index = reverseBits(base + i) >> shift;
        if decay {
            tile[i] = vec2<f32>(0.0);
            if index >= cut && index < captured {
                let edge = min(index - cut, captured - 1u - index);
                tile[i] = pcm[source + index] * taper(edge, rise);
            }
        } else {
            let window = 0.5 - 0.5 * cos(TAU * f32(index) / f32(size));
            tile[i] = pcm[source + index] * window;
        }
    }
    workgroupBarrier();

    // FFT_SIZE complex values fit in 16 KiB of shared memory. A decay transform
    // joins four of these tiles in two subsequent dispatches.
    for (var width = 2u; width <= FFT_SIZE; width *= 2u) {
        let half = width / 2u;
        for (var i = lane; i < FFT_SIZE / 2u; i += FFT_THREADS) {
            let j = i % half;
            let index = i / half * width + j;
            let a = tile[index];
            let b = rotate(tile[index + half], width, j);
            tile[index] = a + b;
            tile[index + half] = a - b;
        }
        workgroupBarrier();
    }
    for (var i = lane; i < FFT_SIZE; i += FFT_THREADS) {
        fft[row * size + base + i] = tile[i];
    }
}

@compute @workgroup_size(FFT_THREADS)
fn normal_fft(@builtin(workgroup_id) group: vec3<u32>, @builtin(local_invocation_index) lane: u32) {
    transform_tile(0u, group.x, lane, false);
}

@compute @workgroup_size(FFT_THREADS)
fn decay_fft(@builtin(workgroup_id) group: vec3<u32>, @builtin(local_invocation_index) lane: u32) {
    transform_tile(group.x, group.y, lane, true);
}

fn merge_fft(id: vec3<u32>, width: u32) {
    let half = width / 2u;
    let j = id.x % half;
    let index = id.y * DECAY_SIZE + id.x / half * width + j;
    let a = fft[index];
    let b = rotate(fft[index + half], width, j);
    fft[index] = a + b;
    fft[index + half] = a - b;
}

@compute @workgroup_size(FFT_THREADS)
fn merge_tiles(@builtin(global_invocation_id) id: vec3<u32>) {
    merge_fft(id, FFT_SIZE * 2u);
}

@compute @workgroup_size(FFT_THREADS)
fn merge_decay(@builtin(global_invocation_id) id: vec3<u32>) {
    merge_fft(id, DECAY_SIZE);
}

fn frequency(fraction: f32) -> f32 {
    return MIN_HZ * pow(MAX_HZ / MIN_HZ, fraction);
}

fn band_level(row: u32, band: u32, decay: bool) -> f32 {
    let size = select(FFT_SIZE, DECAY_SIZE, decay);
    let scale = f32(size) / f32(params.device.x);
    let lo = frequency(f32(band) / f32(BANDS)) * scale;
    let hi = frequency(f32(band + 1u) / f32(BANDS)) * scale;
    let start = clamp(u32(round(lo)), 1u, size / 2u);
    let end = min(max(u32(round(hi)), start + 1u), size / 2u);
    let offset = row * size;
    var power = 0.0;
    for (var bin = start; bin < end; bin += 1u) {
        let positive = fft[offset + bin];
        let negative = fft[offset + size - bin];
        power = max(power, dot(positive, positive) + dot(negative, negative));
    }
    let norm = select(2.0 / f32(FFT_SIZE), 1.0 / f32(min(params.audio.x, DECAY_SIZE)), decay);
    let db = 4.342944819 * log(max(1e-9, power * norm * norm));
    return round(clamp((db + 90.0) / 90.0, 0.0, 1.0) * 255.0) / 255.0;
}

@compute @workgroup_size(64)
fn normal_bands(@builtin(workgroup_id) group: vec3<u32>, @builtin(local_invocation_index) band: u32) {
    let row = (params.audio.w + group.x) % HISTORY;
    plots[row * BANDS + band] = band_level(group.x, band, false);
}

@compute @workgroup_size(64)
fn decay_bands(@builtin(workgroup_id) group: vec3<u32>, @builtin(local_invocation_index) band: u32) {
    plots[DECAY_OFFSET + group.x * BANDS + band] = band_level(group.x, band, true);
}

@compute @workgroup_size(64)
fn waveform(@builtin(global_invocation_id) id: vec3<u32>) {
    let width = FFT_SIZE / WAVE_BINS;
    let start = params.audio.x - FFT_SIZE + id.x * width;
    var low = pcm[start];
    var high = low;
    for (var i = 1u; i < width; i += 1u) {
        low = min(low, pcm[start + i]);
        high = max(high, pcm[start + i]);
    }
    let offset = WAVE_OFFSET + id.x * 4u;
    plots[offset] = low.x;
    plots[offset + 1u] = high.x;
    plots[offset + 2u] = low.y;
    plots[offset + 3u] = high.y;
}

fn magnitude(c: vec3<f32>, cosine: vec3<f32>, sine: vec2<f32>) -> f32 {
    let real = dot(c, cosine);
    let imaginary = dot(c.yz, sine);
    return real * real + imaginary * imaginary;
}

@compute @workgroup_size(64)
fn equalizer(@builtin(global_invocation_id) id: vec3<u32>) {
    let hz = frequency(f32(id.x) / f32(EQ_POINTS - 1u));
    let rate = f32(params.device.x);
    let offset = EQ_OFFSET + id.x;
    if hz > rate * 0.5 {
        plots[offset] = 1000.0;
        return;
    }
    let angle = TAU * hz / rate;
    let cos1 = cos(angle);
    let sin1 = sin(angle);
    let cosine = vec3<f32>(1.0, cos1, cos1 * cos1 - sin1 * sin1);
    let sine = vec2<f32>(sin1, 2.0 * sin1 * cos1);
    var db = 0.0;
    for (var band = 0u; band < 5u; band += 1u) {
        let numerator = magnitude(params.filters[band * 2u].xyz, cosine, sine);
        let denominator = magnitude(params.filters[band * 2u + 1u].xyz, cosine, sine);
        db += 4.342944819 * log(max(1e-20, numerator) / max(1e-20, denominator));
    }
    plots[offset] = clamp(db, -24.0, 24.0);
}
