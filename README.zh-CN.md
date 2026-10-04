# ddc — DEX → Java 反编译器（Rust）

[English](README.md) | **简体中文**

[![CI](https://github.com/ejfkdev/ddc/actions/workflows/ci.yml/badge.svg)](https://github.com/ejfkdev/ddc/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

`ddc` 把 Android DEX 字节码反编译回可读的 Java —— 真实 App 级别的速度、
可以像数据库一样查询，并且**经过 javac 验证**：七个真实 APK 共
909,689 个输出文件全部零语法错误。

## 特性

- **快** —— 226MB/20 dex 的 weibo（9.8 万个类）全量反编译 **~13s**（高质量模式），
  62MB 的 Telegram **~4.5s**；病态类跑在带截止期的受控线程上，不会拖死
  整个运行。
- **能编译** —— 七个基准 APK（reqable/Telegram/WhatsApp/weibo/weixin/
  qq/lark，共 91 万文件）的输出全部通过 javac，**零语法错误**；
  大小写仅差一位的类名对（`X/Cua` vs `X/cua`）两个类都各自成文件，
  不再后者覆盖前者。
- **渐进式反编译** —— 15 个查询子命令（字符串、交叉引用、继承谱、
  manifest、资源、方法粒度反编译）毫秒级出答案：先查元数据、按需定点
  反编译，绝大多数分析不必付全量的代价。
- **DEX 035–041 全版本** —— 多 dex APK、XAPK/APKS/APKM 容器、
  invoke-custom；lambda 与字符串拼接折叠回真 Java（`(a) -> …`、
  `Cls::name`、`a + b`）。
- **可读输出** —— jadx 式局部变量名（`str`、`getName() → name`、
  Kotlin `Intrinsics` 参数名）取代 `v12`；IntDef 魔法数字开箱即用
  按常量名渲染（`setVisibility(8)` → `View.GONE`；内置 android-37
  域表，`--symbols` 可覆盖）；Kotlin 空检查噪声消除、synthetic
  `access$NNN` 桥在调用点内联。
- **可复现输出** —— 无时间戳，两次运行 diff 干净。
- **中英双语 CLI** —— 按环境变量自动识别语言（`DDC_LANG=zh|en` 强制
  指定，回退英文）。
- 基于 [`jdc-core`](https://crates.io/crates/jdc-core) —— 与
  [jcdc](https://github.com/ejfkdev/jcdc) 共用的机器无关反编译核心。

## 安装

**macOS**（Homebrew）：

```bash
brew install ejfkdev/tap/ddc
```

**Windows**（Scoop）——先添加 bucket，再按名安装（scoop 不识别
`user/repo/app` 三段式直装——issue #3）：

```powershell
scoop bucket add ejfkdev https://github.com/ejfkdev/scoop-bucket
scoop install ddc

# 或者不添加 bucket，直接用 manifest URL 安装：
scoop install https://raw.githubusercontent.com/ejfkdev/scoop-bucket/main/bucket/ddc.json
```

**cargo-binstall**（全平台——直接下载预编译 release 二进制，不用本地
编译）：

```bash
cargo binstall ddc-cli
```

**二进制**：每个 [release](https://github.com/ejfkdev/ddc/releases) 附带
裸二进制（无压缩包），覆盖 `linux-amd64`、`linux-arm64`、
`windows-amd64`、`windows-arm64`、`macos-amd64`、`macos-arm64`——非
macOS 平台已 UPX 压缩。

**源码构建**（Rust stable）：

```bash
git clone https://github.com/ejfkdev/ddc && cd ddc
cargo build --release
```

## 用法

```bash
ddc app.apk                          # 全量反编译 → app-out/
ddc app.apk -c com.example.Foo       # 单类输出到 stdout
ddc app.apk -o - | less              # 全部输出到 stdout

ddc info app.apk                      # App 上下文（应用名、包名、版本、
                                      # md5）+ 每镜像计数
ddc mainactivity app.apk             # 入口：包名 + 启动 Activity
ddc findrefs app.apk string token    # 每个 const-string "token" 引用点
ddc getmethod app.apk Foo.toString   # 单方法，含全部重载
ddc pkg app.apk --app -o own/        # 只反编译 App 自身代码
```

完整选项与示例见 `ddc --help`（按工作流分组的子命令菜单）。更详细的
完整参考 —— 每个选项的语义、每个子命令的输出格式与行为 —— 见
[docs/zh-CN/cli.md](docs/zh-CN/cli.md)（[English](docs/cli.md)）。

## 性能

七个真实 APK，release 构建，冷机后实测（Apple Silicon 6P+12E）。
39 个 APK 的验证语料（408 万个反编译文件，每个 APK 的墙钟 /
峰值 RSS / javac 解析门）见
[docs/zh-CN/validation.md](docs/zh-CN/validation.md)
（[English](docs/validation.md)）。

<details>
<summary>全量反编译 —— 7 个真实 APK</summary>

| APK | 大小 | 全量反编译 | 峰值 RSS |
|---|---|---|---|
| reqable | 34 MB | **0.57s** | 162 MB |
| Telegram | 62 MB | **4.50s** | 786 MB |
| WhatsApp | 139 MB | **13.6s** (99,483 个文件——大小写变体类对全部保留) | 944 MB |
| weibo | 226 MB | **13.5s** | 1173 MB |
| weixin | 268 MB | **32.7s** | 1401 MB |
| lark | 398 MB | **16.1s** | 2226 MB |
| qq | 374 MB | **37.1s** | 2268 MB |

</details>

同一批 APK 上的查询子命令（单元格：耗时 / 峰值 RSS；`strings -f <包名>
--with-locations`，`findrefs` 查包名字符串与方法 `onCreate`，
`hierarchy`/`disasm`/`getclass` 用各 App 启动类）：

<details>
<summary>查询子命令 —— 13 个命令 × 7 个 APK</summary>

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

</details>

## 文档

| | |
|---|---|
| [架构](docs/zh-CN/architecture.md) | 三个 crate、提升/结构化/输出管线、DEX 版本、invoke-custom（[EN](docs/architecture.md)） |
| [CLI 参考](docs/zh-CN/cli.md) | 全部选项与子命令详解（[EN](docs/cli.md)） |
| [子命令](docs/zh-CN/subcommands.md) | 渐进式分析完整参考（[EN](docs/subcommands.md)） |
| [基准](docs/zh-CN/benchmarks.md) | 耗时/内存、方法论、与 ASC 对比（[EN](docs/benchmarks.md)） |
| [性能工程](docs/zh-CN/optimization.md) | 5 分钟 → 6s：六轮优化与实测排除的捷径（[EN](docs/optimization.md)） |
| [语料验证](docs/zh-CN/validation.md) | 39 个真实 APK、408 万文件全部 javac 解析零错误，含发现并修复的 bug 清单（[EN](docs/validation.md)） |

## 已知限制

擦除类型（DEX 无 Signature）；d8 反糖的 `-$$Lambda$` 类独立成文件；极少数
R8 巨兽方法超时降级（个别被墙钟截止截断的类输出可能随运行浮动）；
pattern-switch 呈现为反糖分发链。上述 javac 门控是**语法**门——语义级
诊断（缺 Android classpath、极少数寄存器密集巨兽方法中的类型混淆局部
变量）仍会存在。量级参考：带完整 Android classpath 的 javac 电池在四
语料上当前约 7,500 个 ddc 侧错误 / 91 万文件（v0.1.19 时为 11,345，
见[基准文档](docs/zh-CN/benchmarks.md)）；jadx 同电池约为其一半，差距
集中在类型恢复。详见[架构文档](docs/zh-CN/architecture.md)。

## 测试

```bash
cargo test    # 74 个测试
```

## 许可

[MIT](LICENSE) © ejfkdev
