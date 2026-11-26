use super::*;

#[test]
fn a_finished_open_is_discarded_when_a_newer_selection_is_pending() {
    let dir = std::env::temp_dir().join(format!("ironav-gui-selection-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let fixture = |name: &str| {
        let path = dir.join(name);
        let mut bytes = b"YUV4MPEG2 W64 H48 F10:1 Ip A1:1 C420jpeg\n".to_vec();
        bytes.extend(b"FRAME\n");
        bytes.extend(vec![128; 64 * 48 * 3 / 2]);
        std::fs::write(&path, bytes).unwrap();
        path.display().to_string()
    };
    let old = fixture("old.y4m");
    let latest = fixture("latest.y4m");
    let p = Player::open(&old, &OpenOptions { audio: AudioMode::Off, autoplay: false, ..Default::default() }).unwrap();

    let mut app = PlayerApp::new(None, false, egui::Context::default());
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
    fn find_label<'a>(shape: &'a egui::Shape, label: &str) -> Option<&'a egui::epaint::TextShape> {
        match shape {
            egui::Shape::Text(text) if text.galley.job.text == label => Some(text),
            egui::Shape::Vec(shapes) => shapes.iter().find_map(|shape| find_label(shape, label)),
            _ => None,
        }
    }

    let ctx = egui::Context::default();
    let mut app = PlayerApp::new(None, false, ctx.clone());
    app.path = format!("{}720p60.mp4", "playback-diagnostic-".repeat(10));
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(520.0, 320.0));
    let mut output = ctx.run_ui(egui::RawInput { screen_rect: Some(screen), ..Default::default() }, |ui| app.menu_bar(ui));
    output.textures_delta.clear();
    let label = |s| output.shapes.iter().find_map(|item| find_label(&item.shape, s)).expect("label is visible in the player header");
    let file = label(&app.path);
    assert!(label("File").pos.x < label("View").pos.x);
    assert!(label("View").pos.x < label("Subtitles").pos.x);
    assert!(label("Subtitles").pos.x < file.pos.x);
    assert!(file.galley.elided);
    assert!(file.galley.rect.translate(file.pos.to_vec2()).right() <= screen.right());
}
