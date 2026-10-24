use crate::model::{cli_line, encodable_codecs, writable_formats, FilterState, Settings, Source};
use eframe::egui::{self, Color32, RichText};
use ironav_codec::FILTERS;
use ironav_core::{Error, MediaType};
use ironav_engine::plan::Action;
use ironav_engine::{ontology, Plan, Report};
use ironav_formats::FORMATS;
use ironav_hwaccel::{linked_backends, probe_all, Platform, ProbeResult, ProbeState};
use ironav_ontology::Kind;
use std::sync::mpsc::{channel, Receiver, Sender};

const OK: Color32 = Color32::from_rgb(63, 185, 80);
const WARN: Color32 = Color32::from_rgb(210, 153, 34);
const BAD: Color32 = Color32::from_rgb(248, 81, 73);
const ACCENT: Color32 = Color32::from_rgb(249, 115, 22);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Summary,
    Video,
    Audio,
    Filters,
    Hardware,
    Agent,
    Ontology,
}

const TABS: [(Tab, &str); 7] = [
    (Tab::Summary, "Summary"),
    (Tab::Video, "Video"),
    (Tab::Audio, "Audio"),
    (Tab::Filters, "Filters"),
    (Tab::Hardware, "Hardware"),
    (Tab::Agent, "Agent"),
    (Tab::Ontology, "Ontology"),
];

enum Msg {
    Progress { id: u64, bytes_in: u64 },
    Done { id: u64, result: Result<Report, Error> },
}

enum ItemState {
    Queued,
    Running,
    Done(String),
    Failed(Error),
}

struct Item {
    id: u64,
    job: ironav_engine::Job,
    label: String,
    size: u64,
    progress: f32,
    state: ItemState,
}

#[derive(Default)]
struct OntoState {
    query: String,
    kind: Option<Kind>,
    results: Vec<(String, String, bool)>,
    detail: String,
    dirty: bool,
}

pub struct App {
    ctx: egui::Context,
    src: Option<Source>,
    s: Option<Settings>,
    tab: Tab,
    plan: Option<Result<Plan, Error>>,
    open_error: Option<Error>,
    queue: Vec<Item>,
    queue_active: bool,
    next_id: u64,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    probes: Vec<ProbeResult>,
    onto: OntoState,
}

enum Action2 {
    Open,
    Plan,
    Enqueue,
    Start,
}

impl App {
    pub fn new(ctx: egui::Context, initial: Option<String>) -> Self {
        let (tx, rx) = channel();
        let mut a = App {
            ctx,
            src: None,
            s: None,
            tab: Tab::Summary,
            plan: None,
            open_error: None,
            queue: Vec::new(),
            queue_active: false,
            next_id: 1,
            tx,
            rx,
            probes: probe_all(),
            onto: OntoState { dirty: true, ..Default::default() },
        };
        if let Some(p) = initial {
            a.open(&p);
        }
        a
    }

    fn open(&mut self, path: &str) {
        self.plan = None;
        match Source::open(path) {
            Ok(src) => match Settings::for_source(&src) {
                Some(s) => {
                    self.open_error = None;
                    self.s = Some(s);
                    self.src = Some(src);
                    self.tab = Tab::Summary;
                }
                None => {
                    self.open_error = Some(Error::invalid_input("no writable output format fits this source's media type"));
                }
            },
            Err(e) => self.open_error = Some(e),
        }
    }

    fn pick_source(&mut self) {
        let exts: Vec<&str> = FORMATS.iter().filter(|f| f.demux).flat_map(|f| f.extensions.iter().copied()).collect();
        let mut d = rfd::FileDialog::new().set_title("Open source").add_filter("Media files", &exts).add_filter("All files", &["*"]);
        if let Some(dir) = self.src.as_ref().and_then(|s| std::path::Path::new(&s.path).parent()) {
            d = d.set_directory(dir);
        }
        if let Some(p) = d.pick_file() {
            self.open(&p.display().to_string());
        }
    }

    fn enqueue(&mut self) {
        let (Some(src), Some(s)) = (&self.src, &self.s) else { return };
        let job = s.job(src);
        let name = |p: &str| std::path::Path::new(p).file_name().and_then(|n| n.to_str()).unwrap_or(p).to_string();
        self.queue.push(Item {
            id: self.next_id,
            label: format!("{} → {}", name(&job.input), name(&job.output)),
            job,
            size: src.size,
            progress: 0.0,
            state: ItemState::Queued,
        });
        self.next_id += 1;
    }

