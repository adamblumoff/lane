use std::fmt;
use std::io::{self, Write};
use std::time::Instant;

use super::support::elapsed_ms;

#[derive(Clone)]
pub(super) struct ExecObserver {
    enabled: bool,
    lane: String,
    started: Instant,
}

impl ExecObserver {
    pub(super) fn new(lane: &str, enabled: bool) -> Self {
        Self {
            enabled,
            lane: lane.to_owned(),
            started: Instant::now(),
        }
    }

    pub(super) fn event(&self, message: impl fmt::Display) {
        if self.enabled {
            eprintln!(
                "[lane exec {} +{}ms] {message}",
                self.lane,
                elapsed_ms(self.started)
            );
        }
    }

    pub(super) fn mirror_child_stream_chunk(&self, stream: &str, bytes: &[u8]) {
        if !self.enabled || bytes.is_empty() {
            return;
        }
        // Keep process stdout reserved for the final JSON payload.
        let text = String::from_utf8_lossy(bytes);
        let mut stderr = io::stderr().lock();
        for segment in text.split_inclusive('\n') {
            let _ = write!(stderr, "[lane exec {} {stream}] {segment}", self.lane);
            if !segment.ends_with('\n') {
                let _ = writeln!(stderr);
            }
        }
        let _ = stderr.flush();
    }
}
