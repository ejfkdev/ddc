# 基准测试

[English](../benchmarks.md) | [简体中文]

七个真实 APK —— reqable 3.2.23（34MB）、Telegram（62MB）、WhatsApp
（139MB）、weibo 16.9.1（226MB、20 dex）、weixin 8.0.78（268MB）、
飞书 8.0.2（398MB、59 dex）、qq 9.3.65（374MB）—— 墙钟时间冷机
后实测，峰值 RSS 来自 `/usr/bin/time -l`：

| APK | 大小 | 全量反编译 | `--fast` | 峰值 RSS |
|---|---|---|---|---|
| reqable | 34 MB | **0.76s** | 0.46s | 317 MB |
| Telegram | 62 MB | **6.43s** | 3.86s | 931 MB |
| WhatsApp | 139 MB | **23.25s** | 10.34s | 1114 MB |
| weibo | 226 MB | **19.10s** | 9.73s | 1383 MB |
| weixin | 268 MB | **33.20s** | 21.94s | 1519 MB |
| lark | 398 MB | **22.29s** | 12.70s | 2403 MB |
| qq | 374 MB | **47.70s** | 33.51s | 2552 MB |

**编译验证——引用这个数字前请先读口径**：七个 APK 的全部 `.java`（共
909,689 个文件）过 `javac` 解析闸门（`-XDshould-stop.ifNoError=PARSE
-XDshould-stop.ifError=PARSE`）——**语法错误为零**。该闸门只到解析器为止：
**不做类型检查、不做符号解析、不保证挂 classpath 后能编译通过**。

**语义质量另有独立度量——带完整 Android classpath（android-37 jar +
可选库）的 javac 电池**，覆盖四个语料，只计 ddc 侧可归因诊断。该电池
是 0.1.4 之后多轮修复的驱动力，**v0.1.19 → v0.1.22 从 11,345 降到
7,452（−34%）**——其中包括一个自首个版本就存在的多 handler 异常路由
bug（多 handler 方法的所有 catch 全部执行第一个 handler 的代码——任何
解析闸门都不可见）、以及三个靠逐行读输出对照反汇编才发现的 javac 不可
见值丢失缺陷。语义质量在类型恢复上仍落后 jadx（擦除泛型、R8 巨兽方法
的寄存器类型局部）——见 README「已知限制」。

记录于 2026-11-02，v0.1.35：每个单元格 3 次串行运行（算术平均；峰值 RSS 取最大值）。峰值 RSS 来自 `/usr/bin/time -l`。WhatsApp 全量运行有 1 个病态类（`X.K2H`）触发截止期保护并以 1 退出，已写出 99,482 个文件；耗时包含该截止期等待。

## 同一批 APK 上的全部查询子命令

冷机后实测；单元格为 墙钟 / 峰值 RSS。查询内容：`strings -f <包名>
--with-locations`；`findrefs string <包名>`；`findrefs method onCreate`；
`hierarchy`/`disasm`/`getclass` 用各 App 的启动类（Telegram 的
`LaunchActivity` 是特别大的类）：

| 子命令 | reqable | Telegram | WhatsApp | weibo | weixin | lark | qq |
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

测量方法论（冷机实测、串行纪律、CPU 计量陷阱）见
[英文版](../benchmarks.md) 的 Methodology notes 与
[性能工程](optimization.md)。
