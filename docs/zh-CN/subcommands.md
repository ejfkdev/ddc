# 渐进式分析 — 子命令参考

[English](../subcommands.md) | [简体中文]

参考 ASC 的"把编译产物当数据库查"理念：超大 APK 不必全量反编译（weibo 全量
5.5s），先查元数据再定点反编译——元数据加载只需解压+解析 dex 头表（weibo
20 个镜像并行约 0.35s），扫描不经过提升/结构化/渲染管线：

| 命令 | 作用 | weibo (236MB/20dex) |
|---|---|---|
| `ddc manifest app.apk` | AndroidManifest.xml（二进制 AXML）→ 文本 XML，不碰 dex | **0.06s** |
| `ddc findrefs app.apk string token` | 全部字符串字面量引用（常量指令扫描） | **0.14s** |
| `ddc findrefs app.apk type com.example.Foo` | 类型引用（new-instance/check-cast 等） | ~0.5s |
| `ddc findrefs app.apk method onCreate --class android/app/Activity` | 方法调用点 | ~0.5s |
| `ddc findrefs app.apk field CREATOR --class com.example --fuzzy-class` | 字段读写点 | ~0.5s |
| `ddc listclasses app.apk [pattern]` | 类名清单，可模糊过滤 | **0.10s** |
| `ddc info app.apk` | App 上下文（arsc 解析应用名、包名、版本、启动类、SDK、大小、md5）+ 每镜像 dex 计数 | **0.11s**（大文件的 md5 占大头） |
| `ddc getclass app.apk com.example.Foo [-o F.java]` | 单类（含嵌套）定点反编译 | **0.03s**（典型类） |
| `ddc strings app.apk [-f TEXT] [--with-locations]` | 字符串表清单；`--with-locations` 把 const-string 命中映射到所属方法 | **0.04s** |
| `ddc members app.apk [NAME] [--class FQCN] [--method\|--field]` | 方法/字段名检索（jadx `--single-class` 风格） | **0.04s** |
| `ddc hierarchy app.apk FQCN` | 继承谱：extends/implements + sub/impl 反向 | **0.04s** |
| `ddc largest app.apk [-n N]` | 按指令数排序的 top-N 方法（找巨兽方法） | **0.06s** |
| `ddc disasm app.apk FQCN[.method]` | 单类/单方法原始字节码（操作码+pc） | **0.04s** |
| `ddc callers app.apk NAME [FQCN]` | 谁调用了方法 NAME（复用 findrefs 方法扫描） | ~0.5s |
| `ddc getmethod app.apk FQCN.method` | 方法粒度切片：只输出目标方法及其全部重载 | **0.03s** |
| `ddc pkg app.apk com.example.foo [-o DIR]` | 整包反编译（段边界前缀匹配，含子包；只跑选中类的完整管线）；`--app` 自动取 manifest 包名（空时回退 launcher 包） | 0.165s（Telegram tgnet 1561 类）/ `--app` 1.7s（org.telegram 5103 类） |
| `ddc mainactivity app.apk` | manifest 包名 + MAIN/LAUNCHER 入口 Activity，并在 dex 里定位验证 | **0.02s** |
| `ddc res app.apk [entry] [-o FILE]` | 列出全部归档条目（含 XAPK 内层 APK）；`res <apk> res/values/strings.xml` 解码二进制 XML，文本直出，二进制 `-o` 保存 | **0.01s** |
| `ddc manifest app.apk --component launcher` | 组件过滤（launcher/activity/service/receiver/provider） | **0.06s** |
| （全量对照）`ddc app.apk -o out/` | 98,348 个类全部反编译落盘 | 5.45s / 1.28GB |

**定位类在哪个 dex**：`--dex NAME`（可重复，条目名子串匹配）把范围缩到指定镜像——
过滤发生在解析之前（`getclass --dex classes20` 只解析一个镜像，weibo 上 0.09s）；
类名出现在多个镜像时 `getclass` 会警告并列出镜像与 `--dex` 提示；`--dex` 传错时
错误信息列出全部可用条目名。stdout 输出模式完全干净（stderr 静默），耗时只随
`-o` 文件/目录输出打印；`findrefs -o FILE` 把命中行写入文件并打印耗时。

`findrefs` 输出为列表格式（首行表头，对齐 `info` 的风格；类是独立列，
边界无需猜测）。**一个方法一行**：同类多次命中聚合进 refs 列（`; ` 分隔、
去重、按首次命中排序），kind 列为首次命中的指令类型：

```
dex         kind          class method refs
hello       const-string  Greeter greet()Ljava/lang/String;  "hi "
classes50.dex  const-string  bz7/c d(...)Ljava/lang/String;  "both_feishu_doubao"; "only_feishu"
```

聚合语义与 ASC 一致（reqable token：指令级 82 行 → 方法级 69 行，与 ASC
的方法行数完全相同）；比 ASC 多保留方法描述符。

匹配语义：string/type/名称为子串（大小写不敏感）；`--class` 默认精确（
`com.poc.Main`/`com/poc/Main`/`Lcom/poc/Main;` 三种写法都归一化），加
`--fuzzy-class` 变子串。扫描为每镜像一线程并行；命中 0 个时秒回。
AXML 解码器在 `ddc-cli/src/axml.rs`（字符串池 UTF-16/UTF-8 双格式、属性
typedValue 渲染），输入也接受裸 `.axml` 文件。渐进式工作流：
`info → listclasses → findrefs → getclass`，浏览/导航用
`strings/members/hierarchy/largest/disasm/callers`，入口定位用
`mainactivity`，资源侧用 `res`，批量定点用 `pkg`（`--app` 跳过
androidx/三方库），方法粒度用 `getmethod`（只切目标方法及其重载），
最后才按需全量。

### 对比 ASC（同机同查询交替 3 轮取中位）

5 个真实 APK（62-353MB）渐进式查询，stdout 丢弃；正确性交叉验证 107/107 重合：

| 查询 | ddc | ASC | 倍率 |
|---|---|---|---|
| findrefs string（lark 353MB） | **0.32s** | 0.61s | 1.9× |
| findrefs string（weixin 268MB） | **0.22s** | 0.52s | 2.4× |
| findrefs string（weibo 226MB） | **0.19s** | 0.44s | 2.3× |
| findrefs type/method（全部 5 个） | **0.09-0.31s** | 0.29-0.66s | 2-3× |
| getclass（lark/weixin/weibo） | 0.18-0.42s | **0.10-0.12s** | ASC 快 |
| getclass（Telegram，类本身大） | **0.05s** | 0.11s | ddc 快 |
| findrefs 内存（lark） | ~820MB | ~170MB | ASC 省 |

getclass 的分野：ASC 用 zip 比特流探测定位后**只膨胀目标 dex**（多 dex 大 APK
上底座更低）；ddc 解析全部镜像头表（~0.2s 底座）后 lazy 物化——类本身昂贵时
反超，类便宜时让位于底座。findrefs 的扫描域 ddc 全面更快（流水线：解析波次
产出 → 有界通道 → 扫描线程消费即丢弃；Arc 共享 APK 字节消除压缩副本）。
