---
name: Bug report
about: Something's broken or doesn't behave as expected
title: ""
labels: bug
assignees: ""
---

## What happened

<!-- A clear description of what went wrong. -->

## What you expected

<!-- A clear description of what you expected to happen instead. -->

## Steps to reproduce

1. …
2. …
3. …

## Environment

- **hyprlaser version**: `hyprlaser --version`
- **Hyprland version**: `hyprctl version` (first line is enough)
- **GPU + driver**: `lspci -nnk | grep -iA2 vga` (or equivalent)
- **Monitor setup**: paste the relevant parts of `hyprctl monitors -j`
  (resolution, position, scale)

## Debug log

Run hyprlaser with verbose logging and paste the relevant output:

```sh
RUST_LOG=hyprlaser=debug hyprlaser
```

```text
<log output here>
```

## Screenshot / recording (if visual)

<!-- A screenshot or short clip helps a lot for rendering bugs. -->
