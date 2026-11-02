//! The temporary name an output is written under is created exclusively.

use ironav_core::ErrorCode;
use ironav_engine::{plan, run, Job};
use std::path::PathBuf;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-partial-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn y4m() -> Vec<u8> {
    let mut f = b"YUV4MPEG2 W4 H2 F30:1 Ip A1:1 C420jpeg\nFRAME\n".to_vec();
    f.extend_from_slice(&[7u8; 12]);
    f
}

#[test]
fn a_leftover_partial_is_refused_and_not_touched() {
    let d = scratch("left");
    let (a, b, p) = (d.join("in.y4m"), d.join("out.y4m"), d.join("out.y4m.partial"));
    std::fs::write(&a, y4m()).unwrap();
    std::fs::write(&p, b"someone else's").unwrap();
    let job = Job { input: a.display().to_string(), output: b.display().to_string(), hwaccel: "off".into(), ..Job::default() };
    let e = plan(&job).unwrap_err();
    assert_eq!(e.code, ErrorCode::OutputExists);
    assert!(e.hint.unwrap().contains("remove it"));
    let e = run(&job, &mut |_| {}).unwrap_err();
    assert_eq!(e.code, ErrorCode::OutputExists);
    assert_eq!(std::fs::read(&p).unwrap(), b"someone else's");
    assert!(!b.exists());
    // once it is gone the job runs, and leaves no temporary file
    std::fs::remove_file(&p).unwrap();
    run(&job, &mut |_| {}).unwrap();
    assert!(b.exists() && !p.exists());
}

#[test]
fn a_planted_link_is_not_written_through() {
    let d = scratch("link");
    let (a, b, p, victim) = (d.join("in.y4m"), d.join("out.y4m"), d.join("out.y4m.partial"), d.join("victim.txt"));
    std::fs::write(&a, y4m()).unwrap();
    std::fs::write(&victim, b"precious").unwrap();
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(&victim, &p).is_ok();
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_file(&victim, &p).is_ok();
    if !made {
        eprintln!("skipped: cannot create symbolic links here");
        return;
    }
    let job = Job { input: a.display().to_string(), output: b.display().to_string(), hwaccel: "off".into(), ..Job::default() };
    assert_eq!(run(&job, &mut |_| {}).unwrap_err().code, ErrorCode::OutputExists);
    assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
}

#[test]
fn a_leftover_clip_partial_is_refused_by_the_muxer() {
    let d = scratch("mux");
    let p = d.join("clip.y4m.partial");
    std::fs::write(&p, b"x").unwrap();
    let e = ironav_formats::create_muxer_new(&p, "y4m", None).err().expect("refused");
    assert_eq!(e.code, ErrorCode::OutputExists);
    assert_eq!(std::fs::read(&p).unwrap(), b"x");
    std::fs::remove_file(&p).unwrap();
    assert!(ironav_formats::create_muxer_new(&p, "y4m", None).is_ok());
}
