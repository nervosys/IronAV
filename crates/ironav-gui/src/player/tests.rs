use super::*;

fn find_label<'a>(shape: &'a egui::Shape, label: &str) -> Option<&'a egui::epaint::TextShape> {
    match shape {
        egui::Shape::Text(text) if text.galley.job.text == label => Some(text),
        egui::Shape::Vec(shapes) => shapes.iter().find_map(|shape| find_label(shape, label)),
        _ => None,
    }
}

fn visible_label(output: &egui::FullOutput, label: &str) -> egui::Rect {
    output.shapes.iter().find_map(|item| {
        let text = find_label(&item.shape, label)?;
        let bounds = text.galley.rect.translate(text.pos.to_vec2());
        (!text.galley.elided && item.clip_rect.expand(1.0).contains_rect(bounds)).then_some(bounds)
    }).unwrap_or_else(|| panic!("{label} must be visible without clipping"))
}

fn fixture(dir: &std::path::Path, name: &str) -> String {
    let path = dir.join(name);
    let mut bytes = b"YUV4MPEG2 W64 H48 F10:1 Ip A1:1 C420jpeg\n".to_vec();
    for _ in 0..60 {
        bytes.extend(b"FRAME\n");
        bytes.resize(bytes.len() + 64 * 48 * 3 / 2, 128);
    }
    std::fs::write(&path, bytes).unwrap();
    path.display().to_string()
}

