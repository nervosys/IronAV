//! Showing decoded YUV pictures with the GPU.
//!
//! The decoder's planes are uploaded as they are (one `R8Unorm` texture per plane, or `R16Uint`
//! for 10/12-bit pictures) and a fragment shader converts them to RGB while it scales the picture
//! to the window with bilinear filtering. The CPU does no colour conversion, no scaling and no
//! RGB to RGBA pass, and the textures live as long as the picture size and format stay the same:
//! each new frame is a `queue.write_texture` into the existing textures.
//!
//! This is an `egui_wgpu` paint callback. eframe 0.37 renders with wgpu by default (the `wgpu`
//! feature is in its default set, and the workspace does not turn it off), so this needs no extra
//! dependency and no second GL context next to egui's own. If the window has no wgpu render state
//! (another backend) or the picture is a format the shader does not cover (RGB24), the player
//! falls back to the CPU converter, which is still what snapshots use.
//!
//! The colour maths is [`ironav_player::YuvToRgb`]: the shader evaluates `rgb = m * (yuv - offset)`
//! with the matrix and offsets passed in a uniform, and a unit test checks the same function
//! against the CPU converter. A second test renders a picture offscreen on a real adapter, when
//! the machine has one, and compares it with the CPU result.

use eframe::egui;
use eframe::egui_wgpu::{self, wgpu, CallbackResources, CallbackTrait, ScreenDescriptor};
use ironav_core::{PixelFormat, VideoFrame};
use ironav_player::{Colorimetry, TimedFrame, YuvToRgb};
use std::sync::Arc;

/// Can the shader show this picture? Everything the player decodes except packed RGB.
pub fn supported(v: &VideoFrame) -> bool {
    let planes = if v.pix_fmt == PixelFormat::Gray8 { 1 } else { 3 };
    v.pix_fmt != PixelFormat::Rgb24 && v.width > 0 && v.height > 0 && v.planes.len() == planes
}

const COMMON: &str = r"
struct Params {
    m0: vec4<f32>,
    m1: vec4<f32>,
    m2: vec4<f32>,
    off: vec4<f32>,
    info: vec4<f32>, // x: 1 / (2^bits - 1) for integer textures, y: 1 when the target is sRGB
};
@group(0) @binding(4) var<uniform> p: Params;

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VOut {
    let x = f32(i & 1u);
    let y = f32((i >> 1u) & 1u);
    var o: VOut;
    o.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    o.uv = vec2<f32>(x, y);
    return o;
}

fn finish(yuv: vec3<f32>) -> vec4<f32> {
    let d = yuv - p.off.xyz;
    var rgb = clamp(vec3<f32>(dot(p.m0.xyz, d), dot(p.m1.xyz, d), dot(p.m2.xyz, d)), vec3<f32>(0.0), vec3<f32>(1.0));
    if (p.info.y > 0.5) {
        rgb = select(pow((rgb + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4)), rgb / 12.92, rgb <= vec3<f32>(0.04045));
    }
    return vec4<f32>(rgb, 1.0);
}
";

/// 8-bit planes: the sampler's bilinear filter does the scaling.
const EIGHT_BIT: &str = r"
@group(0) @binding(0) var ty: texture_2d<f32>;
@group(0) @binding(1) var tu: texture_2d<f32>;
@group(0) @binding(2) var tv: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;

@fragment
fn fs_main(v: VOut) -> @location(0) vec4<f32> {
    return finish(vec3<f32>(textureSample(ty, samp, v.uv).r, textureSample(tu, samp, v.uv).r, textureSample(tv, samp, v.uv).r));
}
";

/// 10/12-bit planes are 16-bit integers, which cannot be filtered by the hardware: do it by hand.
const SIXTEEN_BIT: &str = r"
@group(0) @binding(0) var ty: texture_2d<u32>;
@group(0) @binding(1) var tu: texture_2d<u32>;
@group(0) @binding(2) var tv: texture_2d<u32>;

fn bilinear(t: texture_2d<u32>, uv: vec2<f32>) -> f32 {
    let dim = vec2<f32>(textureDimensions(t));
    let q = uv * dim - vec2<f32>(0.5);
    let f = fract(q);
    let i = vec2<i32>(floor(q));
    let hi = vec2<i32>(dim) - vec2<i32>(1);
    let lo = vec2<i32>(0);
    let a = f32(textureLoad(t, clamp(i, lo, hi), 0).r);
    let b = f32(textureLoad(t, clamp(i + vec2<i32>(1, 0), lo, hi), 0).r);
    let c = f32(textureLoad(t, clamp(i + vec2<i32>(0, 1), lo, hi), 0).r);
    let d = f32(textureLoad(t, clamp(i + vec2<i32>(1, 1), lo, hi), 0).r);
    return mix(mix(a, b, f.x), mix(c, d, f.x), f.y);
}

