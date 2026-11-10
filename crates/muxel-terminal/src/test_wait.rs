//! Bounded waits for tests on threads and channels, so a lost answer fails
//! the test with what it was waiting for instead of hanging CI.

use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(15);

/// Poll `future` until it is ready.
pub(crate) fn settle<F: std::future::Future>(future: F, what: &str) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let std::task::Poll::Ready(done) = future.as_mut().poll(&mut cx) {
            return done;
        }
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Join `thread` once it has finished.
pub(crate) fn joined<T>(thread: std::thread::JoinHandle<T>, what: &str) -> T {
    let deadline = Instant::now() + PATIENCE;
    while !thread.is_finished() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(1));
    }
    thread.join().unwrap()
}

/// The next message on `rx`.
pub(crate) fn received<T>(rx: &std::sync::mpsc::Receiver<T>, what: &str) -> T {
    rx.recv_timeout(PATIENCE)
        .unwrap_or_else(|error| panic!("{what}: {error}"))
}
