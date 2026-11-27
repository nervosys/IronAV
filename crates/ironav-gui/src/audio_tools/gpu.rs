//! Audio plot callbacks share the player's wgpu device. PCM goes in; transforms
//! and plot data stay on the device for drawing.

use super::{color_scale::ColorScale, plots::{self, PlotKind}};
use crate::player_style::Theme;
use eframe::{egui, egui_wgpu::{self, wgpu, CallbackResources, CallbackTrait, ScreenDescriptor}};
use ironav_player::{
    AudioEffects, AudioPcm, Player, DECAY_FFT_SIZE, DECAY_SLICES, EQ_FREQUENCIES,
    SPECTRUM_BANDS, SPECTRUM_BATCH, SPECTRUM_FFT_SIZE, SPECTRUM_HISTORY, SPECTRUM_MAX_HZ,
    SPECTRUM_MIN_HZ, WAVEFORM_BINS,
};
use std::sync::Arc;

const WORKGROUP: u32 = 64;
const PCM_CAPACITY: usize = 131_072;
const RESPONSE_POINTS: usize = plots::RESPONSE_POINTS;
const DECAY_OFFSET: usize = SPECTRUM_HISTORY * SPECTRUM_BANDS;
const WAVE_OFFSET: usize = DECAY_OFFSET + DECAY_SLICES * SPECTRUM_BANDS;
const EQ_OFFSET: usize = WAVE_OFFSET + WAVEFORM_BINS * 4;
const PLOT_FLOATS: usize = EQ_OFFSET + RESPONSE_POINTS;
const ANALYSIS_WORDS: usize = 8 + EQ_FREQUENCIES.len() * 8;

const _: () = assert!(SPECTRUM_BANDS == WORKGROUP as usize && DECAY_FFT_SIZE == 4 * SPECTRUM_FFT_SIZE);

fn supported(adapter: &wgpu::Adapter, device: &wgpu::Device) -> bool {
    let limits = device.limits();
    let flags = adapter.get_downlevel_capabilities().flags;
    flags.contains(wgpu::DownlevelFlags::COMPUTE_SHADERS | wgpu::DownlevelFlags::VERTEX_STORAGE)
        && limits.max_compute_workgroup_storage_size >= (SPECTRUM_FFT_SIZE * 8) as u32
        && limits.max_compute_invocations_per_workgroup >= WORKGROUP
        && limits.max_compute_workgroup_size_x >= WORKGROUP
        && limits.max_storage_buffers_per_shader_stage >= 4
        && limits.max_storage_buffer_binding_size as usize >= PCM_CAPACITY * 8
        && limits.max_buffer_size >= (PCM_CAPACITY * 8) as u64
}

pub(crate) fn install(state: &egui_wgpu::RenderState) -> bool {
    if state.adapter.get_info().device_type == wgpu::DeviceType::Cpu || !supported(&state.adapter, &state.device) {
        return false;
    }
    let plots = GpuAudio::new(&state.device, state.target_format);
    state.renderer.write().callback_resources.insert(plots);
    true
}

fn shader(device: &wgpu::Device, label: &str, body: &str, fft_threads: u32) -> wgpu::ShaderModule {
    let constants = format!(
        "const FFT_SIZE: u32 = {SPECTRUM_FFT_SIZE}u;
         const DECAY_SIZE: u32 = {DECAY_FFT_SIZE}u;
         const DECAY_SLICES: u32 = {DECAY_SLICES}u;
         const FFT_THREADS: u32 = {fft_threads}u;
         const BANDS: u32 = {SPECTRUM_BANDS}u;
         const HISTORY: u32 = {SPECTRUM_HISTORY}u;
         const WAVE_BINS: u32 = {WAVEFORM_BINS}u;
         const EQ_POINTS: u32 = {RESPONSE_POINTS}u;
         const DECAY_OFFSET: u32 = {DECAY_OFFSET}u;
         const WAVE_OFFSET: u32 = {WAVE_OFFSET}u;
         const EQ_OFFSET: u32 = {EQ_OFFSET}u;
         const MIN_HZ: f32 = {SPECTRUM_MIN_HZ:.1};
         const MAX_HZ: f32 = {SPECTRUM_MAX_HZ:.1};
         {body}"
    );
    device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label), source: wgpu::ShaderSource::Wgsl(constants.into()),
    })
}

fn make_buffer(device: &wgpu::Device, label: &str, bytes: usize, usage: wgpu::BufferUsages) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label), size: bytes as u64, usage, mapped_at_creation: false,
    })
}

