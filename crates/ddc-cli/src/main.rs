//! ddc — DEX decompiler CLI.
//!
//! Usage:
//!   ddc [OPTIONS] <INPUT>... [OUTPUT]
//! INPUT is a .dex image, an .apk/.jar/.zip containing classes*.dex, or a
//! directory scanned recursively for those. Multiple inputs merge into one
//! class pool. OUTPUT (or -o) is a directory, a single .java file (single
//! class only), or `-` for stdout; default: `<input-stem>-out/` sibling.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{bail, Context, Result};

mod arsc;
mod axml;
mod browse;
mod findrefs;
mod inputs;
mod lang;
mod manifest;

use inputs::{
    collect_images, dir_has_dex_files, expand_inputs, filter_images_by_dex, is_dex_ext,
    parse_images,
};

// Expr-tree-heavy workloads do billions of small allocations; the system
// allocator serializes cross-thread frees. mimalloc's per-thread heaps
// unlock the flat thread-scaling curve.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
use crate::lang::{bi, bif};
use ddc_dec::{top_level_classes, ClassOptions, DexPool};
use ddc_dex::DexFile;

fn print_version() {
    match lang::lang() {
        lang::Lang::Zh => println!("ddc {} — DEX → Java 反编译器", env!("CARGO_PKG_VERSION")),
        lang::Lang::En => println!("ddc {} — DEX → Java decompiler", env!("CARGO_PKG_VERSION")),
    }
    println!("https://github.com/ejfkdev/ddc");
}

fn print_help() {
    match lang::lang() {
        lang::Lang::Zh => print_help_zh(),
        lang::Lang::En => print_help_en(),
    }
}

fn print_help_en() {
    println!("ddc {} — DEX → Java decompiler", env!("CARGO_PKG_VERSION"));
    println!("https://github.com/ejfkdev/ddc  (MIT license)");
    println!();
    println!("Language: auto-detected from DDC_LANG/LC_ALL/LANG (zh* selects");
    println!("Chinese, anything else English); DDC_LANG=zh|en forces one.");
    println!();
    println!("Decompiles Android DEX images (versions 035-041, multi-dex APKs,");
    println!("XAPK/APKS/APKM containers, invoke-custom) back into readable Java —");
    println!("fast enough for real-world app bundles (98k classes in ~13s) and");
    println!("queryable like a database through the subcommands below.");
    println!();
    println!("Usage: ddc [OPTIONS] <INPUT>... [OUTPUT]     # full decompile");
    println!("       ddc <SUBCOMMAND> [ARGS...]             # progressive analysis");
    println!("       ddc help | version | -h | -V");
    println!();
    println!("INPUT is a .dex file, an .apk/.jar/.zip archive (classes.dex,");
    println!("classes2.dex, ...), an .xapk/.apks/.apkm container (a zip of APKs:");
    println!("base + config splits; every inner APK's dexes merge, base first),");
    println!("or a directory (scanned recursively). Multiple inputs merge into");
    println!("one class pool (duplicate classes skipped).");
    println!();
    println!("OUTPUT, as the last positional argument or via -o:");
    println!("  <dir>         output root, package structure preserved");
    println!("  <file.java>   one class (single-class input or -c)");
    println!("  -             stdout (`// ===== class =====` separators)");
    println!("  default: <input-stem>-out/ next to the input");
    println!();
    println!("Options:");
    println!("  -o, --output <path>   output location (dir / file.java / -)");
    println!("  -c, --class FQCN      decompile only this class (dotted/slashed)");
    println!("  -l, --list            list class names and exit");
    println!("  -t, --threads <n>     parallel workers (default: CPU count minus the");
    println!("                        file-writer pool; stdout forces one thread for pool order)");
    println!("  --no-comments         omit the provenance header");
    println!("  --symbols <dir>        render IntDef constants as names (built-in");
    println!("                        by default; this rebuilds from an SDK platform");
    println!("                        dir: android.jar + data/annotations.zip)");
    println!("  -v, --verbose         per-dex stats and slow classes on stderr");
    println!("  -h, --help            print this help");
    println!("  -V, --version         print name, version and homepage");
    println!();
    println!("Progressive analysis — query the artifact as a database, no full");
    println!("decompile; metadata loads take well under a second. All accept");
    println!("-d/--dex NAME (repeatable, entry-name substring) to restrict the");
    println!("image set, and most take -o to write results to a file.");
    println!();
    println!("  Get oriented:");
    println!("    ddc info <input>                    app context (label via resources.arsc,");
    println!("                                        package, version, launcher, sdk, size,");
    println!("                                        md5) + per-dex class/method counts");
    println!("    ddc listclasses <input> [pattern]   class names, fuzzy filter");
    println!("    ddc manifest <apk> [--component C]  AndroidManifest.xml → text XML");
    println!("                                        (--component launcher|activity|");
    println!("                                        service|receiver|provider)");
    println!("    ddc mainactivity <apk>              package + launcher activity,");
    println!("                                        verified against the dex");
    println!("    ddc res <apk> [entry] [-o FILE]     list archive entries; dump one");
    println!("                                        (binary XML decoded, binary via -o)");
    println!();
    println!("  Find things:");
    println!("    ddc strings <input> [-f TEXT] [--with-locations]");
    println!("                                        string table; hits mapped to methods");
    println!("    ddc findrefs <input> string TEXT    every string-literal reference");
    println!("    ddc findrefs <input> type|method|field NAME [--class FQCN]");
    println!("                                        refs to a type/call site/field");
    println!("                                        (names fuzzy; --class exact unless");
    println!("                                        --fuzzy-class)");
    println!("    ddc callers <input> NAME [FQCN]     who invokes method NAME");
    println!("    ddc members <input> [NAME] [--class FQCN] [--method|--field]");
    println!("                                        method/field name search");
    println!();
    println!("  Understand structure:");
    println!("    ddc hierarchy <input> FQCN          lineage: extends/implements +");
    println!("                                        subclasses/implementors");
    println!("    ddc largest <input> [-n N]          top-N methods by instruction count");
    println!("    ddc disasm <input> FQCN[.method]    raw bytecode (opcode + pc)");
    println!();
    println!("  Decompile surgically:");
    println!("    ddc getclass <input> FQCN [-o f]    one class (+nested)");
    println!("    ddc getmethod <input> FQCN.method   one method, all overloads");
    println!("    ddc pkg <input> com.foo [-o DIR]    a whole package; --app takes");
    println!("                                        the package from the manifest");
    println!();
    println!("Exit status: 0 ok; 1 some classes failed; 2 usage error.");
    println!();
    println!("Examples:");
    println!("  # full decompile");
    println!("  ddc app.apk                          # → app-out/ next to the apk");
    println!("  ddc app.apk src/                     # dae-style positional output");
    println!("  ddc app.apk -o - | less              # everything to stdout");
    println!("  ddc base.apk patch.dex -o merged/    # split inputs, one pool");
    println!();
    println!("  # one class / one method");
    println!("  ddc app.apk -c com.example.Foo");
    println!("  ddc getclass app.apk com.example.Foo -o Foo.java --dex classes3");
    println!("  ddc getmethod app.apk com.example.Foo.toString");
    println!();
    println!("  # find things");
    println!("  ddc findrefs app.apk string api_key");
    println!("  ddc findrefs app.apk method onCreate --class android/app/Activity");
    println!("  ddc strings app.apk -f token --with-locations");
    println!("  ddc callers app.apk sendMessage");
    println!();
    println!("  # understand the app before decompiling anything");
    println!("  ddc mainactivity app.apk");
    println!("  ddc manifest app.apk --component launcher");
    println!("  ddc hierarchy app.apk androidx.fragment.app.FragmentActivity");
    println!("  ddc pkg app.apk --app -o own/        # just the app's own code");
    println!();
    println!("More: the full CLI reference (every option and subcommand in");
    println!("detail), benchmarks and design notes live in docs/ (English and");
    println!("简体中文) at https://github.com/ejfkdev/ddc");
}

fn print_help_zh() {
    println!("ddc {} — DEX → Java 反编译器", env!("CARGO_PKG_VERSION"));
    println!("https://github.com/ejfkdev/ddc （MIT 许可）");
    println!();
    println!("语言：按 DDC_LANG/LC_ALL/LANG 自动识别（zh* 选中文，其余英文），");
    println!("可用 DDC_LANG=zh|en 强制指定。");
    println!();
    println!("把 Android DEX 镜像（版本 035-041、多 dex APK、XAPK/APKS/APKM 容器、");
    println!("invoke-custom）反编译回可读的 Java —— 真实 App 级别的速度（9.8 万个");
    println!("类约 5 秒），并可通过下面的子命令像查数据库一样查询。");
    println!();
    println!("用法：ddc [选项] <输入>... [输出]          # 全量反编译");
    println!("      ddc <子命令> [参数...]               # 渐进式分析");
    println!("      ddc help | version | -h | -V");
    println!();
    println!("输入是 .dex 文件、.apk/.jar/.zip 归档（classes.dex、classes2.dex、…）、");
    println!(".xapk/.apks/.apkm 容器（一 zip 的 APK：base + config 分包，每个内层");
    println!("APK 的 dex 都并入池，base 优先）或目录（递归扫描）。多个输入合并进");
    println!("一个类池（重名类自动去重）。");
    println!();
    println!("输出（最后一个位置参数或 -o）：");
    println!("  <目录>        输出根目录，保留包结构");
    println!("  <文件.java>   单类（单类输入或 -c）");
    println!("  -             stdout（`// ===== class =====` 分隔）");
    println!("  默认：输入旁的 <输入名>-out/");
    println!();
    println!("选项：");
    println!("  -o, --output <路径>  输出位置（目录 / 文件.java / -）");
    println!("  -c, --class FQCN     只反编译这个类（点分/斜杠均可）");
    println!("  -l, --list           列出类名后退出");
    println!("  -t, --threads <n>    并行 worker 数（默认 CPU 数减去写盘线程；");
    println!("                       stdout 模式强制单线程保证池序）");
    println!("  --no-comments        去掉出处注释头");
    println!("  -v, --verbose        stderr 输出逐 dex 统计与慢类");
    println!("  -h, --help           打印本帮助");
    println!("  -V, --version        打印名称、版本与主页");
    println!();
    println!("渐进式分析 —— 把编译产物当数据库查询，不做全量反编译；元数据加载");
    println!("远低于一秒。所有子命令都接受 -d/--dex NAME（可重复，条目名子串）");
    println!("缩小镜像范围，多数支持 -o 把结果写入文件。");
    println!();
    println!("  先摸清全貌：");
    println!("    ddc info <输入>                     App 上下文（应用名走 resources.arsc");
    println!("                                        解析、包名、版本、启动类、SDK、");
    println!("                                        大小、md5）+ 每镜像类/方法计数");
    println!("    ddc listclasses <输入> [模式]       类名清单，可模糊过滤");
    println!("    ddc manifest <apk> [--component C]  AndroidManifest.xml → 文本 XML");
    println!("                                        （--component launcher|activity|");
    println!("                                        service|receiver|provider）");
    println!("    ddc mainactivity <apk>              包名 + 启动 Activity，并在 dex");
    println!("                                        里定位验证");
    println!("    ddc res <apk> [条目] [-o 文件]      列出归档条目；输出单个内容");
    println!("                                        （二进制 XML 解码，二进制 -o 保存）");
    println!();
    println!("  找东西：");
    println!("    ddc strings <输入> [-f 文本] [--with-locations]");
    println!("                                        字符串表；命中映射到所属方法");
    println!("    ddc findrefs <输入> string 文本     全部字符串字面量引用");
    println!("    ddc findrefs <输入> type|method|field 名字 [--class FQCN]");
    println!("                                        类型/调用点/字段引用（名字模糊");
    println!("                                        匹配；--class 默认精确，");
    println!("                                        --fuzzy-class 变模糊）");
    println!("    ddc callers <输入> 名字 [FQCN]      谁调用了这个方法");
    println!("    ddc members <输入> [名字] [--class FQCN] [--method|--field]");
    println!("                                        方法/字段名检索");
    println!();
    println!("  看清结构：");
    println!("    ddc hierarchy <输入> FQCN           继承谱：extends/implements +");
    println!("                                        子类/实现类");
    println!("    ddc largest <输入> [-n N]           按指令数排序的 top-N 方法");
    println!("    ddc disasm <输入> FQCN[.方法]       原始字节码（操作码 + pc）");
    println!();
    println!("  精准反编译：");
    println!("    ddc getclass <输入> FQCN [-o 文件]  单类（含嵌套）");
    println!("    ddc getmethod <输入> FQCN.方法      单方法，含全部重载");
    println!("    ddc pkg <输入> com.foo [-o 目录]    整个包；--app 自动取 manifest");
    println!("                                        包名");
    println!();
    println!("退出码：0 成功；1 部分类失败；2 用法错误。");
    println!();
    println!("示例：");
    println!("  # 全量反编译");
    println!("  ddc app.apk                          # → apk 旁的 app-out/");
    println!("  ddc app.apk src/                     # dae 风格位置参数输出");
    println!("  ddc app.apk -o - | less              # 全部输出到 stdout");
    println!("  ddc base.apk patch.dex -o merged/    # 分体输入合并一个池");
    println!();
    println!("  # 单类 / 单方法");
    println!("  ddc app.apk -c com.example.Foo");
    println!("  ddc getclass app.apk com.example.Foo -o Foo.java --dex classes3");
    println!("  ddc getmethod app.apk com.example.Foo.toString");
    println!();
    println!("  # 找东西");
    println!("  ddc findrefs app.apk string api_key");
    println!("  ddc findrefs app.apk method onCreate --class android/app/Activity");
    println!("  ddc strings app.apk -f token --with-locations");
    println!("  ddc callers app.apk sendMessage");
    println!();
    println!("  # 反编译之前先摸清这个 App");
    println!("  ddc mainactivity app.apk");
    println!("  ddc manifest app.apk --component launcher");
    println!("  ddc hierarchy app.apk androidx.fragment.app.FragmentActivity");
    println!("  ddc pkg app.apk --app -o own/        # 只反编译 App 自身代码");
    println!();
    println!("更多：完整 CLI 参考（全部选项与子命令详解）、基准与设计文档见");
    println!("docs/（英文与简体中文）https://github.com/ejfkdev/ddc");
}

