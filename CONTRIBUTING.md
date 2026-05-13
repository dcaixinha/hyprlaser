# Contributing to hyprlaser

Thanks for wanting to help! This document is the short version of "what
you should know before opening a PR".

> Using an AI coding assistant? Point it at [`AGENTS.md`](AGENTS.md),
> which is the agent-oriented version of this doc — terse, with the
> exact validation commands and project quirks worth knowing.

## Scope

hyprlaser does one thing: a laser-pointer overlay for
[Hyprland](https://hypr.land). Bug fixes, performance improvements,
small features (new CLI flags, more compositor protocol support) and
documentation improvements are all very welcome.

What's likely out of scope:

- Support for non-Hyprland compositors. The IPC poller is
  Hyprland-specific (it talks to `.socket.sock`) and the cursor-hiding
  trick uses `hyprctl keyword cursor:invisible`. Generalising would be
  a significant rewrite; happy to discuss if you're up for it, but
  please open an issue first.
- A configuration file format. The CLI flags are deliberately the
  whole UI.
- An on-screen annotation/drawing mode. That's a different tool.

If you're not sure whether something fits, open an issue first and
we'll talk it through.

## Setting up

You need:

- A recent Rust toolchain (`rustup default stable` if you don't have
  one).
- The system libraries we depend on are pulled by `cargo build`
  automatically on most distros. On Arch the relevant package is
  `wayland` (for `libwayland-client`). For runtime testing you also
  need `vulkan-icd-loader` and a Vulkan driver for your GPU.
- Hyprland running, so you can actually run the binary end-to-end.

Clone and build:

```sh
git clone git@github.com:dcaixinha/hyprlaser.git
cd hyprlaser
cargo build --release
./target/release/hyprlaser
```

## Before opening a PR

Run the same checks CI will run (these are the exact commands the
[`ci.yml`](.github/workflows/ci.yml) workflow uses, so if they pass
locally CI should pass too):

```sh
cargo fmt --all -- --check                   # code is rustfmt-clean
cargo clippy --all-targets -- -D warnings    # clippy finds nothing
cargo test --all-targets                     # all tests pass
cargo build --release                        # release build works
```

CI also lints `packaging/aur/PKGBUILD` and verifies `.SRCINFO` is up to
date. If you touched the PKGBUILD, run this before pushing:

```sh
cd packaging/aur
makepkg --printsrcinfo > .SRCINFO
```

If you change anything user-facing — a new flag, a behaviour change, a
bug fix that users would notice — **add an entry to
[`CHANGELOG.md`](CHANGELOG.md)** under the `## [Unreleased]` section.
Use the standard subsection headings (`### Added`, `### Changed`,
`### Fixed`, etc). One bullet per change, written in the past tense.
The release script refuses to cut a new version if `[Unreleased]` is
empty.

Pure refactors with no user-visible effect don't need a changelog
entry.

## Code style

- **rustfmt**: default settings, no `rustfmt.toml`. Run `cargo fmt`
  before pushing.
- **clippy**: clean with `-D warnings`. If a lint genuinely doesn't
  fit, `#[allow(...)]` it locally with a brief comment explaining why.
- **Comments**: lean on them. We prefer "why" over "what". If a
  non-obvious choice is being made (e.g. why we render at physical
  pixels not logical), say so right above the code.
- **Tests**: pure helpers (parsers, coord math) should have unit
  tests. Anything that needs a live Wayland connection doesn't.

## Commit messages

- Subject line: imperative mood ("Add output enumeration roundtrip",
  not "Added"), under 72 chars.
- Body (optional but appreciated for non-trivial changes): wrap at 72
  cols, explain *why* the change is needed. Reference issue numbers
  with `Fixes #123` or `Refs #45`.
- Squashing during review is fine. Final landed history doesn't need
  to be one commit per PR, but related changes should land together.

## Reporting bugs

A useful bug report includes:

- Your Hyprland version (`hyprctl version`).
- Your GPU + driver (`lspci -nnk | grep -iA2 vga` or similar).
- Monitor setup: `hyprctl monitors -j` (the relevant parts —
  resolution, position, scale).
- What you ran and what happened, including any log output captured
  with:

  ```sh
  RUST_LOG=hyprlaser=debug hyprlaser
  ```

- If it's a visual bug: a screenshot or short screen-recording goes a
  long way.

## Releases

Releases are cut by the maintainer via `./packaging/release.sh
<version>`. Contributors don't need to touch tags, the PKGBUILD, or
the AUR side — just keep `CHANGELOG.md` honest and the release script
takes care of the rest.

## Code of conduct

Be kind. Disagree with ideas, not with people. If something feels off,
flag it to the maintainer directly.

## License

By contributing, you agree that your contributions will be licensed
under the same [Apache-2.0](LICENSE) terms as the rest of the project.
