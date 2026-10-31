//! `ironav play`: the terminal player. On a terminal it is a full-screen TUI that draws the
//! picture with half-block characters in true colour; otherwise it is a JSON-lines
//! conversation, one command per stdin line and one JSON envelope per reply on stdout, which is
//! what an agent drives.

use ironav_core::{Error, ErrorCode, Result};
use ironav_player::{rgb_frame, to_png, OpenOptions, Player, State};
use serde_json::{json, Value};
use std::io::{BufRead, IsTerminal, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct PlayArgs {
    pub path: String,
    pub opts: OpenOptions,
    /// `Some(true)` forces the TUI, `Some(false)` forces JSON lines.
    pub tui: Option<bool>,
    /// JSON mode: keep playing to the end after stdin closes instead of exiting.
    pub until_end: bool,
}

pub fn run(a: PlayArgs) -> Result<()> {
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    match a.tui.unwrap_or(interactive) {
        true if interactive => tui(a),
        true => Err(Error::invalid_arg("--tui needs a terminal on stdin and stdout").hint("drop --tui to use the JSON-lines mode")),
        false => json_lines(a),
    }
}

fn envelope(r: Result<Value>) -> Value {
    match r {
        Ok(v) => json!({ "ok": true, "result": v }),
        Err(e) => json!({ "ok": false, "error": e }),
    }
}

fn json_lines(a: PlayArgs) -> Result<()> {
    let pl = Player::open(&a.path, &a.opts)?;
    let mut out = std::io::stdout().lock();
    let mut first = pl.status();
    first["event"] = json!("opened");
    writeln!(out, "{}", envelope(Ok(first)))?;
    out.flush()?;
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // A bare word is a command with no arguments: `pause`, `status`.
        let req: std::result::Result<Value, _> = if line.starts_with('{') { serde_json::from_str(line) } else { Ok(json!({ "cmd": line })) };
        let reply = match req {
            Err(e) => envelope(Err(Error::invalid_arg(format!("not valid JSON: {e}")).hint("send one JSON object per line, e.g. {\"cmd\":\"status\"}"))),
            Ok(r) if matches!(r.get("cmd").and_then(Value::as_str), Some("quit" | "stop" | "exit")) => {
                writeln!(out, "{}", envelope(Ok(json!({ "stopped": true }))))?;
                return Ok(());
            }
            Ok(r) => envelope(pl.command(&r)),
        };
        writeln!(out, "{reply}")?;
        out.flush()?;
    }
    if a.until_end {
        while !matches!(pl.state(), State::Ended | State::Error) {
            std::thread::sleep(Duration::from_millis(20));
        }
        writeln!(out, "{}", envelope(Ok(pl.status())))?;
    }
    Ok(())
}

// ---- the terminal UI ------------------------------------------------------------------------

use ironav_player::AudioMode;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

struct Ui {
    msg: Option<(String, Instant)>,
    /// The picture last drawn and the size it was drawn at, to avoid redoing the conversion.
    cache: Option<(usize, u32, u32, u32, u32, Vec<u8>)>,
    bar: Rect,
}

fn fmt_time(s: f64) -> String {
    let s = s.max(0.0) as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

fn tui(a: PlayArgs) -> Result<()> {
    let pl = Player::open(&a.path, &a.opts)?;
    let mut term = ratatui::init();
    let _ = ratatui::crossterm::execute!(std::io::stdout(), ratatui::crossterm::event::EnableMouseCapture);
    let mut ui = Ui { msg: None, cache: None, bar: Rect::default() };
    let result = (|| -> Result<()> {
        loop {
            term.draw(|f| draw(f, &pl, &mut ui))?;
            if event::poll(Duration::from_millis(16))? {
                match event::read()? {
                    Event::Key(k) if k.kind != KeyEventKind::Release => {
                        if key(&pl, &mut ui, k.code, k.modifiers) {
                            return Ok(());
                        }
                    }
                    Event::Mouse(m) if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) && ui.bar.width > 0 && m.row == ui.bar.y && m.column >= ui.bar.x && m.column < ui.bar.x + ui.bar.width => {
                        if let Some(d) = pl.info().duration {
                            pl.seek((m.column - ui.bar.x) as f64 / ui.bar.width as f64 * d);
                        }
                    }
                    _ => {}
                }
            }
        }
    })();
    let _ = ratatui::crossterm::execute!(std::io::stdout(), ratatui::crossterm::event::DisableMouseCapture);
    ratatui::restore();
    result
}

