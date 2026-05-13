# AGENTS.md

Notes for AI coding assistants working in this repo. The human-facing
contributor guide is [CONTRIBUTING.md](CONTRIBUTING.md); this file is
the terse operational reference: the exact commands to run for
validation, the conventions to follow, and the project-specific quirks
worth knowing before you touch the code.

## What this project is

A small Rust binary: a virtual laser pointer overlay for
[Hyprland](https://hypr.land). Single crate, no workspace. Runs as a
foreground process the user launches before screen-sharing.

## Validation: run these after every meaningful change

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --release --locked
```

These four commands are what CI runs (see `.github/workflows/ci.yml`).
If they pass locally, CI will almost certainly pass.

For docs-only changes (`*.md`, comments in non-code files) you can skip
all four — but for *any* code change, run all four before claiming the
change is done. `cargo check` alone is **not enough**: it doesn't run
clippy or tests.

## Toolchain

- **MSRV**: 1.87 (declared in `Cargo.toml`). The CI runner uses
  `dtolnay/rust-toolchain@stable` which floats. If you find yourself
  wanting a feature stabilised after 1.87, bump the MSRV in
  `Cargo.toml` — don't paper over it with `#[allow]` workarounds.
- Don't add `rustup` invocations to the workflow. The toolchain action
  handles that.

## Running the binary

```sh
cargo run --release                # default look
cargo run --release -- --help      # CLI flags
RUST_LOG=hyprlaser=debug cargo run # verbose tracing
```

The binary needs a live Hyprland session to actually run (it spawns a
Wayland layer-shell surface and talks to Hyprland's IPC socket).
**Don't run the binary from inside an agent's sandbox** — it will fail
fast with a clear error if `$HYPRLAND_INSTANCE_SIGNATURE` is missing,
which is the desired behaviour, but it also won't surface anything
useful. The human user has to verify visual changes.

## File map

```
src/
├── main.rs        — CLI parse + bootstrap
├── config.rs      — clap-derived Cli + validated LaserConfig + tests
├── app.rs         — SCTK event loop, one layer-shell surface per output
├── render/
│   ├── mod.rs     — per-output wgpu Renderer (pipeline + uniforms)
│   └── shader.wgsl — fullscreen-triangle vertex + distance-to-polyline
│                    fragment shader for the dot + trail
├── trail.rs       — shared cursor-history ring buffer (Arc<Mutex<>>)
│                    written by the IPC thread, read by render
├── hypr_ipc.rs    — Hyprland `.socket.sock` cursorpos poller (120 Hz)
└── cursor_hide.rs — RAII wrapper for `hyprctl keyword cursor:invisible`

packaging/
├── release.sh     — end-to-end release script (see "Releases" below)
└── aur/           — AUR PKGBUILD + .SRCINFO + packager notes
```

## Conventions

- **Comments**: lean on them. Prefer "why" over "what". Whenever you
  make a non-obvious choice (e.g. why we render at physical pixels not
  logical, why field-order in `AppState` is load-bearing) write it
  down right above the code.
- **Tests**: pure helpers (parsers, coord math, ring-buffer logic)
  must have unit tests. Anything that needs a live Wayland connection
  or a GPU is **untested in CI** — keep that logic minimal and
  obvious.
- **Unsafe**: only in `render/mod.rs` for the `Surface<'static>`
  construction, and the soundness justification is documented on the
  struct definition. Don't add new `unsafe` without a comparable
  rationale comment.
- **`#[allow(...)]`**: avoid. If clippy is flagging something, fix the
  code. The one exception we used to have (around `is_multiple_of`)
  was deleted when we bumped the MSRV — let that pattern guide you.

## Quirks worth knowing

These will save you investigation time:

1. **wgpu Vulkan-only.** `render/mod.rs` forces `Backends::VULKAN`.
   We hit a real ultrawide-monitor crash with the GLES fallback
   (max texture dimension was too small). Don't widen the backends.

2. **Adapter native limits, not defaults.** We call
   `adapter.request_device` with `adapter.limits()`, not
   `wgpu::Limits::default()`. Same reason as above — defaults cap
   `max_texture_dimension_2d` at 8192, which a 5120×1440 monitor at
   scale 2.0 would exceed.

3. **Multi-monitor coordinate translation.** Hyprland reports cursor
   coords in *global virtual desktop* space. Each `Renderer` knows its
   output's origin and subtracts it before scaling. Bugs here usually
   show up as "laser appears on wrong monitor" — see commit history
   for two real instances.

4. **HiDPI**: logical sample positions are multiplied by `scale` to
   produce physical pixel coords for the shader. `dot_radius` and
   `tail_half_width` are also scaled. `edge_softness` stays at 1
   *physical* pixel so anti-aliasing is always one screen pixel wide.

5. **Field order in `AppState` is load-bearing.** `renderer` must drop
   before `layer` and `conn` because wgpu's `Surface<'static>` holds
   raw pointers into them. Don't reorder these struct fields.

6. **Frame pacing**: each output's render loop is driven by
   `wl_surface.frame()` callbacks at vblank. We always present, even
   when the cursor hasn't moved — the shader is cheap enough
   (~milliseconds), and trying to skip frames creates the runaway-
   loop bug we already fixed once (commit `12b18d6` style — see
   `app.rs` for the comment).

7. **Hyprland doesn't emit mousemove events.** Don't try to subscribe
   to `.socket2.sock` for cursor position — it's not there. We poll
   `cursorpos` over `.socket.sock` at 120 Hz instead.

8. **Cursor hiding is a global Hyprland keyword toggle**, not a
   per-surface protocol thing. It affects all of Hyprland while
   hyprlaser is running. The `CursorHider` Drop impl restores it on
   every exit path including panic.

## CHANGELOG.md is required for user-facing changes

Every user-facing change (new flag, behaviour change, bug fix users
would notice) needs an entry under `## [Unreleased]` in
`CHANGELOG.md`. Use the standard subsections: `### Added`,
`### Changed`, `### Fixed`, `### Removed`, `### Deprecated`,
`### Security`. The release script refuses to cut a version when
`[Unreleased]` is empty.

Pure refactors with no visible effect don't need an entry.

## Releases

Don't tag or push releases on your own. The maintainer runs:

```sh
./packaging/release.sh <version>
```

If you're asked to prepare a release, your job is to:

1. Make sure `[Unreleased]` is populated correctly.
2. Run the validation trifecta above.
3. Suggest the version bump (patch/minor/major per SemVer) based on
   the entries.
4. Stop there. The script handles cargo bump, tagging, GitHub
   release, AUR PKGBUILD bump, etc.

`packaging/release.sh --dry-run <version>` previews everything without
mutating anything; use it freely.

## Out-of-scope things

These have been considered and intentionally aren't part of the
project. Don't suggest them without explicit user buy-in:

- **Other compositors.** Hyprland-only is a deliberate choice; the
  cursor-hiding trick and the IPC poller are Hyprland-specific.
- **A config file format.** CLI flags are the entire UI.
- **An on-screen drawing/annotation mode.** Different tool.
- **A daemon mode or system tray icon.** Run-and-Ctrl+C is the model.
- **Cross-platform / Windows / macOS.** Not happening.

## Things to ask before changing

- Adding a new dependency (especially heavy ones — `tokio`, async
  runtimes, GUI toolkits).
- Changing the default visual look (color, radius, trail params).
- Bumping the MSRV.
- Reorganising the module layout.
- Touching `.github/workflows/ci.yml` in ways that change what's
  checked (vs. fixing what's broken).

For everything else, follow the validation steps and your judgement.
