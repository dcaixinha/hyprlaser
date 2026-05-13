#!/usr/bin/env bash
#
# release.sh — cut a new hyprlaser release end-to-end.
#
# What it does, in order:
#
#   1. Refuses to run unless the working tree is clean, you're on main,
#      and you're synced with origin/main.
#   2. Validates the requested version is a sane SemVer bump.
#   3. Updates Cargo.toml + Cargo.lock + CHANGELOG.md.
#   4. Commits the bump and creates an annotated tag.
#   5. Pushes main and the tag.
#   6. (Optional) Creates a GitHub Release with the changelog section as
#      the notes, if `gh` is installed and authenticated.
#   7. Updates the AUR PKGBUILD (pkgver, sha256sums, .SRCINFO).
#   8. Commits and pushes the AUR bump.
#
# Usage:
#
#   packaging/release.sh <version>          # do the release
#   packaging/release.sh --dry-run <version>  # show what would happen
#
# <version> is a bare SemVer string like "0.2.0" (no leading v).
#
# Run from the project root.

set -euo pipefail

# ── helpers ─────────────────────────────────────────────────────────────────

DRY_RUN=0
VERSION=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run|-n) DRY_RUN=1 ;;
    -h|--help)
      # Print the header comment block (everything between line 3 and
      # the last "# Run from the project root." line, exclusive of the
      # `set -euo pipefail` that follows).
      sed -n '3,25p' "$0" | sed 's/^# \?//'
      exit 0
      ;;
    -*)
      echo "error: unknown flag $1" >&2
      exit 2
      ;;
    *)
      if [[ -n "$VERSION" ]]; then
        echo "error: extra positional arg '$1' (already have version '$VERSION')" >&2
        exit 2
      fi
      VERSION="$1"
      ;;
  esac
  shift
done

if [[ -z "$VERSION" ]]; then
  echo "error: missing <version> argument" >&2
  echo "usage: $0 [--dry-run] <version>" >&2
  exit 2
fi

# Pretty status line.
say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }

# Run a command, or print it under --dry-run.
run() {
  if (( DRY_RUN )); then
    printf '\033[2m   $ %s\033[0m\n' "$*"
  else
    eval "$@"
  fi
}

# Like `run`, but for shell snippets that span multiple lines / use
# heredocs / redirections. Pass the snippet as a single arg.
run_block() {
  local block="$1"
  if (( DRY_RUN )); then
    while IFS= read -r line; do
      printf '\033[2m   $ %s\033[0m\n' "$line"
    done <<< "$block"
  else
    bash -ec "$block"
  fi
}

# ── locate the project root ─────────────────────────────────────────────────

# Find the project root by walking up from the script's directory looking
# for a .git dir or Cargo.toml. This works whether the script lives at
# packaging/release.sh inside the project, or is copied/symlinked
# elsewhere — as long as it's invoked from within the project tree.
find_project_root() {
  local start
  if start="$(git rev-parse --show-toplevel 2>/dev/null)"; then
    printf '%s\n' "$start"
    return 0
  fi
  # Fallback for when we're not in a git working tree: assume the script
  # is at <root>/packaging/release.sh.
  local script_dir
  script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
  printf '%s\n' "$(cd -- "$script_dir/.." && pwd)"
}

project_root="$(find_project_root)"
cd "$project_root"

# ── preflight checks ────────────────────────────────────────────────────────

say "Preflight checks"

# Required tools.
for tool in git cargo makepkg updpkgsums awk sed; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "error: required tool '$tool' is not installed" >&2
    exit 1
  fi
done

# Version format: bare SemVer like 0.2.0 or 1.0.0-rc.1. Reject leading v.
if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
  echo "error: '$VERSION' is not a valid SemVer string (e.g. 0.2.0 or 1.0.0-rc.1)" >&2
  echo "       Pass the version *without* a leading 'v'." >&2
  exit 1
fi
TAG="v$VERSION"

# Working tree clean.
if [[ -n "$(git status --porcelain)" ]]; then
  echo "error: working tree is dirty; commit or stash first" >&2
  git status --short >&2
  exit 1
fi

# On main.
current_branch="$(git rev-parse --abbrev-ref HEAD)"
if [[ "$current_branch" != "main" ]]; then
  echo "error: must be on the 'main' branch (currently on '$current_branch')" >&2
  exit 1
fi

