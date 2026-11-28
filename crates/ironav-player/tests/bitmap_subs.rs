//! A bitmap subtitle track (here a Blu-ray `.sup` file) loaded as the player loads it, and the events it
//! gives at times of the playback clock, in any order as after a seek.

use ironav_codec::bitmapsub::testing::*;
use ironav_player::SubtitleTrack;
use std::sync::atomic::AtomicBool;

#[test]
fn a_bitmap_track_gives_the_event_for_any_time_and_the_next_change() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("player-bitmap-subs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let object = |id, x| PgsObject { id, x, y: 100, w: 16, h: 8, pixels: vec![1; 128], crop: None, forced: false };
    let canvas = (1280, 720);
    let palette = [[1, 235, 128, 128, 255]];
    let sets = vec![
        (90_000, pgs_display_set(canvas, 2, &palette, &[object(1, 10)], 0)),
        (180_000, pgs_display_set(canvas, 0, &palette, &[object(2, 20)], 0)),
        (270_000, pgs_clear_set(canvas)),
        (450_000, pgs_display_set(canvas, 2, &palette, &[object(1, 30)], 0)),
        (540_000, pgs_clear_set(canvas)),
    ];
    let file = dir.join("in.sup");
    std::fs::write(&file, sup_file(&sets)).unwrap();
    let track = SubtitleTrack::load(&file, None, &AtomicBool::new(false)).unwrap();
    assert!(track.is_bitmap());
    assert_eq!(track.at(1.5), (String::new(), None), "a bitmap track has no text");
    let at = |t: f64| {
        let (shown, next) = track.bitmap_at(t);
        (shown.map(|(n, b)| (n, b.rects[0].x)), next)
    };
    assert_eq!(at(0.0), (None, Some(1.0)));
    assert_eq!(at(1.0), (Some((0, 10)), Some(2.0)));
    assert_eq!(at(2.5), (Some((1, 20)), Some(3.0)));
    assert_eq!(at(3.5), (None, Some(5.0)));
    assert_eq!(at(5.5), (Some((2, 30)), Some(6.0)));
    // back to an earlier time, as after a seek
    assert_eq!(at(1.99), (Some((0, 10)), Some(2.0)));
    assert_eq!(at(6.0), (None, None));
    // a track that is cancelled stops loading
    assert!(SubtitleTrack::load(&file, None, &AtomicBool::new(true)).is_err());
}