@fragment
fn fs_main(v: VOut) -> @location(0) vec4<f32> {
    return finish(vec3<f32>(bilinear(ty, v.uv), bilinear(tu, v.uv), bilinear(tv, v.uv)) * p.info.x);
}
";

/// The uniform block: three matrix rows, the offsets, and two flags, as `vec4<f32>`s.
fn params_bytes(c: Colorimetry, srgb_target: bool) -> Vec<u8> {
    let k = YuvToRgb::new(c);
    let top = ((1u32 << c.bits) - 1) as f32;
    let words: [f32; 20] = [
        k.m[0][0],
        k.m[0][1],
        k.m[0][2],
        0.0,
        k.m[1][0],
        k.m[1][1],
        k.m[1][2],
        0.0,
        k.m[2][0],
        k.m[2][1],
        k.m[2][2],
        0.0,
        k.offset[0],
        k.offset[1],
        k.offset[2],
        0.0,
        1.0 / top,
        if srgb_target { 1.0 } else { 0.0 },
        0.0,
        0.0,
    ];
    words.iter().flat_map(|w| w.to_ne_bytes()).collect()
}

struct Variant {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
}

/// What the textures were made for. A change means new textures; the same key means update in place.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Key {
    w: u32,
    h: u32,
    fmt: PixelFormat,
}

struct Textures {
    key: Key,
    high: bool,
    planes: [wgpu::Texture; 3],
    bind: wgpu::BindGroup,
}

/// The GPU side of the video view; lives in egui's callback resources for the life of the window.
pub struct GpuVideo {
    variants: [Variant; 2],
    sampler: wgpu::Sampler,
    params: wgpu::Buffer,
    srgb_target: bool,
    textures: Option<Textures>,
    /// The picture the textures hold. Keeping the `Arc` also stops its address being reused.
    shown: Option<Arc<TimedFrame>>,
    color: Option<Colorimetry>,
}

impl GpuVideo {
    pub fn new(device: &wgpu::Device, target: wgpu::TextureFormat) -> Self {
        let make = |label: &str, tail: &str, sampled: bool| {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(label), source: wgpu::ShaderSource::Wgsl(format!("{COMMON}{tail}").into()) });
            let tex = |binding: u32| wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    multisampled: false,
                    sample_type: if sampled { wgpu::TextureSampleType::Float { filterable: true } } else { wgpu::TextureSampleType::Uint },
                    view_dimension: wgpu::TextureViewDimension::D2,
                },
                count: None,
            };
            let mut entries = vec![tex(0), tex(1), tex(2)];
            if sampled {
                entries.push(wgpu::BindGroupLayoutEntry { binding: 3, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None });
            }
            entries.push(wgpu::BindGroupLayoutEntry {
                binding: 4,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: wgpu::BufferSize::new(80) },
                count: None,
            });
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some(label), entries: &entries });
            let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some(label), bind_group_layouts: &[Some(&layout)], immediate_size: 0 });
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&pl),
                vertex: wgpu::VertexState { module: &module, entry_point: Some("vs_main"), compilation_options: Default::default(), buffers: &[] },
                primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState { format: target, blend: None, write_mask: wgpu::ColorWrites::ALL })],
                }),
                multiview_mask: None,
                cache: None,
            });
            Variant { pipeline, layout }
        };
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("ironav video sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let params = device.create_buffer(&wgpu::BufferDescriptor { label: Some("ironav video params"), size: 80, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        Self { variants: [make("ironav yuv8", EIGHT_BIT, true), make("ironav yuv16", SIXTEEN_BIT, false)], sampler, params, srgb_target: target.is_srgb(), textures: None, shown: None, color: None }
    }

    /// Make the textures hold `f`. Allocates only when the picture's size or format changed.
    pub fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, f: &Arc<TimedFrame>) {
        if self.shown.as_ref().is_some_and(|s| Arc::ptr_eq(s, f)) {
            return;
        }
        let v = &f.frame;
        let key = Key { w: v.width, h: v.height, fmt: v.pix_fmt };
        let high = v.pix_fmt.bit_depth() > 8;
        let layout = v.pix_fmt.planes(v.width as usize, v.height as usize);
        if self.textures.as_ref().is_none_or(|t| t.key != key) {
            let make = |w: usize, h: usize| {
                device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("ironav video plane"),
                    size: wgpu::Extent3d { width: w as u32, height: h as u32, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: if high { wgpu::TextureFormat::R16Uint } else { wgpu::TextureFormat::R8Unorm },
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                })
            };
            let planes = if v.pix_fmt == PixelFormat::Gray8 {
                // No chroma: a 1x1 mid-grey stands in, which the matrix turns into a grey picture.
                let grey = || {
                    let t = make(1, 1);
                    write_plane(queue, &t, &[128], 1, 1, 1);
                    t
                };
                [make(layout[0].0, layout[0].1), grey(), grey()]
            } else {
                [make(layout[0].0, layout[0].1), make(layout[1].0, layout[1].1), make(layout[2].0, layout[2].1)]
            };
            let variant = &self.variants[usize::from(high)];
            let views: Vec<wgpu::TextureView> = planes.iter().map(|t| t.create_view(&Default::default())).collect();
            let mut entries: Vec<wgpu::BindGroupEntry> = views.iter().enumerate().map(|(i, view)| wgpu::BindGroupEntry { binding: i as u32, resource: wgpu::BindingResource::TextureView(view) }).collect();
            if !high {
                entries.push(wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&self.sampler) });
            }
            entries.push(wgpu::BindGroupEntry { binding: 4, resource: self.params.as_entire_binding() });
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("ironav video"), layout: &variant.layout, entries: &entries });
            self.textures = Some(Textures { key, high, planes, bind });
        }
        let t = self.textures.as_ref().expect("textures were just made");
        for (i, &(pw, ph, bpp)) in layout.iter().enumerate().take(if v.pix_fmt == PixelFormat::Gray8 { 1 } else { 3 }) {
            write_plane(queue, &t.planes[i], &v.planes[i], pw, ph, bpp);
        }
        let color = Colorimetry::of(v);
        if self.color != Some(color) {
            queue.write_buffer(&self.params, 0, &params_bytes(color, self.srgb_target));
            self.color = Some(color);
        }
        self.shown = Some(f.clone());
    }

    /// Draw the picture over the whole viewport of `pass`.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        let Some(t) = &self.textures else { return };
        pass.set_pipeline(&self.variants[usize::from(t.high)].pipeline);
        pass.set_bind_group(0, &t.bind, &[]);
        pass.draw(0..4, 0..1);
    }
}

