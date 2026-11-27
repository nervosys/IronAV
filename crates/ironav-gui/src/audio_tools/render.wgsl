struct DrawParams {
    info: vec4<u32>, // plot, newest history row, history length, stereo
    dimensions: vec4<f32>, // inverse pixel height, projection depth, sRGB target, inverse width
    background: vec4<f32>,
    traces: vec4<u32>, // palette indices for the two waveform channels
};
@group(0) @binding(0) var<uniform> params: DrawParams;
@group(0) @binding(1) var<storage, read> plots: array<f32>;
@group(0) @binding(2) var<storage, read> palette: array<u32>;

const WATERFALL: u32 = 0u;
const WAVEFORM: u32 = 2u;
const SPECTRUM: u32 = 3u;

fn finish(rgb: vec3<f32>) -> vec4<f32> {
    if params.dimensions.z > 0.5 {
        let linear = select(pow((rgb + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4)), rgb / 12.92, rgb <= vec3<f32>(0.04045));
        return vec4<f32>(linear, 1.0);
    }
    return vec4<f32>(rgb, 1.0);
}

fn color_at(index: u32) -> vec3<f32> {
    return unpack4x8unorm(palette[index]).rgb;
}

fn level_color(level: f32) -> vec3<f32> {
    return color_at(u32(round(clamp(level, 0.0, 1.0) * 255.0)));
}

struct Quad {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

fn clip_position(uv: vec2<f32>) -> vec4<f32> {
    return vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
}

@vertex
fn quad(@builtin(vertex_index) index: u32) -> Quad {
    let uv = vec2<f32>(f32(index & 1u), f32((index >> 1u) & 1u));
    return Quad(clip_position(uv), uv);
}

fn history_color(age: u32, bin: u32) -> vec3<f32> {
    if age >= params.info.z {
        return params.background.rgb;
    }
    let row = (params.info.y + HISTORY - age) % HISTORY;
    return level_color(plots[row * BANDS + bin]);
}

fn waterfall_color(uv: vec2<f32>) -> vec3<f32> {
    let cell = max(vec2<f32>(0.0), uv * vec2<f32>(f32(BANDS), f32(HISTORY)) - 0.5);
    let bin = min(u32(cell.x), BANDS - 1u);
    let next = min(bin + 1u, BANDS - 1u);
    let age = min(u32(cell.y), HISTORY - 1u);
    let a = mix(history_color(age, bin), history_color(age, next), fract(cell.x));
    let b = mix(history_color(age + 1u, bin), history_color(age + 1u, next), fract(cell.x));
    return mix(a, b, fract(cell.y));
}

fn wave_bounds(uv: vec2<f32>) -> vec2<f32> {
    let lanes = select(1.0, 2.0, params.info.w != 0u);
    let channel = min(u32(uv.y * lanes), u32(lanes - 1.0));
    let column = uv.x * f32(WAVE_BINS - 1u);
    let first = u32(column);
    let second = min(first + 1u, WAVE_BINS - 1u);
    let offset = WAVE_OFFSET + channel * 2u;
    let low = mix(plots[offset + first * 4u], plots[offset + second * 4u], fract(column));
    let high = mix(plots[offset + first * 4u + 1u], plots[offset + second * 4u + 1u], fract(column));
    let center = (f32(channel) + 0.5) / lanes;
    return vec2<f32>(center - clamp(high, -1.0, 1.0) * 0.46 / lanes, center - clamp(low, -1.0, 1.0) * 0.46 / lanes);
}

@fragment
fn draw(plot: Quad) -> @location(0) vec4<f32> {
    let uv = clamp(plot.uv, vec2<f32>(0.0), vec2<f32>(1.0));
    if params.info.x == WATERFALL {
        return finish(waterfall_color(uv));
    }
    if params.info.x == SPECTRUM {
        let column = uv.x * f32(BANDS);
        let index = min(u32(column), BANDS - 1u);
        let level = plots[params.info.y * BANDS + index];
        let gap = min(0.22, params.dimensions.w * f32(BANDS));
        if fract(column) > 1.0 - gap || uv.y < 1.0 - level || level <= 0.0 {
            discard;
        }
        return finish(level_color(level));
    }
    if params.info.x == WAVEFORM {
        let bounds = wave_bounds(uv);
        let thickness = params.dimensions.x * 0.5;
        if uv.y < bounds.x - thickness || uv.y > bounds.y + thickness {
            discard;
        }
        return finish(color_at(select(params.traces.x, params.traces.y, params.info.w != 0u && uv.y >= 0.5)));
    }
    let column = uv.x * f32(EQ_POINTS - 1u);
    let index = min(u32(column), EQ_POINTS - 1u);
    let next = min(index + 1u, EQ_POINTS - 1u);
    let a = plots[EQ_OFFSET + index];
    let b = plots[EQ_OFFSET + next];
    let y = 0.5 - mix(a, b, fract(column)) / 48.0;
    if a > 24.0 || b > 24.0 || abs(uv.y - y) > params.dimensions.x {
        discard;
    }
    return finish(color_at(params.traces.x));
}

struct DecayVertex {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec3<f32>,
};

@vertex
fn decay_quad(@builtin(vertex_index) index: u32, @builtin(instance_index) slice: u32) -> DecayVertex {
    let band = index / 2u;
    let baseline = (index & 1u) != 0u;
    let level = plots[DECAY_OFFSET + slice * BANDS + band];
    let depth = f32(slice) / f32(DECAY_SLICES - 1u);
    let x = f32(band) / f32(BANDS - 1u) * (1.0 - params.dimensions.y) + depth * params.dimensions.y;
    let base = 0.55 + depth * 0.45;
    let y = select(base - level * 0.55 - params.dimensions.x * 0.5, base, baseline);
    let color = select(level_color(level), params.background.rgb, baseline);
    return DecayVertex(clip_position(vec2<f32>(x, y)), color);
}

@fragment
fn draw_decay(vertex: DecayVertex) -> @location(0) vec4<f32> {
    return finish(vertex.color);
}