fn buffer_layout(device: &wgpu::Device, label: &str, stages: wgpu::ShaderStages, types: &[wgpu::BufferBindingType]) -> wgpu::BindGroupLayout {
    let entries: Vec<_> = types.iter().enumerate().map(|(binding, &ty)| wgpu::BindGroupLayoutEntry {
        binding: binding as u32, visibility: stages,
        ty: wgpu::BindingType::Buffer { ty, has_dynamic_offset: false, min_binding_size: None }, count: None,
    }).collect();
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some(label), entries: &entries })
}

fn bind_buffers(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, buffers: &[&wgpu::Buffer]) -> wgpu::BindGroup {
    let entries: Vec<_> = buffers.iter().enumerate().map(|(binding, buffer)| wgpu::BindGroupEntry {
        binding: binding as u32, resource: buffer.as_entire_binding(),
    }).collect();
    device.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout, entries: &entries })
}

#[derive(Clone, Copy, PartialEq)]
struct Equalizer {
    rate: u32,
    gains: [f32; EQ_FREQUENCIES.len()],
    filters: [[f32; 8]; EQ_FREQUENCIES.len()],
}

#[derive(Default)]
pub(super) struct PlotState {
    epoch: u64,
    pcm: Arc<AudioPcm>,
    equalizer: Option<Equalizer>,
}

pub(super) struct PlotOptions<'a> {
    pub kind: PlotKind,
    pub scale: ColorScale,
    pub height: f32,
    pub theme: &'a Theme,
    pub effects: AudioEffects,
}

impl PlotState {
    pub fn reset(&mut self) {
        self.epoch += 1;
        self.pcm = Arc::default();
        self.equalizer = None;
    }

    pub fn show(&mut self, ui: &mut egui::Ui, player: &Player, options: PlotOptions<'_>) {
        let PlotOptions { kind, scale, height, theme, effects } = options;
        if kind == PlotKind::Decay {
            player.audio_decay_pcm(&mut self.pcm);
        } else {
            player.audio_visualization_pcm(&mut self.pcm);
        }
        if kind == PlotKind::Equalizer {
            let rate = player.audio_output_rate().unwrap_or_default();
            if self.equalizer.is_none_or(|eq| eq.rate != rate || eq.gains != effects.eq_db) {
                self.equalizer = Some(Equalizer {
                    rate, gains: effects.eq_db, filters: player.equalizer_coefficients(effects),
                });
            }
        }
        let (rect, response) = plots::chart(ui, height, kind.left_margin(), theme);
        let callback = AudioCallback {
            epoch: self.epoch,
            pcm: self.pcm.clone(),
            equalizer: self.equalizer,
            kind, scale, dark: ui.visuals().dark_mode, background: theme.background, rect,
        };
        ui.painter().add(egui_wgpu::Callback::new_paint_callback(rect, callback));
        plots::gpu_axes(ui.painter(), rect, kind, &self.pcm, theme);
        response.on_hover_text(kind.description());
    }
}

struct AudioCallback {
    epoch: u64,
    pcm: Arc<AudioPcm>,
    equalizer: Option<Equalizer>,
    kind: PlotKind,
    scale: ColorScale,
    dark: bool,
    background: egui::Color32,
    rect: egui::Rect,
}

impl CallbackTrait for AudioCallback {
    fn prepare(&self, _device: &wgpu::Device, queue: &wgpu::Queue, screen: &ScreenDescriptor, encoder: &mut wgpu::CommandEncoder, resources: &mut CallbackResources) -> Vec<wgpu::CommandBuffer> {
        if let Some(gpu) = resources.get_mut::<GpuAudio>() {
            gpu.prepare(queue, encoder, self, screen.pixels_per_point);
        }
        Vec::new()
    }

    fn paint(&self, _info: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, resources: &CallbackResources) {
        if let Some(gpu) = resources.get::<GpuAudio>() {
            gpu.draw(pass, self.kind);
        }
    }
}

struct Compute {
    params: wgpu::Buffer,
    pcm: wgpu::Buffer,
    bind: wgpu::BindGroup,
    fft_threads: u32,
    normal: wgpu::ComputePipeline,
    bands: wgpu::ComputePipeline,
    decay: wgpu::ComputePipeline,
    merge_tiles: wgpu::ComputePipeline,
    merge_decay: wgpu::ComputePipeline,
    decay_bands: wgpu::ComputePipeline,
    waveform: wgpu::ComputePipeline,
    equalizer: wgpu::ComputePipeline,
}

