# ddc — DEX → Java 反编译器（Rust）

[English](README.md) | **简体中文**

[![CI](https://github.com/ejfkdev/ddc/actions/workflows/ci.yml/badge.svg)](https://github.com/ejfkdev/ddc/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/ejfkdev/ddc)](https://github.com/ejfkdev/ddc/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/ejfkdev/ddc/total?label=downloads)](https://github.com/ejfkdev/ddc/releases)
[![Latest release downloads](https://img.shields.io/github/downloads/ejfkdev/ddc/latest/total?label=latest%20release)](https://github.com/ejfkdev/ddc/releases/latest)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

[![crates.io](https://img.shields.io/crates/v/ddc-cli)](https://crates.io/crates/ddc-cli)
[![crates.io downloads](https://img.shields.io/crates/d/ddc-cli)](https://crates.io/crates/ddc-cli)
[![npm](https://img.shields.io/npm/v/@ejfkdev/ddc)](https://www.npmjs.com/package/@ejfkdev/ddc)
[![npm downloads](https://img.shields.io/npm/dw/@ejfkdev/ddc)](https://www.npmjs.com/package/@ejfkdev/ddc)
[![Platforms](https://img.shields.io/badge/platform-linux%20%7C%20macOS%20%7C%20Windows-blue)](#安装)
[![WinGet](https://img.shields.io/badge/WinGet-ejfkdev.ddc-0078D4?logo=windows)](https://github.com/microsoft/winget-pkgs/tree/master/manifests/e/ejfkdev/ddc)

`ddc` 把 Android DEX 字节码反编译回可读的 Java —— 真实 App 级别的速度、
可以像数据库一样查询，并且**质量优先**：七个真实 APK 共 909,689 个
输出文件全部通过 `javac` 解析闸门，每个文件还经过精确、语义保持的
源码级简化。

## 特性

- **质量优先，而不只是快** —— 七个基准 APK（reqable/Telegram/WhatsApp/
  weibo/weixin/qq/lark，共 91 万文件）的输出全部通过 `javac` 解析闸门，
  **零语法错误**；带完整 Android classpath 的 javac 电池持续跟踪语义
  回归（当前约 **7,500** 个 ddc 侧错误 / 91 万文件，v0.1.19 为 11,345）。
  默认每个文件还会经过 [cure](https://github.com/ejfkdev/cure) 的精确、
  语义保持源码级简化——寄存器拷贝链内联、if/else 赋值折叠为三元、
  还原 for-each、清除死存储与编译器产物：**输出行数 −15%**，javac 门
  结果不变。大小写仅差一位的类名对（`X/Cua` vs `X/cua`）各自成文件，
  不再后者覆盖前者。
- **快** —— 226MB/20 dex 的 weibo（9.8 万个类）全量反编译 **~19s**（高质量模式；`--fast` 约 **9.7s**），
  62MB 的 Telegram **~6.4s**（`--fast` 约 **3.9s**）；病态类跑在带截止期的受控线程上，不会拖死
  整个运行。
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
- **HTTP REST + MCP，开箱内置**（[xyz-rust](https://github.com/ejfkdev/xyz-rust)）：
  `ddc serve` 把每个查询命令暴露为 REST 路由（GET 绑 query 参数，
  POST 亦接受 JSON body）+ `GET /openapi.json` +
  流式 MCP 端点 `/mcp`（MCP 客户端直接指向 `http://地址/mcp`）；
  `ddc http` 是纯 REST 形态（不挂 `/mcp`）；`ddc mcp stdio|http` 把
  10 个核心查询作为带输入/输出 schema 的 MCP 工具运行。查询子命令支持
  `--format auto|text|table|markdown|md|json|jsonl`（auto：终端表格、
  管道输出稳定文本）。
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

**Windows**（WinGet）——包已进入 `microsoft/winget-pkgs`：

```powershell
winget install --id ejfkdev.ddc -e --source winget
```

**全平台**（npm）——`@ejfkdev/ddc` 是一个小 wrapper，安装时下载并校验
SHA-256（需要 Node 18+）：

```bash
npm install -g @ejfkdev/ddc
```

如果 install scripts 被禁用，`ddc` launcher 会在首次运行时下载二进制。
已有带 `read:packages` 的 GitHub token 时，也可以走 GitHub Packages：

```bash
npm config set @ejfkdev:registry https://npm.pkg.github.com/
npm config set //npm.pkg.github.com/:_authToken "$GITHUB_TOKEN"
npm install -g @ejfkdev/ddc
```

完全不经过包仓库：

```bash
# 改成需要的版本
VERSION=0.1.33
npm install -g "https://github.com/ejfkdev/ddc/releases/download/v$VERSION/ejfkdev-ddc-$VERSION.tgz"
```

**cargo-binstall**（全平台——直接下载预编译 release 二进制，不用本地
编译）：

```bash
cargo binstall ddc-cli
```

**Arch Linux**——AUR 包名将是 `ddc-bin`，但 AUR 注册目前关闭（HTTP
503）。先用 release 里的包手工安装（需要 `base-devel`）：

```bash
# 改成需要的版本
VERSION=0.1.33
curl -LO "https://github.com/ejfkdev/ddc/releases/download/v$VERSION/ddc-bin-$VERSION-1-aur.tar.gz"
tar xzf "ddc-bin-$VERSION-1-aur.tar.gz"
cd ddc-bin && makepkg -si
```

AUR 注册恢复后：`yay -S ddc-bin` 或 `paru -S ddc-bin`。

**二进制**：每个 [release](https://github.com/ejfkdev/ddc/releases) 附带
裸二进制（无压缩包），覆盖 `linux-amd64`、`linux-arm64`、
`windows-amd64`、`windows-arm64`、`macos-amd64`、`macos-arm64`——非
macOS 平台已 UPX 压缩。

**源码构建**（Rust stable）：

```bash
cargo install ddc-cli        # 从 crates.io 编译

# 或者构建 checkout：
git clone https://github.com/ejfkdev/ddc && cd ddc
cargo build --release
```

包清单和 release 产物由 [`packaging/`](packaging/) 生成；AUR/WinGet/npm
状态与多程序支持见
[packaging/README.zh-CN.md](packaging/README.zh-CN.md)。

## 用法

```bash
ddc app.apk                          # 全量反编译 → app-out/
ddc app.apk -c com.example.Foo       # 单类输出到 stdout
ddc app.apk -o - | less              # 全部输出到 stdout

ddc info app.apk                      # App 上下文（应用名、包名、版本、
                                      # md5）+ 每镜像计数
ddc listclasses app.apk              # 带类数的包目录树
                                      # （--full：原始全量）
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

| APK | 大小 | 全量反编译 | `--fast` | 峰值 RSS |
|---|---|---|---|---|
| reqable | 34 MB | **0.76s** | 0.46s | 317 MB |
| Telegram | 62 MB | **6.43s** | 3.86s | 931 MB |
| WhatsApp | 139 MB | **23.25s** | 10.34s | 1114 MB |
| weibo | 226 MB | **19.10s** | 9.73s | 1383 MB |
| weixin | 268 MB | **33.20s** | 21.94s | 1519 MB |
| lark | 398 MB | **22.29s** | 12.70s | 2403 MB |
| qq | 374 MB | **47.70s** | 33.51s | 2552 MB |


记录于 2026-11-02，当前 main（v0.1.33 之后）：每个单元格 3 次串行运行（算术平均；峰值 RSS 取最大值）。峰值 RSS 来自 `/usr/bin/time -l`。WhatsApp 全量运行有 1 个病态类（`X.K2H`）触发截止期保护并以 1 退出，已写出 99,482 个文件；耗时包含该截止期等待。
</details>

同一批 APK 上的查询子命令（单元格：耗时 / 峰值 RSS；`strings -f <包名>
--with-locations`，`findrefs` 查包名字符串与方法 `onCreate`，
`hierarchy`/`disasm`/`getclass` 用各 App 启动类）：

<details>
<summary>查询子命令 —— 13 个命令 × 7 个 APK</summary>

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
cargo test    # 101 个测试
```

## 许可

[MIT](LICENSE) © ejfkdev