/// Copy a plane of `w` x `h` samples (`bpp` bytes each, tightly packed) into its texture.
fn write_plane(queue: &wgpu::Queue, texture: &wgpu::Texture, data: &[u8], w: usize, h: usize, bpp: usize) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        &data[..w * h * bpp],
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some((w * bpp) as u32), rows_per_image: Some(h as u32) },
        wgpu::Extent3d { width: w as u32, height: h as u32, depth_or_array_layers: 1 },
    );
}

/// Set the video view up in `rs`. Call once when the window's renderer exists.
pub fn install(rs: &egui_wgpu::RenderState) {
    let g = GpuVideo::new(&rs.device, rs.target_format);
    rs.renderer.write().callback_resources.insert(g);
}

/// The paint callback: one picture, drawn into the rectangle it was added with.
pub struct VideoCallback {
    pub frame: Arc<TimedFrame>,
}

impl VideoCallback {
    /// A shape to add to a painter that draws `frame` into `rect`.
    pub fn shape(rect: egui::Rect, frame: Arc<TimedFrame>) -> egui::Shape {
        egui::Shape::Callback(egui_wgpu::Callback::new_paint_callback(rect, Self { frame }))
    }
}

impl CallbackTrait for VideoCallback {
    fn prepare(&self, device: &wgpu::Device, queue: &wgpu::Queue, _screen: &ScreenDescriptor, _encoder: &mut wgpu::CommandEncoder, resources: &mut CallbackResources) -> Vec<wgpu::CommandBuffer> {
        if let Some(g) = resources.get_mut::<GpuVideo>() {
            g.upload(device, queue, &self.frame);
        }
        Vec::new()
    }

