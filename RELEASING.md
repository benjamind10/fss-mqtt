# Releasing

## Cutting a release

1. Bump `version` in `Cargo.toml`, run `cargo build` (updates `Cargo.lock`), commit.
2. Tag and push:
   ```sh
   git tag v0.2.0
   git push origin main v0.2.0
   ```
3. The **Release** workflow checks the tag matches `Cargo.toml`, then builds and uploads to a
   GitHub release:

   | Asset | For |
   |---|---|
   | `fss-mqtt-<v>-x86_64-unknown-linux-musl.tar.gz` | Linux x86_64 (static, any distro) |
   | `fss-mqtt-<v>-aarch64-unknown-linux-musl.tar.gz` | Linux arm64 (static) |
   | `fss-mqtt_<v>-1_amd64.deb`, `fss-mqtt_<v>-1_arm64.deb` | Debian/Ubuntu: `sudo apt install ./fss-mqtt_*.deb` |
   | `fss-mqtt-<v>-1.x86_64.rpm`, `fss-mqtt-<v>-1.aarch64.rpm` | Fedora/RHEL/openSUSE: `sudo dnf install ./fss-mqtt-*.rpm` |
   | `fss-mqtt-<v>-{x86_64,aarch64}-apple-darwin.tar.gz` | macOS |
   | `fss-mqtt-<v>-x86_64-pc-windows-msvc.zip` | Windows x64 (also runs on Windows on Arm) |
   | `SHA256SUMS` | checksums for all of the above |

4. If configured (below), it then updates the Homebrew tap and Scoop bucket and opens a winget PR.

The `aarch64-unknown-linux-musl` build uses GitHub's `ubuntu-24.04-arm` runner, which is free for
public repositories. For a private repository, remove that matrix entry or use a paid plan.

## One-time setup for package managers

Each channel is off until its repository **variable** is set
(Settings → Secrets and variables → Actions → Variables); its key or token goes under **Secrets**.

### Homebrew (macOS and Linux) and Scoop (Windows)

Set up for this repository: `mbilling/homebrew-tap` and `mbilling/scoop-bucket`. Each release
pushes `Formula/fss-mqtt.rb` and `bucket/fss-mqtt.json` there; nothing to maintain by hand.

The workflow pushes with a **deploy key** per repository: an SSH key that can write to that one
repository only and doesn't expire. To set it up again (e.g. under another owner):

```sh
ssh-keygen -t ed25519 -N "" -f tap-key
gh repo deploy-key add tap-key.pub -R <owner>/homebrew-tap --allow-write --title "fss-mqtt release workflow"
gh secret set HOMEBREW_TAP_KEY -R <owner>/fss-mqtt < tap-key
gh variable set HOMEBREW_TAP -R <owner>/fss-mqtt --body "<owner>/homebrew-tap"
rm tap-key tap-key.pub
# same for scoop-bucket with SCOOP_BUCKET_KEY / SCOOP_BUCKET
```

Users install with:

```sh
brew install mbilling/tap/fss-mqtt
```
```powershell
scoop bucket add mbilling https://github.com/mbilling/scoop-bucket
scoop install fss-mqtt
```

### Omarchy (OmaMQTT)

On [Omarchy](https://omarchy.org), fss-mqtt is packaged as `omamqtt` for Omarchy's package
repository: the recipe is `pkgbuilds/omamqtt` in
[omacom/omarchy-pkgs](https://github.com/omacom/omarchy-pkgs). A bot there watches this
repository's GitHub releases and proposes each new version itself, so a release needs nothing more.
The recipe builds the release's source and installs `packaging/omarchy/omamqtt.desktop` and
`omamqtt.svg` from it: moving those means changing the recipe too.

### winget (Windows)

winget packages live in [microsoft/winget-pkgs](https://github.com/microsoft/winget-pkgs) and every
version is a pull request that Microsoft reviews (usually within a day or two).

1. Fork `microsoft/winget-pkgs` to the account that owns the token.
2. Submit the **first** version by hand, after the first GitHub release exists. The easiest way is
   [Komac](https://github.com/russellbanks/Komac):
   ```sh
   komac new <Publisher>.FssMqtt --version 0.1.0 \
     --urls https://github.com/<owner>/<repo>/releases/download/v0.1.0/fss-mqtt-0.1.0-x86_64-pc-windows-msvc.zip
   ```
   Choose installer type `zip` with nested type `portable` and command alias `fss-mqtt`.
3. Once that PR is merged: variable `WINGET_ID` = `<Publisher>.FssMqtt`; secret `WINGET_TOKEN` =
   a **classic** token with `public_repo` scope (needed to open PRs against winget-pkgs).
4. Later releases are submitted automatically. Users install with:
   ```powershell
   winget install <Publisher>.FssMqtt
   ```

## Not covered (yet)

- An apt/dnf **repository** (`apt install fss-mqtt` without downloading a file first). That needs
  hosting and signing, e.g. via packagecloud or Cloudsmith, or a distribution's official archive.
- AUR (Arch), Snap and Flatpak, Chocolatey, and crates.io.
- Code signing for Windows and macOS. Unsigned binaries work, but Windows SmartScreen may warn on
  first run of a downloaded zip; Homebrew, Scoop and winget installs are unaffected.
