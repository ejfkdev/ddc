# Binary packaging

This directory is a small, multi-program packaging library for the binaries in
this repository. It works like a Homebrew tap or a Scoop bucket: one catalog
lists programs, and one generator renders the manifests each channel needs.

`ddc` is the first entry. A second binary only needs a new `[[program]]` table;
the AUR, WinGet and npm renderers are already generic.

## Channel status

| Channel | Artifact | Status |
| --- | --- | --- |
| GitHub Releases | raw binaries, eight target triples (includes static musl) | live |
| crates.io | `ddc-cli` | live |
| cargo-binstall | `[package.metadata.binstall]` in `crates/ddc-cli/Cargo.toml` | live and verified |
| Homebrew | `ejfkdev/homebrew-tap` (separate repo, auto-updated) | live |
| Scoop | `ejfkdev/scoop-bucket` (separate repo, auto-updated) | live |
| Arch Linux / AUR | `packaging/aur/ddc-bin/` | AUR pending (registration temporarily closed); PKGBUILD bundle live |
| Windows Package Manager | `packaging/winget/` | live in winget-pkgs (#437825); local zip live |
| npm | `packaging/npm/ddc/` | npmjs live; GitHub Packages live; tarball live |
| PyPI | — | deliberately not used |
| Docker image | — | out of scope |
| Nix | — | not yet; see below |
| Chocolatey | — | not yet; Scoop already covers Windows |

The registry channels are in place: the npm wrapper is live on npmjs and
GitHub Packages and was installed/executed end-to-end (including with install
scripts disabled), the WinGet package is live in `microsoft/winget-pkgs`
(PR #437825, merged 2026-10-30), and the AUR PKGBUILD is generated and
validated. AUR submission is currently blocked because AUR has temporarily
closed new account registration (HTTP 503); the self-contained
`ddc-bin-<version>-1-aur.tar.gz` bundle remains installable with `makepkg`.
The registry-independent npm tarball and WinGet local-manifest zip are also
attached to every release.

**Why no PyPI?** A Python wheel is the wrong container for a native binary:
supporting eight targets means eight platform-tagged wheels, each carrying the
binary, plus a separate release job. npm already covers the "install a CLI with
the language toolchain I have" case with a 3 kB wrapper, so PyPI adds cost
without a new audience.

**Why no Nix yet?** The glibc Linux assets are UPX-compressed and linked
against the distribution glibc, so NixOS would need a patched interpreter.
The static musl assets run directly, but a proper Nix package still belongs
in nixpkgs built from source with `rustPlatform.buildRustPackage` and
`cargoLock`; that is separate from this prebuilt-binary catalog.

## Direct-install assets

Every release also ships registry-independent artifacts. These work today,
without AUR/npm/WinGet credentials:

- **npm tarball** — `ejfkdev-ddc-<version>.tgz`:
  ```bash
  npm install -g https://github.com/ejfkdev/ddc/releases/download/v<version>/ejfkdev-ddc-<version>.tgz
  ```
- **Arch bundle** — `ddc-bin-<version>-1-aur.tar.gz`:
  ```bash
  tar xzf ddc-bin-<version>-1-aur.tar.gz
  cd ddc-bin && makepkg -si
  ```
- **WinGet local manifests** — `ejfkdev.ddc-<version>-winget.zip`:
  unzip it, enable local manifests once as administrator
  (`winget settings --enable LocalManifestFiles`), then
  `winget install --manifest <extracted-directory>`.

`packaging/bundle.py` builds all three plus `.sha256` files. `release.sh`
uploads them to the release after the packaging manifests are committed, so
future versions get the same direct-install paths automatically.

## Layout

```
packaging/
├── programs.toml              # catalog: programs, versions, targets, channels
├── checksums.toml             # generated: release asset SHA-256 and sizes
├── sync.py                    # generator + validator
├── bundle.py                  # direct-install assets (npm / AUR / WinGet)
├── publish.sh                 # AUR / npm publisher, WinGet instructions
├── aur/ddc-bin/               # generated PKGBUILD + .SRCINFO
├── winget/manifests/          # generated WinGet 1.6.0 manifests
└── npm/ddc/                   # generated npm wrapper package
```

`checksums.toml` is generated output. Do not edit it by hand; run `sync.py
--refresh` after a release.

## The catalog

`programs.toml` has one `[[program]]` table per binary:

- `version_from` points at a `Cargo.toml` (workspace inheritance is followed).
  Use `version = "x.y.z"` instead for a binary outside this workspace.
- `asset` describes the release asset name. `{version}` is the tag without
  `v`, `{target}` is the Rust target triple, and `{exe}` is `.exe` on Windows.
- `targets` lists the eight release triples (or fewer for a future program).
- `[program.aur]`, `[program.winget]` and `[program.npm]` select the channels.
  A program without a channel table simply does not render that channel.

To add a program, copy the `[[program]]` block, adjust the metadata and
channel tables, then run:

```bash
python3 packaging/sync.py --refresh --program <name>
```

The release API must already have the assets for the catalog version.

## Release flow

`scripts/release.sh` does this automatically after the release workflow turns
green: it refreshes the digests, regenerates the manifests, commits
`packaging/`, builds/uploads the direct-install assets, and triggers the
GitHub Packages publication. The manual equivalent is:

```bash
python3 packaging/sync.py --refresh --program ddc
python3 packaging/sync.py --check
git add packaging
git commit -m "packaging: ddc $(...)"
```

The release assets are the source of truth. `--refresh` reads GitHub's
`sha256` digest for each asset (falling back to downloading it when the API
does not expose one) and fetches the tagged license file. Rendering is
deterministic; `--check` needs no network and compares bytes.

## Publishing

### Arch Linux (AUR)

The package is `ddc-bin` (a prebuilt package; the `-bin` suffix leaves the
name `ddc` free for a future source package). It supports `x86_64` and
`aarch64`, installs `/usr/bin/ddc` and the license, and sets
`provides`/`conflicts` to `ddc`.

One-time setup:

1. Create an [AUR account](https://aur.archlinux.org/).
2. Add an SSH public key under *My Account*.
3. Push once; AUR creates the package repository on first push.

Then:

```bash
./packaging/publish.sh aur --yes
```

The script validates the generated files, clones
`ssh://aur@aur.archlinux.org/ddc-bin.git` (or initializes it on first
publication), copies `PKGBUILD` and `.SRCINFO`, and pushes `master`. Without
`--yes` it only prints what it would do. The same files are attached to each
release as `ddc-bin-<version>-1-aur.tar.gz` for a manual `makepkg -si`
install.

### Windows Package Manager (WinGet)

The package is live in `microsoft/winget-pkgs` as `ejfkdev.ddc` 0.1.33
(PR [#437825](https://github.com/microsoft/winget-pkgs/pull/437825), merged
2026-10-30; the community manifest uses schema 1.12.0 and adds the VCRedist
dependencies). The generated 1.6.0 manifests under
`packaging/winget/manifests/e/ejfkdev/ddc/<version>/` back the local
`ejfkdev.ddc-<version>-winget.zip` path used by
`winget install --manifest`. The later duplicate PR #466817 was closed.

For a new version, update the community package with either:

**wingetcreate (Windows):**

```powershell
wingetcreate update ejfkdev.ddc --version 0.1.34 `
  --urls https://github.com/ejfkdev/ddc/releases/download/v0.1.34/ddc-v0.1.34-x86_64-pc-windows-msvc.exe `
         https://github.com/ejfkdev/ddc/releases/download/v0.1.34/ddc-v0.1.34-aarch64-pc-windows-msvc.exe `
  --submit
```

**Manual fork:** copy the three YAML files to
`manifests/e/ejfkdev/ddc/<version>/` in a fork of
[`microsoft/winget-pkgs`](https://github.com/microsoft/winget-pkgs) and open a
pull request. `winget validate --manifest <path>` checks it on Windows.

`./packaging/publish.sh winget` prints these instructions with the exact URLs
and hashes for the current version.

### npm

The package is `@ejfkdev/ddc`, a 3 kB wrapper. Its `postinstall` downloads the
release binary for the current platform and verifies the embedded SHA-256; the
`ddc` launcher then executes the native binary. If install scripts are
disabled (or npm blocks them), the launcher downloads on first use instead.
The unscoped `ddc` name is already taken on npm, hence the scope. Until the
registry publication happens, the same tarball is attached to each release;
see [Direct-install assets](#direct-install-assets).

**GitHub Packages (live):** `.github/workflows/publish-github-packages.yml`
publishes the same wrapper with the repository's `GITHUB_TOKEN` and is
triggered by `release.sh`. Install with a GitHub token that has
`read:packages`:

```bash
npm config set @ejfkdev:registry https://npm.pkg.github.com/
npm config set //npm.pkg.github.com/:_authToken "$GITHUB_TOKEN"
npm install -g @ejfkdev/ddc
```

**npmjs (live):** `@ejfkdev/ddc@0.1.33` is published. The publish script
targets `https://registry.npmjs.org/` explicitly, so a local mirror registry
does not redirect the publish, and it preflights `npm whoami`. Log in first:

```bash
npm login --registry=https://registry.npmjs.org/
./packaging/publish.sh npm --yes
```

For unattended publishing, create a granular token with publish permission and
**Bypass 2FA**, then pass it as `NPM_TOKEN`; the script uses a temporary
0600 npmrc and removes it on exit:

```bash
NPM_TOKEN=npm_... ./packaging/publish.sh npm --yes
```

Revoke that token after publishing (the npmjs token page deletes it). A normal
web-login token still asks for an OTP on publish.

Test the exact tarball locally first:

```bash
cd packaging/npm/ddc
npm pack
npm install -g --prefix /tmp/ddc-npm ./ejfkdev-ddc-0.1.34.tgz
/tmp/ddc-npm/bin/ddc --version
```

The install script supports `DDC_BINARY_PATH` for offline installs.

### cargo-binstall

No extra files: `crates/ddc-cli/Cargo.toml` already contains

```toml
[package.metadata.binstall]
pkg-url = "{ repo }/releases/download/v{ version }/ddc-v{ version }-{ target }{ archive-suffix }"
bin-dir = "{ bin }{ binary-ext }"
pkg-fmt = "bin"
```

so `cargo binstall ddc-cli` downloads the raw release binary. This was tested
end-to-end against v0.1.33.

## Validation

`sync.py --check` verifies every generated byte, rejects stale WinGet version
directories, runs `bash -n` on the PKGBUILD, parses every WinGet YAML, checks
JavaScript syntax with `node --check`, and parses `package.json`.
`packaging/bundle.py` additionally builds the npm tarball, AUR bundle and
WinGet zip and refuses to publish an npm tarball that accidentally contains a
downloaded binary. `.github/workflows/packaging.yml` runs the same checks on
packaging changes, builds the bundles, and runs `npm pack --dry-run`.

The WinGet manifests were additionally validated against the official
`manifest.version.1.6.0`, `manifest.defaultLocale.1.6.0` and
`manifest.installer.1.6.0` JSON schemas during development. CI keeps the
structural checks; the full JSON-schema pass only needs `jsonschema` and the
three schema files, so it is easy to repeat locally.

A real `makepkg` run needs an Arch host, so it is not part of this check. The
AUR package is a raw-binary install: one `install` command, a pinned license,
and no build step that could drift from the release asset.

## Security notes

- Every generated manifest pins the asset SHA-256 from the GitHub release API.
- The npm installer verifies that hash after download and refuses to install a
  mismatch.
- AUR sources use the same pinned hashes; `makepkg` verifies them.
- WinGet uses `InstallerSha256` for both architectures.
- `--refresh` uses `GITHUB_TOKEN`/`GH_TOKEN` when present and falls back to the
  authenticated `gh` CLI on rate limits.