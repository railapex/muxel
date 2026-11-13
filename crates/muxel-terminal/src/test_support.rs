//! Switches for test binaries. Only built with the `test-support` feature,
//! which muxel enables for its own tests and never for the app.

use std::sync::atomic::{AtomicBool, Ordering};

static SPAWNS_REFUSED: AtomicBool = AtomicBool::new(false);

/// Make every later terminal launch in this process fail before a PTY or a
/// child process exists. An offscreen render calls this first, so no frame can
/// show a real shell, prompt or path. There is no way back.
pub fn refuse_spawns() {
    SPAWNS_REFUSED.store(true, Ordering::SeqCst);
}

pub(crate) fn spawns_refused() -> bool {
    SPAWNS_REFUSED.load(Ordering::SeqCst)
}