#[test]
fn a_finished_open_is_discarded_when_a_newer_selection_is_pending() {
    let dir = std::env::temp_dir().join(format!("ironav-gui-selection-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let old = fixture(&dir, "old.y4m");
    let latest = fixture(&dir, "latest.y4m");
    let p = Player::open(&old, &OpenOptions { audio: AudioMode::Off, autoplay: false, ..Default::default() }).unwrap();

    let mut app = PlayerApp::new(None, false, egui::Context::default(), None);
    let (send, rx) = channel();
    app.opening = Some(Opening { path: old, rx });
    app.open(&dir.join("superseded.y4m").display().to_string());
    app.open(&latest);
    assert!(send.send(Ok(p)).is_ok());

    app.finish_open();
    assert!(app.player.is_none(), "the superseded player was never published");
    assert_eq!(app.opening.as_ref().unwrap().path, latest);
    let t = Instant::now();
    while app.player.is_none() {
        app.finish_open();
        assert!(app.error.is_none(), "{:?}", app.error);
        assert!(t.elapsed() < Duration::from_secs(5), "the latest selection did not open");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(app.player.as_ref().unwrap().info().path, latest);
}

#[test]
fn a_long_filename_fits_beside_the_player_menus() {
    let ctx = egui::Context::default();
    let mut app = PlayerApp::new(None, false, ctx.clone(), None);
    app.path = format!("{}720p60.mp4", "playback-diagnostic-".repeat(10));
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(520.0, 320.0));
    let mut output = ctx.run_ui(egui::RawInput { screen_rect: Some(screen), ..Default::default() }, |ui| app.menu_bar(ui));
    output.textures_delta.clear();
    let label = |s| output.shapes.iter().find_map(|item| find_label(&item.shape, s)).expect("label is visible in the player header");
    let file = label(&app.path);
    assert!(label("File").pos.x < label("View").pos.x);
    assert!(label("View").pos.x < label("Subtitles").pos.x);
    assert!(label("Subtitles").pos.x < label("Open…").pos.x);
    assert!(label("Open…").pos.x < label("Snapshot").pos.x);
    assert!(label("Snapshot").pos.x < label("Agent view").pos.x);
    assert!(label("Agent view").pos.x < file.pos.x);
    assert!(file.galley.elided);
    assert!(file.galley.rect.translate(file.pos.to_vec2()).right() <= screen.right());
}

#[test]
fn playback_shortcuts_step_speeds_pause_and_respect_keyboard_focus() {
    let dir = std::env::temp_dir().join(format!("ironav-gui-speed-keys-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = fixture(&dir, "shortcuts.y4m");
    let player = Arc::new(Player::open(&path, &OpenOptions {
        audio: AudioMode::Off, autoplay: false, ..Default::default()
    }).unwrap());
    let ctx = egui::Context::default();
    let mut app = PlayerApp::new(None, false, ctx.clone(), None);
    app.player = Some(player.clone());
    let mut text = String::new();
    let mut send = |events, editing| {
        let mut output = ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| {
            if editing {
                ui.text_edit_singleline(&mut text).request_focus();
            }
            app.playback_shortcuts(ui.ctx());
        });
        output.textures_delta.clear();
    };
    let tap = |key, modifiers| [true, false].map(|pressed| egui::Event::Key {
        key, physical_key: None, pressed, repeat: false, modifiers,
    });
    let plain = egui::Modifiers::NONE;

    let [press, release] = tap(egui::Key::J, plain);
    send(vec![press.clone(), press, release], false);
    assert_eq!(player.playback_rate(), -1.0, "holding a key does not skip a speed");
    send((0..5).flat_map(|_| tap(egui::Key::J, plain)).collect(), false);
    assert_eq!(player.playback_rate(), -4.0);

    send(tap(egui::Key::K, plain).to_vec(), false);
    let deadline = Instant::now() + Duration::from_secs(5);
    while player.state() == State::Buffering && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(player.state(), State::Paused);

    player.seek(2.0);
    send(tap(egui::Key::L, plain).to_vec(), false);
    assert_eq!(player.playback_rate(), 1.0);
    send(tap(egui::Key::L, plain).to_vec(), false);
    assert_eq!(player.playback_rate(), 1.5);
    send(tap(egui::Key::J, egui::Modifiers::CTRL).to_vec(), false);
    send(tap(egui::Key::J, plain).to_vec(), true);
    assert_eq!(player.playback_rate(), 1.5, "modified keys and text editing do not change the rate");
}

#[test]
fn rodeo_palettes_style_the_player_and_restore_the_saved_choice() {
    let ctx = egui::Context::default();
    let directory = tempfile::tempdir().unwrap();
    let preference = directory.path().join("theme");
    std::fs::write(&preference, "Removed theme").unwrap();
    let mut app = PlayerApp::new(None, false, ctx.clone(), Some(preference.clone()));
    assert_eq!(player_style::load_theme(None).name, "Monochrome");
    assert_eq!(app.theme.name, "Monochrome");
    assert_eq!(app.theme.accent, Color32::from_gray(138));
    assert_eq!(THEMES.len(), 16);

    for theme in THEMES {
        app.set_color_theme(theme);
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            assert_eq!(ui.visuals().panel_fill, theme.surface, "{}", theme.name);
            assert_eq!(ui.visuals().extreme_bg_color, theme.background, "{}", theme.name);
            assert_eq!(ui.visuals().error_fg_color, theme.danger, "{}", theme.name);
        });
        output.textures_delta.clear();
    }

    let light = THEMES.iter().find(|theme| theme.name == "GitHub Light").unwrap();
    app.set_color_theme(light);
    drop(app);
    let ctx = egui::Context::default();
    let reopened = PlayerApp::new(None, false, ctx.clone(), Some(preference));
    assert_eq!(reopened.theme.name, light.name);
    assert_eq!(reopened.theme.accent, Color32::from_rgb(9, 105, 218));
    let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
        assert!(!ui.visuals().dark_mode);
        assert_eq!(ui.visuals().panel_fill, Color32::from_rgb(246, 248, 250));
    });
    output.textures_delta.clear();
}

