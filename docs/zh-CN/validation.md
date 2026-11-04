**2026-11-05 使用 ddc 0.1.35 重跑：39 个真实 Android 应用，6.4 GB，5,409,658 个类，4,102,812 个反编译文件，4,102,811 个通过 javac 解析（1 个 QQ 长整数字面量错误）。**

同一台 Apple Silicon 机器上，每个 APK 单次串行运行；墙钟与峰值 RSS 来自
`/usr/bin/time -l`。全量反编译输出到临时目录，再用 `javac` 的解析阶段
（`-XDshould-stop.ifNoError/ifError=PARSE`）分批验证全部 `.java`。
测试期间机器上还有其它构建/测试任务，绝对墙钟仅供量级参考。

## 验证过程中发现并修复的 bug

首轮验证（2026-09-19，v0.1.2）中，39 个 APK 有 27 个无法通过 javac 解析，
暴露出七个真实缺陷（均已修复并带回归验证）：

| # | 症状（首轮错误数） | 根因 | 修复 |
|---|---|---|---|
| 1 | 改名类 2,518–46,774 错/包（MinisApp、WhatsApp、AutoClaw 等 22 个 APK） | 大小写变体类（`Y2.a` vs `y2.a`）重命名为 `_2/_3` 后，构造器/文件名应用了新名，**类头声明没有**——`class a` 里的 `a_2(...)` 构造器被 javac 读成无返回类型的方法 | `emit_class_body` 头部与改名同步 |
| 2 | 支付宝 >50 万错（撞 -Xmaxerrs 上限） | 字段/参数名是单个非 ASCII 字符（`支`）——标识符清洗的字符映射把每个非法字符替换成 `_`，**单字符名恰好产出裸 `_`**（Java 9 起保留） | 两侧 `java_ident`：映射结果为 `_` 时再转义为 `__` |
| 3 | 淘宝 1 错 / 钉钉 20 错 | 类名叫 `var`/`record` 等受限上下文名（Java 10+ 禁止用于类型声明） | `var/yield/record/sealed/permits` 加入关键字转义列表（声明/引用/文件名三处一致） |
| 4 | 豆包(nova) 46 错 | 字段名 `1x`/`2x` 数字开头——声明侧有 digit-start 转义，**jdc-core 引用侧没有** | jdc-core `java_ident` 补 digit-start 分支 |
| 5 | 知乎(bangcle) 423 错 / 钉钉残留 | 类型引用的分段清洗器（`sanitize_fq`/`sanitize_source_name`）缺单下划线守卫 | 同 bug 2 的守卫补到分段清洗器 |
| 6 | 钉钉残留 5 错 | R8 类名以 `$` 结尾（`ThreadMsg$$$`）——`rsplit('$')` 得到**空段**，qualified-new 渲染成 `v9.new (args)` | 三处空 `$`-段回退到完整简名 |
| 7 | UU远程 整跑失败 | `assets/` 里放着假 `.dex`（加密载荷，magic 非法）——一个坏镜像让全量反编译直接退出 | `parse_images` 跳过不可解析镜像（stderr 提示），全部失败才报错 |

## 全量反编译

| 应用 | 包名 | 版本 | 大小 | MD5 | dex | 类 | 墙钟 | 峰值 RSS | 文件 | 语法错误 |
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

## 测试时间与版本

- **测试时间**：2026-11-05
- **版本**：ddc 0.1.35（release 二进制）
- **机器**：Apple Silicon 6P+12E；每个 APK 单次串行运行
- **口径**：全量反编译输出到临时目录，`/usr/bin/time -l` 取墙钟与峰值 RSS；
  `javac -XDshould-stop.ifNoError=PARSE -XDshould-stop.ifError=PARSE` 分批解析全部 `.java`。
  测试期间机器上还有其它构建/测试任务，墙钟仅供量级参考。

总计：4,102,812 个文件，1 个语法错误（1 个失败 javac 批次）。
失败的 APK：WhatsApp(rc=1), 小红书(rc=1)

## 查询子命令

查询延迟的现行基准（7 个 APK × 13 个代表性命令，3 次串行均值）在
[README 的 Performance 段](../README.zh-CN.md#性能)和
[benchmarks](benchmarks.md)；本页只记录 39-APK 语料的全量反编译与语法验证。

## 备注

- **加固 APK**：学信网/中国移动只有 4–7 个类、UU远程/学习通仅数百类——壳 dex 只包含
  加载器，真实代码在加密载荷里（这本身就是有效情报）；知乎与招商银行的启动类不在
  可解析 dex 中（`getclass` 返回明确的 not found 提示，其余子命令正常）。
- **病态类超时**：WhatsApp 仍有 1 个 R8 巨型方法类触发截止期保护（`X.K2H`，rc=1），
  该类缺失、其余 99,482 个文件完整；这是受控线程 + 方法论截止期的
  已知设计行为，不是崩溃。
- **假 dex 跳过**：UU远程的 `assets/39285EFA.dex` 是加密载荷，非 DEX magic——
  跳过并提示，真实类正常输出。
- **本轮新发现（未修复）**：QQ 的
  `com/tencent/qlink/activity/QlinkSendDefaultActivity.java:202` 中，cure 把
  `long` 常量异或折叠成超出 int 范围的十进制字面量 `307638947254`（缺 `L`），
  javac 报“整数太大”。其余 4,102,811 个文件通过；该问题需在
  cure 的字面量折叠/打印处修复。
- **历史 bug 清单**：上方 7 个缺陷来自 2026-09-19 的首轮验证（v0.1.2），均已修复；
  本轮重跑确认当前输出的解析状态；仅上述 QQ 长整数字面量 1 处失败。

