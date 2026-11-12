# Packaging

| Format | Where | Built by |
|---|---|---|
| Arch (AUR) | [`aur/PKGBUILD`](aur/PKGBUILD) | `makepkg`, from the GitHub release source tarball |
| `.deb` (Debian 13+, Ubuntu 25.04+) | `[package.metadata.deb]` in `Cargo.toml` | `cargo deb` in a `debian:trixie` container |
| `.rpm` (Fedora 42+) | `[package.metadata.generate-rpm]` in `Cargo.toml` | `cargo generate-rpm` in a `fedora:42` container |

## New release checklist

1. Bump `version` in `Cargo.toml`, commit, tag `vX.Y.Z`, push the tag.
2. Build `.deb` and `.rpm` (needs Docker); they land in `dist/`:
   ```sh
   packaging/build-packages.sh
   ```
3. Create the GitHub release and attach the packages:
   ```sh
   gh release create vX.Y.Z dist/*.deb dist/*.rpm --title "sshdeck X.Y.Z" --notes-file dist/RELEASE_NOTES.md
   ```
4. Update the AUR package:
   ```sh
   cd packaging/aur
   # set pkgver=X.Y.Z, pkgrel=1, then refresh the checksum and metadata:
   updpkgsums
   makepkg --printsrcinfo > .SRCINFO
   makepkg -f        # test build (add -d if cargo comes from ~/.cargo, not pacman)
   ```
   Then copy `PKGBUILD` and `.SRCINFO` into your clone of `ssh://aur@aur.archlinux.org/sshdeck.git`, commit, and push.
