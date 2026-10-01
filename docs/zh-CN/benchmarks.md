# 基准测试

[English](../benchmarks.md) | [简体中文]

七个真实 APK —— reqable 3.2.23（34MB）、Telegram（62MB）、WhatsApp
（139MB）、weibo 16.9.1（226MB、20 dex）、weixin 8.0.78（268MB）、
飞书 8.0.2（398MB、59 dex）、qq 9.3.65（374MB）—— 墙钟时间 3 连测取
平均，峰值 RSS 来自 `/usr/bin/time -l`：

| APK | 大小 | 全量反编译 | 峰值 RSS |
|---|---|---|---|
| reqable | 34 MB | **0.57s** | 162 MB |
| Telegram | 62 MB | **4.50s** | 786 MB |
| WhatsApp | 139 MB | **29.8s** (99,277 个文件——大小写变体类对全部保留) | 970 MB |
| weibo | 226 MB | **13.5s** | 1173 MB |
| weixin | 268 MB | **32.7s** | 1401 MB |
| lark | 398 MB | **16.1s** | 2226 MB |
| qq | 374 MB | **37.1s** | 2268 MB |

**编译验证——引用这个数字前请先读口径**：七个 APK 的全部 `.java`（共
909,689 个文件）过 `javac` 解析闸门（`-XDshould-stop.ifNoError=PARSE
-XDshould-stop.ifError=PARSE`）——**语法错误为零**。该闸门只到解析器为止：
**不做类型检查、不做符号解析、不保证挂 classpath 后能编译通过**。语义质量
（类型恢复、交叉引用）目前落后于 jadx——见 README「已知限制」。全量 `javac`
编译口径下的符号/类型错误数另行统计，正是 0.1.4 之后多轮修复的驱动力
（重复声明类错误：lark 22,316→14、weibo 14,871→3；null 落局部的
`str = 0` 错误家族已在 0.1.6 根治）。

## 同一批 APK 上的全部查询子命令

3 连测取平均；单元格为 墙钟 / 峰值 RSS。查询内容：`strings -f <包名>
--with-locations`；`findrefs string <包名>`；`findrefs method onCreate`；
`hierarchy`/`disasm`/`getclass` 用各 App 的启动类（Telegram 的
`LaunchActivity` 是特别大的类）：

| 子命令 | reqable | Telegram | WhatsApp | weibo | weixin | lark | qq |
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

测量方法论（3 连测、串行纪律、CPU 计量陷阱）见
[英文版](../benchmarks.md) 的 Methodology notes 与
[性能工程](optimization.md)。
