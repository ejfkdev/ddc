# Benchmarks

[English] | [简体中文](zh-CN/benchmarks.md)

All numbers from the author's machine (Apple Silicon, 6P+12E cores),
measured after a cooldown, release profile with
`target-cpu=native`. The benchmark harness is kept out of the published tree (local APK paths); the tables
above are the recorded results.

Recorded 2026-11-02 with the current main build (post-v0.1.33): three serial runs per cell (arithmetic mean; peak RSS is the run maximum). Peak RSS is `/usr/bin/time -l`. WhatsApp's full run hits the deadline guard on one pathological class (`X.K2H`) and exits 1 after writing 99,482 files; its timing includes that deadline.

## Full decompile

Seven real-world APKs — reqable 3.2.23 (34MB), Telegram (62MB), WhatsApp
(139MB), weibo 16.9.1 (226MB, 20 dex), weixin 8.0.78 (268MB), lark 8.0.2
(398MB, 59 dex), qq 9.3.65 (374MB) — wall time, peak RSS via `/usr/bin/time -l`,
measured after a cooldown:

| APK | Size | Full decompile | `--fast` | Peak RSS (full) |
|---|---|---|---|---|
| reqable | 34 MB | **0.76s** | 0.46s | 317 MB |
| Telegram | 62 MB | **6.43s** | 3.86s | 931 MB |
| WhatsApp | 139 MB | **23.25s** | 10.34s | 1114 MB |
| weibo | 226 MB | **19.10s** | 9.73s | 1383 MB |
| weixin | 268 MB | **33.20s** | 21.94s | 1519 MB |
| lark | 398 MB | **22.29s** | 12.70s | 2403 MB |
| qq | 374 MB | **47.70s** | 33.51s | 2552 MB |

**Compile validation — read the scope before quoting this number**:
every `.java` of all seven APKs (909,689 files) passes `javac`'s parse
gate (`-XDshould-stop.ifNoError=PARSE -XDshould-stop.ifError=PARSE`)
with **zero syntax errors**. That gate stops at the parser: it does NOT
type-check, resolve symbols, or guarantee the output compiles against a
classpath.

**Semantic quality is measured separately — a javac battery with the
full Android classpath** (android-37 jar + optional libraries) over four
corpora, counting ddc-attributable diagnostics. That battery is the
driver of the post-0.1.4 fix rounds and has gone **11,345 → 7,452
errors (−34%) from v0.1.19 to v0.1.22**, including a multi-handler
exception-routing bug present since the first release (every catch of a
multi-handler method ran the first handler's code — invisible to any
parse gate) and three javac-blind value-loss defects found by reading
the output against the disassembly. Semantic quality still trails jadx
on type recovery (erased generics, register-typed locals in R8 monster
methods) — see "Known limitations" in the README.

## Every query subcommand, same seven APKs

Measured after a cooldown; cells are wall time / peak RSS. Query targets:
`strings -f <package> --with-locations`; `findrefs string <package>`;
`findrefs method onCreate`; `hierarchy`/`disasm`/`getclass` use each
app's launcher class (Telegram's `LaunchActivity` is an exceptionally
large class):

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

## Methodology notes

- **Serial runs after a cooldown.** Same-machine variance reaches ±25% (E-core
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