impl Compute {
    fn new(device: &wgpu::Device, data: &wgpu::Buffer) -> Self {
        use wgpu::util::DeviceExt;
        use wgpu::{BufferBindingType as Binding, BufferUsages as Usage};
        let params = make_buffer(device, "audio analysis parameters", ANALYSIS_WORDS * 4, Usage::UNIFORM | Usage::COPY_DST);
        let pcm = make_buffer(device, "audio samples", PCM_CAPACITY * 8, Usage::STORAGE | Usage::COPY_DST);
        let fft_size = (SPECTRUM_BATCH * SPECTRUM_FFT_SIZE).max(DECAY_SLICES * DECAY_FFT_SIZE);
        let fft = make_buffer(device, "audio FFT scratch", fft_size * 8, Usage::STORAGE);
        let rotations: Vec<[f32; 2]> = (0..DECAY_FFT_SIZE / 2).map(|index| {
            let (sin, cos) = (-std::f32::consts::TAU * index as f32 / DECAY_FFT_SIZE as f32).sin_cos();
            [cos, sin]
        }).collect();
        let rotations = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("audio FFT rotations"), contents: bytemuck::cast_slice(&rotations),
            usage: Usage::STORAGE,
        });
        let layout = buffer_layout(device, "audio analysis", wgpu::ShaderStages::COMPUTE, &[
            Binding::Uniform, Binding::Storage { read_only: true },
            Binding::Storage { read_only: false }, Binding::Storage { read_only: false },
            Binding::Storage { read_only: true },
        ]);
        let bind = bind_buffers(device, &layout, &[&params, &pcm, &fft, data, &rotations]);
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("audio analysis"), bind_group_layouts: &[Some(&layout)], immediate_size: 0,
        });
        let limits = device.limits();
        let limit = limits.max_compute_workgroup_size_x.min(limits.max_compute_invocations_per_workgroup);
        let fft_threads = [256, 128, WORKGROUP].into_iter().find(|&threads| threads <= limit).unwrap_or(WORKGROUP);
        let module = shader(device, "audio analysis", include_str!("analysis.wgsl"), fft_threads);
        let make = |entry| device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some(entry), layout: Some(&pipeline_layout), module: &module,
            entry_point: Some(entry), compilation_options: Default::default(), cache: None,
        });
        Self {
            params, pcm, bind, fft_threads, normal: make("normal_fft"), bands: make("normal_bands"),
            decay: make("decay_fft"), merge_tiles: make("merge_tiles"), merge_decay: make("merge_decay"),
            decay_bands: make("decay_bands"), waveform: make("waveform"), equalizer: make("equalizer"),
        }
    }

    fn dispatch(&self, encoder: &mut wgpu::CommandEncoder, pipeline: &wgpu::ComputePipeline, x: u32, y: u32) {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &self.bind, &[]);
        pass.dispatch_workgroups(x, y, 1);
    }
}

struct GpuAudio {
    compute: Compute,
    params: wgpu::Buffer,
    palette: wgpu::Buffer,
    #[cfg(test)]
    results: wgpu::Buffer,
    bind: wgpu::BindGroup,
    flat: wgpu::RenderPipeline,
    decay: wgpu::RenderPipeline,
    srgb: bool,
    scale: Option<ColorScale>,
    source: Option<(u64, u64)>,
    sample_end: Option<u64>,
    spectrum_end: Option<u64>,
    decay_end: Option<u64>,
    waveform_end: Option<u64>,
    equalizer: Option<Equalizer>,
    head: usize,
    rows: usize,
    draw_params: Option<[u32; 16]>,
}