#[allow(dead_code)]
unsafe fn mimalloc_sys_collect(force: bool) {
    extern "C" {
        fn mi_collect(force: bool);
    }
    mi_collect(force);
}

/// ru_maxrss peak so far, in MB — DDC_STATS phase attribution of the RSS
/// high-water mark (which phase establishes it: raw images, pool
/// materialization, rename registries, or the decompile loop).
#[cfg(unix)]
fn peak_rss_mb() -> f64 {
    // macOS ru_maxrss is BYTES (timeval utime/stime are 16B each, so
    // maxrss sits at offset 32); Linux reports KB. Windows has no
    // getrusage — the old not(apple) fallback referenced it and failed
    // the MSVC link (LNK2019) in the v0.1.18 CI.
    #[repr(C)]
    struct Rusage {
        pad: [i64; 4],
        maxrss: i64,
        // Oversized on purpose: the kernel writes sizeof(struct rusage)
        // (~144B on macOS) regardless of what we read back.
        tail: [i64; 16],
    }
    extern "C" {
        fn getrusage(who: i32, r: *mut Rusage) -> i32;
    }
    let mut r = Rusage {
        pad: [0; 4],
        maxrss: 0,
        tail: [0; 16],
    };
    unsafe { getrusage(0, &mut r) };
    #[cfg(target_vendor = "apple")]
    let scale = 1024.0 * 1024.0;
    #[cfg(not(target_vendor = "apple"))]
    let scale = 1024.0;
    r.maxrss as f64 / scale
}
#[cfg(not(unix))]
fn peak_rss_mb() -> f64 {
    0.0
}

/// CURRENT resident size (MB) on macOS — ru_maxrss is monotone, so
/// purge effects need the instantaneous figure.
#[cfg(target_vendor = "apple")]
fn current_rss_mb() -> f64 {
    #[repr(C)]
    struct TaskBasicInfo {
        virtual_size: u64,
        resident_size: u64,
        resident_size_max: u64,
        total_user_time: u64,
        total_sys_time: u64,
        policy: i32,
        suspend_count: i32,
    }
    extern "C" {
        fn mach_task_self() -> u32;
        fn task_info(
            target_port: u32,
            flavor: i32,
            task_info_out: *mut TaskBasicInfo,
            task_info_out_cnt: *mut u32,
        ) -> i32;
    }
    let mut info = TaskBasicInfo {
        virtual_size: 0,
        resident_size: 0,
        resident_size_max: 0,
        total_user_time: 0,
        total_sys_time: 0,
        policy: 0,
        suspend_count: 0,
    };
    let mut cnt = (std::mem::size_of::<TaskBasicInfo>() / 4) as u32;
    let kr = unsafe { task_info(mach_task_self(), 20, &mut info, &mut cnt) };
    if kr == 0 {
        info.resident_size as f64 / (1024.0 * 1024.0)
    } else {
        0.0
    }
}
#[cfg(not(target_vendor = "apple"))]
fn current_rss_mb() -> f64 {
    0.0
}

fn stats_rss(phase: &str) {
    if std::env::var("DDC_STATS").is_ok() {
        eprintln!(
            "[rss] {phase}: peak {:.0} MB, live {:.0} MB",
            peak_rss_mb(),
            current_rss_mb()
        );
    }
}

fn main() {
    // mimalloc reads MIMALLOC_* lazily on each option's first use; set
    // the purge delay before any significant freeing happens. The 10ms
    // default made the allocator madvise-purge and re-commit segments
    // constantly under the decompiler's bursty per-method IR churn
    // (posix_madvise + arena mutex waits ≈ 5% of weixin's CPU profile);
    // 1s keeps segments hot across bursts without the RSS creep of a
    // longer window (lark: 1624MB at 10s vs 1523MB at 1s, same wall). User-set values
    // win.
    if std::env::var_os("MIMALLOC_PURGE_DELAY").is_none() {
        std::env::set_var("MIMALLOC_PURGE_DELAY", "1000");
    }
    // `ddc ... | less` with the reader quitting closes the pipe: std
    // ignores SIGPIPE, so println! panics with "failed printing to
    // stdout: Broken pipe". Restore the default disposition — a quiet
    // exit, like every other CLI. (zlib-ng-sys already pulls libc into
    // the tree; this makes it a direct, one-line user.)
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = load_symbols_from_args(&args) {
        eprintln!("ddc: {e:?}");
        std::process::exit(2);
    }
    // --symbols (and the generation-only --symbols-out) are consumed
    // here for the WHOLE invocation; strip the pairs so neither the
    // full-run option loop nor the subcommand positional parsing sees
    // them again.
    for flag in ["--symbols", "--symbols-out"] {
        if let Some(i) = args.iter().position(|a| a == flag) {
            args.drain(i..(i + 2).min(args.len()));
        }
    }
    // Leading subcommand word (unless an actual path shadows it) routes
    // to the metadata fast paths: query the artifact as a database
    // instead of decompiling everything.
    if let Some(cmd) = args.first() {
        if is_subcommand(cmd) && !Path::new(cmd).exists() {
            if let Err(e) = run_subcommand(cmd, &args[1..]) {
                eprintln!("ddc: {e:?}");
                eprintln!();
                print_help();
                std::process::exit(2);
            }
            return;
        }
    }
    if let Err(e) = run() {
        // Invalid invocation: show what went wrong, then the full help so
        // the user does not have to re-run with -h.
        eprintln!("ddc: {e:?}");
        eprintln!();
        print_help();
        std::process::exit(2);
    }
}


/// `--symbols <sdk-platform-dir>`: install the IntDef/LongDef constant
/// database (android.jar constants + data/annotations.zip domains) that
/// renders `setVisibility(8)` as `android.view.View.GONE`. Accepts the
/// platform directory (`platforms/android-37.0`) or a bare android.jar
/// (metadata looked up in `<parent>/data/annotations.zip`). Scanned
/// once here — every path (full run and subcommands) flows through
/// main().
/// Built-in platform symbol table: the readable
/// `src/platform_symbols.txt`, raw-DEFLATE'd by build.rs into OUT_DIR.
/// Text form for maintenance, compressed form for the binary.
static BUILTIN_SYMBOLS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/platform_symbols.txt.gz"));

fn load_symbols_from_args(args: &[String]) -> Result<()> {
    let Some(idx) = args.iter().position(|a| a == "--symbols") else {
        // Built-in default (android-37, see scripts/gen-platform-symbols.sh):
        // 1350 IntDef domains as a deflated 83KB blob. Installed at every
        // startup — the table is consulted only when a literal matches a
        // platform call site, so the cost is one 750KB deserialize.
        let raw = inflate(BUILTIN_SYMBOLS)?;
        if let Some(syms) = ddc_dec::platform::PlatformSymbols::from_text(&raw) {
            ddc_dec::platform::set_symbols(syms);
        }
        return Ok(());
    };
    // Generation mode (scripts/gen-platform-symbols.sh): build the
    // table from this platform and write the embedded-format blob.
    if let Some(out) = args
        .iter()
        .position(|a| a == "--symbols-out")
        .and_then(|i| args.get(i + 1))
    {
        let platform = args
            .get(idx + 1)
            .context("--symbols-out needs --symbols <platform-dir>")?;
        let syms = build_symbols(Path::new(platform))?;
        std::fs::write(out, syms.to_text())?;
        eprintln!(
            "{}",
            bif!(
                "ddc: wrote {0} ({1} domains, text format)",
                "ddc：已写出 {0}（{1} 个域，文本格式）";
                out, syms.domain_count()
            )
        );
        return Ok(());
    }
    let platform = args
        .get(idx + 1)
        .context("--symbols needs a path (the SDK platform directory, e.g. ~/Library/Android/sdk/platforms/android-37.0)")?;
    let syms = build_symbols(Path::new(platform))?;
    ddc_dec::platform::set_symbols(syms);
    Ok(())
}

/// android.jar constants + data/annotations.zip domains → the symbol
/// table. The caller decides whether to install or serialize it.
fn build_symbols(path: &Path) -> Result<ddc_dec::platform::PlatformSymbols> {
    let (jar, zip) = if path.is_dir() {
        let jar = path.join("android.jar");
        let zip = path.join("data").join("annotations.zip");
        anyhow::ensure!(jar.is_file(), "--symbols: {} has no android.jar", path.display());
        (jar, Some(zip))
    } else {
        anyhow::ensure!(
            path.file_name().and_then(|n| n.to_str()) == Some("android.jar"),
            "--symbols: expected the platform directory or an android.jar, got {}",
            path.display()
        );
        let zip = path.parent().and_then(|p| p.parent()).map(|p| p.join("data").join("annotations.zip"));
        (path.to_path_buf(), zip)
    };
    // ---- constants out of every .class in android.jar ----
    let src = inputs::map_source(&jar)?;
    let bytes = src.bytes();
    let mut constants: std::collections::HashMap<(String, String), i64> =
        std::collections::HashMap::new();
    for e in zip_entries(bytes)? {
        if !e.name.ends_with(".class") {
            continue;
        }
        if let Ok(raw) = manifest::entry_bytes(bytes, &e) {
            if let Some((cons, _)) = ddc_dec::platform::classfile_constants(&raw) {
                constants.extend(cons);
            }
        }
    }
    // ---- domains out of annotations.zip ----
    let mut xmls: Vec<Vec<u8>> = Vec::new();
    if let Some(zip) = zip.filter(|z| z.is_file()) {
        let zsrc = inputs::map_source(&zip)?;
        let zbytes = zsrc.bytes();
        for e in zip_entries(zbytes)? {
            if e.name.ends_with("annotations.xml") {
                if let Ok(raw) = manifest::entry_bytes(zbytes, &e) {
                    xmls.push(raw);
                }
            }
        }
    }
    let refs: Vec<&[u8]> = xmls.iter().map(|v| v.as_slice()).collect();
    Ok(ddc_dec::platform::PlatformSymbols::from_metadata(&refs, &constants))
}

fn is_subcommand(word: &str) -> bool {
    matches!(
        word,
        "getclass"
            | "listclasses"
            | "findrefs"
            | "manifest"
            | "info"
            | "strings"
            | "members"
            | "hierarchy"
            | "largest"
            | "disasm"
            | "callers"
            | "pkg"
            | "getmethod"
            | "mainactivity"
            | "res"
    )
}