    fn start_next(&mut self) {
        if self.queue.iter().any(|i| matches!(i.state, ItemState::Running)) {
            return;
        }
        let Some(item) = self.queue.iter_mut().find(|i| matches!(i.state, ItemState::Queued)) else {
            self.queue_active = false;
            return;
        };
        item.state = ItemState::Running;
        let (id, job, tx, ctx) = (item.id, item.job.clone(), self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let result = ironav_engine::run(&job, &mut |p| {
                let _ = tx.send(Msg::Progress { id, bytes_in: p.bytes_in });
                ctx.request_repaint();
            });
            let _ = tx.send(Msg::Done { id, result });
            ctx.request_repaint();
        });
    }

    fn drain_messages(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Progress { id, bytes_in } => {
                    if let Some(i) = self.queue.iter_mut().find(|i| i.id == id) {
                        i.progress = (bytes_in as f32 / i.size.max(1) as f32).min(0.99);
                    }
                }
                Msg::Done { id, result } => {
                    if let Some(i) = self.queue.iter_mut().find(|i| i.id == id) {
                        match result {
                            Ok(r) => {
                                i.progress = 1.0;
                                let rt = r.realtime_factor.map(|x| format!("{x:.0}× real time · ")).unwrap_or_default();
                                i.state = ItemState::Done(format!(
                                    "{:.2}s · {rt}{:.0} MiB/s · {:.1} KiB written",
                                    r.wall_ms as f64 / 1000.0,
                                    r.throughput_mib_s,
                                    r.bytes_out as f64 / 1024.0
                                ));
                            }
                            Err(e) => i.state = ItemState::Failed(e),
                        }
                    }
                }
            }
        }
        if self.queue_active {
            self.start_next();
        }
    }
}

fn error_box(ui: &mut egui::Ui, e: &Error) {
    egui::Frame::group(ui.style()).stroke(egui::Stroke::new(1.0, BAD)).show(ui, |ui| {
        ui.colored_label(BAD, format!("{}: {}", serde_json::to_value(e.code).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default(), e.message));
        if let Some(h) = &e.hint {
            ui.label(format!("Hint: {h}"));
        }
        if !e.alternatives.is_empty() {
            ui.label(format!("Try: {}", e.alternatives.join(", ")));
        }
    });
}

fn fmt_time(s: Option<f64>) -> String {
    match s {
        None => "unknown".into(),
        Some(s) if s >= 60.0 => format!("{}m {:.1}s", (s / 60.0) as u32, s % 60.0),
        Some(s) => format!("{s:.2}s"),
    }
}

