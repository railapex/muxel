//! Scenes: muxel's real window drawn offscreen from a scene file and saved as
//! one PNG per frame, so a change can be reviewed as pictures beside its design.
//!
//! A scene names a workspace fixture and what each pane shows. The runner
//! builds that state directly, draws, reads the frame back and crops it to a
//! named region. `scene.rs` describes the file.
//!
//! Run it alone and by name, after a build:
//!
//! ```text
//! MUXEL_SCENES=<scene file or folder> MUXEL_SCENE_OUT=<folder> \
//!   cargo test -p muxel --bin muxel app::scenes::render -- --ignored --exact --nocapture
//! ```
//!
//! `MUXEL_SCENE_REPEAT=n` draws every frame n times. The run refuses every
//! terminal launch for the rest of its process, so no frame can show a live
//! shell. Only Windows reads frames back; a GPUI build without the readback,
//! and every other platform, prints that it skipped and passes.

#[cfg(windows)]
mod runner;
#[cfg(windows)]
mod scene;

#[test]
#[ignore = "draws scenes offscreen; run by name with MUXEL_SCENES and MUXEL_SCENE_OUT set"]
fn render() {
    #[cfg(windows)]
    runner::run();
    #[cfg(not(windows))]
    eprintln!("scenes: skipped, this platform has no frame readback");
}
