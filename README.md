# ddc — DEX → Java decompiler in Rust

**English** | [简体中文](README.zh-CN.md)

[![CI](https://github.com/ejfkdev/ddc/actions/workflows/ci.yml/badge.svg)](https://github.com/ejfkdev/ddc/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/ejfkdev/ddc)](https://github.com/ejfkdev/ddc/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/ejfkdev/ddc/total?label=downloads)](https://github.com/ejfkdev/ddc/releases)
[![Latest release downloads](https://img.shields.io/github/downloads/ejfkdev/ddc/latest/total?label=latest%20release)](https://github.com/ejfkdev/ddc/releases/latest)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

[![crates.io](https://img.shields.io/crates/v/ddc-cli)](https://crates.io/crates/ddc-cli)
[![crates.io downloads](https://img.shields.io/crates/d/ddc-cli)](https://crates.io/crates/ddc-cli)
[![npm](https://img.shields.io/npm/v/@ejfkdev/ddc)](https://www.npmjs.com/package/@ejfkdev/ddc)
[![npm downloads](https://img.shields.io/npm/dw/@ejfkdev/ddc)](https://www.npmjs.com/package/@ejfkdev/ddc)
[![Platforms](https://img.shields.io/badge/platform-linux%20%7C%20macOS%20%7C%20Windows-blue)](#install)
[![WinGet](https://img.shields.io/badge/WinGet-ejfkdev.ddc-0078D4?logo=windows)](https://github.com/microsoft/winget-pkgs/tree/master/manifests/e/ejfkdev/ddc)

`ddc` decompiles Android DEX bytecode back into readable Java — at
real-world app scale, queryable like a database, and **quality-first**:
output is checked with `javac`'s parse gate and a full Android classpath,
and every file is simplified by an exact, semantic-preserving source pass.

## Highlights

- **Quality first, not just fast** — a `javac` parse gate and a
  full-classpath battery continuously check the output; every file also
  goes through [cure](https://github.com/ejfkdev/cure), an exact
  simplification that cuts **15% of output lines** with identical gate
  results. The 39-APK validation corpus currently sits at **4,102,811 /
  4,102,812** files parsing clean (one long-literal caveat, see
  [validation](docs/validation.md)).
- **Fast** — a 226 MB / 20-dex APK (weibo, 98k classes) fully decompiles
  in **~19s** (**~9.7s** with `--fast`); Telegram (62 MB) in **~6.4s**
  (**~3.9s**). Pathological classes run on deadline-bounded monitored
  threads instead of hanging the run.
- **Progressive queries** — 15 subcommands (strings, cross-references,
  hierarchies, manifest, resources, method-level decompilation) answer in
  milliseconds: query metadata first, decompile on demand.
- **DEX 035–041, complete** — multi-dex APKs, XAPK/APKS/APKM containers,
  invoke-custom; lambdas and string concats fold back into real Java
  (`(a) -> …`, `Cls::name`, `a + b`).
- **Readable output** — jadx-style local names instead of `v12`; IntDef
  literals render as constants (`setVisibility(8)` → `View.GONE`) with a
  built-in android-37 table; Kotlin null-check noise is elided and
  `access$NNN` bridges inline.
- **Reproducible** — no timestamps; two runs diff cleanly.
- **HTTP REST + MCP, built in** ([xyz-rust](https://github.com/ejfkdev/xyz-rust)) —
  `ddc serve` exposes every query over REST, OpenAPI and a streamable
  `/mcp` endpoint; `ddc mcp` provides 10 MCP tools. Output formats:
  text, table, markdown, JSON/JSONL.
- **Bilingual CLI** — `DDC_LANG=zh|en`, automatic locale detection.
- Built on [`jdc-core`](https://crates.io/crates/jdc-core), shared with
  [jcdc](https://github.com/ejfkdev/jcdc).

## Install

**macOS** (Homebrew):

```bash
brew install ejfkdev/tap/ddc
```

**Windows** (Scoop) — add the bucket once, then install by name (scoop
does not resolve a bare `user/repo/app` form — issue #3):

```powershell
scoop bucket add ejfkdev https://github.com/ejfkdev/scoop-bucket
scoop install ddc

# or install straight from the manifest URL, no bucket needed:
scoop install https://raw.githubusercontent.com/ejfkdev/scoop-bucket/main/bucket/ddc.json
```

**Windows** (WinGet) — the package is live in `microsoft/winget-pkgs`:

```powershell
winget install --id ejfkdev.ddc -e --source winget
```

**Any platform** (npm) — `@ejfkdev/ddc` is a small wrapper that downloads and
SHA-256-verifies the release binary (Node 18+); Linux glibc and fully static
musl builds are selected automatically:

```bash
npm install -g @ejfkdev/ddc
```

If install scripts are disabled, the `ddc` launcher downloads the binary on
first use. GitHub Packages is also available for users with a token that has
`read:packages`:

```bash
npm config set @ejfkdev:registry https://npm.pkg.github.com/
npm config set //npm.pkg.github.com/:_authToken "$GITHUB_TOKEN"
npm install -g @ejfkdev/ddc
```

Without any package registry:

```bash
# replace with the release you want
VERSION=0.1.35
npm install -g "https://github.com/ejfkdev/ddc/releases/download/v$VERSION/ejfkdev-ddc-$VERSION.tgz"
```

**cargo-binstall** (any platform — fetches the prebuilt release binary
instead of compiling):

```bash
cargo binstall ddc-cli
```

**Arch Linux** — install the release bundle manually (requires
`base-devel`); it contains the same files as the `ddc-bin` AUR package:

```bash
# replace with the release you want
VERSION=0.1.35
curl -LO "https://github.com/ejfkdev/ddc/releases/download/v$VERSION/ddc-bin-$VERSION-1-aur.tar.gz"
tar xzf "ddc-bin-$VERSION-1-aur.tar.gz"
cd ddc-bin && makepkg -si
```

**Binaries**: every [release](https://github.com/ejfkdev/ddc/releases)
ships raw binaries (no archives) for `linux-amd64`, `linux-arm64`,
`linux-amd64-musl`, `linux-arm64-musl`, `windows-amd64`, `windows-arm64`,
`macos-amd64` and `macos-arm64` — non-macOS builds UPX-compressed. The musl
builds are fully static and run on Alpine and other musl distributions.

**From source** (Rust stable):

```bash
cargo install ddc-cli        # from crates.io, compiles locally

# or build the checkout:
git clone https://github.com/ejfkdev/ddc && cd ddc
cargo build --release
```

Package manifests and release bundles are generated from
[`packaging/`](packaging/); see [packaging/README.md](packaging/README.md) for
AUR/WinGet/npm status and multi-program support.

## Usage

```bash
ddc app.apk                          # full decompile → app-out/
ddc app.apk -c com.example.Foo       # one class to stdout
ddc app.apk -o - | less              # everything to stdout

ddc info app.apk                      # app context (label via arsc, package,
                                      # version, md5) + per-dex counts
ddc listclasses app.apk              # package tree with class counts
                                      # (--full: raw names)
ddc mainactivity app.apk             # entry point: package + launcher
ddc findrefs app.apk string token    # every const-string "token" site
ddc getmethod app.apk Foo.toString   # one method, all overloads
ddc pkg app.apk --app -o own/        # the app's own code only
```

Full options and examples: `ddc --help` (subcommand menu grouped by
workflow). The complete reference — every option's semantics, every
subcommand's output format and behavior — lives in
[docs/cli.md](docs/cli.md) ([中文](docs/zh-CN/cli.md)).

## Performance

Seven real-world APKs, release build, measured after a
cooldown (Apple Silicon, 6P+12E). The 39-APK validation corpus (4.10M
decompiled files, per-APK wall time / peak RSS / javac parse gate) is
in [docs/validation.md](docs/validation.md)
([中文](docs/zh-CN/validation.md)).

<details>
<summary>Full decompile — 7 real-world APKs</summary>

| APK | Size | Full decompile | `--fast` | Peak RSS (full) |
|---|---|---|---|---|
| reqable | 34 MB | **0.76s** | 0.46s | 317 MB |
| Telegram | 62 MB | **6.43s** | 3.86s | 931 MB |
| WhatsApp | 139 MB | **23.25s** | 10.34s | 1114 MB |
| weibo | 226 MB | **19.10s** | 9.73s | 1383 MB |
| weixin | 268 MB | **33.20s** | 21.94s | 1519 MB |
| lark | 398 MB | **22.29s** | 12.70s | 2403 MB |
| qq | 374 MB | **47.70s** | 33.51s | 2552 MB |


Recorded 2026-11-02 with v0.1.35: three serial runs per cell (arithmetic mean; peak RSS is the run maximum). Peak RSS is `/usr/bin/time -l`. WhatsApp's full run hits the deadline guard on one pathological class (`X.K2H`) and exits 1 after writing 99,482 files; its timing includes that deadline.
</details>

Query subcommands on the same APKs (cells: time / peak RSS;
`strings -f <package> --with-locations`, `findrefs` on the package
string and method `onCreate`, `hierarchy`/`disasm`/`getclass` on each
app's launcher class):

<details>
<summary>Query subcommands — 13 representative commands × 7 APKs</summary>

| Command | reqable | Telegram | WhatsApp | weibo | weixin | lark | qq |
|---|---|---|---|---|---|---|---|
| `info` | 0.10s / 14MB | 0.18s / 24MB | 0.42s / 58MB | 0.67s / 72MB | 0.79s / 60MB | 1.15s / 111MB | 1.15s / 92MB |
| `listclasses` | 0.01s / 19MB | 0.03s / 54MB | 0.04s / 67MB | 0.11s / 214MB | 0.15s / 220MB | 0.16s / 159MB | 0.25s / 205MB |
| `manifest` | 0.00s / 12MB | 0.00s / 19MB | 0.00s / 26MB | 0.01s / 49MB | 0.01s / 37MB | 0.00s / 34MB | 0.01s / 54MB |
| `mainactivity` | 0.02s / 19MB | 0.05s / 69MB | 0.03s / 83MB | 0.16s / 206MB | 0.16s / 214MB | 0.13s / 153MB | 0.17s / 210MB |
| `res` | 0.00s / 12MB | 0.00s / 20MB | 0.00s / 27MB | 0.01s / 57MB | 0.01s / 37MB | 0.00s / 36MB | 0.02s / 57MB |
| `largest` | 0.01s / 19MB | 0.02s / 56MB | 0.04s / 137MB | 0.07s / 176MB | 0.08s / 178MB | 0.10s / 198MB | 0.17s / 287MB |
| `strings` | 0.01s / 20MB | 0.03s / 56MB | 0.11s / 118MB | 0.19s / 176MB | 0.24s / 191MB | 0.13s / 198MB | 0.47s / 297MB |
| `findrefs-string` | 0.02s / 29MB | 0.03s / 80MB | 0.04s / 218MB | 0.07s / 410MB | 0.07s / 446MB | 0.05s / 268MB | 0.12s / 602MB |
| `findrefs-method` | 0.02s / 29MB | 0.05s / 93MB | 0.04s / 221MB | 0.06s / 399MB | 0.07s / 409MB | 0.08s / 494MB | 0.13s / 562MB |
| `members` | 0.01s / 19MB | 0.02s / 56MB | 0.03s / 68MB | 0.08s / 175MB | 0.08s / 185MB | 0.11s / 145MB | 0.17s / 176MB |
| `hierarchy` | 0.01s / 19MB | 0.03s / 53MB | 0.02s / 61MB | 0.09s / 189MB | 0.10s / 194MB | 0.10s / 148MB | 0.13s / 182MB |
| `disasm` | 0.02s / 19MB | 0.05s / 62MB | 0.03s / 58MB | 0.09s / 194MB | 0.13s / 199MB | 0.07s / 122MB | 0.12s / 155MB |
| `getclass` | 0.03s / 33MB | 0.24s / 133MB | 0.22s / 266MB | 0.49s / 407MB | 0.59s / 581MB | 0.77s / 710MB | 1.40s / 884MB |

</details>

## Documentation

| | |
|---|---|
| [Architecture](docs/architecture.md) | crates, the lift/structure/emit pipeline, DEX versions, invoke-custom ([中文](docs/zh-CN/architecture.md)) |
| [CLI reference](docs/cli.md) | every option and subcommand, in detail ([中文](docs/zh-CN/cli.md)) |
| [Subcommands](docs/subcommands.md) | the progressive-analysis reference ([中文](docs/zh-CN/subcommands.md)) |
| [Benchmarks](docs/benchmarks.md) | timings, memory, methodology, ASC comparison ([中文](docs/zh-CN/benchmarks.md)) |
| [Performance engineering](docs/optimization.md) | 5min → 6s: six rounds, measured dead ends ([中文](docs/zh-CN/optimization.md)) |
| [Corpus validation](docs/validation.md) | 39 real APKs, 4.10M files, javac parse gate (one long-literal caveat), bugs found & fixed ([中文](docs/zh-CN/validation.md)) |

## Known limitations

Generic signatures are not reconstructed: fields and methods print raw
`List`/`Map`, and class type parameters are dropped. d8-desugared
`-$$Lambda$` classes stay separate files; a few R8 monster methods hit the
deadline guard (one WhatsApp 9.3.65 class still does), so a truncated class
can vary between runs; Java 21 pattern switches still render as desugared
`switchDispatch` chains and are not fully reconstructed. The javac gate is
a **syntax** gate: missing Android classpath and rare type-confused locals
remain semantic diagnostics.
A full-classpath javac battery puts ddc at roughly 7,500 attributable
errors across 0.91M files; jadx is about half, the gap concentrated in type
recovery. Details: [architecture](docs/architecture.md).

## Testing

```bash
cargo test    # 101 tests
```

## License

[MIT](LICENSE) © ejfkdev

<div align="center">
<sub><a href="README.zh-CN.md"> 简体中文</a> · 友情链接 https://linux.do </sub>
</div>