/// Handle a key; true means quit.
fn key(pl: &Player, ui: &mut Ui, code: KeyCode, mods: KeyModifiers) -> bool {
    let say = |ui: &mut Ui, m: String| ui.msg = Some((m, Instant::now()));
    let vol = |pl: &Player| pl.status()["volume"].as_f64().unwrap_or(1.0) as f32;
    match code {
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => return true,
        KeyCode::Char(' ') | KeyCode::Char('k') => pl.toggle(),
        KeyCode::Left => pl.seek_by(-5.0),
        KeyCode::Right => pl.seek_by(5.0),
        KeyCode::Char('j') => pl.seek_by(-10.0),
        KeyCode::Char('l') => pl.seek_by(10.0),
        KeyCode::Home => pl.seek(0.0),
        KeyCode::Up | KeyCode::Char('+') | KeyCode::Char('=') => {
            let v = (vol(pl) + 0.05).min(2.0);
            pl.set_volume(v);
            say(ui, format!("volume {}%", (v * 100.0).round()));
        }
        KeyCode::Down | KeyCode::Char('-') => {
            let v = (vol(pl) - 0.05).max(0.0);
            pl.set_volume(v);
            say(ui, format!("volume {}%", (v * 100.0).round()));
        }
        KeyCode::Char('m') => {
            let on = !pl.status()["muted"].as_bool().unwrap_or(false);
            pl.set_muted(on);
            say(ui, if on { "muted".into() } else { "unmuted".into() });
        }
        KeyCode::Char('r') => {
            let on = !pl.status()["looping"].as_bool().unwrap_or(false);
            pl.set_looping(on);
            say(ui, format!("repeat {}", if on { "on" } else { "off" }));
        }
        KeyCode::Char('s') => {
            let name = format!("ironav-{}.png", pl.position() as u64);
            let m = match pl.snapshot_png(None, None) {
                Ok((_, _, png)) => match std::fs::write(&name, png) {
                    Ok(()) => format!("saved {name}"),
                    Err(e) => format!("cannot write {name}: {e}"),
                },
                Err(e) => e.message,
            };
            say(ui, m);
        }
        KeyCode::Char(c @ '0'..='9') => {
            if let Some(d) = pl.info().duration {
                pl.seek(d * (c as u8 - b'0') as f64 / 10.0);
            }
        }
        _ => {}
    }
    false
}

