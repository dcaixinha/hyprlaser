# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

<!--
Add entries here as you merge PRs into main. Use the subsection headings
that apply:

  ### Added       — new features
  ### Changed     — changes to existing functionality
  ### Deprecated  — features still present but slated for removal
  ### Removed     — features removed in this release
  ### Fixed       — bug fixes
  ### Security    — security-relevant changes

When you cut a release, the release script will rename this section to
`## [X.Y.Z] - YYYY-MM-DD` and add a fresh empty `## [Unreleased]` at the
top.
-->

## [0.1.0] - 2026-05-14

### Added

- Initial release.
- Google-Slides-style red laser pointer rendered on a click-through
  Wayland overlay above all windows.
- Tapering motion trail behind the cursor, with configurable duration
  and tail thickness.
- Multi-monitor support: one `wlr-layer-shell` overlay per `wl_output`,
  continuous trail across monitor boundaries.
- HiDPI and fractional scaling via `wp_fractional_scale_v1` +
  `wp_viewporter`, with an integer-scale fallback for compositors that
  don't expose those staging protocols.
- OS cursor automatically hidden while running (via the
  `cursor:invisible` hyprctl keyword) and restored on every exit path
  (Ctrl+C, panic, compositor close).
- CLI flags: `--color`, `--dot-radius`, `--tail-half-width`,
  `--trail-ms`, `--keep-cursor`.
- AUR packaging recipe at `packaging/aur/`.

[Unreleased]: https://github.com/dcaixinha/hyprlaser/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/dcaixinha/hyprlaser/releases/tag/v0.1.0