/// Progressive-analysis commands (ASC-style "query, don't decompile"):
/// each loads only the metadata it needs.
fn run_subcommand(cmd: &str, args: &[String]) -> Result<()> {
    let t0 = std::time::Instant::now();
    match cmd {
        "manifest" => cmd_manifest(args, t0),
        "info" => cmd_info(args, t0),
        "listclasses" => cmd_listclasses(args, t0),
        "getclass" => cmd_getclass(args, t0),
        "findrefs" => cmd_findrefs(args, t0),
        "strings" => browse::cmd_strings(args),
        "members" => browse::cmd_members(args),
        "hierarchy" => browse::cmd_hierarchy(args),
        "largest" => browse::cmd_largest(args),
        "disasm" => browse::cmd_disasm(args),
        "callers" => browse::cmd_callers(args),
        "pkg" => browse::cmd_pkg(args),
        "getmethod" => browse::cmd_getmethod(args),
        "mainactivity" => browse::cmd_mainactivity(args),
        "res" => browse::cmd_res(args),
        _ => bail!(
            "{}",
            bif!("unknown subcommand: {0}", "未知子命令：{0}"; cmd)
        ),
    }
}

/// Shared: first positional argument of a subcommand (the input).
fn sub_input(args: &[String], cmd: &str) -> Result<PathBuf> {
    args.iter()
        .find(|a| !a.starts_with('-'))
        .map(PathBuf::from)
        .with_context(|| format!("{cmd} needs an input file"))
}

/// `142.9 MB` style human size.
fn fmt_bytes(n: u64) -> String {
    if n >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", n as f64 / 1073741824.0)
    } else if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f64 / 1048576.0)
    } else if n >= 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{n} B")
    }
}

/// Compact MD5 (RFC 1321) for sample identification — one hash line, no
/// crypto-crate dependency for it.
fn md5_hex(path: &std::path::Path) -> Result<String> {
    let src = inputs::map_source(path)?;
    let data = src.bytes();
    let mut state: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    let (full, rem) = data.as_chunks::<64>();
    for b in full {
        md5_block(&mut state, b);
    }
    let bitlen = (data.len() as u64).wrapping_mul(8);
    let mut last = [0u8; 64];
    last[..rem.len()].copy_from_slice(rem);
    last[rem.len()] = 0x80;
    if rem.len() < 56 {
        last[56..64].copy_from_slice(&bitlen.to_le_bytes());
        md5_block(&mut state, &last);
    } else {
        md5_block(&mut state, &last);
        let mut extra = [0u8; 64];
        extra[56..64].copy_from_slice(&bitlen.to_le_bytes());
        md5_block(&mut state, &extra);
    }
    // MD5 words serialize little-endian: digest = concat(le_bytes(a..d)).
    Ok(state
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn md5_block(state: &mut [u32; 4], block: &[u8; 64]) {
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14,
        20, 5, 9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11,
        16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let mut m = [0u32; 16];
    for (i, w) in m.iter_mut().enumerate() {
        *w = u32::from_le_bytes(block[i * 4..i * 4 + 4].try_into().unwrap());
    }
    let (mut a, mut b, mut c, mut d) = (state[0], state[1], state[2], state[3]);
    for i in 0..64 {
        let (f, g) = match i / 16 {
            0 => ((b & c) | (!b & d), i),
            1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
            2 => (b ^ c ^ d, (3 * i + 5) % 16),
            _ => (c ^ (b | !d), (7 * i) % 16),
        };
        let tmp = d;
        d = c;
        c = b;
        let sum = a.wrapping_add(f).wrapping_add(K[i]).wrapping_add(m[g]);
        b = b.wrapping_add(sum.rotate_left(S[i]));
        a = tmp;
    }
    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
}

// ---- manifest ---------------------------------------------------------------

fn cmd_manifest(args: &[String], t0: std::time::Instant) -> Result<()> {
    let input = sub_input(args, "manifest")?;
    let mut out: Option<PathBuf> = None;
    let mut component: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" | "--output" => {
                out = Some(PathBuf::from(
                    args.get(i + 1)
                        .context(bi!("-o needs a value", "-o 需要一个值"))?,
                ));
                i += 1;
            }
            "-c" | "--component" => {
                component = Some(
                    args.get(i + 1)
                        .context(bi!("--component needs a value", "--component 需要一个值"))?
                        .clone(),
                );
                i += 1;
            }
            a if a.starts_with('-') => bail!(
                "{}",
                bif!("manifest: unknown option {0}", "manifest：未知选项 {0}"; a)
            ),
            _ => {}
        }
        i += 1;
    }
    // Extraction lives in manifest.rs (shared with mainactivity/pkg --app).
    let (label, xml) = manifest::manifest_xml(&input)?;
    let text = match &component {
        Some(c) => manifest::component_xml(&xml, c),
        None => xml,
    };
    match out {
        Some(f) => {
            if let Some(parent) = f.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(&f, &text)?;
            eprintln!(
                "ddc: wrote manifest ({label}) to {} in {}",
                f.display(),
                fmt_secs(t0.elapsed())
            );
        }
        None => {
            // stdout mode: the XML only — no trailing timing noise.
            print!("{text}");
        }
    }
    Ok(())
}

// ---- info -------------------------------------------------------------------

fn cmd_info(args: &[String], _t0: std::time::Instant) -> Result<()> {
    let input = sub_input(args, "info")?;
    // Context header — best effort: only when a manifest exists (an APK
    // or container; a bare .dex/jar drops straight to the table). The
    // label resolves `@0x…` refs through resources.arsc.
    if let Ok(facts) = manifest::facts_for(&input) {
        let label = facts
            .label
            .as_deref()
            .map(|l| arsc::resolve_string_ref(&input, l).unwrap_or_else(|| l.to_string()));
        let row = |k: &str, v: String| println!("{:<12}{}", k, v);
        row(
            bi!("label", "应用名"),
            label.unwrap_or_else(|| bi!("-", "无").to_string()),
        );
        row(bi!("package", "包名"), facts.package.clone());
        row(
            bi!("version", "版本"),
            match (&facts.version_name, &facts.version_code) {
                (Some(n), Some(c)) => format!("{n} ({c})"),
                (Some(n), None) => n.clone(),
                (None, Some(c)) => format!("({c})"),
                (None, None) => bi!("-", "无").to_string(),
            },
        );
        if let Some(app) = &facts.application {
            row(bi!("application", "应用类"), app.clone());
        }
        row(
            bi!("launcher", "启动类"),
            facts
                .launcher
                .clone()
                .unwrap_or_else(|| bi!("-", "无").to_string()),
        );
        row(
            bi!("sdk", "SDK"),
            match (&facts.min_sdk, &facts.target_sdk) {
                (Some(m), Some(t)) => format!("{m}–{t}"),
                (Some(m), None) => format!("min {m}"),
                (None, Some(t)) => format!("target {t}"),
                (None, None) => bi!("-", "无").to_string(),
            },
        );
    }
    if input.is_file() {
        let size = std::fs::metadata(&input)?.len();
        println!(
            "{:<12}{}",
            bi!("size", "大小"),
            bif!(
                "{0} ({1} bytes)",
                "{0}（{1} 字节）";
                fmt_bytes(size),
                size
            )
        );
        println!("{:<12}{}", "md5", md5_hex(&input)?);
    }
    println!();
    let files = expand_inputs(&[input])?;
    let parsed = parse_images(collect_images(&files)?)?;
    let mut total_classes = 0usize;
    println!(
        "{:>10}  {:>8}  {:>10}  {:>10}  {:>9}  {:>10}",
        bi!("image", "镜像"),
        bi!("dex", "版本"),
        bi!("classes", "类"),
        bi!("methods", "方法"),
        bi!("fields", "字段"),
        bi!("strings", "字符串")
    );
    for (label, dex) in &parsed {
        let classes = dex.class_defs.len();
        total_classes += classes;
        println!(
            "{:>10}  {:>8}  {:>10}  {:>10}  {:>9}  {:>10}",
            label,
            dex.version,
            classes,
            dex.method_count(),
            dex.field_count(),
            dex.string_count(),
        );
    }
    println!(
        "{}",
        bif!(
            "total: {0} image(s), {1} classes",
            "合计：{0} 个镜像，{1} 个类";
            parsed.len(),
            total_classes
        )
    );
    Ok(())
}

// ---- listclasses ------------------------------------------------------------

fn cmd_listclasses(args: &[String], _t0: std::time::Instant) -> Result<()> {
    let mut input: Option<PathBuf> = None;
    let mut pattern: Option<String> = None;
    let mut dex_filters: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-d" | "--dex" => {
                dex_filters.push(
                    args.get(i + 1)
                        .context(bi!("--dex needs a value", "--dex 需要一个值"))?
                        .to_string(),
                );
                i += 1;
            }
            a if a.starts_with('-') => bail!(
                "{}",
                bif!("listclasses: unknown option {0}", "listclasses：未知选项 {0}"; a)
            ),
            a => {
                if input.is_none() {
                    input = Some(PathBuf::from(a));
                } else if pattern.is_none() {
                    pattern = Some(a.to_string());
                } else {
                    bail!(
                        "{}",
                        bi!(
                            "listclasses: too many arguments (input and optional pattern)",
                            "listclasses：参数过多（输入 + 可选模式）"
                        )
                    );
                }
            }
        }
        i += 1;
    }
    let input = input.context(bi!(
        "listclasses needs an input file",
        "listclasses 需要输入文件"
    ))?;
    // Class names need only each image's class_defs → type_ids → the
    // class-name STRING ENTRIES: prefix decoding straight off the inflated
    // bytes (a full DexFile::parse decodes the whole string table — the
    // names are a small slice of it).
    let files = expand_inputs(&[input])?;
    let images = filter_images_by_dex(collect_images(&files)?, &dex_filters)?;
    let mut _total = 0usize;
    let mut all: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let handles: Vec<_> = images
        .into_iter()
        .map(|img| {
            std::thread::spawn(move || {
                let raw = match img.method {
                    ZipMethod::Stored => img.data.bytes()[img.range].to_vec(),
                    ZipMethod::Deflate => {
                        // Stream and STOP at the class-name working set
                        // (id tables + string data): the code section —
                        // most of the bytes — is never inflated.
                        inputs::inflate_until(img.data.bytes(), img.range.clone(), |out| {
                            match inputs::scan_prefix_needed(out) {
                                None => inputs::PrefixStep::Continue(0),
                                Some(needed) => {
                                    if out.len() >= needed {
                                        inputs::PrefixStep::Abort
                                    } else {
                                        inputs::PrefixStep::Continue(needed - out.len())
                                    }
                                }
                            }
                        })
                        .unwrap_or_default()
                    }
                };
                DexFile::class_names_from_image(&raw)
            })
        })
        .collect();
    for h in handles {
        if let Ok(names) = h.join() {
            _total += names.len();
            for name in names {
                if seen.insert(name.clone()) {
                    all.push(name);
                }
            }
        }
    }
    let shown: Vec<String> = match &pattern {
        Some(p) => {
            let lp = p.to_ascii_lowercase();
            all.into_iter()
                .filter(|n| n.to_ascii_lowercase().contains(&lp))
                .collect()
        }
        None => all,
    };
    // stdout mode: names only — no trailing summary/timing noise.
    for name in &shown {
        println!("{}", name.replace('/', "."));
    }
    Ok(())
}

// ---- getclass ---------------------------------------------------------------