#[test]
fn the_video_dock_can_recolor_the_waterfall_during_a_pause() {
    use std::process::Command;
    use ironav_player::{AudioAnalysis, SPECTRUM_BANDS, SPECTRUM_HISTORY};

    if !Command::new("ffmpeg").arg("-version").output().is_ok_and(|status| status.status.success()) {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("audio-dock.mp4");
    let clip = Command::new("ffmpeg").args([
        "-v", "error", "-nostdin", "-f", "lavfi", "-i", "testsrc2=size=64x48:rate=10",
        "-f", "lavfi", "-i", "sine=frequency=1000:sample_rate=48000",
        "-t", "3", "-c:v", "libx264", "-c:a", "aac",
    ]).arg(&path).status().unwrap();
    assert!(clip.success());
    let player = Arc::new(Player::open(path.to_str().unwrap(), &OpenOptions {
        audio: AudioMode::Silent, hwaccel: "off".into(), autoplay: false, ..Default::default()
    }).unwrap());
    let ctx = egui::Context::default();
    let theme_file = folder.path().join("player-theme");
    let mut app = PlayerApp::new(None, false, ctx.clone(), Some(theme_file.clone()));
    app.player = Some(player.clone());
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(520.0, 320.0));
    let render = |app: &mut PlayerApp, events| {
        let mut picture_area = egui::Rect::NOTHING;
        let mut output = ctx.run_ui(egui::RawInput { screen_rect: Some(screen), events, ..Default::default() }, |ui| {
            app.menu_bar(ui);
            app.transport_panel(ui).show(ui, |ui| app.controls(ui));
            app.audio_dock(ui);
            egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
                picture_area = ui.max_rect();
            });
        });
        let uploads: Vec<_> = output.textures_delta.set.values()
            .flat_map(|deltas| deltas.iter().map(|delta| delta.image.clone()))
            .collect();
        output.textures_delta.clear();
        (output, picture_area, uploads)
    };
    let center_of = |output: &egui::FullOutput, label| {
        output.shapes.iter().find_map(|item| find_label(&item.shape, label))
            .map(|text| text.pos + text.galley.rect.center().to_vec2())
            .unwrap_or_else(|| panic!("{label} is visible"))
    };
    let click = |pos| {
        let mut events = vec![egui::Event::PointerMoved(pos)];
        events.extend([true, false].map(|pressed| egui::Event::PointerButton {
            pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE,
        }));
        events
    };
    let (menu, full_picture, _) = render(&mut app, Vec::new());
    let (view, _, _) = render(&mut app, click(center_of(&menu, "View")));
    let view = if view.shapes.iter().any(|item| find_label(&item.shape, "Audio tools").is_some()) {
        view
    } else {
        render(&mut app, Vec::new()).0
    };
    render(&mut app, click(center_of(&view, "Audio tools")));
    assert!(app.show_audio_tools, "the menu enabled the dock");
    let (docked, picture, _) = render(&mut app, Vec::new());
    let waterfall = center_of(&docked, "Waterfall");
    let scale_picker = center_of(&docked, "Jet");
    visible_label(&docked, "Waterfall");
    visible_label(&docked, "Jet");
    assert!(picture.width() < full_picture.width() && picture.width() >= 190.0, "{picture:?}");
    assert!(waterfall.x > picture.right() && screen.contains(waterfall));
    assert!(scale_picker.x > waterfall.x && screen.contains(scale_picker));

    player.play();
    let mut analysis = AudioAnalysis::default();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        player.audio_analysis(&mut analysis);
        if analysis.decay.is_some() && analysis.waveform.is_some() && player.frame().is_some_and(|frame| frame.time >= 0.1) {
            break;
        }
        assert!(Instant::now() < deadline, "video and waterfall did not advance together");
        std::thread::sleep(Duration::from_millis(5));
    }

    let image_of = |uploads: &[egui::ImageData], size| {
        uploads.iter().rev().find(|image| image.size() == size).map(|image| match image {
            egui::ImageData::Color(image) => image.clone(),
        })
    };
    player.pause();
    let (before, _, uploads) = render(&mut app, Vec::new());
    let size = [SPECTRUM_BANDS, SPECTRUM_HISTORY];
    let jet = image_of(&uploads, size).expect("the recorded rows were rendered in Jet");
    let (picker, _, _) = render(&mut app, click(center_of(&before, "Jet")));
    let picker = if picker.shapes.iter().any(|item| find_label(&item.shape, "Parula").is_some()) {
        picker
    } else {
        render(&mut app, Vec::new()).0
    };
    let (_, _, uploads) = render(&mut app, click(center_of(&picker, "Parula")));
    let (after, _, _) = render(&mut app, Vec::new());
    assert!(screen.contains(center_of(&after, "Parula")));
    let parula = image_of(&uploads, size).expect("selecting Parula recolors the paused rows");
    assert_ne!(jet.pixels, parula.pixels);
    let legend = image_of(&uploads, [256, 1]).expect("selection updates the legend");
    assert_eq!(legend.pixels[0], Color32::from_rgb(53, 42, 135));
    assert_eq!(legend.pixels[255], Color32::from_rgb(249, 251, 14));

    let mut plot = "Waterfall";
    let mut frame = after;
    for next in ["Waveform", "Spectrum", "Spectral decay", "EQ response"] {
        let (picker, _, _) = render(&mut app, click(center_of(&frame, plot)));
        let picker = if picker.shapes.iter().any(|item| find_label(&item.shape, next).is_some()) {
            picker
        } else {
            render(&mut app, Vec::new()).0
        };
        render(&mut app, click(center_of(&picker, next)));
        frame = render(&mut app, Vec::new()).0;
        assert!(screen.contains_rect(visible_label(&frame, next)));
        assert!(screen.contains_rect(visible_label(&frame, "Parula")));
        plot = next;
    }
    let scroll = vec![
        egui::Event::PointerMoved(egui::pos2(screen.right() - 24.0, picture.bottom() - 24.0)),
        egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, -4000.0),
            modifiers: egui::Modifiers::NONE,
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Wheel,
        },
    ];
    render(&mut app, scroll);
    let (effects, _, _) = render(&mut app, Vec::new());
    for label in ["EQ response", "Parula", "Compression", "Echo", "Reset audio"] {
        visible_label(&effects, label);
    }
    drop(app);
    let mut reopened = PlayerApp::new(None, false, ctx.clone(), Some(theme_file));
    reopened.player = Some(player);
    reopened.show_audio_tools = true;
    let (reopened, _, _) = render(&mut reopened, Vec::new());
    assert!(screen.contains(center_of(&reopened, "EQ response")));
    assert!(screen.contains(center_of(&reopened, "Parula")));
}

