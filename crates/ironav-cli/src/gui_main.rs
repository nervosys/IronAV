//! `ironav-gui` — the native windows, kept out of `ironav` so that command-line and agent calls
//! do not pay to load the graphics stack (about 70 ms of process start-up on Windows).
//! `ironav gui [--play] [file]` runs this program.

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let player = match args.iter().position(|a| a == "--play" || a == "--player") {
        Some(i) => {
            args.remove(i);
            true
        }
        None => false,
    };
    let file = args.into_iter().next();
    let result = if player { ironav_gui::run_player(file) } else { ironav_gui::run(file) };
    if let Err(e) = result {
        println!("{}", serde_json::to_string(&serde_json::json!({ "ok": false, "error": e })).unwrap_or_default());
        std::process::exit(e.code.exit_code());
    }
}