fn cmd_getclass(args: &[String], t0: std::time::Instant) -> Result<()> {
    // All positionals but the LAST are inputs; the last is the class.
    let mut fqcn: Option<String> = None;
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut out: Option<PathBuf> = None;
    let mut dex_filters: Vec<String> = Vec::new();
    let mut positionals: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" | "--output" => {
                out = Some(PathBuf::from(
                    args.get(i + 1)
                        .context(bi!("-o needs a value", "-o 需要一个值"))?,
                ));
                i += 1;
            }
            "-d" | "--dex" => {
                dex_filters.push(
                    args.get(i + 1)
                        .context(bi!("--dex needs a value", "--dex 需要一个值"))?
                        .to_string(),
                );
                i += 1;
            }
            a if a.starts_with('-') => bail!(
                "{}",
                bif!("getclass: unknown option {0}", "getclass：未知选项 {0}"; a)
            ),
            a => positionals.push(a.to_string()),
        }
        i += 1;
    }
    if let Some((last, heads)) = positionals.split_last() {
        fqcn = Some(last.clone());
        inputs = heads.iter().map(PathBuf::from).collect();
    }
    let fqcn = fqcn.context(bi!(
        "getclass needs a class name (com.example.Foo)",
        "getclass 需要类名（com.example.Foo）"
    ))?;
    inputs
        .first()
        .cloned()
        .context(bi!("getclass needs an input file", "getclass 需要输入文件"))?;
    let (text, defining_names) = getclass_text(&inputs, &fqcn, &dex_filters)?;
    let text = format!("{text}\n");
    if defining_names.len() > 1 {
        eprintln!(
            "{}",
            bif!(
                "ddc: class {0} is defined in {1} images: {2} — using {3} (pass --dex <name> to pick another)",
                "ddc：类 {0} 定义在 {1} 个镜像中：{2} —— 使用 {3}（可用 --dex <name> 指定）";
                fqcn,
                defining_names.len(),
                defining_names.join(", "),
                defining_names[0]
            )
        );
    }
    match out {
        Some(f) => {
            if let Some(parent) = f.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(&f, &text)?;
            eprintln!(
                "{}",
                bif!("ddc: wrote {0} in {1}", "ddc：已写出 {0}，用时 {1}"; f.display(), fmt_secs(t0.elapsed()))
            );
        }
        None => {
            // stdout mode: the source only — no trailing timing noise.
            print!("{text}");
        }
    }
    Ok(())
}

/// The decompiled source of ONE class + the short names of every image
/// that defines it. Shared by getclass (whole class) and getmethod
/// (method-level slice of the same text).
#[allow(clippy::type_complexity)]
pub(crate) fn getclass_text(
    inputs: &[PathBuf],
    fqcn: &str,
    dex_filters: &[String],
) -> Result<(String, Vec<String>)> {
    let files = expand_inputs(inputs)?;
    let parsed = parse_images(filter_images_by_dex(collect_images(&files)?, dex_filters)?)?;
    let internal = fqcn.replace('.', "/");

    // Which images actually define the class? (The pool is first-wins —
    // a name present in several dexes would otherwise resolve silently.)
    let defining: Vec<usize> = parsed
        .iter()
        .enumerate()
        .filter(|(_, (_, dex))| {
            dex.class_defs
                .iter()
                .any(|cd| dex.class_name(cd.class_idx) == internal)
        })
        .map(|(i, _)| i)
        .collect();
    if defining.is_empty() {
        let hint = if dex_filters.is_empty() {
            " (try `ddc listclasses <input> <pattern>`)"
        } else {
            ""
        };
        bail!(
            "{}",
            bif!("class {0} not found in the selected image(s){1}", "在所选镜像中找不到类 {0}{1}"; fqcn, hint)
        );
    }
    let defining_names: Vec<String> = defining
        .iter()
        .map(|&i| {
            parsed[i]
                .0
                .rsplit_once('!')
                .map(|(_, e)| e.to_string())
                .unwrap_or_else(|| parsed[i].0.clone())
        })
        .collect();

    // Register the defining image FIRST so the first-wins pool resolves
    // the class from it; names registered, classes materialized on
    // demand — the annotation-read cost is paid for ONE family, not every
    // class in the artifact.
    let mut order: Vec<usize> = Vec::with_capacity(parsed.len());
    order.extend(defining.iter().copied());
    for i in 0..parsed.len() {
        if !defining.contains(&i) {
            order.push(i);
        }
    }
    let mut slots: Vec<Option<(String, DexFile)>> = parsed.into_iter().map(Some).collect();
    let mut pool = DexPool::new();
    for i in order {
        let (label, dex) = slots[i].take().context("image index out of range")?;
        let idx = pool.add_dex_lazy(dex);
        pool.set_dex_label(idx, label);
    }
    let pool = std::sync::Arc::new(pool);
    let pc = pool.get(&internal).with_context(|| {
        bif!("class {0} not found (try `ddc listclasses <input> <pattern>`)", "找不到类 {0}（可用 `ddc listclasses <输入> <模式>`）"; fqcn)
    })?;
    // AFTER materializing the target: member renames (field/method
    // collisions) need the pooled fields of the classes being emitted.
    ddc_dec::install_case_renames(&pool);

    let pending: std::sync::Mutex<Vec<ddc_dec::classdec::PendingMonitor>> =
        std::sync::Mutex::new(Vec::new());
    let opts = ClassOptions::default();
    let result = ddc_dec::classdec::decompile_class(&pool, pc, &opts, &pending)
        .map_err(|e| anyhow::anyhow!("{:#}", e));
    let text = match result {
        Ok(t) => t,
        Err(e) => {
            if format!("{e:#}").contains("deferred to monitored thread") {
                let (rx, name, deadline) = pending.into_inner().unwrap().remove(0);
                let now = std::time::Instant::now();
                let remaining = deadline.saturating_duration_since(now);
                rx.recv_timeout(remaining)
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "{}",
                            bif!("{0}: decompile timed out", "{0}：反编译超时"; name)
                        )
                    })?
                    .map_err(|e| anyhow::anyhow!("{e}"))?
            } else {
                return Err(e);
            }
        }
    };
    Ok((text, defining_names))
}

/// Stream the entry; resolve the query's targets COMPLETELY as soon as the
/// prefix holds the tables + the whole string data section. Zero targets →
/// abort with empty (the code section — the bulk of the bytes — is never
/// inflated); targets resolved → drain the rest and hand the resolution to
/// the scanner (single pass: no second string matching); unresolved →
/// drain and let the scanner resolve on the full image.
/// The query's resolved targets: the kind bit and the string/type id set.
type ResolvedTargets = (u8, std::collections::BTreeSet<u32>);

fn prefix_or_full(
    data: &[u8],
    range: &std::ops::Range<usize>,
    query: &findrefs::FindQuery,
) -> std::result::Result<(Vec<u8>, Option<ResolvedTargets>), String> {
    use std::collections::BTreeSet;
    let mut resolved: Option<Option<(u8, BTreeSet<u32>)>> = None;
    let image = inputs::inflate_until(data, range.clone(), |out| {
        match inputs::scan_prefix_needed(out) {
            None => inputs::PrefixStep::Continue(0),
            Some(needed) => {
                if out.len() >= needed {
                    match findrefs::resolve_on_prefix(out, query) {
                        // Zero targets: skip the code section entirely.
                        Some((bit, set)) if set.is_empty() => {
                            resolved = Some(Some((bit, set)));
                            inputs::PrefixStep::Abort
                        }
                        // Targets resolved: drain, pass them along.
                        p @ Some(_) => {
                            resolved = Some(p);
                            inputs::PrefixStep::Continue(usize::MAX)
                        }
                        // Cannot resolve yet: drain, scanner resolves.
                        None => inputs::PrefixStep::Continue(usize::MAX),
                    }
                } else {
                    inputs::PrefixStep::Continue(needed - out.len())
                }
            }
        }
    })
    .map_err(|e| e.to_string())?;
    let pre = resolved.flatten();
    Ok((image, pre))
}

// ---- findrefs ---------------------------------------------------------------

fn cmd_findrefs(args: &[String], t0: std::time::Instant) -> Result<()> {
    let mut positionals: Vec<String> = Vec::new();
    let mut class: Option<String> = None;
    let mut fuzzy_class = false;
    let mut dex_filters: Vec<String> = Vec::new();
    let mut out: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--class" | "-C" => {
                class = Some(
                    args.get(i + 1)
                        .context(bi!("--class needs a value", "--class 需要一个值"))?
                        .to_string(),
                );
                i += 1;
            }
            "--fuzzy-class" => fuzzy_class = true,
            "-d" | "--dex" => {
                dex_filters.push(
                    args.get(i + 1)
                        .context(bi!("--dex needs a value", "--dex 需要一个值"))?
                        .to_string(),
                );
                i += 1;
            }
            "-o" | "--output" => {
                out = Some(PathBuf::from(
                    args.get(i + 1)
                        .context(bi!("-o needs a value", "-o 需要一个值"))?,
                ));
                i += 1;
            }
            a if a.starts_with('-') => bail!(
                "{}",
                bif!("findrefs: unknown option {0}", "findrefs：未知选项 {0}"; a)
            ),
            a => positionals.push(a.to_string()),
        }
        i += 1;
    }
    if positionals.len() < 3 {
        bail!(
            "{}",
            bi!(
                "findrefs needs: <input> <string|type|method|field> <query> \\
             (method/field also take --class X, --fuzzy-class)",
                "findrefs 需要：<输入> <string|type|method|field> <查询> \\
             （method/field 还可加 --class X、--fuzzy-class）"
            )
        );
    }
    let input = PathBuf::from(&positionals[0]);
    let kind = positionals[1].clone();
    let value = positionals[2].clone();
    let query = match kind.as_str() {
        "string" => findrefs::FindQuery::String(value),
        "type" => findrefs::FindQuery::Type(value),
        "method" => findrefs::FindQuery::Method {
            class,
            name: value,
            fuzzy_class,
        },
        "field" => findrefs::FindQuery::Field {
            class,
            name: value,
            fuzzy_class,
        },
        other => bail!(
            "{}",
            bif!("findrefs: unknown kind {0:?} (string|type|method|field)", "findrefs：未知类别 {0:?}（string|type|method|field）"; other)
        ),
    };

    let files = expand_inputs(&[input])?;
    let t_wall = std::time::Instant::now();
    let images = filter_images_by_dex(collect_images(&files)?, &dex_filters)?;
    #[allow(unused_variables)]
    let t_cd = t_wall.elapsed();
    // PIPELINED scan: a producer parses images in small waves and feeds a
    // channel; scanner threads consume and DROP each dex — the inflate of
    // wave N+1 overlaps the scan of wave N, so the parallel decode
    // bandwidth survives while resident memory stays bounded to the
    // in-flight images (holding every image resident cost ~1.2GB on a
    // 353MB APK; ASC's per-worker streaming runs ~170MB).
    const SCANNERS: usize = 8;
    const IN_FLIGHT: usize = 12;
    type Payload = (
        String,
        Vec<u8>,
        Option<(u8, std::collections::BTreeSet<u32>)>,
    );
    // One inflate/parse thread PER IMAGE (full decode parallelism); the
    // bounded channel backpressures the producers, so resident memory is
    // capped at IN_FLIGHT inflated images regardless of the dex count —
    // the old fixed-width waves serialized the inflate bursts.
    let chan = std::sync::Arc::new(Chan::<Payload>::new(IN_FLIGHT));
    let producer = {
        let chan = chan.clone();
        let query = query.clone();
        std::thread::spawn(move || -> Result<()> {
            let handles: Vec<std::thread::JoinHandle<std::result::Result<(), String>>> = images
                .into_iter()
                .map(|img| {
                    let chan = chan.clone();
                    let query = query.clone();
                    std::thread::spawn(move || -> std::result::Result<(), String> {
                        // Inflate only: the scan path works on the raw
                        // image (zero table materialization). Deflated
                        // entries stream through a PREFIX decider:
                        // once the id tables + string data are in, the
                        // query resolves on the prefix; a dex with NO
                        // matching targets is dropped without paying
                        // for its code section (the bulk of the bytes).
                        let (raw, pre): (Vec<u8>, _) = match img.method {
                            ZipMethod::Stored => (img.data.bytes()[img.range].to_vec(), None),
                            ZipMethod::Deflate => {
                                prefix_or_full(img.data.bytes(), &img.range, &query)?
                            }
                        };
                        if raw.is_empty() {
                            // Prefix resolution found no targets here.
                            return Ok(());
                        }
                        chan.push((img.label, raw, pre));
                        Ok(())
                    })
                })
                .collect();
            let mut first_err: Option<String> = None;
            for h in handles {
                match h.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        first_err.get_or_insert(e);
                    }
                    Err(_) => {
                        first_err.get_or_insert("parse thread panicked".into());
                    }
                }
            }
            chan.close();
            match first_err {
                Some(e) => Err(anyhow::anyhow!("{e}")),
                None => Ok(()),
            }
        })
    };
    let mut hits = Vec::new();
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..SCANNERS {
            let chan = &chan;
            let query = query.clone();
            handles.push(scope.spawn(move || -> Vec<findrefs::Hit> {
                let mut out = Vec::new();
                while let Some((label, image, pre)) = chan.pop() {
                    if let Ok(mut one) = findrefs::scan_image(&label, &image, &query, pre) {
                        out.append(&mut one);
                    }
                }
                out
            }));
        }
        for h in handles {
            if let Ok(v) = h.join() {
                hits.extend(v);
            }
        }
    });
    producer
        .join()
        .map_err(|_| anyhow::anyhow!("parse producer panicked"))??;
    let t_scan = t_wall.elapsed();
    if std::env::var("DDC_WALL").is_ok() {
        eprintln!(
            "[wall] cd+collect={:?} inflate+scan={:?} hits={}",
            t_cd,
            t_scan - t_cd,
            hits.len()
        );
    }
    hits.sort_by(|a, b| a.class.cmp(&b.class).then(a.method.cmp(&b.method)));

    // Column format like `info`: header row first, then one row per
    // METHOD (multi-hit methods keep one line; the refs column lists
    // every matched target, "; " separated). Fixed columns keep the
    // class/method boundary explicit without the pipe-delimited blob.
    let lines: Vec<String> = hits
        .iter()
        .map(|h| {
            format!(
                "{:10}  {:<12}  {} {}  {}",
                h.dex,
                h.insn,
                h.class,
                h.method,
                h.targets.join("; ")
            )
        })
        .collect();
    let header = format!(
        "{:10}  {:<12}  {}",
        bi!("dex", "镜像"),
        bi!("kind", "类型"),
        bi!("class method refs", "类 方法 引用")
    );
    let mut all = vec![header];
    all.extend(lines);
    match out {
        Some(f) => {
            if let Some(parent) = f.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let mut text = all.join("\n");
            text.push('\n');
            std::fs::write(&f, &text)?;
            eprintln!(
                "ddc: findrefs {} → {} method(s) in {} to {}",
                query.kind(),
                all.len() - 1,
                fmt_secs(t0.elapsed()),
                f.display()
            );
        }
        None => {
            // stdout mode: results only — no trailing timing noise.
            for l in &all {
                println!("{l}");
            }
        }
    }
    Ok(())
}