# Synced with origin/main: no unpushed commits, no commits we don't have.
git fetch --quiet origin main
local_sha="$(git rev-parse HEAD)"
remote_sha="$(git rev-parse origin/main)"
if [[ "$local_sha" != "$remote_sha" ]]; then
  ahead="$(git rev-list --count origin/main..HEAD)"
  behind="$(git rev-list --count HEAD..origin/main)"
  echo "error: local main is not in sync with origin/main" >&2
  echo "       ahead by $ahead, behind by $behind" >&2
  echo "       run 'git pull --rebase' or 'git push' first" >&2
  exit 1
fi

# Current version from Cargo.toml.
current_version="$(awk -F\" '/^version = "/ { print $2; exit }' Cargo.toml)"
if [[ -z "$current_version" ]]; then
  echo "error: couldn't extract current version from Cargo.toml" >&2
  exit 1
fi

# New version must be > current. Use sort -V for SemVer-aware ordering.
if [[ "$current_version" == "$VERSION" ]]; then
  echo "error: requested version '$VERSION' equals current; bump it" >&2
  exit 1
fi
highest="$(printf '%s\n%s\n' "$current_version" "$VERSION" | sort -V | tail -1)"
if [[ "$highest" != "$VERSION" ]]; then
  echo "error: requested version '$VERSION' is older than current '$current_version'" >&2
  exit 1
fi

# Tag must not already exist (local or remote).
if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
  echo "error: tag '$TAG' already exists locally" >&2
  exit 1
fi
if git ls-remote --exit-code --tags origin "$TAG" >/dev/null 2>&1; then
  echo "error: tag '$TAG' already exists on origin" >&2
  exit 1
fi

# CHANGELOG.md must have an [Unreleased] section with non-trivial content.
if [[ ! -f CHANGELOG.md ]]; then
  echo "error: CHANGELOG.md is missing" >&2
  exit 1
fi
unreleased_body="$(
  awk '
    /^## \[Unreleased\]/ { in_section = 1; next }
    in_section && /^## \[/ { exit }
    in_section { print }
  ' CHANGELOG.md
)"
# Strip HTML comments and blank lines, see what's left.
unreleased_meaningful="$(
  printf '%s\n' "$unreleased_body" \
    | awk 'BEGIN { in_comment = 0 }
           /<!--/ { in_comment = 1 }
           !in_comment && NF { print }
           /-->/ { in_comment = 0 }'
)"
if [[ -z "$unreleased_meaningful" ]]; then
  echo "error: [Unreleased] section in CHANGELOG.md is empty" >&2
  echo "       Add at least one bullet point under a subsection heading" >&2
  echo "       (### Added, ### Fixed, etc.) before releasing." >&2
  exit 1
fi

say "All preflight checks passed."
echo "    Current version : $current_version"
echo "    New version     : $VERSION"
echo "    New tag         : $TAG"
echo "    Dry run         : $((DRY_RUN))"
echo

# ── 1. bump Cargo.toml ──────────────────────────────────────────────────────

say "Updating Cargo.toml: $current_version → $VERSION"
# Replace only the FIRST `version = "..."` line — that's the [package] one.
run "sed -i '0,/^version = \"$current_version\"$/ s//version = \"$VERSION\"/' Cargo.toml"

# ── 2. refresh Cargo.lock ───────────────────────────────────────────────────

say "Refreshing Cargo.lock"
run "cargo check --quiet"

# ── 3. update CHANGELOG.md ──────────────────────────────────────────────────

say "Rewriting CHANGELOG.md: [Unreleased] → [$VERSION] - $(date +%F)"
today="$(date +%F)"

if (( DRY_RUN )); then
  printf '\033[2m   (would rewrite CHANGELOG.md and update compare links)\033[0m\n'
else
  # Use a Python-less awk pipeline:
  #   1. Insert a fresh empty [Unreleased] block above the existing one.
  #   2. Rename the old [Unreleased] heading to [VERSION] - TODAY.
  #   3. Update the link definitions at the bottom.
  tmp="$(mktemp)"
  awk -v ver="$VERSION" -v today="$today" '
    BEGIN { inserted = 0 }
    /^## \[Unreleased\]/ && !inserted {
      print "## [Unreleased]"
      print ""
      print "<!--"
      print "Add entries here as you merge PRs into main. The release"
      print "script will rename this section to ## [X.Y.Z] - YYYY-MM-DD"
      print "and add a fresh empty Unreleased on top of it."
      print "-->"
      print ""
      print "## [" ver "] - " today
      inserted = 1
      next
    }
    { print }
  ' CHANGELOG.md > "$tmp"
  mv "$tmp" CHANGELOG.md

  # Update the link definitions at the bottom. Strategy:
  #   - Bump [Unreleased] target to compare against the new tag.
  #   - Insert a [VERSION] target right below it.
  tmp="$(mktemp)"
  awk -v ver="$VERSION" -v prev="$current_version" '
    /^\[Unreleased\]:/ {
      print "[Unreleased]: https://github.com/dcaixinha/hyprlaser/compare/v" ver "...HEAD"
      print "[" ver "]: https://github.com/dcaixinha/hyprlaser/compare/v" prev "...v" ver
      next
    }
    { print }
  ' CHANGELOG.md > "$tmp"
  mv "$tmp" CHANGELOG.md
