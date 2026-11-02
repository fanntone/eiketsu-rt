//! The browser build's API, run natively: the page's loop (16 ms slices, input
//! between slices, picture and sound pulled after each) must play the game like
//! the desktop build. Golden hashes are the ones checked by hand for
//! harness/01_password_title.txt and harness/02_new_game_prologue.txt.

use eiketsu_web::{Run, Web};
use std::path::PathBuf;

fn game_copy(tag: &str) -> PathBuf {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    assert!(src.join("REKO3.COM").exists(), "game data missing at {}", src.display());
    let dst = std::env::temp_dir().join(format!("eiketsu-web-{tag}-{}", std::process::id()));
    std::fs::remove_dir_all(&dst).ok();
    std::fs::create_dir_all(&dst).unwrap();
    for e in std::fs::read_dir(&src).unwrap().flatten() {
        if e.file_type().unwrap().is_file() {
            std::fs::copy(e.path(), dst.join(e.file_name())).unwrap();
        }
    }
    dst
}

/// Run frames of 16 ms until `until_ms`, pulling picture and sound like the page.
fn play_until(w: &mut Web, until_ms: u64, sound: &mut f32) {
    while w.m.now_ms() < until_ms {
        assert_eq!(w.run(16), Run::Running, "{}", w.error);
        assert!(!w.frame().is_empty());
        let peak = w.audio(48_000, 768).iter().fold(0f32, |a, s| a.max(s.abs()));
        *sound = sound.max(peak);
    }
}

fn click(w: &mut Web, x: i32, y: i32, sound: &mut f32) {
    w.mouse(x, y, 0);
    let t = w.m.now_ms();
    play_until(w, t + 30, sound);
    w.mouse(x, y, 1);
    play_until(w, t + 150, sound);
    w.mouse(x, y, 0);
}

#[test]
fn page_loop_plays_title_and_prologue() {
    let game = game_copy("api");
    let mut w = Web::new(game.clone()).unwrap();
    let mut sound = 0f32;
    play_until(&mut w, 9000, &mut sound);
    // Password screen: any key + Enter.
    w.key(0x1e, true);
    w.key(0x1e, false);
    play_until(&mut w, 9600, &mut sound);
    w.key(0x1c, true);
    w.key(0x1c, false);
    play_until(&mut w, 14000, &mut sound);
    assert_eq!(w.size(), (640, 480));
    assert_eq!(w.m.screen_hash(), 0x093a158fdd5a2c4b, "title menu");
    // 開始新遊戲, given in picture pixels.
    click(&mut w, 320, 187, &mut sound);
    play_until(&mut w, 17000, &mut sound);
    std::fs::remove_dir_all(&game).ok();
    assert_eq!(w.m.screen_hash(), 0xf4d72a9cca685e35, "prologue chapter title");
    // The title screen is silent; the prologue starts the music.
    assert!(sound > 0.01, "no sound from the prologue music (peak {sound}, {} OPL writes)", w.m.mem.opl.writes);
}

/// Input must land where it is aimed even if the page never read the picture
/// (the mouse is given in picture pixels of the current mode).
#[test]
fn mouse_lands_without_reading_the_picture() {
    let game = game_copy("nopic");
    let mut w = Web::new(game.clone()).unwrap();
    let run_to = |w: &mut Web, ms: u64| while w.m.now_ms() < ms { assert_eq!(w.run(16), Run::Running, "{}", w.error) };
    run_to(&mut w, 9000);
    w.key(0x1e, true);
    w.key(0x1e, false);
    run_to(&mut w, 9600);
    w.key(0x1c, true);
    w.key(0x1c, false);
    run_to(&mut w, 14000);
    w.mouse(320, 187, 0);
    run_to(&mut w, 14030);
    w.mouse(320, 187, 1);
    run_to(&mut w, 14150);
    w.mouse(320, 187, 0);
    run_to(&mut w, 17000);
    std::fs::remove_dir_all(&game).ok();
    assert_eq!(w.m.screen_hash(), 0xf4d72a9cca685e35, "开始新遊戲 was not clicked");
}
