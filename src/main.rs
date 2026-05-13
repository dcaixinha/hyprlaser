//! hyprlaser — a Google-Slides-style laser-pointer overlay for Hyprland.
//!
//! Module layout:
//!
//! - `config` — CLI parsing and validated runtime config.
//! - `app` — SCTK Wayland app: spawns one layer-shell overlay per
//!   `wl_output` and drives the frame loop.
//! - `render` — wgpu pipeline + WGSL shader.
//! - `trail` — shared cursor-history buffer (writer: IPC thread,
//!   readers: render thread).
//! - `hypr_ipc` — Hyprland cursorpos poller over `.socket.sock`.
//! - `cursor_hide` — RAII wrapper around the `cursor:invisible` hyprctl
//!   keyword so the OS cursor is restored on every exit path.

mod app;
mod config;
mod cursor_hide;
mod hypr_ipc;
mod render;
mod trail;

use clap::Parser;

use crate::config::{Cli, LaserConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Default: show our own info logs, but mute wgpu/naga/SCTK chatter.
    // Override with RUST_LOG (e.g. `RUST_LOG=debug` or `RUST_LOG=wgpu=info`).
    let default_filter =
        "hyprlaser=info,wgpu=warn,wgpu_core=warn,wgpu_hal=warn,naga=warn,sctk=warn";
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_filter))
        .init();

    let cli = Cli::parse();
    let config = LaserConfig::from_cli(cli)?;
    app::run(config)
}
