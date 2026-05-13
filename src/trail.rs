//! Motion-trail history buffer.
//!
//! The trail is a ring of recent cursor positions in **global**
//! virtual-desktop coordinates (the same coordinate space Hyprland's
//! `cursorpos` reports). Each per-output `Renderer` translates this
//! global trail into surface-local pixels when uploading the uniform.
//!
//! Capacity is bounded at `MAX_SAMPLES` so the GPU shader sees a
//! fixed-size array. Samples older than `duration` are aged out.
//!
//! Ownership: the trail is shared between the Hyprland IPC poll thread
//! (which pushes new samples whenever the cursor moves) and the main
//! Wayland thread (which reads `samples()` each frame to build a
//! uniform). An `Arc<Mutex<TrailBuffer>>` is plenty — the critical
//! section is microseconds and contention is negligible.

use std::time::{Duration, Instant};

/// Hard cap on history entries. Must match `TRAIL_SAMPLES` in
/// `shader.wgsl` and `render::TRAIL_SAMPLES`.
pub const MAX_SAMPLES: usize = 32;

/// One entry in the trail ring.
#[derive(Copy, Clone, Debug)]
pub struct TrailSample {
    pub pos: (f32, f32),
    pub timestamp: Instant,
}

/// Ring of recent cursor positions, newest entries at the front.
#[derive(Debug)]
pub struct TrailBuffer {
    samples: Vec<TrailSample>,
    duration: Duration,
}

impl TrailBuffer {
    pub fn new(duration: Duration) -> Self {
        Self {
            samples: Vec::with_capacity(MAX_SAMPLES),
            duration,
        }
    }

    pub fn duration(&self) -> Duration {
        self.duration
    }

    /// Insert a new sample at the head, coalescing stationary cursor
    /// (so the head's timestamp is just refreshed rather than wasting
    /// a slot on a duplicate), age out any samples past `duration`,
    /// and truncate to `MAX_SAMPLES`.
    pub fn push(&mut self, pos: (f32, f32), now: Instant) {
        if let Some(head) = self.samples.first_mut() {
            if head.pos == pos {
                head.timestamp = now;
                self.samples
                    .retain(|s| now.duration_since(s.timestamp) <= self.duration);
                return;
            }
        }
        self.samples.insert(
            0,
            TrailSample {
                pos,
                timestamp: now,
            },
        );
        self.samples
            .retain(|s| now.duration_since(s.timestamp) <= self.duration);
        if self.samples.len() > MAX_SAMPLES {
            self.samples.truncate(MAX_SAMPLES);
        }
    }

    /// Borrow the current samples. Index 0 is the newest.
    pub fn samples(&self) -> &[TrailSample] {
        &self.samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_into_empty() {
        let mut b = TrailBuffer::new(Duration::from_millis(500));
        b.push((10.0, 20.0), Instant::now());
        assert_eq!(b.samples().len(), 1);
        assert_eq!(b.samples()[0].pos, (10.0, 20.0));
    }

    #[test]
    fn coalesce_same_position() {
        let mut b = TrailBuffer::new(Duration::from_millis(500));
        let now = Instant::now();
        b.push((10.0, 20.0), now);
        b.push((10.0, 20.0), now + Duration::from_millis(10));
        b.push((10.0, 20.0), now + Duration::from_millis(20));
        assert_eq!(b.samples().len(), 1);
    }

    #[test]
    fn distinct_positions_stack() {
        let mut b = TrailBuffer::new(Duration::from_millis(500));
        let now = Instant::now();
        b.push((10.0, 20.0), now);
        b.push((11.0, 20.0), now);
        b.push((12.0, 20.0), now);
        assert_eq!(b.samples().len(), 3);
        // Newest at index 0.
        assert_eq!(b.samples()[0].pos, (12.0, 20.0));
        assert_eq!(b.samples()[2].pos, (10.0, 20.0));
    }

    #[test]
    fn ages_out_old_samples() {
        let mut b = TrailBuffer::new(Duration::from_millis(100));
        let t0 = Instant::now();
        b.push((1.0, 0.0), t0);
        b.push((2.0, 0.0), t0 + Duration::from_millis(50));
        // Push a fresh sample 500 ms later — older ones should be gone.
        b.push((3.0, 0.0), t0 + Duration::from_millis(500));
        assert_eq!(b.samples().len(), 1);
        assert_eq!(b.samples()[0].pos, (3.0, 0.0));
    }

    #[test]
    fn caps_at_max_samples() {
        let mut b = TrailBuffer::new(Duration::from_secs(10));
        let t0 = Instant::now();
        for i in 0..(MAX_SAMPLES + 5) {
            b.push((i as f32, 0.0), t0 + Duration::from_millis(i as u64));
        }
        assert_eq!(b.samples().len(), MAX_SAMPLES);
    }
}
