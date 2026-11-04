**Re-run on 2026-11-05 with ddc 0.1.35: 39 real-world Android apps, 6.4 GB, 5,409,658 classes, 4,102,812 decompiled files, 4,102,811 parse clean (one QQ long-literal error).**

One serial run per APK on the same Apple Silicon machine; wall time and
peak RSS from `/usr/bin/time -l`. The full decompile writes to a temporary
directory, then every `.java` is parsed in batches with javac's parse phase
(`-XDshould-stop.ifNoError/ifError=PARSE`). Other builds/tests were running on
the machine, so absolute wall times are order-of-magnitude only.

## Bugs found (and fixed) during validation

In the first validation round (2026-09-19, v0.1.2), 27 of 39 APKs failed the javac
parse gate, exposing seven real defects (all fixed, each verified):

| # | Symptom (first-pass errors) | Root cause | Fix |
|---|---|---|---|
| 1 | 2,518–46,774 errors/APK on renamed classes (MinisApp, WhatsApp, AutoClaw, 22 APKs total) | After a case-variant rename (`Y2.a` vs `y2.a` → `y2.a_2`), the constructor and file name took the new name but the **class header did not** — `a_2(...)` inside `class a` reads as a method with no return type | `emit_class_body` header follows the rename |
| 2 | Alipay >500k errors (hit the -Xmaxerrs cap) | A field/param named with a single non-ASCII char (`支`) — the identifier sanitizer maps every illegal char to `_`, and a one-char name lands on **bare `_`** (reserved since Java 9) | Both `java_ident`s escape a `_` result to `__` |
| 3 | Taobao 1 / DingTalk 20 errors | Classes named `var`-style restricted contextual identifiers (forbidden in type declarations since Java 10) | `var/yield/record/sealed/permits` join the keyword escape lists (declaration/reference/filename stay consistent) |
| 4 | nova (Doubao) 46 errors | Digit-start field names `1x`/`2x` — the declaration side escaped them, the **jdc-core reference side did not** | jdc-core `java_ident` gains the digit-start branch |
| 5 | Zhihu (bangcle) 423 / DingTalk residuals | The segment sanitizers for type references (`sanitize_fq`/`sanitize_source_name`) lacked the lone-underscore guard | Same guard as bug 2, applied to the segment sanitizers |
| 6 | DingTalk residual 5 | R8 class names ending in `$` (`ThreadMsg$$$`) — `rsplit('$')` yields an **empty segment**, and the qualified-new rendered `v9.new (args)` | Three empty-`$`-segment sites fall back to the full simple name |
| 7 | UU remote: whole run aborted | A fake `.dex` in `assets/` (encrypted payload, bad magic) — one bad image killed the full decompile | `parse_images` skips unparsable images (stderr notice); errors only when nothing parses |

## Full decompile

