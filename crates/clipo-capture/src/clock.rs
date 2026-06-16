//! Master clock shared by the video + audio capture pipelines so both
//! tracks emit aligned PTS through pause/resume cycles.

use std::time::{Duration, Instant};

use parking_lot::Mutex;

pub struct MasterClock {
    start: Instant,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    // Bundled in one mutex so the audio thread can't read a torn
    // (paused_at, accumulated) pair. A previous atomics-only design
    // had a race during pause/resume where the audio thread read the
    // new `accumulated` against the old `paused_at`, producing a PTS
    // that regressed for one packet — the MF `SinkWriter` rejected it.
    // ~50ns mutex acquire is fine: audio thread reads ~10×/s.
    paused_at: Option<Instant>,
    accumulated: Duration,
}

impl MasterClock {
    #[must_use]
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Effective elapsed time in 100-ns ticks (Media Foundation's unit).
    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    pub fn elapsed_100ns(&self) -> i64 {
        let (paused_at, accumulated) = {
            let inner = self.inner.lock();
            (inner.paused_at, inner.accumulated)
        };
        let effective = paused_at
            .map_or_else(
                || self.start.elapsed(),
                |t| t.saturating_duration_since(self.start),
            )
            .saturating_sub(accumulated);
        (effective.as_nanos() / 100) as i64
    }

    pub fn pause(&self) {
        let mut inner = self.inner.lock();
        if inner.paused_at.is_none() {
            inner.paused_at = Some(Instant::now());
        }
    }

    pub fn resume(&self) {
        let mut inner = self.inner.lock();
        if let Some(t) = inner.paused_at.take() {
            inner.accumulated += t.elapsed();
        }
    }

    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.inner.lock().paused_at.is_some()
    }
}

impl Default for MasterClock {
    fn default() -> Self {
        Self::new()
    }
}