fn draw(f: &mut Frame, pl: &Player, ui: &mut Ui) {
    let [video, info, bar, help] = Layout::vertical([Constraint::Min(1), Constraint::Length(1), Constraint::Length(1), Constraint::Length(1)]).areas(f.area());
    let st = pl.status();
    let state = pl.state();
    let pos = pl.position();

    if pl.has_video() {
        draw_video(f, pl, ui, video);
    } else {
        draw_meter(f, &st, video);
    }

    let dur = pl.info().duration;
    let icon = match state {
        State::Playing => "▶",
        State::Paused => "⏸",
        State::Buffering => "…",
        State::Ended => "■",
        State::Error => "✖",
    };
    let mut line = vec![
        Span::styled(format!(" {icon} "), Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(format!("{} / {}", fmt_time(pos), dur.map_or("?".into(), fmt_time))),
        Span::raw(format!("   vol {}%{}", (st["volume"].as_f64().unwrap_or(1.0) * 100.0).round(), if st["muted"] == true { " (muted)" } else { "" })),
    ];
    if let Some(v) = st["video"].as_object() {
        line.push(Span::raw(format!("   {} {}x{} via {}  {} dropped", v["codec"].as_str().unwrap_or(""), v["width"], v["height"], v["decoder"].as_str().unwrap_or(""), v["frames_skipped"])));
    }
    if st["looping"] == true {
        line.push(Span::raw("   repeat"));
    }
    if let Some((m, t)) = &ui.msg {
        if t.elapsed() < Duration::from_secs(2) {
            line.push(Span::styled(format!("   {m}"), Style::new().fg(Color::Yellow)));
        }
    }
    if let Some(e) = st["error"]["message"].as_str() {
        line.push(Span::styled(format!("   {e}"), Style::new().fg(Color::Red)));
    }
    f.render_widget(Paragraph::new(Line::from(line)), info);

    ui.bar = bar;
    let frac = dur.filter(|d| *d > 0.0).map_or(0.0, |d| (pos / d).clamp(0.0, 1.0));
    let filled = (frac * bar.width as f64).round() as usize;
    let track: String = (0..bar.width as usize).map(|i| if i < filled { '━' } else { '─' }).collect();
    f.render_widget(Paragraph::new(track).style(Style::new().fg(Color::Cyan)), bar);

    let warn = st["warnings"].as_array().and_then(|w| w.first()).and_then(Value::as_str).unwrap_or("");
    let text = if warn.is_empty() { " space play/pause  ←/→ 5s  j/l 10s  ↑/↓ volume  m mute  r repeat  0-9 jump  s snapshot  q quit".to_string() } else { format!(" ! {warn}") };
    f.render_widget(Paragraph::new(text).style(Style::new().fg(Color::DarkGray)), help);
}

fn draw_video(f: &mut Frame, pl: &Player, ui: &mut Ui, area: Rect) {
    let Some(fr) = pl.frame() else { return };
    let key = Arc::as_ptr(&fr) as usize;
    let (mw, mh) = (area.width as u32, area.height as u32 * 2);
    let fresh = !matches!(&ui.cache, Some((k, w, h, ..)) if *k == key && *w == mw && *h == mh);
    if fresh {
        let (w, h, rgb) = rgb_frame(&fr.frame, Some(mw), Some(mh));
        ui.cache = Some((key, mw, mh, w, h, rgb));
    }
    let Some((_, _, _, w, h, rgb)) = &ui.cache else { return };
    let (cols, rows) = (*w as u16, (*h as u16).div_ceil(2));
    let (x0, y0) = (area.x + area.width.saturating_sub(cols) / 2, area.y + area.height.saturating_sub(rows) / 2);
    let px = |x: u32, y: u32| {
        let i = ((y.min(h - 1)) * w + x) as usize * 3;
        Color::Rgb(rgb[i], rgb[i + 1], rgb[i + 2])
    };
    let buf = f.buffer_mut();
    for cy in 0..rows {
        for cx in 0..cols {
            if let Some(cell) = buf.cell_mut((x0 + cx, y0 + cy)) {
                cell.set_char('▀').set_fg(px(cx as u32, cy as u32 * 2)).set_bg(px(cx as u32, cy as u32 * 2 + 1));
            }
        }
    }
}

/// For files without a picture: a level meter, which also shows that sound is being produced.
fn draw_meter(f: &mut Frame, st: &Value, area: Rect) {
    let level = st["audio"]["level"].as_f64().unwrap_or(0.0).clamp(0.0, 1.0);
    let name = st["path"].as_str().unwrap_or("");
    let w = area.width.saturating_sub(8) as usize;
    let n = (level.sqrt() * w as f64).round() as usize;
    let meter: String = (0..w).map(|i| if i < n { '█' } else { '░' }).collect();
    let lines = vec![
        Line::from(name.to_string()).centered(),
        Line::from(format!("{} Hz, {} ch, {}", st["audio"]["sample_rate"], st["audio"]["channels"], st["audio"]["device"].as_str().unwrap_or(""))).centered(),
        Line::from(""),
        Line::from(Span::styled(meter, Style::new().fg(Color::Green))).centered(),
    ];
    let y = area.y + area.height.saturating_sub(lines.len() as u16) / 2;
    f.render_widget(Paragraph::new(lines), Rect { y, height: area.height.saturating_sub(y - area.y), ..area });
}

/// Parse `ironav play` arguments (after the word `play`).
pub fn parse(a: &mut crate::Args) -> Result<PlayArgs> {
    let mut opts = OpenOptions::default();
    if let Some(m) = a.flag(&["--audio"])? {
        opts.audio = match m.as_str() {
            "auto" => AudioMode::Auto,
            "silent" | "none" => AudioMode::Silent,
            "off" => AudioMode::Off,
            other => return Err(Error::new(ErrorCode::InvalidArgument, format!("unknown --audio '{other}'")).alternatives(["auto", "silent", "off"])),
        };
    }
    if let Some(h) = a.flag(&["--hwaccel"])? {
        opts.hwaccel = h;
    }
    let num = |name: &str, v: Option<String>| -> Result<Option<f64>> {
        v.map(|s| s.parse::<f64>().map_err(|_| Error::invalid_arg(format!("{name} must be a number, got '{s}'")))).transpose()
    };
    if let Some(s) = num("--start", a.flag(&["--start", "-ss"])?)? {
        opts.start = s;
    }
    if let Some(v) = num("--volume", a.flag(&["--volume"])?)? {
        opts.volume = v as f32;
    }
    opts.looping = a.switch(&["--loop"]);
    opts.autoplay = !a.switch(&["--paused"]);
    let tui = if a.switch(&["--tui"]) { Some(true) } else if a.switch(&["--json"]) { Some(false) } else { None };
    let until_end = a.switch(&["--until-end"]);
    let path = a.positional("file to play")?;
    if let Some(extra) = a.rest.first() {
        return Err(Error::invalid_arg(format!("unexpected argument '{extra}'")));
    }
    Ok(PlayArgs { path, opts, tui, until_end })
}

#[allow(dead_code)]
fn _png_is_used(w: u32, h: u32, rgb: &[u8]) -> Result<Vec<u8>> {
    to_png(w, h, rgb)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn y4m() -> String {
        let p = std::env::temp_dir().join(format!("ironav-tui-{}.y4m", std::process::id()));
        let mut f = b"YUV4MPEG2 W64 H48 F10:1 Ip C420jpeg\n".to_vec();
        for _ in 0..20 {
            f.extend(b"FRAME\n");
            f.extend(vec![81u8; 64 * 48]); // red-ish
            f.extend(vec![90u8; 32 * 24]);
            f.extend(vec![240u8; 32 * 24]);
        }
        std::fs::write(&p, f).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn the_screen_shows_the_picture_and_the_transport_line_and_keys_drive_the_player() {
        let opts = OpenOptions { audio: AudioMode::Off, autoplay: false, ..Default::default() };
        let pl = Player::open(&y4m(), &opts).unwrap();
        let mut term = Terminal::new(TestBackend::new(110, 14)).unwrap();
        let mut ui = Ui { msg: None, cache: None, bar: Rect::default() };
        let t = Instant::now();
        let screen = loop {
            term.draw(|f| draw(f, &pl, &mut ui)).unwrap();
            let b = term.backend().buffer().clone();
            if b.content().iter().any(|c| c.symbol() == "▀") || t.elapsed() > Duration::from_secs(5) {
                break b;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let reds = screen.content().iter().filter(|c| c.symbol() == "▀" && matches!(c.fg, Color::Rgb(r, g, b) if r > 240 && g < 15 && b < 15)).count();
        assert!(reds > 100, "the red picture is drawn in true colour, got {reds} red cells");
        let text: String = screen.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("0:00 / 0:02"), "{text}");
        assert!(text.contains("quit"), "the key help is visible");

        assert!(!key(&pl, &mut ui, KeyCode::Char(' '), KeyModifiers::NONE), "space does not quit");
        assert!(pl.status()["state"] != "paused", "space starts playback");
        assert!(key(&pl, &mut ui, KeyCode::Char('q'), KeyModifiers::NONE), "q quits");
    }
}
