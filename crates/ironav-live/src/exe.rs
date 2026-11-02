//! Where the programs IronAV starts (yt-dlp, arecord) come from. A bare name is looked up on `PATH`
//! and the absolute path found is what runs and what is reported, so a hint or a plan can say which
//! program was used.

use ironav_core::{Error, ErrorCode, Result};
use std::path::{Path, PathBuf};

/// The first file called `name` in a `PATH` directory (with `.exe` added on Windows).
pub fn locate(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let names: Vec<String> = if cfg!(windows) && Path::new(name).extension().is_none() { vec![format!("{name}.exe"), name.to_string()] } else { vec![name.to_string()] };
    std::env::split_paths(&path).filter(|d| d.is_absolute()).flat_map(|d| names.iter().map(move |n| d.join(n))).find(|p| p.is_file())
}

/// yt-dlp: the absolute path in `IRONAV_YTDLP` (a relative one is refused: it would depend on where
/// IronAV was started), else the one found on `PATH`.
pub fn ytdlp() -> Result<PathBuf> {
    match std::env::var_os("IRONAV_YTDLP").filter(|v| !v.is_empty()) {
        Some(v) => {
            let p = PathBuf::from(v);
            if !p.is_absolute() {
                return Err(Error::invalid_arg(format!("IRONAV_YTDLP must be an absolute path, got '{}'", p.display())).hint("the operator should set it to the full path of yt-dlp"));
            }
            Ok(p)
        }
        None => locate("yt-dlp").ok_or_else(|| {
            Error::new(ErrorCode::NotFound, "could not find yt-dlp on PATH").hint("install yt-dlp (https://github.com/yt-dlp/yt-dlp), or set IRONAV_YTDLP to its absolute path")
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn found_programs_have_absolute_paths_and_unknown_ones_are_none() {
        assert!(locate("ironav-no-such-program-xyz").is_none());
        if let Some(p) = locate(if cfg!(windows) { "cmd" } else { "sh" }) {
            assert!(p.is_absolute() && p.is_file());
        }
    }
}
