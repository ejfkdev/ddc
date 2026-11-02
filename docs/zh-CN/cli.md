# CLI 参考

[English](../cli.md) | [简体中文]

`ddc --help` 的全部内容，外加更详细的说明：调用模式、每个选项的语义、
每个子命令的输出格式与行为。`--help` 是速查卡，这里是手册。

四种调用形态：

```
ddc [选项] <输入>... [输出]             # 全量反编译
ddc <子命令> [参数...]                  # 渐进式分析（查询）
ddc serve|http [--addr 主机:端口]       # REST + OpenAPI（serve 另挂 /mcp）
ddc mcp stdio|http                      # MCP 工具服务器
```

## 语言

消息（帮助、错误、摘要、表头）自动本地化。判定顺序：`DDC_LANG`（显式
`zh`/`en`）优先于 `LC_ALL` > `LC_MESSAGES` > `LANG` > `LANGUAGE`，取第一
个声明了语言的变量 —— 任何 `zh*` 值选中文（`zh`、`zh_CN`、`zh-Hans`、
`zh_TW.UTF-8`、…），其他语言选英文。`LANGUAGE` 是冒号分隔的优先列表
（`zh:en`），只取第一项；`C`、`POSIX`、空值不声明语言，继续沿链路查找。
环境变量全部缺省时（原生 cmd.exe / PowerShell 不导出任何区域变量），
Windows 回退到用户界面语言（即 Windows 自身显示所用的语言）；需固定
语言时设置 `DDC_LANG`。Git Bash、Cygwin、WSL 导出 `LANG`，由链路覆盖。

```bash
DDC_LANG=zh ddc --help    # 中文帮助
LANG=zh_CN.UTF-8 ddc -V   # 同样是中文
LC_ALL=C LANG=zh_CN ddc -V  # C 不声明语言 → LANG 决定 → 中文
DDC_LANG=en ddc -V        # zh 区域下强制英文
```

## 全量反编译

`ddc [选项] <输入>... [输出]`

**输入**（多个输入合并进一个类池，重名类自动去重、首个生效）：

- `.dex` —— 版本 035–041
- `.apk` / `.jar` / `.zip` —— 全部 `classes.dex`、`classes2.dex`、… 条目，
  按数值序合并
- `.xapk` / `.apks` / `.apkm` —— 装 APK 的容器：每个内层 APK 的 dex 都
  并入，base 优先；标签三层（`容器!base.apk!classes.dex`）
- 目录 —— 递归扫描上述所有扩展名

**输出** —— 最后一个位置参数或 `-o`：

- `<目录>` —— 输出根目录，保留包结构；默认输入旁的 `<输入名>-out/`
- `<文件.java>` —— 仅单类（单类输入或 `-c`）
- `-` —— stdout，类之间以 `// ===== class =====` 分隔、按池序（强制
  单线程）

位置参数消歧：无 `-o` 且有两个以上位置参数时，**最后一个**若不是输入
形态（无 dex 类扩展名、不是文件、也不是含 dex 的目录）就视为输出 ——
所以 `ddc app.apk out/` 写进预先建好的 `out/`，而 `ddc a.dex dump/` 里
真实的 dex 目录保持输入身份。

| 选项 | 语义 |
|---|---|
| `-o, --output <路径>` | 输出位置：目录 / `文件.java` / `-` |
| `-c, --class FQCN` | 只反编译这个类（点分/斜杠均可；其嵌套/匿名/局部类一并输出） |
| `-l, --list` | 列出类名后退出 |
| `-t, --threads <n>` | worker 数（默认 CPU 数减去写文件线程池；stdout 模式强制单线程保池序） |
| `--no-comments` | 去掉出处注释头 |
| `--fast` | 跳过 cure 代码简化 pass，仅保留格式化（大 APK 约省一半时间） |
| `--symbols <目录>` | 从 SDK platform 目录（`android.jar` + `data/annotations.zip`）重建 IntDef 符号表 |
| `-v, --verbose` | stderr 输出逐 dex 统计与慢类 |
| `-h, --help` / `-V, --version` | 帮助 / 名称+版本+主页 |

`--opt=value` 与 `-o=path` 写法均可；裸的 `ddc help` / `ddc version` 与
`-h` / `-V` 等价；完全不带参数的 `ddc` 直接打印帮助（退出码 0）。
`ddc help <子命令>` 与 `ddc <子命令> --help` 打印该命令自己的参数与
选项，而非主帮助。

`ddc help serve|http|mcp`（或 `ddc serve -h` 等）给出每个服务器模式的
旗标、端点与连接示例。

### 输出格式（有结构化报告的查询命令都支持 `--format`；`res` 与 `largest` 仅输出文本）