#[test]
fn audio_plot_fullscreen_uses_the_viewport_and_restores_player_controls() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audio.wav");
    let data_size = 16_000u32;
    let mut wav = b"RIFF".to_vec();
    wav.extend_from_slice(&(data_size + 36).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt \x10\0\0\0\x01\0\x01\0\x40\x1f\0\0\x80\x3e\0\0\x02\0\x10\0data");
    wav.extend_from_slice(&data_size.to_le_bytes());
    wav.resize(wav.len() + data_size as usize, 0);
    std::fs::write(&path, wav).unwrap();
    let player = Player::open(path.to_str().unwrap(), &OpenOptions {
        audio: AudioMode::Silent, autoplay: false, ..Default::default()
    }).unwrap();
    let ctx = egui::Context::default();
    let mut app = PlayerApp::new(None, false, ctx.clone(), None);
    app.player = Some(Arc::new(player));
    app.show_agent = true;
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(520.0, 320.0));
    let render = |app: &mut PlayerApp, events| {
        let mut output = ctx.run_ui(egui::RawInput { screen_rect: Some(screen), events, ..Default::default() }, |ui| {
            eframe::App::ui(app, ui, &mut eframe::Frame::_new_kittest());
        });
        output.textures_delta.clear();
        output
    };
    let click = |pos| {
        let mut events = vec![egui::Event::PointerMoved(pos)];
        events.extend([true, false].map(|pressed| egui::Event::PointerButton {
            pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE,
        }));
        events
    };
    let fullscreen_command = |output: &egui::FullOutput, expected| {
        assert!(output.viewport_output[&egui::ViewportId::ROOT].commands.iter().any(|command| {
            matches!(command, egui::ViewportCommand::Fullscreen(value) if *value == expected)
        }));
    };
    let normal = render(&mut app, Vec::new());
    for label in ["Repeat", "Enable MCP", "Waterfall", "Jet"] {
        visible_label(&normal, label);
    }
    let view = render(&mut app, click(visible_label(&normal, "View").center()));
    let view = if view.shapes.iter().any(|item| find_label(&item.shape, "Full screen audio plot").is_some()) {
        view
    } else {
        render(&mut app, Vec::new())
    };
    let enter = render(&mut app, click(visible_label(&view, "Full screen audio plot").center()));
    fullscreen_command(&enter, true);
    assert!(app.plot_fullscreen);

    let expanded = render(&mut app, Vec::new());
    visible_label(&expanded, "Waterfall");
    let axis = visible_label(&expanded, "16k Hz");
    assert!(axis.bottom() > screen.bottom() - 24.0);
    assert!(expanded.shapes.iter().all(|item| find_label(&item.shape, "Repeat").is_none()));
    let esc = || vec![egui::Event::Key {
        key: egui::Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE,
    }];
    let restored = render(&mut app, esc());
    fullscreen_command(&restored, false);
    visible_label(&restored, "Repeat");
    visible_label(&restored, "Waterfall");
    assert!(!app.plot_fullscreen && app.show_agent);

    app.toggle_fullscreen();
    app.set_plot_fullscreen(true);
    render(&mut app, Vec::new());
    let restored = render(&mut app, esc());
    fullscreen_command(&restored, true);
    assert!(!app.plot_fullscreen && app.fullscreen);
}