| App | Package | Version | Size | MD5 | DEX | Classes | Wall | Peak RSS | Files | Parse errors |
|---|---|---|---|---|---|---|---|---|---|---|
| 滴滴 | `com.sdu.didi.psnger` | 8.0.14 | 70 MB | `938b1d94` | 8 | 85,460 | 9.18s | 893 MB | 63,039 | 0 |
| 高德地图 | `com.autonavi.minimap` | 17.00.0.2005 | 186 MB | `e63b59b0` | 8 | 73,064 | 8.24s | 807 MB | 52,954 | 0 |
| 学信网 | `cn.com.chsi.chsiapp` | 2.5.13 | 130 MB | `5222c338` | 1 | 4 | 0.08s | 36 MB | 4 | 0 |
| 中国移动 | `com.greenpoint.android.mc10086.activity` | 12.5.4 | 219 MB | `d32b44be` | 1 | 7 | 0.04s | 31 MB | 6 | 0 |
| 招商银行 | `cmb.pb` | 14.4.0 | 159 MB | `6baf69c9` | 14 | 41,051 | 10.46s | 766 MB | 30,746 | 0 |
| 智谱清言 | `com.zhipuai.qingyan` | 3.7.9 | 98 MB | `f11f004a` | 3 | 29,329 | 3.20s | 482 MB | 18,540 | 0 |
| Minis | `com.openminis.app` | 1.13 | 39 MB | `c0d167a7` | 1 | 7,772 | 3.42s | 957 MB | 7,748 | 0 |
| 网易云音乐 | `com.netease.cloudmusic` | 9.5.95 | 224 MB | `b38a4723` | 21 | 177,777 | 23.00s | 1426 MB | 101,390 | 0 |
| Telegram | `org.telegram.messenger.web` | 12.10.2 | 62 MB | `f4abf9c8` | 4 | 20,134 | 8.75s | 923 MB | 19,282 | 0 |
| 京东 | `com.jingdong.app.mall` | 16.0.0 | 85 MB | `1a78ab63` | 14 | 84,489 | 16.75s | 1056 MB | 61,634 | 0 |
| WhatsApp | `com.whatsapp` | 2.26.36.75 | 139 MB | `569cf81e` | 12 | 100,350 | 31.38s | 1085 MB | 99,482 | 0 |
| 支付宝 | `com.eg.android.AlipayGphone` | 12.12.26.8100 | 177 MB | `7fb233eb` | 21 | 179,460 | 43.45s | 1842 MB | 147,917 | 0 |
| AutoClaw | `com.zai.autoclaw` | 1.5.0 | 64 MB | `b80a00ed` | 2 | 20,852 | 5.29s | 648 MB | 20,077 | 0 |
| AutoGLM | `com.zhipu.agent` | 2.0.20 | 122 MB | `55086238` | 4 | 43,516 | 9.79s | 1217 MB | 33,296 | 0 |
| 懂车帝 | `com.ss.android.auto` | 9.0.8 | 174 MB | `e3148f92` | 27 | 213,523 | 40.95s | 1887 MB | 163,517 | 0 |
| 抖音 | `com.ss.android.ugc.aweme` | 40.5.0 | 339 MB | `bb10859f` | 54 | 527,000 | 143.36s | 2705 MB | 518,483 | 0 |
| 百度 | `com.baidu.searchbox` | 15.76.0.10 | 160 MB | `d43d5c5d` | 21 | 185,901 | 25.19s | 1325 MB | 99,574 | 0 |
| DeepSeek | `com.deepseek.chat` | 2.5.2 | 14 MB | `538e8991` | 3 | 13,311 | 3.07s | 556 MB | 13,015 | 0 |
| 知乎 | `com.zhihu.android` | 11.10.0 | 163 MB | `d2e27da3` | 18 | 167,486 | 22.74s | 1219 MB | 107,135 | 0 |
| Meituan | `com.sankuai.meituan` | 12.66.202 | 98 MB | `c368a84e` | 12 | 118,168 | 15.08s | 955 MB | 76,048 | 0 |
| 哔哩哔哩 | `tv.danmaku.bili` | 9.12.0 | 207 MB | `22eeb540` | 32 | 302,829 | 47.75s | 2032 MB | 221,420 | 0 |
| Kimi | `com.moonshot.kimichat` | 3.1.0 | 34 MB | `2196a886` | 3 | 29,752 | 6.54s | 879 MB | 27,946 | 0 |
| Lark | `com.larksuite.suite` | 8.0.2 | 398 MB | `ad291240` | 49 | 292,118 | 29.32s | 2084 MB | 118,502 | 0 |
| MiniMax | `com.xproducer.yingshiai` | 4.11.1 | 33 MB | `b453a746` | 10 | 30,505 | 6.73s | 676 MB | 18,196 | 0 |
| 拼多多 | `com.xunmeng.pinduoduo` | 8.17.0 | 95 MB | `9bb8ba84` | 6 | 57,336 | 6.40s | 688 MB | 39,069 | 0 |
| Toutiao | `com.ss.android.article.news` | 18.5.0 | 176 MB | `2855fb2f` | 33 | 306,489 | 54.50s | 1919 MB | 286,768 | 0 |
| 豆包 | `com.larus.nova` | 15.1.0 | 395 MB | `62d70e63` | 40 | 474,226 | 72.38s | 3132 MB | 346,129 | 0 |
| QQ | `com.tencent.mobileqq` | 9.3.65 | 374 MB | `8e1f7337` | 41 | 428,076 | 58.95s | 2325 MB | 329,101 | 1 |
| Reqable | `com.reqable.android` | 3.2.23 | 34 MB | `948d4b61` | 1 | 8,201 | 0.94s | 304 MB | 5,579 | 0 |
| DingDing | `com.alibaba.android.rimet` | 8.5.5 | 317 MB | `0833d290` | 33 | 193,909 | 27.63s | 1730 MB | 121,830 | 0 |
| 学习通 | `com.chaoxing.mobile` | 7.0.4 | 248 MB | `64fd5211` | 2 | 558 | 0.24s | 197 MB | 380 | 0 |
| 淘宝 | `com.taobao.taobao` | 10.66.10 | 90 MB | `12525ac9` | 13 | 102,146 | 45.44s | 1166 MB | 72,150 | 0 |
| 同花顺 | `com.hexin.plat.android` | 11.62.03 | 198 MB | `20feb874` | 11 | 72,014 | 10.51s | 736 MB | 50,836 | 0 |
| 剪映 | `com.lemon.lv` | 21.5.0 | 438 MB | `39c03cf8` | 46 | 329,997 | 97.07s | 2709 MB | 272,325 | 0 |
| Weibo | `com.sina.weibo` | 16.9.1 | 226 MB | `20117d66` | 20 | 145,492 | 25.28s | 1334 MB | 110,525 | 0 |
| WeChat | `com.tencent.mm` | 8.0.78 | 268 MB | `846b4111` | 17 | 249,909 | 40.46s | 1476 MB | 239,600 | 0 |
| 小红书 | `com.xingin.xhs` | 8.84.5.5 | 112 MB | `418e60cc` | 14 | 170,579 | 61.58s | 2237 MB | 116,844 | 0 |
| 星野 | `com.xingye.app` | 2.56.702 | 146 MB | `757cfe55` | 38 | 108,449 | 23.92s | 1537 MB | 74,591 | 0 |
| UU远程 | `com.netease.uuremote` | 4.40.0 | 25 MB | `de426ecb` | 2 | 18,419 | 3.51s | 633 MB | 17,134 | 0 |