fi

# ── 4. commit, tag ──────────────────────────────────────────────────────────

say "Committing release"
run "git add Cargo.toml Cargo.lock CHANGELOG.md"
run "git commit -m 'Release v$VERSION'"

# Extract the changelog section for this version. The annotated tag's
# message body becomes the default for `gh release create --notes-from-tag`,
# and is also nice context for `git log`.
if (( DRY_RUN )); then
  printf '\033[2m   (would extract [%s] section from CHANGELOG.md for tag/release notes)\033[0m\n' "$VERSION"
  notes="<changelog section for $VERSION>"
else
  notes="$(awk -v ver="$VERSION" '
    $0 ~ "^## \\[" ver "\\]" { in_section = 1; next }
    in_section && /^## \[/ { exit }
    in_section { print }
  ' CHANGELOG.md | awk '
    # Trim leading and trailing blank lines.
    NF { found = 1 }
    found { buf = buf $0 "\n" }
    END {
      # Strip trailing newlines.
      sub(/\n+$/, "", buf)
      print buf
    }
  ')"
fi

say "Creating annotated tag $TAG"
if (( DRY_RUN )); then
  printf '\033[2m   $ git tag -a %s -m "hyprlaser %s\\n\\n<changelog>"\033[0m\n' "$TAG" "$TAG"
else
  git tag -a "$TAG" -m "hyprlaser $TAG

$notes"
fi

# ── 5. push ─────────────────────────────────────────────────────────────────

say "Pushing main and tag to origin"
run "git push origin main"
run "git push origin $TAG"

# ── 6. GitHub Release (optional) ────────────────────────────────────────────

if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
  say "Creating GitHub Release for $TAG"
  if (( DRY_RUN )); then
    printf '\033[2m   $ gh release create %s --title %s --notes "<extracted changelog>"\033[0m\n' "$TAG" "$TAG"
  else
    gh release create "$TAG" \
      --title "$TAG" \
      --notes "$notes"
  fi
else
  say "Skipping GitHub Release (gh not installed or not authenticated)"
fi

# ── 7. bump AUR PKGBUILD ────────────────────────────────────────────────────

say "Bumping packaging/aur/PKGBUILD"
run "sed -i 's/^pkgver=.*/pkgver=$VERSION/' packaging/aur/PKGBUILD"
run "sed -i 's/^pkgrel=.*/pkgrel=1/' packaging/aur/PKGBUILD"

say "Running updpkgsums to fetch the new tarball hash"
# updpkgsums will hit https://github.com/dcaixinha/hyprlaser/archive/refs/tags/vX.Y.Z.tar.gz
# and rewrite sha256sums in place. Requires the repo to be public, or this
# step will fail with 404.
run_block "cd packaging/aur && updpkgsums"

say "Regenerating .SRCINFO"
run_block "cd packaging/aur && makepkg --printsrcinfo > .SRCINFO"

# ── 8. commit and push AUR bump ─────────────────────────────────────────────

say "Committing AUR bump"
run "git add packaging/aur/PKGBUILD packaging/aur/.SRCINFO"
run "git commit -m 'AUR: bump to v$VERSION'"

say "Pushing AUR bump to origin"
run "git push origin main"

# ── done ────────────────────────────────────────────────────────────────────

echo
if (( DRY_RUN )); then
  say "Dry run complete. Re-run without --dry-run to actually release."
else
  say "Release v$VERSION done!"
  echo
  echo "Next manual step: sync the PKGBUILD + .SRCINFO to your AUR clone:"
  echo "    cd ~/Dev/AUR/hyprlaser"
  echo "    cp $project_root/packaging/aur/{PKGBUILD,.SRCINFO} ."
  echo "    git add PKGBUILD .SRCINFO && git commit -m 'v$VERSION' && git push origin master"
fi
