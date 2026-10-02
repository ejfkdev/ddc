# ddc — DEX → Java decompiler in Rust

**English** | [简体中文](README.zh-CN.md)

[![CI](https://github.com/ejfkdev/ddc/actions/workflows/ci.yml/badge.svg)](https://github.com/ejfkdev/ddc/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

`ddc` decompiles Android DEX bytecode back into readable Java — at
real-world app scale, queryable like a database, and **javac-verified**:
every one of the 909,689 files it emits across seven real-world APKs
parses cleanly.

## Highlights

- **Fast** — a 226 MB / 20-dex APK (weibo, 98k classes) fully
  decompiles in **~13s** at high quality, a 62 MB app (Telegram) in
  **~4.5s**; pathological classes run on deadline-bounded monitored
  threads instead of hanging the run.
- **Compiles** — the full output of all seven benchmark APKs
  (reqable, Telegram, WhatsApp, weibo, weixin, qq, lark — 0.91M
  files) passes `javac` with **zero syntax errors**; every class of
  a case-variant name pair (`X/Cua` vs `X/cua`) is preserved as
  its own file instead of the last one silently overwriting.
- **Progressive decompilation** — 20+ query subcommands (strings,
  cross-references, hierarchies, manifest, resources, per-method
  decompiles) answer in milliseconds: query metadata first, decompile
  on demand, skip the full run entirely.
- **DEX 035–041, complete** — multi-dex APKs, XAPK/APKS/APKM
  containers, invoke-custom; lambdas and string concats fold back into
  real Java (`(a) -> …`, `Cls::name`, `a + b`).
- **Readable output** — jadx-style local names (`str`, `getName() →
  name`, Kotlin `Intrinsics` parameter strings) instead of `v12`;
  IntDef literals render as their constants out of the box
  (`setVisibility(8)` → `View.GONE`; a built-in android-37 domain
  table, `--symbols` overrides); Kotlin null-check noise is elided and
  synthetic `access$NNN` bridges inline at their call sites.
- **Reproducible output** — no timestamps; two runs diff cleanly.
- **Bilingual CLI** — messages localize automatically
  (`DDC_LANG=zh|en` forces; English fallback).
- Built on [`jdc-core`](https://crates.io/crates/jdc-core), the
  machine-neutral core shared with
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

**cargo-binstall** (any platform — fetches the prebuilt release binary
instead of compiling):

```bash
cargo binstall ddc-cli
```

**Binaries**: every [release](https://github.com/ejfkdev/ddc/releases)
ships raw binaries (no archives) for `linux-amd64`, `linux-arm64`,
`windows-amd64`, `windows-arm64`, `macos-amd64` and `macos-arm64` —
non-macOS builds UPX-compressed.

**From source** (Rust stable):

```bash
git clone https://github.com/ejfkdev/ddc && cd ddc
cargo build --release
```

## Usage

```bash
ddc app.apk                          # full decompile → app-out/
ddc app.apk -c com.example.Foo       # one class to stdout
ddc app.apk -o - | less              # everything to stdout

ddc info app.apk                      # app context (label via arsc, package,
                                      # version, md5) + per-dex counts
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
cooldown (Apple Silicon, 6P+12E). The 39-APK validation corpus (4.08M
decompiled files, per-APK wall time / peak RSS / javac parse gate) is
in [docs/validation.md](docs/validation.md)
([中文](docs/zh-CN/validation.md)).

<details>
<summary>Full decompile — 7 real-world APKs</summary>

| APK | Size | Full decompile | Peak RSS |
|---|---|---|---|
| reqable | 34 MB | **0.57s** | 162 MB |
| Telegram | 62 MB | **4.50s** | 786 MB |
| WhatsApp | 139 MB | **13.6s** (99,277 files — case-variant class pairs all preserved) | 970 MB |
| weibo | 226 MB | **13.5s** | 1173 MB |
| weixin | 268 MB | **32.7s** | 1401 MB |
| lark | 398 MB | **16.1s** | 2226 MB |
| qq | 374 MB | **37.1s** | 2268 MB |

</details>

Query subcommands on the same APKs (cells: time / peak RSS;
`strings -f <package> --with-locations`, `findrefs` on the package
string and method `onCreate`, `hierarchy`/`disasm`/`getclass` on each
app's launcher class):

<details>
<summary>Query subcommands — 13 commands × 7 APKs</summary>

| Command | reqable | Telegram | WhatsApp | weibo | weixin | lark | qq |
|---|---|---|---|---|---|---|---|
| `info` | 0.12s / 40MB | 0.21s / 88MB | 0.42s / 305MB | 0.65s / 610MB | 0.79s / 550MB | 1.10s / 907MB | 1.08s / 1240MB |
| `listclasses` | 0.04s / 21MB | 0.06s / 84MB | 0.08s / 93MB | 0.13s / 396MB | 0.21s / 417MB | 0.20s / 299MB | 0.27s / 391MB |
| `manifest` | 0.03s / 7MB | 0.03s / 14MB | 0.02s / 20MB | 0.03s / 43MB | 0.04s / 31MB | 0.03s / 28MB | 0.03s / 47MB |
| `mainactivity` | 0.04s / 24MB | 0.06s / 86MB | 0.08s / 224MB | 0.10s / 358MB | 0.13s / 371MB | 0.14s / 421MB | 0.21s / 554MB |
| `res` | 0.02s / 7MB | 0.03s / 15MB | 0.03s / 22MB | 0.05s / 60MB | 0.04s / 32MB | 0.04s / 31MB | 0.05s / 53MB |
| `largest` | 0.05s / 29MB | 0.08s / 78MB | 0.15s / 236MB | 0.31s / 500MB | 0.32s / 500MB | 0.46s / 676MB | 0.66s / 810MB |
| `strings` | 0.05s / 24MB | 0.10s / 85MB | 0.21s / 222MB | 0.39s / 354MB | 0.45s / 395MB | 0.52s / 431MB | 0.81s / 551MB |
| `findrefs-string` | 0.04s / 26MB | 0.05s / 70MB | 0.06s / 214MB | 0.10s / 403MB | 0.12s / 446MB | 0.07s / 335MB | 0.14s / 871MB |
| `findrefs-method` | 0.05s / 26MB | 0.07s / 70MB | 0.07s / 219MB | 0.09s / 410MB | 0.10s / 458MB | 0.10s / 620MB | 0.15s / 896MB |
| `members` | 0.07s / 23MB | 0.21s / 86MB | 0.62s / 222MB | 1.32s / 366MB | 1.17s / 381MB | 1.64s / 443MB | 2.66s / 572MB |
| `hierarchy` | 0.04s / 24MB | 0.05s / 86MB | 0.09s / 212MB | 0.12s / 396MB | 0.14s / 386MB | 0.17s / 429MB | 0.22s / 548MB |
| `disasm` | 0.04s / 23MB | 0.07s / 88MB | 0.09s / 214MB | 0.12s / 371MB | 0.13s / 392MB | 0.16s / 422MB | 0.20s / 533MB |
| `getclass` | 0.05s / 34MB | 0.28s / 182MB | 0.14s / 357MB | 0.33s / 740MB | 0.37s / 942MB | 0.50s / 1235MB | 0.76s / 1651MB |

</details>

## Documentation

| | |
|---|---|
| [Architecture](docs/architecture.md) | crates, the lift/structure/emit pipeline, DEX versions, invoke-custom ([中文](docs/zh-CN/architecture.md)) |
| [CLI reference](docs/cli.md) | every option and subcommand, in detail ([中文](docs/zh-CN/cli.md)) |
| [Subcommands](docs/subcommands.md) | the progressive-analysis reference ([中文](docs/zh-CN/subcommands.md)) |
| [Benchmarks](docs/benchmarks.md) | timings, memory, methodology, ASC comparison ([中文](docs/zh-CN/benchmarks.md)) |
| [Performance engineering](docs/optimization.md) | 5min → 6s: six rounds, measured dead ends ([中文](docs/zh-CN/optimization.md)) |
| [Corpus validation](docs/validation.md) | 39 real APKs, 4.08M files, javac parse-clean, bugs found & fixed ([中文](docs/zh-CN/validation.md)) |

## Known limitations

Erased types (DEX has no Signature); d8-desugared `-$$Lambda$` classes
stay separate files; a few R8 monster methods degrade via timeout (and
a handful of deadline-truncated ones can vary between runs);
pattern-switch renders as desugared dispatch chains. The javac gate
above is a SYNTAX gate — semantic diagnostics (missing Android
classpath, and in rare register-heavy monster methods a type-confused
local) remain in the output. For scale: a full-classpath javac battery
over four corpora currently sits at ~7,500 ddc-attributable errors
across 0.91M files (v0.1.19 measured 11,345 — see
[benchmarks](docs/benchmarks.md)); jadx on the same battery lands at
roughly half that, the gap concentrated in type recovery. Details:
[architecture](docs/architecture.md).

## Testing

```bash
cargo test    # 74 tests
```

## License

[MIT](LICENSE) © ejfkdev

<div align="center">
<sub><a href="README.zh-CN.md"> 简体中文</a> · 友情链接 https://linux.do </sub>
</div>