impl eframe::App for App {
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_messages();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if let Some(path) = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf())) {
            self.open(&path.display().to_string());
        }

        let mut action: Option<Action2> = None;
        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading(RichText::new("IronAV").color(ACCENT));
                ui.separator();
                if ui.button("Open Source…").clicked() {
                    action = Some(Action2::Open);
                }
                let ready = self.src.is_some() && self.s.as_ref().is_some_and(|s| !s.output.is_empty());
                if ui.add_enabled(ready, egui::Button::new("Preview Plan")).clicked() {
                    action = Some(Action2::Plan);
                }
                if ui.add_enabled(ready, egui::Button::new("Add to Queue")).clicked() {
                    action = Some(Action2::Enqueue);
                }
                let any_queued = self.queue.iter().any(|i| matches!(i.state, ItemState::Queued));
                if ui.add_enabled(any_queued && !self.queue_active, egui::Button::new(RichText::new("Start Queue").strong())).clicked() {
                    action = Some(Action2::Start);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak(format!("v{} · {}", env!("CARGO_PKG_VERSION"), Platform::current().name()));
                });
            });
        });

        egui::Panel::bottom("queue").resizable(true).default_size(170.0).show(ui, |ui| {
            ui.strong("Queue");
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                if self.queue.is_empty() {
                    ui.weak("Empty. Configure a conversion and choose “Add to Queue”.");
                }
                let mut remove = None;
                for (n, item) in self.queue.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(&item.label);
                        let (txt, col) = match &item.state {
                            ItemState::Queued => ("queued", Color32::GRAY),
                            ItemState::Running => ("running", WARN),
                            ItemState::Done(_) => ("done", OK),
                            ItemState::Failed(_) => ("failed", BAD),
                        };
                        ui.colored_label(col, txt);
                        if !matches!(item.state, ItemState::Running) && ui.small_button("✕").on_hover_text("Remove").clicked() {
                            remove = Some(n);
                        }
                    });
                    ui.add(egui::ProgressBar::new(item.progress).desired_height(6.0));
                    match &item.state {
                        ItemState::Done(s) => {
                            ui.weak(s);
                        }
                        ItemState::Failed(e) => error_box(ui, e),
                        _ => {}
                    }
                    ui.add_space(4.0);
                }
                if let Some(n) = remove {
                    self.queue.remove(n);
                }
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            self.source_ui(ui);
            ui.separator();
            ui.horizontal(|ui| {
                for (t, name) in TABS {
                    if ui.selectable_label(self.tab == t, name).clicked() {
                        self.tab = t;
                    }
                }
            });
            ui.separator();
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.tab {
                Tab::Hardware => self.hardware_ui(ui),
                Tab::Ontology => self.ontology_ui(ui),
                _ if self.src.is_none() => {
                    ui.weak("Open a source (or drop a file here) to configure the conversion. Hardware and Ontology are available any time.");
                }
                Tab::Summary => self.summary_ui(ui),
                Tab::Video => self.codec_ui(ui, MediaType::Video),
                Tab::Audio => self.codec_ui(ui, MediaType::Audio),
                Tab::Filters => self.filters_ui(ui),
                Tab::Agent => self.agent_ui(ui),
            });
        });

        match action {
            Some(Action2::Open) => self.pick_source(),
            Some(Action2::Plan) => {
                if let (Some(src), Some(s)) = (&self.src, &self.s) {
                    self.plan = Some(ironav_engine::plan(&s.job(src)));
                    self.tab = Tab::Summary;
                }
            }
            Some(Action2::Enqueue) => self.enqueue(),
            Some(Action2::Start) => {
                self.queue_active = true;
                self.start_next();
            }
            None => {}
        }
    }
}

