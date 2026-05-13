//! Hyprland IPC client — cursor position polling.
//!
//! Hyprland exposes two Unix sockets per session under
//! `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/`:
//!
//! - `.socket.sock`  — request/response (the same channel `hyprctl` uses)
//! - `.socket2.sock` — push-based events
//!
//! For our purposes we want live cursor coordinates. Hyprland deliberately
//! does *not* emit a `mousemove` event on `.socket2.sock` (the rate would
//! flood the socket on a 1 kHz gaming mouse), so the only way to track the
//! cursor is to poll `cursorpos` over `.socket.sock`.
//!
//! The format of a `cursorpos` reply is a single line:
//!
//! ```text
//! 4916, 976
//! ```
//!
//! (Two i32s in *global virtual-desktop* coordinates, comma-space separated,
//! terminated by either EOF or `\n` depending on Hyprland version.)
//!
//! ## Architecture
//!
//! We spawn one background thread that:
//!   1. Opens a fresh `UnixStream` per query (Hyprland closes the stream
//!      after each request — it's a stateless RPC protocol).
//!   2. Sends `cursorpos`.
//!   3. Reads the reply, parses two ints.
//!   4. Packs them into a `u64` and stores it in a shared `AtomicU64`.
//!   5. Sleeps to maintain a configurable poll rate.
//!
//! The render thread reads the latest position with a single relaxed
//! `AtomicU64::load`, unpacks the two i32s, and uses them as a uniform.

use std::{
    env,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::trail::TrailBuffer;

/// Lock-free shared latest-known cursor position in global coordinates.
///
/// Packs `(x as u32) << 32 | (y as u32)` into a single 64-bit atomic so the
/// reader always sees a coherent pair. We cast `i32 → u32` (bitwise) so
/// negative coordinates (multi-monitor with the origin not at the top-left
/// of the leftmost output) round-trip correctly.
#[derive(Debug)]
pub struct CursorPos {
    packed: AtomicU64,
    /// `true` once the poller has produced at least one valid sample.
    valid: AtomicBool,
}

impl CursorPos {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            packed: AtomicU64::new(0),
            valid: AtomicBool::new(false),
        })
    }

    fn store(&self, x: i32, y: i32) {
        let packed = ((x as u32 as u64) << 32) | (y as u32 as u64);
        self.packed.store(packed, Ordering::Relaxed);
        self.valid.store(true, Ordering::Release);
    }

    /// Returns the latest sampled cursor position, or `None` if the poller
    /// hasn't produced a sample yet.
    pub fn get(&self) -> Option<(i32, i32)> {
        if !self.valid.load(Ordering::Acquire) {
            return None;
        }
        let packed = self.packed.load(Ordering::Relaxed);
        let x = (packed >> 32) as u32 as i32;
        let y = (packed & 0xFFFF_FFFF) as u32 as i32;
        Some((x, y))
    }
}

/// Build the request-socket path from environment variables. Returns an
/// error if `HYPRLAND_INSTANCE_SIGNATURE` or `XDG_RUNTIME_DIR` are unset —
/// which means we're not running inside a Hyprland session.
fn socket_path() -> Result<PathBuf, String> {
    let runtime = env::var("XDG_RUNTIME_DIR")
        .map_err(|_| "XDG_RUNTIME_DIR is not set; is this a real session?".to_string())?;
    let sig = env::var("HYPRLAND_INSTANCE_SIGNATURE").map_err(|_| {
        "HYPRLAND_INSTANCE_SIGNATURE is not set; are you running inside Hyprland?".to_string()
    })?;
    Ok(PathBuf::from(runtime)
        .join("hypr")
        .join(sig)
        .join(".socket.sock"))
}

/// One round-trip: open the socket, write the command, read the reply.
fn query(socket: &PathBuf, command: &str) -> std::io::Result<String> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_millis(100)))?;
    stream.set_write_timeout(Some(Duration::from_millis(100)))?;
    stream.write_all(command.as_bytes())?;
    let mut buf = String::with_capacity(64);
    stream.read_to_string(&mut buf)?;
    Ok(buf)
}

/// Parse `"X, Y"` (or `"X,Y"`, just in case) into a coordinate pair.
fn parse_cursorpos(reply: &str) -> Option<(i32, i32)> {
    let line = reply.trim();
    let (x, y) = line.split_once(',')?;
    let x: i32 = x.trim().parse().ok()?;
    let y: i32 = y.trim().parse().ok()?;
    Some((x, y))
}