/// Bounded MPMC hand-off queue: producers block when full (memory stays
/// capped at `cap` in-flight items), consumers block while empty and wake
/// correctly on close. (std mpsc has no multi-consumer; wrapping its
/// recv in a Mutex serializes all waiters.)
struct Chan<T> {
    q: std::sync::Mutex<std::collections::VecDeque<T>>,
    not_empty: std::sync::Condvar,
    not_full: std::sync::Condvar,
    cap: usize,
    closed: std::sync::atomic::AtomicBool,
}

impl<T> Chan<T> {
    fn new(cap: usize) -> Self {
        Chan {
            q: std::sync::Mutex::new(std::collections::VecDeque::new()),
            not_empty: std::sync::Condvar::new(),
            not_full: std::sync::Condvar::new(),
            cap,
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }
    fn push(&self, v: T) {
        let mut q = self.q.lock().unwrap();
        while q.len() >= self.cap && !self.closed.load(std::sync::atomic::Ordering::Acquire) {
            q = self.not_full.wait(q).unwrap();
        }
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        q.push_back(v);
        self.not_empty.notify_one();
    }
    fn close(&self) {
        // The flag flip MUST hold the queue lock: otherwise a consumer can
        // observe closed=false, release the lock, and enter wait() after
        // the notify_all has already fired — a lost wakeup that hangs the
        // scanner forever (seen as a 12-minute zombie test process).
        let _q = self.q.lock().unwrap();
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        self.not_empty.notify_all();
        self.not_full.notify_all();
    }
    fn pop(&self) -> Option<T> {
        let mut q = self.q.lock().unwrap();
        loop {
            if let Some(v) = q.pop_front() {
                self.not_full.notify_one();
                return Some(v);
            }
            if self.closed.load(std::sync::atomic::Ordering::Acquire) {
                return None;
            }
            q = self.not_empty.wait(q).unwrap();
        }
    }
}

/// Where the decompiled source goes.
#[derive(Debug, Clone)]
enum Sink {
    /// Print to stdout with `// ===== class =====` separators.
    Stdout,
    /// Root directory; each class at its package-relative path.
    Dir(PathBuf),
    /// One explicit .java file (single emitted class only).
    File(PathBuf),
}

fn run() -> Result<()> {
    let t_start = std::time::Instant::now();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut positionals: Vec<PathBuf> = Vec::new();
    let mut out: Option<String> = None;
    let mut only: Option<String> = None;
    let mut list = false;
    let mut comments = true;
    let mut workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let mut threads_explicit = false;
    let mut verbose = false;

    let mut i = 0;
    while i < args.len() {
        // Bare commands (only when they cannot be an input yet).
        if positionals.is_empty() && !args[i].starts_with('-') {
            match args[i].as_str() {
                "help" => {
                    print_help();
                    return Ok(());
                }
                "version" => {
                    print_version();
                    return Ok(());
                }
                _ => {}
            }
        }
        // Split `--opt=value` / `-o=path` into name + inline value.
        let (name, inline_val) = match args[i].split_once('=') {
            Some((n, v)) if n.starts_with('-') => (n.to_string(), Some(v.to_string())),
            _ => (args[i].clone(), None),
        };
        macro_rules! take_value {
            () => {{
                match inline_val.clone() {
                    Some(v) => v,
                    None => {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .with_context(|| format!("{} needs a value", name))?
                    }
                }
            }};
        }
        match name.as_str() {
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            "-V" | "--version" => {
                print_version();
                return Ok(());
            }
            "-o" | "--output" => out = Some(take_value!()),
            "-c" | "--class" => only = Some(take_value!()),
            "-l" | "--list" => list = true,
            "--no-comments" => comments = false,
            // consumed in main() for the whole invocation; skip here.
            "--symbols" => {
                let _ = take_value!();
            }
            "-t" | "--threads" => {
                workers = take_value!().parse().unwrap_or(4);
                threads_explicit = true;
            }
            "-v" | "--verbose" => verbose = true,
            other => {
                if other.starts_with('-') {
                    bail!(
                        "{}",
                        bif!("unknown option {0} (try --help)", "未知选项 {0}（见 --help）"; other)
                    );
                }
                positionals.push(PathBuf::from(other));
            }
        }
        i += 1;
    }
    if positionals.is_empty() {
        bail!(
            "{}",
            bi!("no input given (try --help)", "未提供输入文件（见 --help）")
        );
    }
    // dae/pycdc-style positional output: with no -o and TWO OR MORE
    // positionals, a LAST argument is the OUTPUT unless it looks like an
    // input (`ddc base.apk patch.dex merged/`). "Looks like an input":
    // has a dex-bearing extension, is a file, or is a directory that
    // actually CONTAINS dex-bearing files. A directory without any
    // (e.g. a pre-created output dir, or last run's output) is the
    // OUTPUT — treating it as an input made `ddc apk weibo` with an
    // existing empty `weibo/` silently fall back to the default
    // sibling dir.
    if out.is_none() && positionals.len() >= 2 {
        let last = positionals[positionals.len() - 1].clone();
        let looks_input = is_dex_ext(&last)
            || (last.exists() && !last.is_dir())
            || (last.is_dir() && dir_has_dex_files(&last));
        if !looks_input {
            positionals.pop();
            out = Some(last.display().to_string());
        }
    }
    let inputs = positionals;

    let t_wall = std::time::Instant::now();
    let files = expand_inputs(&inputs)?;
    let t_read = t_wall.elapsed();
    let parsed = parse_images(collect_images(&files)?)?;
    let t_inflate = t_wall.elapsed();
    stats_rss("after parse (raw images resident)");

    // Lazy registration + parallel materialization: identical semantics
    // to the eager build (the outer map consults annotations only after
    // everything is materialized), but the 145k annotation reads run on
    // worker threads instead of the serial pool build.
    let mut pool = DexPool::new();
    for (label, dex) in parsed {
        let idx = pool.add_dex_lazy(dex);
        pool.set_dex_label(idx, label.clone());
    }
    pool.materialize_all(workers);
    stats_rss("after materialize_all (pool built)");
    let dex_count = pool.dex_count();
    if std::env::var("DDC_WALL").is_ok() {
        eprintln!(
            "[wall] read={:?} zip-scan={:?} parse+pool={:?} total={:?}",
            t_read,
            t_inflate - t_read,
            t_wall.elapsed() - t_inflate,
            t_wall.elapsed()
        );
    }

    if verbose {
        eprintln!("[i] {} dex file(s), {} classes", dex_count, pool.len());
    }

    if list {
        for name in pool.class_names() {
            println!("{}", name.replace('/', "."));
        }
        eprintln!(
            "ddc: listed {} classes in {}",
            pool.len(),
            fmt_secs(t_start.elapsed())
        );
        return Ok(());
    }

    let pool = std::sync::Arc::new(pool);
    if std::env::var("DDC_PHASES").is_ok() {
        ddc_dec::method::phases_enable();
        ddc_dec::method::dom_counters_enable();
    }
    let opts = ClassOptions {
        provenance: comments,
    };
    let targets: Vec<String> = match &only {
        Some(one) => {
            let internal = one.replace('.', "/");
            if pool.get(&internal).is_none() {
                bail!(
                    "{}",
                    bif!("class not found: {0} (try --list)", "找不到类 {0}（可用 --list 列出）"; one)
                );
            }
            vec![internal]
        }
        None => top_level_classes(&pool),
    };

    // Case-collision renames: classes whose internal names differ only
    // in case cannot share one case-insensitive directory — install the
    // deterministic suffix map BEFORE any worker or writer spawns (the
    // registry is read-only afterwards).
    ddc_dec::install_case_renames(&pool);
    stats_rss("after rename registries");

    // ---- resolve the output sink ----
    let sink = resolve_sink(&inputs, out.as_deref(), &targets)?;
    if let Sink::Dir(d) = &sink {
        std::fs::create_dir_all(d)?;
        // Case-collision census: dex legally carries packages/classes that
        // differ only by case (autoclaw ships BOTH `Z1/` and `z1/`), but a
        // case-insensitive filesystem (macOS/Windows default) folds them
        // into ONE directory — same-named files silently overwrite each
        // other, losing classes and poisoning any javac gate run on the
        // tree (autoclaw: 706 phantom errors, 无法访问t ×103 from a merged
        // Z1/t.java ↔ z1/t.java). Detect and warn; the tree is only
        // faithful on a case-sensitive volume. O(N) once per run, off
        // the writer hot path.
        let mut first_by_lower: std::collections::HashMap<String, String> =
            std::collections::HashMap::with_capacity(targets.len());
        let mut collisions: Vec<(String, String)> = Vec::new();
        for name in &targets {
            let p = source_path(d, name);
            let ps = p.to_string_lossy().into_owned();
            let lower = ps.to_lowercase();
            match first_by_lower.get(&lower) {
                Some(prev) if *prev != ps => collisions.push((prev.clone(), ps)),
                None => {
                    let _ = first_by_lower.insert(lower, ps);
                }
                _ => {}
            }
        }
        if !collisions.is_empty() {
            collisions.sort();
            let shown: Vec<String> = collisions
                .iter()
                .take(5)
                .map(|(a, b)| format!("{} <-> {}", a, b))
                .collect();
            eprintln!(
                "[!] WARNING: {} output path pair(s) differ only by CASE (e.g. {}) — \
                 on a case-insensitive filesystem they overwrite each other and the \
                 tree loses classes; decompile onto a case-sensitive volume for a \
                 faithful tree.",
                collisions.len(),
                shown.join(", ")
            );
        }
    }
    // Pre-create the package directories on a background thread while the
    // parse runs: writers then hit zero mkdir syscalls in the common case
    // (mkdir was ~8% of writer time — they are the throughput ceiling).
    // Lexicographic order puts every parent before its children, so each
    // create_dir_all resolves in one mkdir (or a cheap EEXIST). The writer
    // keeps a NotFound fallback for the race where it outruns this thread.
    let dir_maker: Option<std::thread::JoinHandle<()>> = match &sink {
        Sink::Dir(d) if targets.len() > 64 && std::env::var("DDC_NOWRITE").is_err() => {
            let d = d.clone();
            let names = targets.clone();
            Some(std::thread::spawn(move || {
                let mut dirs: std::collections::HashSet<std::path::PathBuf> =
                    std::collections::HashSet::new();
                for name in &names {
                    if let Some(p) = source_path(&d, name).parent() {
                        dirs.insert(p.to_path_buf());
                    }
                }
                let mut sorted: Vec<_> = dirs.into_iter().collect();
                sorted.sort();
                for p in sorted {
                    let _ = std::fs::create_dir_all(p);
                }
            }))
        }
        _ => None,
    };
    // Stdout must be deterministic: pool order, one worker.
    let stdout_mode = matches!(sink, Sink::Stdout);
    if stdout_mode && workers > 1 && targets.len() > 1 {
        workers = 1;
    }

    // Detached monitored threads for pathological classes:
    // (receiver, class name, deadline Instant). Deadlines arm at SPAWN
    // time, so draining at the end costs at most one deadline total.
    let pending: std::sync::Mutex<Vec<ddc_dec::classdec::PendingMonitor>> =
        std::sync::Mutex::new(Vec::new());
    let pending_ref = &pending;

    let failed = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    // Files actually created+written by the writer pool. The summary used
    // to report `total - failed`, which counts CLASSES ATTEMPTED — on
    // bin.mt.plus it printed "wrote 22634 file(s)" over 3 physical files,
    // because the writer's EEXIST path silently overwrote. Count the
    // syscalls that succeeded instead, so the number is auditable.
    let written = std::sync::Arc::new(AtomicUsize::new(0));
    // Writer EEXIST hits (the silent-overwrite branch). Zero on a healthy
    /// run; reported under DDC_STATS.
    static OVERWRITES: AtomicUsize = AtomicUsize::new(0);
    // Writer syscalls that failed outright (ENAMETOOLONG, ENOSPC,
    // EACCES…). The old writer swallowed these with `let _ =` — a class
    // whose name overflows NAME_MAX simply never appeared, at exit 0.
    let write_errors = std::sync::Arc::new(AtomicUsize::new(0));
    // Image retirement (full mode only): workers report each finished
    // class; a dex whose counter hits zero releases its inflated bytes —
    // on lark that is ~360MB reclaimed progressively instead of resident
    // for the whole run.
    let retire_mode = std::env::var("DDC_NORETIRE").is_err() && !matches!(sink, Sink::Stdout);
    if retire_mode {
        // Accessor bodies must be snapshotted BEFORE images can retire:
        // inline_accessors reads them cross-image during worker runs, and
        // a released image silently skips the inline (run-to-run output
        // nondeterminism: var-id flips in enum constant args).
        pool.snapshot_accessor_code();
        pool.arm_retirement();
    }
    let total = targets.len();
    let pool_ref = &pool;
    let opts_ref = &opts;
    let sink_ref = &sink;
    let failed_ref = &failed;
    let done_ref = &done;
    let nowrite = std::env::var("DDC_NOWRITE").is_ok();
    let class_time = std::env::var("DDC_CLASSTIME").is_ok();

    // ---- path-collision assertion -------------------------------------
    // Two distinct classes mapping to one output path is not a cosmetic
    // problem: the writer opens with `create_new`, takes EEXIST, unlinks
    // and rewrites, so the last writer wins and every earlier class is
    // destroyed — with exit code 0 and a summary that still counts them
    // all. That is exactly how 22,633 of bin.mt.plus's 30,768 classes
    // vanished (see `lossy_sanitize_renames`). The renamer is supposed
    // to make the mapping injective; assert it here rather than trusting
    // it, and fail loudly if any family slips through.
    if let Sink::Dir(d) = &sink {
        if !nowrite {
            let mut first: std::collections::HashMap<PathBuf, &str> =
                std::collections::HashMap::new();
            let mut clashes: Vec<(PathBuf, &str, &str)> = Vec::new();
            for name in &targets {
                let p = source_path(d, name);
                match first.entry(p) {
                    std::collections::hash_map::Entry::Occupied(e) => {
                        clashes.push((e.key().clone(), *e.get(), name.as_str()));
                    }
                    std::collections::hash_map::Entry::Vacant(v) => {
                        v.insert(name.as_str());
                    }
                }
            }
            if !clashes.is_empty() {
                for (p, a, b) in clashes.iter().take(5) {
                    eprintln!("[!]   {} <- {}, {}", p.display(), a, b);
                }
                if std::env::var("DDC_ALLOW_PATH_COLLISION").is_ok() {
                    eprintln!(
                        "{}",
                        bif!(
                            "[!] {0} class(es) share an output path; the last writer wins and the rest are LOST (DDC_ALLOW_PATH_COLLISION set, continuing)",
                            "[!] {0} 个类共用同一输出路径；最后一个写入者胜出，其余将丢失（已设 DDC_ALLOW_PATH_COLLISION，继续执行）";
                            clashes.len()
                        )
                    );
                } else {
                    bail!(
                        "{}",
                        bif!(
                            "{0} class(es) map to an output path another class already owns (first: {1}) — writing would silently destroy them. This is a sanitizer/rename bug; re-run with DDC_ALLOW_PATH_COLLISION=1 to override.",
                            "{0} 个类与其它类落盘到同一路径（首个：{1}）——继续写入会静默销毁它们。这是净化/改名规则的 bug；可用 DDC_ALLOW_PATH_COLLISION=1 强制继续。";
                            clashes.len(),
                            clashes[0].0.display()
                        )
                    );
                }
            }
        }
    }

    // Writer pool: decompile workers hand finished sources to dedicated
    // writer threads through a bounded MPMC queue (std mpsc has no
    // multi-consumer). Measured on weibo: inline fs::write stalled
    // workers ~64s of wall (APFS metadata + page-cache flushing under 12
    // concurrent writers) while user CPU was only ~146s — workers sat
    // blocked-in-kernel at "100% busy". Writers keep per-thread mkdir
    // caches (create_dir_all is idempotent, no shared lock).
    //
    // Items are BATCHES (one per worker chunk): 18 workers × 4 writers
    // meeting on one mutex per FILE made the queue lock a top-3 profile
    // entry (push-side condvar waits dominated the lark sample). One
    // lock acquisition per ~32 files removes it; the bound counts
    // FILES (not batches), so the memory ceiling is unchanged.
    // (pending batches, total pending FILES — the bound counts files so
    // batching cannot inflate the memory ceiling).
    type WqState = (
        std::collections::VecDeque<Vec<(std::path::PathBuf, String)>>,
        usize,
    );
    struct WriteQueue {
        q: std::sync::Mutex<WqState>,
        not_empty: std::sync::Condvar,
        not_full: std::sync::Condvar,
        cap: usize,
    }
    impl WriteQueue {
        fn push_batch(&self, batch: Vec<(std::path::PathBuf, String)>) {
            if batch.is_empty() {
                return;
            }
            let mut q = self.q.lock().unwrap();
            // `q.1 > 0` guard: an oversized batch must not deadlock on an
            // otherwise-empty queue.
            while q.1 > 0 && q.1 + batch.len() > self.cap {
                q = self.not_full.wait(q).unwrap();
            }
            q.1 += batch.len();
            q.0.push_back(batch);
            drop(q);
            self.not_empty.notify_one();
        }
        fn close(&self) {
            let mut q = self.q.lock().unwrap();
            // Shutdown marker: an EMPTY batch writers exit on.
            q.0.push_back(Vec::new());
            drop(q);
            self.not_empty.notify_all();
        }
        fn pop(&self) -> Option<Vec<(std::path::PathBuf, String)>> {
            let mut q = self.q.lock().unwrap();
            loop {
                if let Some(item) = q.0.pop_front() {
                    if item.is_empty() {
                        // Shutdown marker consumed: restore it for the
                        // other writers, then exit.
                        q.0.push_back(item);
                        drop(q);
                        self.not_empty.notify_one();
                        return None;
                    }
                    q.1 -= item.len();
                    drop(q);
                    self.not_full.notify_one();
                    return Some(item);
                }
                q = self.not_empty.wait(q).unwrap();
            }
        }
    }
    let wq = std::sync::Arc::new(WriteQueue {
        q: std::sync::Mutex::new((std::collections::VecDeque::new(), 0)),
        not_empty: std::sync::Condvar::new(),
        not_full: std::sync::Condvar::new(),
        // Bounded at 1024 pending sources: enough runway that writers
        // never starve the workers (APFS metadata is the writer cost, not
        // throughput), while capping the transient text buffer memory.
        cap: 1024,
    });
    // Writer threads: bounded pool draining the write queue. DDC_WRITERS
    // overrides for tuning (APFS metadata throughput varies by volume).
    let n_writers = std::env::var("DDC_WRITERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or_else(|| (workers / 4).clamp(2, 4))
        .clamp(1, 32);
    let uses_writers = matches!(sink, Sink::Dir(_)) && !nowrite;
    // Reserve cores for the writer pool when the user did not pin -t:
    // oversubscribing (18 workers + 4 writers on 18 cores) slowed BOTH
    // sides — writers are syscall-bound and need CPU slots to run their
    // syscalls (lark: 5.3s -> 4.8s at 14 workers + 4 writers).
    if uses_writers && !stdout_mode && !threads_explicit && workers > n_writers + 1 {
        workers -= n_writers;
    }
    let writer_handles: Vec<std::thread::JoinHandle<()>> = if uses_writers {
        (0..n_writers)
            .map(|_| {
                let wq = wq.clone();
                let written = written.clone();
                let write_errors = write_errors.clone();
                std::thread::spawn(move || {
                    while let Some(batch) = wq.pop() {
                        for (path, text) in batch {
                            // create_new: one open syscall mints a fresh
                            // inode — no blanket unlink (the old writer
                            // paid remove+open+write+close per file).
                            // The EEXIST fallback covers BOTH a pre-
                            // existing file from an earlier run and a
                            // case-VARIANT pair (X/CUA vs X/Cua) on
                            // case-insensitive filesystems: remove drops
                            // the old name so the on-disk NAME matches
                            // the LAST writer's declared class. The
                            // remove orphans any concurrently open fd
                            // (its bytes die with the inode), so no
                            // cross-writer shard lock is needed — the
                            // final file is always exactly one writer's
                            // complete content under one consistent name.
                            //
                            // NB: the EEXIST branch OVERWRITES. It must
                            // stay unreachable for two distinct classes —
                            // the driver asserts path injectivity before
                            // any worker starts, and DDC_STATS surfaces
                            // the count if that assertion is ever relaxed.
                            use std::io::Write;
                            match std::fs::OpenOptions::new()
                                .write(true)
                                .create_new(true)
                                .open(&path)
                            {
                                Ok(mut f) => {
                                    match f.write_all(text.as_bytes()) {
                                        Ok(()) => {
                                            written.fetch_add(1, Ordering::Relaxed);
                                        }
                                        Err(e) => {
                                            write_errors.fetch_add(1, Ordering::Relaxed);
                                            eprintln!(
                                                "[!] write {}: {}",
                                                path.display(),
                                                e
                                            );
                                        }
                                    }
                                }
                                Err(e) => {
                                    // NotFound: the dir pre-creator has not
                                    // reached this package yet (or the output
                                    // dir is foreign) — create it and retry.
                                    if e.kind() == std::io::ErrorKind::NotFound {
                                        if let Some(parent) = path.parent() {
                                            let _ = std::fs::create_dir_all(parent);
                                        }
                                    }
                                    if std::env::var("DDC_STATS").is_ok() {
                                        OVERWRITES.fetch_add(1, Ordering::Relaxed);
                                    }
                                    let _ = std::fs::remove_file(&path);
                                    match std::fs::write(&path, text.as_bytes()) {
                                        Ok(()) => {
                                            written.fetch_add(1, Ordering::Relaxed);
                                        }
                                        Err(e) => {
                                            write_errors.fetch_add(1, Ordering::Relaxed);
                                            eprintln!(
                                                "[!] write {}: {}",
                                                path.display(),
                                                e
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                })
            })
            .collect()
    } else {
        Vec::new()
    };

    // Dynamic work queue: small slices pulled via an atomic cursor —
    // slow (efficiency) cores naturally take less, so static-chunk
    // stragglers disappear.
    const CHUNK: usize = 32;
    static BUSY_MICROS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static WORKER_MICROS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let queue: Vec<Vec<String>> = if stdout_mode || targets.len() <= 1 {
        // One ordered chunk: stdout emits in pool order (single worker
        // forced above); a lone class has nothing to split.
        vec![targets.clone()]
    } else {
        targets.chunks(CHUNK).map(|c| c.to_vec()).collect()
    };
    let cursor = AtomicUsize::new(0);
    let queue_ref = &queue;
    let cursor_ref = &cursor;
    std::thread::scope(|scope| {
        let wq_ref = &wq;
        let mut handles = Vec::new();
        for _ in 0..workers.min(queue.len()).max(1) {
            handles.push(
                std::thread::Builder::new()
                    .stack_size(64 * 1024 * 1024)
                    .spawn_scoped(scope, move || -> usize {
                        let mut local_failed = 0usize;
                        let worker_start = std::time::Instant::now();
                        let mut busy = std::time::Duration::ZERO;
                        let mut iter_start = std::time::Instant::now();
                        let trace_class = std::env::var_os("DDC_TRACE_CLASS").is_some();
                        // Per-worker write batches: flushed at chunk ends
                        // (32 classes) so the shared queue sees ~1/32 of
                        // the lock acquisitions.
                        let mut batch: Vec<(std::path::PathBuf, String)> = Vec::new();
                        loop {
                            let qi = cursor_ref.fetch_add(1, Ordering::Relaxed);
                            let Some(chunk) = queue_ref.get(qi) else {
                                break;
                            };
                            // (includes the fetch itself — negligible next to a
                            // chunk of 32 classes)
                            busy += iter_start.elapsed();
                            iter_start = std::time::Instant::now();
                            for name in chunk {
                                if trace_class {
                                    eprintln!("[trace-class] {name}");
                                }
                                let ct0 = std::time::Instant::now();
                                let Some(pc) = pool_ref.get(name) else {
                                    continue;
                                };
                                // A panic in one class must not take down the worker
                                // (the whole chunk would be lost); pathological CFGs run
                                // on a detached monitored thread (decompile_class
                                // decides) whose handle is awaited with its deadline at
                                // the END — the worker never blocks on it.
                                let out =
                                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                        ddc_dec::classdec::decompile_class(
                                            pool_ref,
                                            pc,
                                            opts_ref,
                                            pending_ref,
                                        )
                                    }));
                                let finished = match &out {
                                    Ok(Ok(_)) | Ok(Err(_)) | Err(_) => true,
                                };
                                if retire_mode {
                                    if let Some(image) = pool_ref.report_class_done(name) {
                                        // Last class of this image: release it right
                                        // here (mark_released is &self-safe; bytes drop
                                        // when the final snapshot drops).
                                        pool_ref.release_images(&[image]);
                                    }
                                }
                                let _ = finished;
                                match out {
                                    Ok(Ok(text)) => {
                                        match sink_ref {
                                            Sink::Stdout => {
                                                if total > 1 {
                                                    println!(
                                                        "\n// ===== {} =====",
                                                        name.replace('/', ".")
                                                    );
                                                }
                                                println!("{}", text);
                                            }
                                            Sink::File(f) => {
                                                if !nowrite {
                                                    if let Some(parent) = f.parent() {
                                                        let _ = std::fs::create_dir_all(parent);
                                                    }
                                                    let _ = std::fs::write(f, text);
                                                }
                                            }
                                            Sink::Dir(d) => {
                                                if !nowrite {
                                                    // Batched handoff to the writer
                                                    // pool; the bounded queue blocks
                                                    // only when writers fall behind
                                                    // (backpressure, not a stall).
                                                    batch.push((source_path(d, name), text));
                                                    if batch.len() >= 32 {
                                                        wq_ref.push_batch(std::mem::take(
                                                            &mut batch,
                                                        ));
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    Ok(Err(e)) => {
                                        if format!("{:#}", e)
                                            .contains("deferred to monitored thread")
                                        {
                                            // Result arrives via the pending registry
                                            // (awaited after the chunk phase).
                                            continue;
                                        }
                                        local_failed += 1;
                                        eprintln!("[!] {}: {:#}", name.replace('/', "."), e);
                                    }
                                    Err(e) => {
                                        local_failed += 1;
                                        let msg = e
                                            .downcast_ref::<String>()
                                            .cloned()
                                            .or_else(|| {
                                                e.downcast_ref::<&str>().map(|m| m.to_string())
                                            })
                                            .unwrap_or_else(|| "panic".into());
                                        eprintln!("[!] {}: {}", name.replace('/', "."), msg);
                                    }
                                }
                                let n = done_ref.fetch_add(1, Ordering::Relaxed) + 1;
                                if class_time && ct0.elapsed().as_millis() >= 50 {
                                    eprintln!(
                                        "[ctime] {:>6}ms {}",
                                        ct0.elapsed().as_millis(),
                                        name
                                    );
                                }
                                if verbose && n.is_multiple_of(500) {
                                    eprintln!("[i] {}/{} classes", n, total);
                                }
                            }
                            // Chunk end: flush the partial batch so the
                            // writers see every finished source promptly.
                            wq_ref.push_batch(std::mem::take(&mut batch));
                        }
                        WORKER_MICROS.fetch_add(
                            worker_start.elapsed().as_micros() as u64,
                            Ordering::Relaxed,
                        );
                        BUSY_MICROS.fetch_add(busy.as_micros() as u64, Ordering::Relaxed);
                        local_failed
                    }),
            );
        }
        for h in handles.into_iter().flatten() {
            failed_ref.fetch_add(h.join().unwrap_or(0), Ordering::Relaxed);
        }
    });
    let t_work = t_wall.elapsed();

    // Drain the detached pathological-class threads: deadlines armed at
    // spawn, so the total tail is bounded by one deadline.
    let mut pending: Vec<_> = pending.into_inner().unwrap();
    if std::env::var("DDC_WALL").is_ok() {
        eprintln!("[wall] deferred-to-monitored: {} classes", pending.len());
    }
    pending.sort_by_key(|(_, _, dl)| *dl);
    for (rx, name, deadline) in pending {
        let now = std::time::Instant::now();
        let remaining = if deadline > now {
            deadline - now
        } else {
            std::time::Duration::ZERO
        };
        match rx.recv_timeout(remaining) {
            Ok(Ok(text)) => match &sink {
                Sink::Stdout => {
                    if total > 1 {
                        println!("\n// ===== {} =====", name.replace('/', "."));
                    }
                    println!("{}", text);
                }
                Sink::File(f) => {
                    if !nowrite {
                        if let Some(parent) = f.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let _ = std::fs::write(f, text);
                    }
                }
                Sink::Dir(d) => {
                    if !nowrite {
                        wq.push_batch(vec![(source_path(d, &name), text)]);
                    }
                }
            },
            Ok(Err(e)) => {
                failed.fetch_add(1, Ordering::Relaxed);
                eprintln!(
                    "{}",
                    bif!("[!] {0}: {1}", "[!] {0}：{1}"; name.replace('/', "."), e)
                );
            }
            Err(_) => {
                failed.fetch_add(1, Ordering::Relaxed);
                eprintln!(
                    "{}",
                    bif!(
                        "[!] {0}: decompile timed out (pathological CFG)",
                        "[!] {0}：反编译超时（病态 CFG）";
                        name.replace('/', ".")
                    )
                );
            }
        }
    }

    // All producers done: release the writers and join them (flush the
    // remaining queue before exiting).
    wq.close();
    if let Some(h) = dir_maker {
        let _ = h.join();
    }
    for h in writer_handles {
        let _ = h.join();
    }
    let t_flush = t_wall.elapsed();

    if std::env::var("DDC_WALL").is_ok() {
        let busy = BUSY_MICROS.load(Ordering::Relaxed) as f64 / 1e6;
        let worker = WORKER_MICROS.load(Ordering::Relaxed) as f64 / 1e6;
        eprintln!(
            "[wall] work={:?} drain={:?} flush={:?} busy={:.1}s worker={:.1}s util={:.0}%",
            t_work - t_read,
            t_flush - t_work,
            t_wall.elapsed() - t_flush,
            busy,
            worker,
            100.0 * busy / worker.max(0.001)
        );
        {
            let (dc, db) = ddc_dec::method::dom_counters();
            eprintln!(
                "[dom] {} compute_dominators calls, {} total blocks scanned",
                dc, db
            );
        }
    }

    if std::env::var("DDC_PHASES").is_ok() {
        let ph = ddc_dec::method::phases_dump();
        let labels = ["fixpoint", "structure+convert", "passes", "print"];
        for (i, l) in labels.iter().enumerate() {
            eprintln!("[phase] {:>18}: {:8.1}s", l, ph[i] as f64 / 1e6);
        }
        let pb = ddc_dec::method::phases_bucket_dump();
        let blabels = ["<=1 blk", "2-5 blk", "6-20 blk", "21-100 blk", ">100 blk"];
        eprintln!("[phase x bucket] (cumulative from fixpoint start)");
        for (i, l) in labels.iter().enumerate().take(3) {
            let cells: Vec<String> = (0..5)
                .map(|b| format!("{:6.1}s", pb[i][b] as f64 / 1e6))
                .collect();
            eprintln!("  {:>16}: {}", l, cells.join(" "));
        }
        eprintln!("  buckets           : {}", blabels.join("  "));
    }
    if std::env::var("DDC_BUCKETS").is_ok() {
        let (b, i) = ddc_dec::method::dump_builds();
        eprintln!("[builds] {} block-lifts, {} insns lifted", b, i);
        let (ch, ci) = ddc_dec::method::dump_caps();
        eprintln!(
            "[caps] {} methods hit visit cap, {} (insns*1000+blocks sum)",
            ch, ci
        );
        let b = ddc_dec::method::dump_buckets();
        let labels = ["<=1 blk", "2-5 blk", "6-20 blk", "21-100 blk", ">100 blk"];
        eprintln!("[buckets]");
        for (i, (n, us)) in b.iter().enumerate() {
            if *n > 0 {
                eprintln!(
                    "  {:>10}: {:6} methods {:8.1}s total {:7.1}us avg",
                    labels[i],
                    n,
                    *us as f64 / 1e6,
                    *us as f64 / *n as f64
                );
            }
        }
    }
    // Summary + exit status (0 all ok, 1 some classes failed).
    let failed_n = failed.load(Ordering::Relaxed);
    let elapsed = fmt_secs(t_start.elapsed());
    let failed_part = if failed_n > 0 {
        bif!(", {0} failed", "（{0} 个失败）"; failed_n)
    } else {
        String::new()
    };
    // Dir sink reports FILES ON DISK (writer syscalls that succeeded), not
    // classes attempted: on a lossy path mapping the two differ by the
    // entire overwritten set (bin.mt.plus: 22634 claimed, 3 written). If
    // they ever diverge again, say so instead of printing a number that
    // cannot be checked.
    let dir_written = if nowrite {
        total - failed_n
    } else {
        written.load(Ordering::Relaxed)
    };
    let lost_part = if !nowrite && dir_written != total - failed_n {
        bif!(
            ", {0} of {1} classes did not reach disk",
            "（{0}/{1} 个类未落盘）";
            (total - failed_n).saturating_sub(dir_written),
            total - failed_n
        )
    } else {
        String::new()
    };
    let werr = write_errors.load(Ordering::Relaxed);
    if std::env::var("DDC_STATS").is_ok() {
        let ow = OVERWRITES.load(Ordering::Relaxed);
        if ow > 0 {
            eprintln!("[writer] {ow} EEXIST overwrite(s) — output paths were NOT injective");
        }
    }
    let write_err_part = if werr > 0 {
        bif!(", {0} write error(s)", "（{0} 个写入错误）"; werr)
    } else {
        String::new()
    };
    match &sink {
        Sink::Dir(d) => eprintln!(
            "{}",
            bif!(
                "ddc: wrote {0} file(s) to {1}{2}{3}{4} in {5}",
                "ddc：已写出 {0} 个文件到 {1}{2}{3}{4}，用时 {5}";
                dir_written,
                d.display(),
                failed_part,
                lost_part,
                write_err_part,
                elapsed
            )
        ),
        Sink::File(f) => eprintln!(
            "{}",
            bif!(
                "ddc: wrote {0} file to {1}{2} in {3}",
                "ddc：已写出 {0} 个文件到 {1}{2}，用时 {3}";
                total - failed_n,
                f.display(),
                failed_part,
                elapsed
            )
        ),
        Sink::Stdout => {
            // stdout results carry no trailing summary/timing (failure
            // notices above are the only stderr noise).
        }
    }
    if ddc_dec::ssa_census::enabled() {
        eprintln!("{}", ddc_dec::ssa_census::report());
    }
    if std::env::var("DDC_COLLECT").is_ok() {
        unsafe { mimalloc_sys_collect(true) };
    }
    if failed_n > 0 || werr > 0 {
        // A write error is a lost class: the old code ignored it (exit 0)
        // and the summary still counted the class as written.
        std::process::exit(1);
    }
    Ok(())
}

/// `0.004s` under a second, `0.24s` under ten, `5.6s` above (precision
/// where it reads).
fn fmt_secs(d: std::time::Duration) -> String {
    let s = d.as_secs_f64();
    if s < 1.0 {
        format!("{s:.3}s")
    } else if s < 10.0 {
        format!("{s:.2}s")
    } else {
        format!("{s:.1}s")
    }
}

/// Decide where the decompiled source goes. `-o`/positional-output
/// semantics: `-` forces stdout; a `.java` suffix (or an existing
/// non-directory) is a single output file (single emitted class only —
/// with `-c` or a one-class pool); anything else is an output root
/// directory. With no output given, every input writes to a sibling
/// `<stem>-out/` directory (single .dex inputs included: a dex always
/// carries many classes, unlike jcdc's single .class).
fn resolve_sink(inputs: &[PathBuf], out: Option<&str>, targets: &[String]) -> Result<Sink> {
    let single_class = targets.len() == 1;
    match out {
        Some("-") => Ok(Sink::Stdout),
        Some(o) => {
            let p = PathBuf::from(o);
            let is_file = p.extension().and_then(|e| e.to_str()) == Some("java")
                || (p.exists() && p.is_file());
            if is_file {
                if !single_class {
                    bail!(
                        "{}",
                        bif!(
                            "-o {0} names a file but {1} class(es) would be written \
                             (use a directory, `-c`, or `-`)",
                            "-o {0} 指向文件，但要写出 {1} 个类（请用目录、`-c` 或 `-`）";
                            o,
                            targets.len()
                        )
                    );
                }
                return Ok(Sink::File(p));
            }
            Ok(Sink::Dir(p))
        }
        None => {
            // A single emitted class (-c, or a one-class dex) prints to
            // stdout like jcdc's single .class; everything else needs a
            // directory.
            if single_class {
                return Ok(Sink::Stdout);
            }
            let input = inputs.first().context(bi!(
                "no input to derive an output path from",
                "没有输入，无法推导输出路径"
            ))?;
            let stem = input
                .file_stem()
                .or_else(|| input.file_name())
                .and_then(|s| s.to_str())
                .context(bi!("input has no usable name", "输入没有可用文件名"))?;
            let dir = input
                .parent()
                .unwrap_or(Path::new("."))
                .join(format!("{}-out", stem));
            eprintln!(
                "ddc: no output given; writing to {} (pass -o or a positional OUTPUT to override, -o - for stdout)",
                dir.display()
            );
            Ok(Sink::Dir(dir))
        }
    }
}

/// `com/foo/Bar$Inner` → `<out>/com/foo/Bar$Inner.java`.
///
/// Both the segment mapping and the declared name come from ddc-dec, so a
/// file can never disagree with the class inside it. (This used to be a
/// second, drifted copy of the sanitizer that lacked the lone-`_` escape:
/// `l.֡` wrote `l/_.java` containing `class __`.)
fn source_path(out: &Path, internal: &str) -> PathBuf {
    // Case/package/lossy-sanitize renames (identity when none installed):
    // the FILE name must match the DECLARED class name.
    let cow = ddc_dec::apply_class_rename(internal);
    let mut p = out.to_path_buf();
    for seg in cow.split('/') {
        p.push(ddc_dec::sanitize_seg(seg));
    }
    p.set_extension("java");
    p
}

// ---------------------------------------------------------------------------
// Minimal ZIP reader (stored + deflate), enough for APK/JAR class extraction.
// ---------------------------------------------------------------------------

pub(crate) enum ZipMethod {
    Stored,
    Deflate,
}

/// One zip entry as a (name, compressed byte range, method) — zero-copy
/// slices of the archive image; the caller inflates on demand.
pub(crate) struct ZipEntry {
    pub name: String,
    pub range: std::ops::Range<usize>,
    pub method: ZipMethod,
    /// Uncompressed size from the central directory (0 = unknown /
    /// zip64 placeholder). Inflating with `reserve_exact(hint)` skips
    /// the doubling reallocs — on QQ's 41 dexes the growth copies were
    /// part of the memmove leaf (the #1 sampled frame) and the
    /// transient peak carried 2× the final image per realloc step.
    pub usize_hint: usize,
}

pub(crate) fn zip_entries(data: &[u8]) -> Result<Vec<ZipEntry>> {
    // Find EOCD (22 bytes + optional comment).
    let eocd = find_eocd(data).context("zip: EOCD not found")?;
    let cd_size = u32le(data, eocd + 12) as usize;
    let cd_off = u32le(data, eocd + 16) as usize;
    // The EOCD entry count is a u16 (bytes 10..12). Reading it as u32 also
    // swallowed the low half of cd_size, so `with_capacity` below asked for
    // ~2^32 entries worth of memory (202 GB on an archive whose cd_size low
    // half was 0xFBCA) and the process aborted before doing any work.
    let n = u16le(data, eocd + 10) as usize;
    let mut out = Vec::with_capacity(n);
    let mut p = cd_off;
    for _ in 0..n {
        if p + 46 > data.len() || u32le(data, p) != 0x0201_4b50 {
            break;
        }
        let method = u16le(data, p + 10);
        let csize = u32le(data, p + 20) as usize;
        let usize_raw = u32le(data, p + 24);
        // 0xFFFFFFFF = zip64 placeholder — unknown, inflate grows.
        let usize_hint = if usize_raw == 0xFFFF_FFFF { 0 } else { usize_raw as usize };
        let name_len = u16le(data, p + 28) as usize;
        let extra_len = u16le(data, p + 30) as usize;
        let comm_len = u16le(data, p + 32) as usize;
        let lho = u32le(data, p + 42) as usize;
        let name = String::from_utf8_lossy(&data[p + 46..p + 46 + name_len]).into_owned();
        // Local header: fixed 30 bytes + name + extra (name length from the
        // local header may differ from the central one).
        if lho + 30 <= data.len() {
            let l_name_len = u16le(data, lho + 26) as usize;
            let l_extra_len = u16le(data, lho + 28) as usize;
            let start = lho + 30 + l_name_len + l_extra_len;
            let end = (start + csize).min(data.len());
            let m = match method {
                0 => ZipMethod::Stored,
                8 => ZipMethod::Deflate,
                _ => {
                    p += 46 + name_len + extra_len + comm_len;
                    continue;
                }
            };
            out.push(ZipEntry {
                name,
                range: start..end,
                method: m,
                usize_hint,
            });
        }
        p += 46 + name_len + extra_len + comm_len;
    }
    let _ = cd_size;
    Ok(out)
}

fn find_eocd(data: &[u8]) -> Option<usize> {
    if data.len() < 22 {
        return None;
    }
    let start = data.len().saturating_sub(22 + 0xffff);
    let mut i = data.len() - 22;
    loop {
        if u32le(data, i) == 0x0605_4b50 {
            return Some(i);
        }
        if i == start {
            return None;
        }
        i -= 1;
    }
}

fn u16le(data: &[u8], off: usize) -> u16 {
    if off + 2 > data.len() {
        return 0;
    }
    u16::from_le_bytes([data[off], data[off + 1]])
}

fn u32le(data: &[u8], off: usize) -> u32 {
    if off + 4 > data.len() {
        return 0;
    }
    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}

pub(crate) fn inflate(data: &[u8]) -> Result<Vec<u8>> {
    inflate_hint(data, 0)
}

/// `inflate` with the central directory's uncompressed size: one exact
/// allocation instead of the doubling realloc chain.
pub(crate) fn inflate_hint(data: &[u8], usize_hint: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    if usize_hint > 0 {
        out.reserve_exact(usize_hint);
    }
    let mut dec = flate2::read::DeflateDecoder::new(data);
    dec.read_to_end(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u16b(v: u16) -> [u8; 2] {
        v.to_le_bytes()
    }

    fn u32b(v: u32) -> [u8; 4] {
        v.to_le_bytes()
    }

    /// Hand-built STORED-only zip (no compressor dependency). Each entry's
    /// payload is its own name, which is all these tests need.
    fn stored_zip(names: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cd = Vec::new();
        for name in names {
            let off = out.len() as u32;
            let body = name.as_bytes();
            out.extend_from_slice(&u32b(0x0403_4b50)); // local file header
            out.extend_from_slice(&u16b(20)); // version needed
            out.extend_from_slice(&u16b(0)); // flags
            out.extend_from_slice(&u16b(0)); // method: stored
            out.extend_from_slice(&u32b(0)); // mtime
            out.extend_from_slice(&u32b(0)); // crc32
            out.extend_from_slice(&u32b(body.len() as u32)); // csize
            out.extend_from_slice(&u32b(body.len() as u32)); // usize
            out.extend_from_slice(&u16b(name.len() as u16));
            out.extend_from_slice(&u16b(0)); // extra len
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(body);

            cd.extend_from_slice(&u32b(0x0201_4b50)); // central directory
            cd.extend_from_slice(&u16b(20)); // version made by
            cd.extend_from_slice(&u16b(20)); // version needed
            cd.extend_from_slice(&u16b(0)); // flags
            cd.extend_from_slice(&u16b(0)); // method: stored
            cd.extend_from_slice(&u32b(0)); // mtime
            cd.extend_from_slice(&u32b(0)); // crc32
            cd.extend_from_slice(&u32b(body.len() as u32)); // csize
            cd.extend_from_slice(&u32b(body.len() as u32)); // usize
            cd.extend_from_slice(&u16b(name.len() as u16));
            cd.extend_from_slice(&u16b(0)); // extra len
            cd.extend_from_slice(&u16b(0)); // comment len
            cd.extend_from_slice(&u16b(0)); // disk number
            cd.extend_from_slice(&u16b(0)); // internal attrs
            cd.extend_from_slice(&u32b(0)); // external attrs
            cd.extend_from_slice(&u32b(off)); // local header offset
            cd.extend_from_slice(name.as_bytes());
        }
        let cd_off = out.len() as u32;
        let cd_size = cd.len() as u32;
        out.extend_from_slice(&cd);
        out.extend_from_slice(&u32b(0x0605_4b50)); // EOCD
        out.extend_from_slice(&u16b(0)); // this disk
        out.extend_from_slice(&u16b(0)); // disk with cd start
        out.extend_from_slice(&u16b(names.len() as u16)); // entries on disk
        out.extend_from_slice(&u16b(names.len() as u16)); // total entries
        out.extend_from_slice(&u32b(cd_size));
        out.extend_from_slice(&u32b(cd_off));
        out.extend_from_slice(&u16b(0)); // comment len
        out
    }

    #[test]
    fn reads_stored_entry_payload() {
        let z = stored_zip(&["a.bin"]);
        let entries = zip_entries(&z).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "a.bin");
        assert_eq!(&z[entries[0].range.clone()], b"a.bin");
    }

    /// Regression test for the archive that used to abort ddc: its
    /// cd_size = 981_962 = 0x000E_FBCA, so the 4-byte read at eocd + 10 saw
    /// 0xFBCA * 2^16 + 2 = 4_224_319_490 entries and `with_capacity` asked
    /// the allocator for 4_224_319_490 * 48 B = 202_767_335_520 B.
    #[test]
    fn eocd_entry_count_is_u16_not_u32() {
        let mut z = stored_zip(&["classes.dex", "AndroidManifest.xml"]);
        let eocd = z.len() - 22;
        z[eocd + 12..eocd + 16].copy_from_slice(&u32b(981_962));

        let entries = zip_entries(&z).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "classes.dex");
        assert_eq!(entries[1].name, "AndroidManifest.xml");
        assert!(entries.iter().all(|e| e.range.end <= z.len()));
    }
}