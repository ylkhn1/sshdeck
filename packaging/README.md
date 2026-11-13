# Packaging

| Format | Where | Built by |
|---|---|---|
| Arch (AUR) | [`aur/PKGBUILD`](aur/PKGBUILD) | `makepkg`, from the git tag `v$pkgver` |
| `.deb` (Debian 13+, Ubuntu 25.04+) | `[package.metadata.deb]` in `Cargo.toml` | `cargo deb` in a `debian:trixie` container |
| `.rpm` (Fedora 42+) | `[package.metadata.generate-rpm]` in `Cargo.toml` | `cargo generate-rpm` in a `fedora:42` container |
| `.tar.gz` (other x86_64 distros) | [`build-packages.sh`](build-packages.sh) | `cargo build --release` in a `debian:trixie` container |

Each build is told how it was installed through `SSHDECK_INSTALL_KIND` (`deb`, `rpm`, `tarball`, `aur`), which decides how the app updates itself (see "Updates" in the main README). That is why the tarball has its own build instead of reusing the binary from the `.deb`. A plain `cargo build` has no install kind and only reports new versions.

## Signing key (one time)

Release files are signed with [minisign](https://jedisct1.github.io/minisign/). The app embeds the public key from [`minisign.pub`](minisign.pub) and installs an update only if its signature verifies with that key.

> **The `minisign.pub` in the repository is a temporary development key.** Its secret half was only used for tests and is not kept anywhere safe. `build-packages.sh` and `sign-release.sh` refuse to work while it is in place (`check-release-key.sh` lists its key id, `AE5012490955CDF5`). Replace it before the first release with updates:

```sh
sudo pacman -S minisign
mkdir -p ~/.minisign
minisign -G -p packaging/minisign.pub -s ~/.minisign/sshdeck.key   # asks for a password
packaging/check-release-key.sh                                     # prints the new key id
cargo test verify                                                  # the embedded key still parses
git add packaging/minisign.pub && git commit -m "Release signing key"
```

Keep `~/.minisign/sshdeck.key` and its password backed up offline (for example, on an encrypted USB drive and in a password manager). If the secret key is lost, installed copies can no longer verify updates and users have to update by hand once to a build with a new key. If it leaks, anyone can sign an "update": generate a new key, release a version with it, and tell users to install that version manually.

## New release checklist

1. Bump `version` in `Cargo.toml`, commit, tag `vX.Y.Z`, push the tag.
2. Build the `.deb`, the `.rpm` and the tarball (needs Docker); they land in `dist/`:
   ```sh
   packaging/build-packages.sh
   ```
3. Sign them (asks for the key password once; checks each signature with `packaging/minisign.pub`):
   ```sh
   packaging/sign-release.sh X.Y.Z
   ```
4. Create the GitHub release with the 3 files and their 3 `.minisig` signatures. The file names must stay exactly as built: the app looks for `sshdeck-X.Y.Z-x86_64-linux.tar.gz`, `sshdeck_X.Y.Z-1_amd64.deb` and `sshdeck-X.Y.Z-1.x86_64.rpm` plus `.minisig` next to each. Do not mark it as a pre-release, or the app will not offer it:
   ```sh
   cd dist
   gh release create vX.Y.Z \
       sshdeck-X.Y.Z-x86_64-linux.tar.gz sshdeck-X.Y.Z-x86_64-linux.tar.gz.minisig \
       sshdeck_X.Y.Z-1_amd64.deb sshdeck_X.Y.Z-1_amd64.deb.minisig \
       sshdeck-X.Y.Z-1.x86_64.rpm sshdeck-X.Y.Z-1.x86_64.rpm.minisig \
       --title "sshdeck X.Y.Z" --notes-file RELEASE_NOTES.md
   ```
5. Update the AUR package (it builds from the git tag `v$pkgver`, so there is no checksum to refresh):
   ```sh
   cd packaging/aur
   # set pkgver=X.Y.Z and pkgrel=1, then:
   makepkg --printsrcinfo > .SRCINFO
   makepkg -f        # test build after the tag is pushed (add -d if cargo comes from ~/.cargo, not pacman)
   ```
   Then copy `PKGBUILD` and `.SRCINFO` into your clone of `ssh://aur@aur.archlinux.org/sshdeck.git`, commit, and push. Until the package is on the AUR, users update a PKGBUILD build with `git pull && cd packaging/aur && makepkg -si` in their clone of this repository; the app shows that command.

## Testing updates locally

Debug builds (`cargo build`) accept test-only overrides; release builds ignore them:

| Variable | Effect |
|---|---|
| `SSHDECK_UPDATE_URL=http://127.0.0.1:8000` | API and downloads go to a local server instead of GitHub: `<url>/repos/ylkhn1/sshdeck/releases/latest` and `<url>/ylkhn1/sshdeck/releases/download/vX.Y.Z/<file>` |
| `SSHDECK_UPDATE_KIND=deb\|rpm\|aur\|tarball` | pretends the build was made for that install kind; for `deb`/`rpm` it also skips the `/usr/bin` and package manager checks |
| `SSHDECK_UPDATE_PKEXEC=<script>` | runs the script instead of `pkexec` (gets `apt-get install -y <file>` or `dnf install -y <file>` as arguments) |

Serve a fake release with `python3 -m http.server` and sign its files with a throwaway key whose public half you temporarily put into `packaging/minisign.pub`.