#[test]
fn the_primary_transport_button_stays_centered_at_minimum_width() {
    fn play_rect(shape: &egui::Shape, accent: Color32) -> Option<egui::Rect> {
        match shape {
            egui::Shape::Rect(rect) if rect.fill == accent && (rect.rect.width() - PLAY_WIDTH).abs() < 2.0 => Some(rect.rect),
            egui::Shape::Vec(shapes) => shapes.iter().find_map(|shape| play_rect(shape, accent)),
            _ => None,
        }
    }

    let dir = std::env::temp_dir().join(format!("ironav-gui-transport-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = fixture(&dir, "paused.y4m");
    let player = Player::open(&path, &OpenOptions { audio: AudioMode::Off, autoplay: false, ..Default::default() }).unwrap();
    let ctx = egui::Context::default();
    let mut app = PlayerApp::new(None, false, ctx.clone(), None);
    app.player = Some(Arc::new(player));
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(520.0, 320.0));
    let mut output = ctx.run_ui(egui::RawInput { screen_rect: Some(screen), ..Default::default() }, |ui| {
        app.transport_panel(ui).show(ui, |ui| app.controls(ui));
    });
    output.textures_delta.clear();
    let play = output.shapes.iter().find_map(|item| play_rect(&item.shape, app.theme.accent)).expect("the primary control is visible");
    assert!((play.center().x - screen.center().x).abs() <= 1.0, "play button at {play:?}");
    let label = |s| output.shapes.iter().find_map(|item| find_label(&item.shape, s));
    let volume = label("100%")
        .or_else(|| label("100").filter(|_| label("%").is_some()))
        .expect("the editable volume percentage is visible");
    let repeat = label("Repeat").expect("the Repeat button is visible");
    let rate = label("1×").expect("the speed button shows the current rate");
    let arrows: Vec<_> = output.shapes.iter().filter_map(|item| {
        let egui::Shape::Path(path) = &item.shape else { return None };
        let [top, tip, bottom] = path.points.as_slice() else { return None };
        if path.closed && tip.x < rate.pos.x && top.x < rate.pos.x && tip.y > play.top() && tip.y < play.bottom() {
            Some(tip.x > bottom.x)
        } else {
            None
        }
    }).collect();
    assert_eq!(arrows, [false, false, true, true], "both speed buttons show double arrows");
    assert!(rate.galley.rect.translate(rate.pos.to_vec2()).right() < play.left());
    assert!(play.right() < volume.pos.x);
    assert!(volume.pos.x < repeat.pos.x);
    assert!(screen.contains_rect(visible_label(&output, "Repeat")));
}
