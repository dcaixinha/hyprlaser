//! Hides the OS cursor for the duration of a hyprlaser session by
//! flipping Hyprland's `cursor:invisible` keyword over its request
//! socket. The same socket and protocol we use for `cursorpos` in
//! `hypr_ipc.rs`.
//!
//! On Wayland a normal client cannot hide the system cursor — the
//! compositor draws it. The portable way to do this on Hyprland is to
//! ask the compositor to stop drawing the cursor entirely while our
//! laser is up, then restore on shutdown.
//!
//! This affects **all** of Hyprland, not just our surface — but that
//! is what we want: when the laser is active there is no real cursor
//! arrow, only the red dot we render.
//!
//! `CursorHider` restores the cursor in its `Drop` impl, so even a
//! panic in the main loop or a SIGINT (handled cooperatively in
//! `app::run`) will leave the user with a visible cursor again.

use std::{
    env,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    time::Duration,
};

pub struct CursorHider {
    socket: PathBuf,
    hidden: bool,
}

impl CursorHider {
    /// Hide the OS cursor immediately and return a handle that will
    /// restore it on drop.
    pub fn hide() -> Result<Self, String> {
        let socket = socket_path()?;
        let mut hider = Self {
            socket,
            hidden: false,
        };
        hider.send("keyword cursor:invisible 1")?;
        hider.hidden = true;
        log::info!("OS cursor hidden via hyprctl keyword");
        Ok(hider)
    }

    fn send(&self, command: &str) -> Result<(), String> {
        let mut stream = UnixStream::connect(&self.socket)
            .map_err(|e| format!("failed to connect to Hyprland socket: {e}"))?;
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .ok();
        stream
            .set_write_timeout(Some(Duration::from_millis(500)))
            .ok();
        stream
            .write_all(command.as_bytes())
            .map_err(|e| format!("failed to write '{command}' to Hyprland socket: {e}"))?;
        // Drain the reply (typically "ok"); we don't actually care about
        // the contents but the server may block on us reading it.
        let mut buf = [0u8; 64];
        let _ = stream.read(&mut buf);
        Ok(())
    }
}

impl Drop for CursorHider {
    fn drop(&mut self) {
        if self.hidden {
            // Best effort. If this fails the user can run
            // `hyprctl keyword cursor:invisible 0` manually.
            if let Err(err) = self.send("keyword cursor:invisible 0") {
                log::error!(
                    "failed to restore OS cursor on shutdown: {err}.\n\
                     Run `hyprctl keyword cursor:invisible 0` to fix it."
                );
            } else {
                log::info!("OS cursor restored (Drop)");
            }
        }
    }
}

fn socket_path() -> Result<PathBuf, String> {
    let runtime =
        env::var("XDG_RUNTIME_DIR").map_err(|_| "XDG_RUNTIME_DIR is not set".to_string())?;
    let sig = env::var("HYPRLAND_INSTANCE_SIGNATURE")
        .map_err(|_| "HYPRLAND_INSTANCE_SIGNATURE is not set".to_string())?;
    Ok(PathBuf::from(runtime)
        .join("hypr")
        .join(sig)
        .join(".socket.sock"))
}
