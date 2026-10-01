# Benchmarks

[English] | [简体中文](zh-CN/benchmarks.md)

All numbers from the author's machine (Apple Silicon, 6P+12E cores),
3-run averages, release profile with `target-cpu=native`. The benchmark
harness is kept out of the published tree (local APK paths); the tables
above are the recorded results.

## Full decompile

Seven real-world APKs — reqable 3.2.23 (34MB), Telegram (62MB), WhatsApp
(139MB), weibo 16.9.1 (226MB, 20 dex), weixin 8.0.78 (268MB), lark 8.0.2
(398MB, 59 dex), qq 9.3.65 (374MB) — 3-run averages of wall time, peak
RSS via `/usr/bin/time -l`:

| APK | Size | Full decompile | Peak RSS |
|---|---|---|---|
| reqable | 34 MB | **0.57s** | 162 MB |
| Telegram | 62 MB | **4.50s** | 786 MB |
| WhatsApp | 139 MB | **29.8s** (99,277 files — case-variant class pairs all preserved) | 970 MB |
| weibo | 226 MB | **13.5s** | 1173 MB |
| weixin | 268 MB | **32.7s** | 1401 MB |
| lark | 398 MB | **16.1s** | 2226 MB |
| qq | 374 MB | **37.1s** | 2268 MB |

**Compile validation — read the scope before quoting this number**:
every `.java` of all seven APKs (909,689 files) passes `javac`'s parse
gate (`-XDshould-stop.ifNoError=PARSE -XDshould-stop.ifError=PARSE`)
with **zero syntax errors**. That gate stops at the parser: it does NOT
type-check, resolve symbols, or guarantee the output compiles against a
classpath. Semantic quality (type recovery, cross-references) trails
jadx — see "Known limitations" in the README. Symbol/typing error
counts under full `javac` compilation are tracked separately and have
been the driver of the post-0.1.4 fix rounds (duplicate declarations:
lark 22,316 → 14, weibo 14,871 → 3, null-into-local `str = 0` family
root-caused and fixed in 0.1.6).

## Every query subcommand, same seven APKs

3-run averages; cells are wall time / peak RSS. Query targets:
`strings -f <package> --with-locations`; `findrefs string <package>`;
`findrefs method onCreate`; `hierarchy`/`disasm`/`getclass` use each
app's launcher class (Telegram's `LaunchActivity` is an exceptionally
large class):

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

## Methodology notes

- **Serial, 3-round medians.** Same-machine variance reaches ±25% (E-core
  scheduling + memory bandwidth contention); a single run proves nothing.
- **`/usr/bin/time`'s user time includes all threads** — a "busy" worker
  is not CPU (write-back-pressured threads tick wall time without cycles).
  Look at user+sys ÷ effective cores vs wall; the gap is scheduling/blocking.
- **Wall-clock critical paths migrate**: first CPU, then disk writes, then
  deadline idling on timed-out classes. Re-profile every round; don't keep
  optimizing the old hotspot.
- `DDC_PHASES=1` (phase timings, cumulative — difference them) and
  `DDC_BUCKETS=1` (per-class by block count) answer "which methods eat
  CPU" more directly than stack sampling; `sample <pid> 3 -file` early and
  late (different phases, different hotspots) when you need line-level
  attribution. RSS curves via `ps -o rss= -p $PID` every 5s locate the
  allocation phase (lark: 733MB at t=5s → 2086MB at t=10s = materialize
  peak, before any retirement fires). `/usr/bin/time -l` reports RSS in
  **bytes, not KB**.

## vs. ASC (earlier APK snapshot, same machine, alternating 3-round medians)

Five real APKs (62–353 MB), progressive queries, stdout discarded;
correctness cross-checked: 107/107 hits coincide:

| Query | ddc | ASC | Ratio |
|---|---|---|---|
| findrefs string (lark 353MB) | **0.32s** | 0.61s | 1.9× |
| findrefs string (weixin 268MB) | **0.22s** | 0.52s | 2.4× |
| findrefs string (weibo 226MB) | **0.19s** | 0.44s | 2.3× |
| findrefs type/method (all five) | **0.09–0.31s** | 0.29–0.66s | 2–3× |
| getclass (lark/weixin/weibo) | 0.18–0.42s | **0.10–0.12s** | ASC wins |
| getclass (Telegram, big class) | **0.05s** | 0.11s | ddc wins |
| findrefs memory (lark) | ~820MB | ~170MB | ASC leaner |

The getclass split: ASC bit-stream-probes the zip to inflate only the
target dex (a lower floor on huge multi-dex APKs); ddc parses all image
headers (~0.2s floor) then materializes lazily — it overtakes when the
class itself is expensive, loses to the floor when it isn't. On findrefs
scan domains ddc is uniformly faster (pipeline: parse waves → bounded
channel → scan threads consume-and-drop; `Arc`-shared APK bytes eliminate
compression copies).
