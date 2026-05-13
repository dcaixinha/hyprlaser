# AUR packaging

This directory holds the [Arch User Repository](https://aur.archlinux.org)
PKGBUILD and metadata for `hyprlaser`.

## Files

- `PKGBUILD` — the package build recipe. Source: GitHub release tarball
  at the `v$pkgver` tag.
- `.SRCINFO` — generated metadata file required by the AUR. Always
  regenerate after editing the PKGBUILD with:
  ```sh
  makepkg --printsrcinfo > .SRCINFO
  ```

## Releasing a new version

1. Bump `version` in the workspace `Cargo.toml`, commit, tag, push.
2. In this directory, bump `pkgver` (reset `pkgrel` to `1`) in
   `PKGBUILD`.
3. Run `updpkgsums` to fetch the new release tarball and write the real
   `sha256sums` value.
4. Regenerate `.SRCINFO`.
5. Build locally to confirm:
   ```sh
   makepkg -fs
   ```
6. Push the AUR git repo (see "Submitting to the AUR" below).

## Submitting to the AUR

The AUR is a separate git host (`ssh://aur@aur.archlinux.org/<pkg>.git`).
The standard workflow is to keep the AUR repo as a parallel checkout
containing only `PKGBUILD` and `.SRCINFO`:

```sh
git clone ssh://aur@aur.archlinux.org/hyprlaser.git aur-hyprlaser
cd aur-hyprlaser
cp ../hyprlaser/packaging/aur/{PKGBUILD,.SRCINFO} .
git add PKGBUILD .SRCINFO
git commit -m "v$pkgver"
git push
```

## Building / installing locally without the AUR

From a clone of this repo:

```sh
cd packaging/aur
makepkg -si      # build + install via pacman
```

`makepkg` will fetch the source tarball from GitHub and verify the
sha256 before building.