## Test time and version

- **Tested**: 2026-11-05
- **Version**: ddc 0.1.35 (release binary)
- **Machine**: Apple Silicon 6P+12E; one serial run per APK
- **Method**: full decompile into a temporary directory; wall time and peak RSS from
  `/usr/bin/time -l`; every `.java` parsed in batches with
  `javac -XDshould-stop.ifNoError=PARSE -XDshould-stop.ifError=PARSE`.
  Other builds/tests were running on the machine, so wall times are order-of-magnitude only.

Totals: 4,102,812 files, 1 syntax error, 1 failed javac batch.
Failed APKs: WhatsApp(rc=1), 小红书(rc=1)

## Query subcommands

Current query latency numbers (7 APKs × 13 representative commands, three serial
runs) live in the [README Performance section](../README.md#performance) and
[benchmarks](benchmarks.md). This page covers the 39-APK full-decompile and
syntax-validation corpus only.

## Notes

- **Packed APKs**: CHSI/CM10086 carry 4–7 classes, UU remote/Chaoxing a few
  hundred — the shell dex holds only the loader; real code lives in the encrypted
  payload. Zhihu's and CMB's launcher classes are outside the parsable dex
  (`getclass` returns a clear not-found; every other subcommand works).
- **Pathological timeout**: WhatsApp still has one R8 monster class hit the
  deadline guard (`X.K2H`, rc=1); that class is dropped and the other
  99,482 files emit. Monitored threads + a method deadline are the
  documented design, not a crash.
- **Fake dex skip**: UU remote's `assets/39285EFA.dex` is an encrypted payload
  with a non-DEX magic — skipped with a notice; the real classes still emit.
- **New in this run (unfixed)**: in QQ's
  `com/tencent/qlink/activity/QlinkSendDefaultActivity.java:202`, cure folds a
  long constant XOR into the decimal literal `307638947254` without the `L`
  suffix, which javac rejects as an integer that is too large. The other
  4,102,811 files parse; the fix belongs in cure's literal
  folding/printing.
- **Historical bug list**: the seven defects above come from the first validation
  round (2026-09-19, v0.1.2) and are fixed; this run re-checks the current output; only
the QQ long-literal issue above fails.