/// Spawn the polling thread. The thread polls Hyprland's `cursorpos`
/// at `poll_hz` and writes every distinct sample into:
///
/// - `CursorPos` for the latest-known position (cheap lock-free read
///   from any thread), and
/// - `TrailBuffer` for the recent history (used by the render thread to
///   draw the streak across all output surfaces).
///
/// Returns the cursor handle plus the join handle. The thread runs
/// until the process exits.
pub fn spawn(
    poll_hz: u32,
    trail: Arc<Mutex<TrailBuffer>>,
) -> Result<(Arc<CursorPos>, JoinHandle<()>), String> {
    let socket = socket_path()?;

    // Probe once up front so we fail fast with a useful error if the
    // socket exists but doesn't speak the expected protocol.
    let probe = query(&socket, "cursorpos")
        .map_err(|e| format!("failed to read Hyprland socket {}: {e}", socket.display()))?;
    let (x0, y0) = parse_cursorpos(&probe).ok_or_else(|| {
        format!(
            "couldn't parse Hyprland cursorpos reply (expected 'X, Y', got {:?})",
            probe
        )
    })?;

    let state = CursorPos::new();
    state.store(x0, y0);
    if let Ok(mut tb) = trail.lock() {
        tb.push((x0 as f32, y0 as f32), Instant::now());
    }
    log::info!(
        "Hyprland IPC ready at {} (initial cursor: {x0}, {y0})",
        socket.display()
    );

    let interval = Duration::from_micros(1_000_000 / poll_hz.max(1) as u64);

    let state_clone = Arc::clone(&state);
    let handle = thread::Builder::new()
        .name("hypr_ipc_poll".into())
        .spawn(move || poll_loop(socket, state_clone, trail, interval))
        .map_err(|e| format!("failed to spawn IPC poll thread: {e}"))?;

    Ok((state, handle))
}

fn poll_loop(
    socket: PathBuf,
    state: Arc<CursorPos>,
    trail: Arc<Mutex<TrailBuffer>>,
    interval: Duration,
) {
    // Consecutive-failure counter so we can back off and warn loudly rather
    // than tight-looping on a closed socket.
    let mut consecutive_errors = 0u32;
    // Last logged position — we DEBUG-log every change so the cursor poll
    // can be sanity-checked with RUST_LOG=hyprlaser=debug.
    let mut last_logged: Option<(i32, i32)> = state.get();

    loop {
        let start = Instant::now();

        match query(&socket, "cursorpos") {
            Ok(reply) => match parse_cursorpos(&reply) {
                Some((x, y)) => {
                    if consecutive_errors > 0 {
                        log::info!("Hyprland IPC recovered after {consecutive_errors} errors");
                    }
                    consecutive_errors = 0;
                    state.store(x, y);
                    // Push every sample into the trail. The TrailBuffer
                    // coalesces stationary cursor and ages out the tail.
                    if let Ok(mut tb) = trail.lock() {
                        tb.push((x as f32, y as f32), start);
                    }
                    if last_logged != Some((x, y)) {
                        log::debug!("cursor: ({x}, {y})");
                        last_logged = Some((x, y));
                    }
                }
                None => {
                    consecutive_errors += 1;
                    if consecutive_errors == 1 || consecutive_errors.is_multiple_of(60) {
                        log::warn!("unparseable cursorpos reply: {reply:?}");
                    }
                }
            },
            Err(err) => {
                consecutive_errors += 1;
                if consecutive_errors == 1 || consecutive_errors.is_multiple_of(60) {
                    log::warn!("cursorpos query failed: {err}");
                }
            }
        }

        // Sleep for the remainder of the poll interval. If a query took
        // longer than the interval (unusual), proceed immediately.
        let elapsed = start.elapsed();
        if let Some(remaining) = interval.checked_sub(elapsed) {
            thread::sleep(remaining);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_with_space() {
        assert_eq!(parse_cursorpos("4916, 976"), Some((4916, 976)));
    }

    #[test]
    fn parse_without_space() {
        assert_eq!(parse_cursorpos("4916,976"), Some((4916, 976)));
    }

    #[test]
    fn parse_with_trailing_newline() {
        assert_eq!(parse_cursorpos("4916, 976\n"), Some((4916, 976)));
    }

    #[test]
    fn parse_negative() {
        assert_eq!(parse_cursorpos("-100, -200"), Some((-100, -200)));
    }

    #[test]
    fn parse_rejects_junk() {
        assert_eq!(parse_cursorpos("not a coord"), None);
        assert_eq!(parse_cursorpos(""), None);
        assert_eq!(parse_cursorpos("1,"), None);
    }

    #[test]
    fn pack_round_trip_positive() {
        let cp = CursorPos::new();
        cp.store(4916, 976);
        assert_eq!(cp.get(), Some((4916, 976)));
    }

    #[test]
    fn pack_round_trip_negative() {
        let cp = CursorPos::new();
        cp.store(-1, -2);
        assert_eq!(cp.get(), Some((-1, -2)));
    }

    #[test]
    fn pack_round_trip_mixed() {
        let cp = CursorPos::new();
        cp.store(-1000, 1000);
        assert_eq!(cp.get(), Some((-1000, 1000)));
    }

    #[test]
    fn get_before_first_sample_returns_none() {
        let cp = CursorPos::new();
        assert_eq!(cp.get(), None);
    }
}