impl App {
    fn source_ui(&self, ui: &mut egui::Ui) {
        ui.strong("Source");
        if let Some(e) = &self.open_error {
            error_box(ui, e);
        }
        let Some(src) = &self.src else {
            ui.weak("No source selected. Use “Open Source…” or drop a file onto this window.");
            return;
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(&src.path).strong());
            ui.weak(format!("{} · {:.1} KiB · {}", src.format, src.size as f64 / 1024.0, fmt_time(src.duration)));
        });
        egui::Grid::new("streams").striped(true).num_columns(4).show(ui, |ui| {
            for h in ["#", "Type", "Codec", "Details"] {
                ui.weak(h);
            }
            ui.end_row();
            for st in &src.streams {
                ui.label(st.index.to_string());
                ui.label(format!("{:?}", st.params.media_type()).to_lowercase());
                ui.label(st.codec.name());
                ui.label(match &st.params {
                    ironav_core::StreamParams::Audio { sample_rate, channels } => format!("{sample_rate} Hz · {channels} ch"),
                    ironav_core::StreamParams::Video { width, height, pix_fmt, frame_rate } => {
                        format!("{width}×{height} · {} · {:.3} fps", pix_fmt.name(), frame_rate.as_f64())
                    }
                });
                ui.end_row();
            }
        });
    }

    fn summary_ui(&mut self, ui: &mut egui::Ui) {
        let (Some(src), Some(s)) = (self.src.as_ref(), self.s.as_mut()) else { return };
        let spec = s.spec();
        egui::Grid::new("summary").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Container");
            let mut chosen = None;
            egui::ComboBox::from_id_salt("fmt").selected_text(s.format).show_ui(ui, |ui| {
                for f in writable_formats(Some(src)) {
                    if ui.selectable_label(s.format == f.name, f.name).clicked() {
                        chosen = Some(f.name);
                    }
                }
            });
            if let Some(n) = chosen {
                s.set_format(Some(src), n);
            }
            ui.end_row();
            ui.label("");
            ui.weak(format!("{} · holds {} only", spec.description, format!("{:?}", spec.media).to_lowercase()));
            ui.end_row();

            ui.label("Save as");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut s.output).desired_width(ui.available_width() - 90.0));
                if ui.button("Browse…").clicked() {
                    let p = std::path::Path::new(&s.output);
                    let mut d = rfd::FileDialog::new().set_title("Save as").add_filter(spec.name, spec.extensions);
                    if let Some(dir) = p.parent().filter(|d| d.is_dir()) {
                        d = d.set_directory(dir);
                    }
                    if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                        d = d.set_file_name(name);
                    }
                    if let Some(out) = d.save_file() {
                        s.output = out.display().to_string();
                        // The dialog already asked about replacing an existing file.
                        s.overwrite = out.exists();
                    }
                }
            });
            ui.end_row();
            ui.label("Overwrite existing");
            ui.checkbox(&mut s.overwrite, "replace the output file if it exists");
            ui.end_row();
        });
        ui.add_space(8.0);
        match &self.plan {
            None => {}
            Some(Err(e)) => error_box(ui, e),
            Some(Ok(p)) => {
                ui.strong("Plan (nothing written yet)");
                for st in &p.steps {
                    ui.horizontal_wrapped(|ui| {
                        match st.action {
                            Action::Copy => ui.colored_label(OK, "copy"),
                            Action::Transcode => ui.colored_label(WARN, "transcode"),
                        };
                        let mut line = format!("{} → {}", st.from_codec.name(), st.to_codec.name());
                        if !st.filters.is_empty() {
                            line += &format!(" · filters: {}", st.filters.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(", "));
                        }
                        if !st.hardware.is_empty() {
                            line += &format!(" · hw: {}", st.hardware.iter().map(|h| h.summary.as_str()).collect::<Vec<_>>().join("; "));
                        }
                        ui.label(line);
                    });
                }
                for w in &p.warnings {
                    ui.weak(format!("⚠ {w}"));
                }
            }
        }
    }

    fn codec_ui(&mut self, ui: &mut egui::Ui, media: MediaType) {
        let Some(s) = self.s.as_mut() else { return };
        let spec = s.spec();
        if spec.media != media {
            ui.weak(format!("The {} container holds {:?} only; choose a {:?} container on the Summary tab to configure this.", spec.name, spec.media, media).to_lowercase());
            return;
        }
        let (label, slot) = if media == MediaType::Video { ("Video codec", &mut s.vcodec) } else { ("Audio codec", &mut s.acodec) };
        ui.horizontal(|ui| {
            ui.label(label);
            egui::ComboBox::from_id_salt(label).selected_text(slot.as_str()).show_ui(ui, |ui| {
                for c in std::iter::once("copy").chain(encodable_codecs(spec)) {
                    ui.selectable_value(slot, c.to_string(), c);
                }
            });
        });
        ui.weak(if slot == "copy" { "Packets pass through untouched: fastest, no quality loss." } else { "Re-encoded in software." });
        ui.weak("Compressed codecs (H.264, HEVC, AV1, AAC, Opus…) are in the ontology as planned and appear here once implemented.");
    }

    fn filters_ui(&mut self, ui: &mut egui::Ui) {
        let Some(s) = self.s.as_mut() else { return };
        let media = s.spec().media;
        for f in FILTERS.iter().filter(|f| f.media == media) {
            let st = s.filters.entry(f.name.to_string()).or_insert_with(FilterState::default);
            ui.checkbox(&mut st.enabled, RichText::new(f.name).strong());
            ui.weak(f.description);
            for p in f.params {
                ui.horizontal(|ui| {
                    ui.label(p.name);
                    let v = st.params.entry(p.name.to_string()).or_insert(p.default.unwrap_or(0.0));
                    ui.add(egui::DragValue::new(v).speed(0.01).range(0.0..=100.0));
                    ui.weak(p.description);
                });
            }
            ui.add_space(8.0);
        }
    }

    fn hardware_ui(&mut self, ui: &mut egui::Ui) {
        if let Some(s) = self.s.as_mut() {
            ui.horizontal(|ui| {
                ui.label("Policy");
                let names: Vec<String> = ["auto", "off", "require"]
                    .into_iter()
                    .map(String::from)
                    .chain(self.probes.iter().filter(|p| p.state != ProbeState::NotApplicable).map(|p| p.backend.name().to_string()))
                    .collect();
                egui::ComboBox::from_id_salt("hw").selected_text(s.hwaccel.as_str()).show_ui(ui, |ui| {
                    for n in names {
                        ui.selectable_value(&mut s.hwaccel, n.clone(), n);
                    }
                });
            });
            ui.weak("auto = use hardware when usable, else software · require = fail if no hardware is usable");
        }
        if linked_backends().is_empty() {
            ui.add_space(4.0);
            ui.colored_label(WARN, "This build links no native hardware providers yet, so every job runs in software even where drivers are detected.");
        }
        ui.add_space(6.0);
        if ui.button("Re-scan").clicked() {
            self.probes = probe_all();
        }
        egui::Grid::new("hwgrid").striped(true).num_columns(4).spacing([14.0, 4.0]).show(ui, |ui| {
            for h in ["Backend", "Driver", "Provider linked", "Evidence"] {
                ui.weak(h);
            }
            ui.end_row();
            for p in &self.probes {
                ui.label(p.backend.name());
                let (t, c) = match p.state {
                    ProbeState::Detected => ("detected", OK),
                    ProbeState::NotDetected => ("not detected", WARN),
                    ProbeState::NotApplicable => ("n/a on this OS", Color32::GRAY),
                };
                ui.colored_label(c, t);
                ui.label(if p.linked { "yes" } else { "no" });
                ui.weak(&p.evidence);
                ui.end_row();
            }
        });
    }

    fn agent_ui(&mut self, ui: &mut egui::Ui) {
        let (Some(src), Some(s)) = (self.src.as_ref(), self.s.as_ref()) else { return };
        let job = s.job(src);
        let cli = cli_line(&job);
        let json = serde_json::to_string_pretty(&job).unwrap_or_default();
        ui.weak("Exactly what this form will run. Agents can use the same job through the CLI or the MCP server (`ironav mcp`).");
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.strong("Command line");
            if ui.small_button("Copy").clicked() {
                ui.ctx().copy_text(cli.clone());
            }
        });
        ui.add(egui::TextEdit::multiline(&mut cli.as_str()).code_editor().desired_width(f32::INFINITY).desired_rows(2));
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.strong("Job (JSON) — `ironav run job.json` or MCP tool “run”");
            if ui.small_button("Copy").clicked() {
                ui.ctx().copy_text(json.clone());
            }
        });
        ui.add(egui::TextEdit::multiline(&mut json.as_str()).code_editor().desired_width(f32::INFINITY));
    }

    fn ontology_ui(&mut self, ui: &mut egui::Ui) {
        let o = &mut self.onto;
        ui.horizontal(|ui| {
            let r = ui.add(egui::TextEdit::singleline(&mut o.query).hint_text("search codecs, containers, filters, hardware…").desired_width(320.0));
            if r.changed() {
                o.dirty = true;
            }
            let label = o.kind.map(|k| k.prefix()).unwrap_or("(all kinds)");
            egui::ComboBox::from_id_salt("kind").selected_text(label).show_ui(ui, |ui| {
                if ui.selectable_label(o.kind.is_none(), "(all kinds)").clicked() {
                    o.kind = None;
                    o.dirty = true;
                }
                for k in Kind::ALL {
                    if ui.selectable_label(o.kind == Some(k), k.prefix()).clicked() {
                        o.kind = Some(k);
                        o.dirty = true;
                    }
                }
            });
        });
        if o.dirty {
            o.dirty = false;
            o.results = ontology()
                .search(&o.query, o.kind, 60)
                .into_iter()
                .map(|e| (e.id.clone(), e.label.clone(), e.status == ironav_ontology::Status::Implemented))
                .collect();
        }
        ui.add_space(4.0);
        ui.columns(2, |cols| {
            egui::ScrollArea::vertical().id_salt("onto-list").show(&mut cols[0], |ui| {
                for (id, label, imp) in &o.results {
                    ui.horizontal(|ui| {
                        if ui.selectable_label(false, id).on_hover_text(label).clicked() {
                            o.detail = ontology().describe(id).map(|v| serde_json::to_string_pretty(&v).unwrap_or_default()).unwrap_or_default();
                        }
                        ui.colored_label(if *imp { OK } else { WARN }, if *imp { "implemented" } else { "planned" });
                    });
                }
            });
            egui::ScrollArea::vertical().id_salt("onto-detail").show(&mut cols[1], |ui| {
                if o.detail.is_empty() {
                    ui.weak("Click an entity for its description and relations.");
                } else {
                    ui.add(egui::Label::new(RichText::new(&o.detail).monospace().size(12.0)).wrap());
                }
            });
        });
    }
}