    fn paint(&self, _info: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, resources: &CallbackResources) {
        if let Some(g) = resources.get::<GpuVideo>() {
            g.draw(pass);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_player::rgb_frame;
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    #[test]
    fn what_the_shader_gets_in_its_uniform_is_the_cpu_matrix() {
        let c = Colorimetry { matrix: ironav_player::Matrix::Bt709, full_range: false, bits: 10 };
        let b = params_bytes(c, true);
        assert_eq!(b.len(), 80, "the WGSL `Params` block is five vec4<f32>");
        let f = |i: usize| f32::from_ne_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
        let k = YuvToRgb::new(c);
        assert_eq!([f(0), f(1), f(2)], k.m[0]);
        assert_eq!([f(4), f(5), f(6)], k.m[1]);
        assert_eq!([f(8), f(9), f(10)], k.m[2]);
        assert_eq!([f(12), f(13), f(14)], k.offset);
        assert!((f(16) - 1.0 / 1023.0).abs() < 1e-9 && f(17) == 1.0);
    }

    #[test]
    fn rgb24_and_malformed_pictures_take_the_cpu_path() {
        let rgb = VideoFrame { width: 2, height: 2, pix_fmt: PixelFormat::Rgb24, pts: 0, planes: vec![vec![0; 12]] };
        assert!(!supported(&rgb));
        let bad = VideoFrame { width: 2, height: 2, pix_fmt: PixelFormat::Yuv420p, pts: 0, planes: vec![vec![0; 4]] };
        assert!(!supported(&bad));
        let ok = VideoFrame { width: 2, height: 2, pix_fmt: PixelFormat::Yuv420p10, pts: 0, planes: vec![vec![0; 8], vec![0; 2], vec![0; 2]] };
        assert!(supported(&ok));
    }

    fn block_on<T>(f: impl Future<Output = T>) -> T {
        let mut f = std::pin::pin!(f);
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
                return v;
            }
            std::thread::yield_now();
        }
    }

    fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::default();
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).ok()?;
        block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()
    }

    /// Render `f` offscreen at its own size and return RGBA bytes.
    fn render(device: &wgpu::Device, queue: &wgpu::Queue, f: VideoFrame) -> Vec<u8> {
        let (w, h) = (f.width, f.height);
        assert_eq!(w * 4 % 256, 0, "the readback needs 256-byte rows");
        let mut g = GpuVideo::new(device, wgpu::TextureFormat::Rgba8Unorm);
        g.upload(device, queue, &Arc::new(TimedFrame { time: 0.0, frame: f }));
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let out = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: u64::from(w * h * 4), usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        let mut enc = device.create_command_encoder(&Default::default());
        {
            let view = target.create_view(&Default::default());
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &view, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store } })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            g.draw(&mut pass);
        }
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &target, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo { buffer: &out, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) } },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        queue.submit([enc.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        out.slice(..).map_async(wgpu::MapMode::Read, move |r| tx.send(r).unwrap());
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        rx.recv().unwrap().unwrap();
        let view = out.slice(..).get_mapped_range().expect("the buffer is mapped");
        view.to_vec()
    }

    fn noise_planes(fmt: PixelFormat, w: u32, h: u32, chroma: [u16; 2]) -> VideoFrame {
        let mut seed = 99u32;
        let mut next = move || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 16) as u16
        };
        let high = fmt.bit_depth() > 8;
        let top = (1u16 << fmt.bit_depth()) - 1;
        let planes = fmt
            .planes(w as usize, h as usize)
            .iter()
            .enumerate()
            .map(|(i, &(pw, ph, bpp))| {
                (0..pw * ph)
                    .flat_map(|_| {
                        // Luma: any value in the studio range's neighbourhood. Chroma: flat, so the
                        // GPU's bilinear chroma agrees with the CPU's nearest.
                        let s = if i == 0 { next() % (top + 1) } else { chroma[i - 1] };
                        if high && bpp == 2 { s.to_le_bytes().to_vec() } else { vec![s as u8] }
                    })
                    .collect()
            })
            .collect();
        VideoFrame { width: w, height: h, pix_fmt: fmt, pts: 0, planes }
    }

    #[test]
    fn the_shader_agrees_with_the_cpu_converter_on_a_real_adapter() {
        let Some((device, queue)) = device() else {
            eprintln!("no GPU adapter here; the offscreen shader test is skipped");
            return;
        };
        // 64x48 is SD (BT.601); 64x720 is HD (BT.709).
        for (fmt, w, h, chroma) in [
            (PixelFormat::Yuv420p, 64, 48, [90u16, 240]),
            (PixelFormat::Yuv420p, 64, 720, [200, 60]),
            (PixelFormat::Yuv444p, 64, 48, [128, 128]),
            (PixelFormat::Yuv422p, 64, 48, [30, 140]),
            (PixelFormat::Gray8, 64, 48, [128, 128]),
            (PixelFormat::Yuv420p10, 64, 48, [90 << 2, 240 << 2]),
        ] {
            let f = noise_planes(fmt, w, h, chroma);
            let gpu = render(&device, &queue, f.clone());
            let (_, _, cpu) = rgb_frame(&f, None, None);
            let worst = cpu.as_chunks::<3>().0.iter().zip(gpu.as_chunks::<4>().0.iter()).flat_map(|(c, g)| (0..3).map(move |i| (i32::from(c[i]) - i32::from(g[i])).abs())).max().unwrap();
            assert!(worst <= 2, "{fmt:?} {w}x{h}: the GPU and CPU differ by {worst}");
        }
    }
}