impl GpuAudio {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        use wgpu::{BufferBindingType as Binding, BufferUsages as Usage};
        let data = make_buffer(device, "audio plot results", PLOT_FLOATS * 4, Usage::STORAGE | Usage::COPY_SRC);
        let compute = Compute::new(device, &data);
        let params = make_buffer(device, "audio plot parameters", 64, Usage::UNIFORM | Usage::COPY_DST);
        let palette = make_buffer(device, "audio color scale", 1024, Usage::STORAGE | Usage::COPY_DST);
        let layout = buffer_layout(device, "audio plot", wgpu::ShaderStages::VERTEX_FRAGMENT, &[
            Binding::Uniform, Binding::Storage { read_only: true }, Binding::Storage { read_only: true },
        ]);
        let bind = bind_buffers(device, &layout, &[&params, &data, &palette]);
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("audio plot"), bind_group_layouts: &[Some(&layout)], immediate_size: 0,
        });
        let module = shader(device, "audio plot", include_str!("render.wgsl"), WORKGROUP);
        let make = |vertex, fragment| device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(vertex), layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState { module: &module, entry_point: Some(vertex), compilation_options: Default::default(), buffers: &[] },
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
            depth_stencil: None, multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module, entry_point: Some(fragment), compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState { format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
            }),
            multiview_mask: None, cache: None,
        });
        Self {
            compute, params, palette, bind, flat: make("quad", "draw"), decay: make("decay_quad", "draw_decay"),
            #[cfg(test)]
            results: data,
            srgb: format.is_srgb(), scale: None, source: None, sample_end: None, spectrum_end: None,
            decay_end: None, waveform_end: None, equalizer: None, head: 0, rows: 0,
            draw_params: None,
        }
    }

    fn prepare(&mut self, queue: &wgpu::Queue, encoder: &mut wgpu::CommandEncoder, frame: &AudioCallback, pixels_per_point: f32) {
        let pcm = &frame.pcm;
        let source = (frame.epoch, pcm.revision);
        if self.source != Some(source) {
            self.source = Some(source);
            self.sample_end = None;
            self.spectrum_end = None;
            self.decay_end = None;
            self.waveform_end = None;
            self.head = 0;
            self.rows = 0;
        }
        let samples = &pcm.samples[pcm.samples.len().saturating_sub(PCM_CAPACITY)..];
        let spectrum_end = pcm.spectrum_end();
        let trailing = (pcm.end_frame - spectrum_end) as usize;
        let spectrum_samples = samples.len().saturating_sub(trailing);
        let mut rows = 0;
        if spectrum_samples >= SPECTRUM_FFT_SIZE {
            let hop = pcm.hop.max(1);
            let available = 1 + (spectrum_samples - SPECTRUM_FFT_SIZE) / hop;
            let elapsed = self.spectrum_end.map_or(SPECTRUM_BATCH, |last| spectrum_end.saturating_sub(last) as usize / hop);
            rows = available.min(elapsed).min(SPECTRUM_BATCH);
        }
        let decay = frame.kind == PlotKind::Decay && samples.len() >= SPECTRUM_FFT_SIZE
            && self.decay_end != Some(pcm.end_frame);
        let waveform = frame.kind == PlotKind::Waveform && samples.len() >= SPECTRUM_FFT_SIZE && self.waveform_end != Some(pcm.end_frame);
        let equalizer = if frame.kind == PlotKind::Equalizer {
            frame.equalizer.filter(|eq| eq.rate != 0 && self.equalizer != Some(*eq))
        } else {
            None
        };
        if (rows != 0 || decay || waveform) && self.sample_end != Some(pcm.end_frame) {
            queue.write_buffer(&self.compute.pcm, 0, bytemuck::cast_slice(samples));
            self.sample_end = Some(pcm.end_frame);
        }
        if rows != 0 || decay || waveform || equalizer.is_some() {
            let mut params = [0u32; ANALYSIS_WORDS];
            params[..6].copy_from_slice(&[
                samples.len() as u32, pcm.hop as u32, rows as u32, self.head as u32,
                equalizer.map_or(pcm.rate, |eq| eq.rate), trailing as u32,
            ]);
            if let Some(eq) = equalizer {
                params[8..].copy_from_slice(bytemuck::cast_slice(&eq.filters));
            }
            queue.write_buffer(&self.compute.params, 0, bytemuck::cast_slice(&params));
            self.analyze(encoder, rows, decay, waveform, equalizer, pcm.end_frame);
            if rows != 0 {
                self.spectrum_end = Some(spectrum_end);
            }
        }
        if self.scale != Some(frame.scale) {
            let rgba = frame.scale.colors().map(|color| u32::from_le_bytes([color.r(), color.g(), color.b(), 255]));
            queue.write_buffer(&self.palette, 0, bytemuck::cast_slice(&rgba));
            self.scale = Some(frame.scale);
        }
        let rect = frame.rect;
        let [left, right] = plots::trace_indices(frame.dark);
        let draw = [
            frame.kind as u32, ((self.head + SPECTRUM_HISTORY - 1) % SPECTRUM_HISTORY) as u32, self.rows as u32,
            u32::from(pcm.stereo),
            ((rect.height() * pixels_per_point).max(1.0).recip()).to_bits(),
            (plots::decay_depth(rect) / rect.width().max(1.0)).to_bits(),
            (if self.srgb { 1.0f32 } else { 0.0 }).to_bits(),
            ((rect.width() * pixels_per_point).max(1.0).recip()).to_bits(),
            (f32::from(frame.background.r()) / 255.0).to_bits(), (f32::from(frame.background.g()) / 255.0).to_bits(),
            (f32::from(frame.background.b()) / 255.0).to_bits(), 1.0f32.to_bits(),
            left as u32, right as u32, 0, 0,
        ];
        if self.draw_params != Some(draw) {
            queue.write_buffer(&self.params, 0, bytemuck::cast_slice(&draw));
            self.draw_params = Some(draw);
        }
    }

    fn analyze(&mut self, encoder: &mut wgpu::CommandEncoder, rows: usize, decay: bool, waveform: bool, equalizer: Option<Equalizer>, end: u64) {
        let compute = &self.compute;
        if rows != 0 {
            compute.dispatch(encoder, &compute.normal, rows as u32, 1);
            compute.dispatch(encoder, &compute.bands, rows as u32, 1);
            self.head = (self.head + rows) % SPECTRUM_HISTORY;
            self.rows = (self.rows + rows).min(SPECTRUM_HISTORY);
        }
        if decay {
            let slices = DECAY_SLICES as u32;
            let merge_groups = DECAY_FFT_SIZE as u32 / (2 * compute.fft_threads);
            compute.dispatch(encoder, &compute.decay, (DECAY_FFT_SIZE / SPECTRUM_FFT_SIZE) as u32, slices);
            compute.dispatch(encoder, &compute.merge_tiles, merge_groups, slices);
            compute.dispatch(encoder, &compute.merge_decay, merge_groups, slices);
            compute.dispatch(encoder, &compute.decay_bands, slices, 1);
            self.decay_end = Some(end);
        }
        if waveform {
            compute.dispatch(encoder, &compute.waveform, WAVEFORM_BINS as u32 / WORKGROUP, 1);
            self.waveform_end = Some(end);
        }
        if let Some(eq) = equalizer {
            compute.dispatch(encoder, &compute.equalizer, RESPONSE_POINTS as u32 / WORKGROUP, 1);
            self.equalizer = Some(eq);
        }
    }

    fn draw(&self, pass: &mut wgpu::RenderPass<'_>, kind: PlotKind) {
        let valid = match kind {
            PlotKind::Waterfall | PlotKind::Spectrum => self.rows != 0,
            PlotKind::Decay => self.decay_end.is_some(),
            PlotKind::Waveform => self.waveform_end.is_some(),
            PlotKind::Equalizer => self.equalizer.is_some(),
        };
        if !valid {
            return;
        }
        pass.set_bind_group(0, &self.bind, &[]);
        if kind == PlotKind::Decay {
            pass.set_pipeline(&self.decay);
            pass.draw(0..(2 * SPECTRUM_BANDS) as u32, 0..DECAY_SLICES as u32);
        } else {
            pass.set_pipeline(&self.flat);
            pass.draw(0..4, 0..1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::tests::block_on;

    #[test]
    fn the_device_analyzes_and_draws_every_audio_plot() {
        let instance = wgpu::Instance::default();
        let Ok(adapter) = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())) else {
            eprintln!("skipped: no wgpu adapter");
            return;
        };
        let Ok((device, queue)) = block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())) else {
            eprintln!("skipped: no wgpu device");
            return;
        };
        if !supported(&adapter, &device) {
            eprintln!("skipped: adapter lacks compute or storage support");
            return;
        }
        eprintln!("audio plots on {}", adapter.get_info().name);
        const WIDTH: u32 = 128;
        const HEIGHT: u32 = 64;
        let extent = wgpu::Extent3d { width: WIDTH, height: HEIGHT, depth_or_array_layers: 1 };
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut gpu = GpuAudio::new(&device, format);
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: None, size: extent, mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2,
            format, usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC, view_formats: &[],
        });
        let view = target.create_view(&Default::default());
        let image = make_buffer(&device, "audio plot image test", (WIDTH * HEIGHT * 4) as usize,
            wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ);
        let levels = make_buffer(&device, "audio analysis test", PLOT_FLOATS * 4,
            wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ);
        let samples = (0..DECAY_FFT_SIZE).map(|i| {
            let sample = (std::f32::consts::TAU * i as f32 / 48.0).sin() * 0.5;
            [sample, -sample * 0.5]
        }).collect();
        let pcm = Arc::new(AudioPcm {
            revision: 1, end_frame: DECAY_FFT_SIZE as u64, rate: 48_000, stereo: true, hop: 1920, samples,
        });
        let mut filters = [[1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0]; EQ_FREQUENCIES.len()];
        filters[0][0] = 2.0;
        let mut frame = AudioCallback {
            epoch: 0,
            pcm,
            equalizer: Some(Equalizer { rate: 48_000, gains: [0.0; EQ_FREQUENCIES.len()], filters }),
            kind: PlotKind::Waterfall,
            scale: ColorScale::Jet,
            dark: true,
            background: egui::Color32::BLACK,
            rect: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(WIDTH as f32, HEIGHT as f32)),
        };
        let map = |buffer: &wgpu::Buffer| {
            let (tx, rx) = std::sync::mpsc::channel();
            buffer.slice(..).map_async(wgpu::MapMode::Read, move |result| tx.send(result).unwrap());
            device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            rx.recv().unwrap().unwrap();
            let bytes = buffer.slice(..).get_mapped_range().unwrap().to_vec();
            buffer.unmap();
            bytes
        };
        let render = |gpu: &GpuAudio, encoder: &mut wgpu::CommandEncoder, kind| {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view, depth_slice: None, resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            gpu.draw(&mut pass, kind);
        };
        for kind in PlotKind::ALL {
            frame.kind = kind;
            let mut encoder = device.create_command_encoder(&Default::default());
            gpu.prepare(&queue, &mut encoder, &frame, 1.0);
            render(&gpu, &mut encoder, kind);
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo { texture: &target, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                wgpu::TexelCopyBufferInfo { buffer: &image, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(WIDTH * 4), rows_per_image: Some(HEIGHT) } },
                extent,
            );
            encoder.copy_buffer_to_buffer(&gpu.results, 0, &levels, 0, (PLOT_FLOATS * 4) as u64);
            queue.submit([encoder.finish()]);
            let rgba = map(&image);
            assert!(rgba.as_chunks::<4>().0.iter().any(|pixel| pixel[..3] != [0, 0, 0]), "{kind:?} produced no color");
        }
        let computed: Vec<f32> = map(&levels).as_chunks::<4>().0.iter().copied().map(f32::from_le_bytes).collect();
        let row = (gpu.head + SPECTRUM_HISTORY - 1) % SPECTRUM_HISTORY;
        let row = &computed[row * SPECTRUM_BANDS..(row + 1) * SPECTRUM_BANDS];
        let (band, level) = row.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap();
        let edge = |i| SPECTRUM_MIN_HZ * (SPECTRUM_MAX_HZ / SPECTRUM_MIN_HZ).powf(i as f32 / SPECTRUM_BANDS as f32);
        assert!((edge(band)..edge(band + 1)).contains(&1000.0) && *level > 0.89);
        let early = computed[DECAY_OFFSET + band];
        let late = computed[DECAY_OFFSET + (DECAY_SLICES - 1) * SPECTRUM_BANDS + band];
        assert!(early > late && late > 0.75, "{early} → {late}");
        let wave = &computed[WAVE_OFFSET..EQ_OFFSET];
        assert!(wave.as_chunks::<4>().0.iter().any(|bin| bin[0] < -0.49 && bin[3] > 0.24));
        assert!((computed[EQ_OFFSET + RESPONSE_POINTS / 2] - 6.0206).abs() < 0.01);

        // The first short window can draw CSD, and a 60 Hz preview tick can update
        // the CSD without moving the waterfall's 25 Hz row.
        frame.kind = PlotKind::Decay;
        frame.epoch += 1;
        Arc::make_mut(&mut frame.pcm).samples.truncate(SPECTRUM_FFT_SIZE);
        for end in [2048, 2848] {
            Arc::make_mut(&mut frame.pcm).end_frame = end;
            let mut encoder = device.create_command_encoder(&Default::default());
            gpu.prepare(&queue, &mut encoder, &frame, 1.0);
            render(&gpu, &mut encoder, frame.kind);
            queue.submit([encoder.finish()]);
            assert_eq!(gpu.decay_end, Some(end));
            assert_eq!(gpu.spectrum_end, Some(2048));
        }
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    }
}