`auto`（默认）按 stdout 分流：终端取人类渲染，管道取脚本一直在解析的
稳定文本。

| 格式 | 渲染 |
|---|---|
| `auto` | 终端表格、管道文本 |
| `text` | 固定宽度列格式（传统、稳定） |
| `table` | 对齐表格 + 表头分隔线 |
| `markdown` / `md` | 管道表格；源码形报告（`getclass`、`disasm`、`manifest`）围栏代码块 |
| `json` | HTTP/MCP 前端服务的同构类型数据 |
| `jsonl` | 同上，逐元素紧凑一行 |

### 服务器模式（HTTP REST、OpenAPI、MCP）

一个进程三个前端 —— 每个查询命令基于
[xyz-rust](https://github.com/ejfkdev/xyz-rust) 一次定义：

| 命令 | 服务内容 | `/mcp` |
|---|---|---|
| `ddc serve [--addr :8080]` | 10 条 REST 路由 + `GET /openapi.json` + `GET /healthz` | 流式 MCP 端点 |
| `ddc http [--addr :8080]` | 仅 REST + OpenAPI | 不挂载（404） |
| `ddc mcp stdio` | 走 stdin/stdout 的 MCP（Agent 子进程） | — |
| `ddc mcp http` | 纯 MCP 流式服务器 | — |

共享旗标：`--addr`、`--bearer 令牌1,令牌2`（REST 与 `/mcp` 的
Authorization: Bearer）、`--timeout`、`--cors`、`--tls-cert` +
`--tls-key`（HTTPS）、`--default k=v`（通道默认值）。MCP 模式另有
`--versions`（协议修订）、`--stateless`、`--session-timeout`、`--name`、
`--server-version`。

serve 运行时 MCP 客户端连接 `http://<地址>/mcp`（该端点对普通 curl
GET 返回 406 —— 它讲 MCP 流式协议）。每条 REST 路由同时接受 GET
（query 参数）与 POST（JSON body）；REST 示例：

```bash
ddc serve --addr 127.0.0.1:8080
curl '127.0.0.1:8080/class?input=app.apk&class=com.example.Foo'
curl -X POST '127.0.0.1:8080/class' -H 'Content-Type: application/json' \
    -d '{"input":"app.apk","class":"com.example.Foo"}'
curl '127.0.0.1:8080/findrefs?input=app.apk&kind=string&query=token'
# Claude Desktop：mcpServers → { ddc: { command: ddc, args: [mcp, stdio] } }
```

**出处注释头**（`--no-comments` 关闭）：

```java
// Decompiled by https://github.com/ejfkdev/ddc 1.2.3
// From: app!classes3.dex (DEX 038)
// Source file: Foo.java
```

`From:` 标明输入文件、dex 镜像与 DEX 版本 —— 定位类所在镜像的最快线索
（jadx 的 `loaded from:` 模式）。注释头不含时间戳，两次运行 diff 干净；
极少数变量编号仍可能因 std HashMap 随机种子而不同（语义等价）。

**退出码**：`0` 成功；`1` 部分类失败（如病态 CFG 超时）；`2` 用法错误
（只输出错误一行，参考信息用 `ddc --help` 获取）。运行结束在 stderr 打一行摘要：
`ddc: wrote 98348 file(s) to out/, 1 failed in 6.13s`。

## 渐进式分析子命令

全部接受 `-d/--dex NAME`（可重复，条目名子串——过滤发生在解析之前，
`getclass --dex classes20` 只解析一个镜像），多数接受 `-o FILE` 写结果。
查询不经过提升/结构化/渲染管线；stdout 干净（耗时只随 `-o` 打印）。

### 先摸清全貌

- **`ddc info <输入>`** —— 一条命令看全貌。先是上下文头（有 manifest
  时）：应用名（`@0x…` 引用经最小 resources.arsc 解析还原，字面值
  直出）、包名、`版本名 (版本号)`、自定义 Application 类、启动
  Activity、`uses-sdk` 上下界、文件大小与 MD5（label 查找与 manifest
  一样遵循容器 base 优先规则）。随后是逐 dex 表：每个镜像一行的
  版本、类/方法/字段/字符串计数，外加合计行。裸 `.dex` 输入没有
  manifest，跳过头部只打印表格。
- **`ddc listclasses <输入> [模式]`** —— 以压缩前缀目录视图输出类结构（默认）：
  每个分叉包一行 `包/ (类数)`，加上类叶子——20 万类的应用一屏即可看结构。
  模式为大小写不敏感子串过滤（先过滤再建树）。用 `--prefix P` 钻取
  （段级精确：`a.b` 匹配 `a.b` 与 `a.b.*`），`--depth N` 在范围下恰好
  N 段处聚合；`--sort count` 大类优先。`--limit`/`--offset` 对行
  （平铺视图为类名）翻页：`--format json` 携带
  `total`/`offset`/`returned`/`next_offset`，摘要视图始终是带
  `rows[].path/kind/classes` 的 JSON 对象。翻页顺序确定：镜像按输入序、
  镜像内按表格序，因此 `--offset` 续页不会跳过或重复行。`--full` 恢复
  旧行为：全量平铺类名。
- **`ddc manifest <apk> [--component C] [-o FILE]`** —— 把二进制
  AndroidManifest.xml 解码成文本 XML。`--component` 过滤到一种元素：
  `launcher`（MAIN/LAUNCHER 入口 Activity）、`activity`、`service`、
  `receiver`、`provider`、`permission`、`activity-alias`、`application`。
  也接受裸 `.axml` 文件。XAPK/APKS 容器取 base APK 的 manifest。
- **`ddc mainactivity <apk>`** —— 包名、自定义 Application 类（若有）与
  启动 Activity；相对名规则（`.MainActivity` → 包名前缀；裸单词 →
  包名.单词）与 activity-alias 的 `targetActivity` 都会解析；随后在 dex
  镜像里验证（报告定义它的镜像）。
- **`ddc res <apk> [条目] [-o FILE]`** —— 无条目：输出归档目录摘要
  （按前缀统计条目数，如 `res/drawable/ (1234)`）；`--full` 为平铺表格
  （压缩方式、压缩大小；XAPK 内层 APK 摊平成 `apk!name` 标签），
  `--prefix` 展开目录，另有 `--depth/--sort/--limit/--offset`。带条目：
  输出内容 —— 二进制 XML（首块 `0x0003`）走 AXML 解码器，文本直出，
  二进制经 `-o` 保存（否则错误会提示你）。条目匹配：先精确名，再唯一
  子串。默认视图是目录前沿（`res/drawable/ (1234)`），支持同样的摘要参数。

### 找东西

- **`ddc strings <输入> [-f 文本] [--with-locations]`** —— 字符串表（每串
  一行）。`-f` 子串过滤；`--with-locations` 遍历每个方法的 const-string
  指令，加一列 `used-by` 把命中映射到所属方法。`--summary` 按**引用方
  类/包**分组统计匹配的引用位点（"哪些包在用这个 URL/密钥"），用
  `--prefix`/`--depth` 钻取、`--samples N` 附示例类；`--full` 输出原始
  字符串表（默认视图）。`--limit`/`--offset` 翻页（JSON 附带
  `total`/`next_offset`）；MCP 工具默认封顶 500 行并在末尾附截断提示。
- **`ddc findrefs <输入> <string|type|class|method|field> <查询> [--class
  FQCN] [--fuzzy-class] [-o 文件]`** —— 字符串字面量 / 类型 / 方法调用点 /
  字段访问的全部引用（`class` 是 `type` 的常用词别名；继承/实现等定义级
  关系请用 `ddc hierarchy`）。默认视图是**引用方类**的前缀目录（按包
  统计命中数——"谁在用这个字符串"一眼可见），用 `--prefix`/`--depth`
  钻取；`--full` 输出逐条命中行：列式带表头（`dex kind class method
  refs`），**一个方法一行**，多次命中聚合进 refs（`; ` 分隔、去重）。
  `--sort`/`--limit`/`--offset` 两种视图通用。匹配语义：查询是大小写
  不敏感子串；`--class` 默认精确（点分、斜杠、`L…;` 描述符形式都归一
  化），`--fuzzy-class` 变子串。
- **`ddc callers <输入> 名字 [FQCN]`** —— 谁调用了这个方法（复用
  findrefs 的方法扫描，可选限定到一个类；同样接受
  `--full/--prefix/--depth/--sort/--limit/--offset`）。
- **`ddc members <输入> [名字] [--class FQCN] [--fuzzy-class]
  [--method|--field]`** —— 方法/字段名检索；`--method` / `--field` 限定
  种类。默认视图按所属类/包分组计数；`--full` 输出逐条成员行；
  支持 `--format json`；摘要接受
  `--prefix/--depth/--sort/--limit/--offset/--samples N`。

### 看清结构

- **`ddc hierarchy <输入> FQCN`** —— 类的继承谱：正向 `class` /
  `extends` / `implements`，反向对每个继承/实现它的类输出 `sub` / `impl`。
  跨镜像有效（父类在别的 dex 时按名字匹配）。默认视图按 relation 分组
  的前缀目录（`sub com.example/ (123)`），15 万子类也一屏可见；`--full`
  输出逐条关系行，`--prefix/--depth/--sort/--limit/--offset` 调整视图，
  `--samples N` 给目录行附示例类。
- **`ddc largest <输入> [-n N]`** —— 按指令数排序的 top-N 方法（找巨兽；
  默认 20）。
- **`ddc disasm <输入> FQCN[.方法]`** —— 一个类或一个方法的原始字节码：
  每条指令一行（`pc 操作码 助记符`）。目标先按整串类名解析、失败再按
  最后一个点分割 —— `org.foo.Cells.t1`（类）与 `Greeter.greet`（方法）
  都能解析。只写方法名会列出全部重载；追加完整描述符
  （`'Greeter.greet()Ljava/lang/String;'`）可精确指定一个。

### 精准反编译

- **`ddc getclass <输入> FQCN [-o 文件] [--dex NAME] [--fast]
  [--low-memory]`** —— 单类（含其嵌套/匿名/局部类）走完整管线。类名出现
  在多个镜像时警告并列出镜像；定义镜像先注册进池，保证从它解析。默认
  路径构建依赖闭包：解析定义镜像，以及目标类的父类/接口、签名、代码
  引用和外层类所在镜像；渲染时仍未解析的请求会补充镜像并重试（最多
  3 轮）。四个及以下镜像、或闭包覆盖约 90% 镜像时走全量路径。
  `--low-memory` 选择两级按需模式：从定义镜像开始，只有渲染器真正请求
  某个类时才加载其所在镜像并重新渲染，高连接类可能更慢；该模式仅支持
  文本输出。`--fast` 跳过 cure。默认路径支持所有输出格式。
- **`ddc getmethod <输入> FQCN[.方法] [-o 文件]`** —— 单方法：从反编译
  后的类里切片 —— 出处头 + package 行 + 全部同名重载，去掉缩进。未命中
  时列出该类的可用方法名。裸类名回退为整类输出。
- **`ddc pkg <输入> 包名 [-o 目录] [-t N] [--app]`** —— 整包反编译走完整
  管线：包名按段边界前缀匹配，`com.example.app` 命中其下全部类，
  含 `com.example.app.ui` 等子包（默认输出：输入旁的
  `<包名下划线>-pkg/`）。`""` 或 `.` 表示根
  （含默认包）。`--app` 自动取 manifest 包名 —— 该包下没有类时（Telegram：
  manifest 写 `org.telegram.messenger.web`，代码在
  `org.telegram.messenger`）回退用 launcher 类所在的包，应用自身代码总
  聚簇在那里。

## 输出质量 pass

四个 pass 默认全部开启：

- **jadx 式局部命名** —— 合成名 `v12`/`p3` 永不存活：Kotlin
  `Intrinsics.checkNotNullParameter(x, "name")` 用字符串命名 x
  （编译器把真参数名写进了检查）；唯一定义调用（`getFoo() → foo`、
  `new File(…) → file`）；jadx 的类型别名表（`str/cls/it/…`）+
  小写类简名回退。碰撞取 `2、3、…`；debug 信息名永不覆盖。
- **Kotlin 空检查消除** —— 语句位的 `Intrinsics.checkNotNull…` 是
  运行时断言；命名 pass 收割字符串后删除（lark：59,106 → 9）。
- **synthetic accessor 内联** —— `access$NNN` 静态桥在调用点内联
  （identity/字段 getter/方法转发三种形状，d8 APM 埋点容忍）。
  只碰 STATIC+SYNTHETIC。lark：9,214 个调用点的 43%。
- **IntDef 常量渲染** —— 见下。

## 平台符号

ddc 开箱即用地把 IntDef/LongDef 字面量实参按常量名渲染
（`setVisibility(8)` → `android.view.View.GONE`）。域表
（android-37）以**可读、可 diff 的文本**放在 repo 里——
`crates/ddc-cli/src/platform_symbols.txt`，一行一域——build.rs
把它 raw-DEFLATE 进二进制（72KB）。用
`scripts/gen-platform-symbols.sh [平台目录]` 重新生成并提交 .txt。
表是精确匹配的——组合 flag 值保持数字；对 API 版本不敏感：不在
内嵌级别里的方法保持数字。启动成本低于 5ms。

`--symbols <SDK平台目录>`（如
`~/Library/Android/sdk/platforms/android-37.0`，需含 `android.jar`
和 `data/annotations.zip`）对该次调用按此平台重建表并覆盖内置
——全量反编译与子命令同样生效。

## 子命令的退出码

用法错误（缺参数、未知选项、`--dex` 传错）只输出错误信息一行，退出
`2`。查询未命中（类/方法/条目找不到）同样是 `2`，但错误里带具体
信息 —— `getmethod` 列出可用方法名，`--dex` 列出可用镜像名。
