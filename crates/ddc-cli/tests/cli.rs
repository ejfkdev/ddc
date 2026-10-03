//! CLI integration tests: argument semantics and output routing, run
//! against the built binary via CARGO_BIN_EXE (fixture: the d8-built
//! hello.dex from ddc-dec).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn ddc() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_ddc"));
    c.env_remove("DDC_NOWRITE").env_remove("DDC_CLASSTIME");
    // Pin English by default AND strip the runner's whole locale set:
    // the language tests set exactly the variables they exercise, and a
    // CI image exporting LC_ALL=en_US would otherwise outrank the LANG
    // under test (correctly, per POSIX priority — so it must not leak).
    c.env("DDC_LANG", "en");
    for v in ["LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE"] {
        c.env_remove(v);
    }
    c
}

fn fixture() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("../ddc-dec/tests/fixtures/hello.dex");
    p
}

fn tmp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("ddc-cli-test-{name}"));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn run(c: &mut Command) -> Output {
    c.output().expect("spawn ddc")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn count_java(dir: &Path) -> usize {
    fn walk(p: &Path, n: &mut usize) {
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                let path = e.path();
                if path.is_dir() {
                    walk(&path, n);
                } else if path.extension().and_then(|e| e.to_str()) == Some("java") {
                    *n += 1;
                }
            }
        }
    }
    let mut n = 0;
    walk(dir, &mut n);
    n
}

#[test]
fn help_and_version() {
    let o = run(ddc().arg("--help"));
    assert!(o.status.success());
    let out = stdout(&o);
    assert!(
        out.contains("Usage:\n  ddc [OPTIONS] <INPUT>... [OUTPUT]"),
        "usage:\n{}",
        out
    );
    // `ddc help [SUBCOMMAND]` is part of the usage block.
    assert!(
        out.contains("ddc help [SUBCOMMAND]"),
        "help routing:\n{}",
        out
    );
    // Name, version and homepage lead the help.
    assert!(out.starts_with("ddc "), "header:\n{}", out);
    assert!(out.contains(env!("CARGO_PKG_VERSION")), "version:\n{}", out);
    assert!(
        out.contains("https://github.com/ejfkdev/ddc"),
        "homepage:\n{}",
        out
    );
    // Worked examples and the subcommand menu are part of the help.
    assert!(out.contains("Examples:"), "examples:\n{}", out);
    assert!(
        out.contains("pkg <input> com.foo [-o DIR]"),
        "subcommands:\n{}",
        out
    );
    // Every listed subcommand documents its own options.
    assert!(
        out.contains("ddc help <SUBCOMMAND>"),
        "per-command help hint:\n{}",
        out
    );

    // Same for the bare `help` command.
    let o = run(ddc().arg("help"));
    assert!(o.status.success());
    assert!(stdout(&o).starts_with("ddc "));

    // -V and `version` both print name, version, homepage.
    for flag in ["-V", "version"] {
        let o = run(ddc().arg(flag));
        assert!(o.status.success());
        let out = stdout(&o);
        assert!(out.starts_with("ddc "), "{flag} header:\n{}", out);
        assert!(
            out.contains(env!("CARGO_PKG_VERSION")),
            "{flag} version:\n{}",
            out
        );
        assert!(
            out.contains("https://github.com/ejfkdev/ddc"),
            "{flag} homepage:\n{}",
            out
        );
    }
}

#[test]
fn no_args_prints_help_usage_errors_print_one_line() {
    // No arguments at all: the help is the default action (stdout, 0).
    let o = run(&mut ddc());
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stdout(&o).starts_with("ddc "));
    assert_eq!(stderr(&o), "");

    // A usage error prints exactly the message — no help dump after it.
    let o = run(ddc().arg("--bogus"));
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(stdout(&o), "");
    let err = stderr(&o);
    assert!(err.contains("unknown option"), "{}", err);
    assert_eq!(err.lines().count(), 1, "error must be one line:\n{}", err);
}

#[test]
fn subcommand_help_and_error_routing() {
    // `ddc help <cmd>` and `ddc <cmd> --help` show the command's own
    // signature: positional arguments plus its specific options.
    for invocation in [vec!["help", "getclass"], vec!["getclass", "--help"]] {
        let o = run(ddc().args(&invocation));
        assert!(o.status.success(), "{}", stderr(&o));
        let out = stdout(&o);
        assert!(out.contains("ddc getclass — "), "{}", out);
        assert!(
            out.contains("Usage: ddc getclass <INPUT>... <FQCN>"),
            "{}",
            out
        );
        assert!(out.contains("Arguments:"), "{}", out);
        assert!(out.contains("-o, --output FILE"), "{}", out);
        assert!(out.contains("-d, --dex NAME"), "{}", out);
        // Each command's card carries worked examples.
        assert!(out.contains("Examples:"), "{}", out);
    }

    // Unknown help topic: one error line, exit 2.
    let o = run(ddc().arg("help").arg("nonsense"));
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("unknown help topic"), "{}", stderr(&o));
    assert_eq!(stdout(&o), "");

    // A subcommand error prints the message only — no help dump.
    let o = run(ddc().arg("getclass"));
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(stdout(&o), "");
    let err = stderr(&o);
    assert!(err.contains("needs a class name"), "{}", err);
    assert_eq!(err.lines().count(), 1, "error must be one line:\n{}", err);
}

#[test]
fn default_output_is_input_sibling_dir() {
    let out = tmp("sibling");
    std::fs::create_dir_all(&out).unwrap();
    let dex = out.join("hello.dex");
    std::fs::copy(fixture(), &dex).unwrap();

    let o = run(ddc().arg(&dex));
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(count_java(&out.join("hello-out")), 2);
    assert!(stderr(&o).contains("wrote 2 file(s)"));
    // Total time rides the summary line (stderr, never stdout).
    assert!(
        regex_secs(&stderr(&o)),
        "summary has no elapsed:\n{}",
        stderr(&o)
    );
    let _ = std::fs::remove_dir_all(&out);
}

/// `in <secs>` on a summary line: 1-3 decimals + 's'.
fn regex_secs(s: &str) -> bool {
    let line = s.lines().rev().find(|l| l.contains("in ")).unwrap_or("");
    line.contains(" in ") && line.ends_with('s') && line.contains('.')
}

#[test]
fn provenance_header_names_the_input() {
    let out = tmp("prov").join("src");
    let o = run(ddc().arg(fixture()).arg(&out));
    assert!(o.status.success(), "{}", stderr(&o));
    let text = std::fs::read_to_string(out.join("Hello.java")).unwrap();
    assert!(text.starts_with("// Decompiled by https://github.com/ejfkdev/ddc "));
    // The dex image label (input stem) + DEX version, before Source file.
    assert!(
        text.contains("// From: hello (DEX "),
        "From line:\n{}",
        text
    );
    let _ = std::fs::remove_dir_all(tmp("prov"));
}

#[test]
fn positional_output_dir() {
    let out = tmp("positional").join("src");
    let o = run(ddc().arg(fixture()).arg(&out));
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(count_java(&out), 2);
    let _ = std::fs::remove_dir_all(tmp("positional"));
}

#[test]
fn stdout_sink_has_separators_and_order() {
    let o = run(ddc().arg(fixture()).arg("-o").arg("-"));
    assert!(o.status.success());
    let s = stdout(&o);
    assert_eq!(s.matches("// =====").count(), 2);
    // Output order matches `--list` (the pool's class_defs order), even
    // though stdout mode runs a single worker by design.
    let l = run(ddc().arg(fixture()).arg("-l"));
    let list_out = stdout(&l);
    let list: Vec<&str> = list_out.lines().collect();
    let mut prev = 0usize;
    for name in &list {
        let needle = format!("// ===== {} =====", name);
        let pos = s
            .find(&needle)
            .unwrap_or_else(|| panic!("{} missing", needle));
        assert!(prev <= pos, "{} out of order", name);
        prev = pos;
    }
}

#[test]
fn single_class_flag_defaults_to_stdout() {
    let o = run(ddc().arg(fixture()).arg("-c").arg("Greeter"));
    assert!(o.status.success());
    assert!(stdout(&o).contains("class Greeter {"));
}

#[test]
fn single_class_flag_to_file() {
    let f = tmp("onefile").join("G.java");
    let o = run(ddc()
        .arg(fixture())
        .arg("-c")
        .arg("Greeter")
        .arg("-o")
        .arg(&f));
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(f.is_file());
    let text = std::fs::read_to_string(&f).unwrap();
    assert!(text.contains("class Greeter {"));
    let _ = std::fs::remove_dir_all(tmp("onefile"));
}

#[test]
fn file_output_rejected_for_multiple_classes() {
    let o = run(ddc().arg(fixture()).arg("-o").arg("out.java"));
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("would be written"));
}

#[test]
fn directory_input_scans_recursively() {
    let root = tmp("dirin");
    let sub = root.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::copy(fixture(), root.join("a.dex")).unwrap();
    std::fs::copy(fixture(), sub.join("b.dex")).unwrap();
    // Same classes in both images: dedup keeps the count at 2.
    let out = root.join("out");
    let o = run(ddc().arg(&root).arg(&out));
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(count_java(&out), 2);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn multi_input_positional_merge() {
    // The last positional is the OUTPUT when it neither exists nor has a
    // dex-ish extension; two hello.dex images merge (dedup → 2 classes).
    let out = tmp("merge");
    let o = run(ddc().arg(fixture()).arg(fixture()).arg(&out));
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(count_java(&out), 2);
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn not_a_dex_reports_cleanly() {
    let f = tmp("bogus").join("x.txt");
    std::fs::create_dir_all(f.parent().unwrap()).unwrap();
    std::fs::write(&f, "hello").unwrap();
    let o = run(ddc().arg(&f));
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("not a DEX image or ZIP/APK archive"));
    let _ = std::fs::remove_dir_all(tmp("bogus"));
}

#[test]
fn existing_dir_without_dex_is_output() {
    // A pre-created (or leftover-output) directory with no dex-bearing
    // files is the OUTPUT, not an input: `ddc app.apk weibo/` with a
    // `weibo/` of last run's .java files must write INTO it.
    let root = tmp("precreated");
    let target = root.join("weibo");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("old.java"), "// previous output").unwrap();
    let o = run(ddc().arg(fixture()).arg(&target));
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stderr(&o).contains("wrote 2 file(s) to"), "{}", stderr(&o));
    assert_eq!(count_java(&target), 3); // old.java + Hello + Greeter
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn dex_bearing_dir_stays_input() {
    // A directory that CONTAINS dex files is an input even in the last
    // positional slot: no positional output is extracted, and the
    // default dir follows the FIRST input.
    let root = tmp("dirinput");
    std::fs::create_dir_all(&root).unwrap();
    let first = root.join("first.dex");
    std::fs::copy(fixture(), &first).unwrap();
    let dump = root.join("dump");
    std::fs::create_dir_all(&dump).unwrap();
    std::fs::copy(fixture(), dump.join("hello.dex")).unwrap();

    let o = run(ddc().arg(&first).arg(&dump));
    assert!(o.status.success(), "{}", stderr(&o));
    // Both inputs merged (dedup → 2 classes); output at the first
    // input's sibling, NOT inside dump/.
    assert_eq!(count_java(&root.join("first-out")), 2);
    assert!(!dump.join("Hello.java").exists(), "dump/ became the output");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn empty_dir_input_bails_loudly() {
    let root = tmp("emptydir");
    std::fs::create_dir_all(root.join("void")).unwrap();
    let o = run(ddc().arg(root.join("void")));
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("no .dex/.apk/.jar/.zip files under"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn inline_option_values() {
    let out = tmp("inline");
    let o = run(ddc()
        .arg(fixture())
        .arg(format!("--output={}", out.display())));
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(count_java(&out), 2);
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn language_autodetect_and_override() {
    // DDC_LANG=zh selects Chinese outright.
    let o = run(ddc().env("DDC_LANG", "zh").arg("-V"));
    assert!(stdout(&o).contains("反编译器"), "{}", stdout(&o));

    // LANG=zh alone (DDC_LANG unset) selects Chinese.
    let o = run(ddc()
        .env_remove("DDC_LANG")
        .env("LANG", "zh_CN.UTF-8")
        .arg("-V"));
    assert!(stdout(&o).contains("反编译器"), "{}", stdout(&o));
    let o = run(ddc()
        .env("LANG", "zh_CN.UTF-8")
        .env("DDC_LANG", "en")
        .arg("-V"));
    assert!(stdout(&o).contains("decompiler"), "{}", stdout(&o));

    // English fallback for non-zh locales, with no DDC_LANG.
    let o = run(ddc()
        .env_remove("DDC_LANG")
        .env("LANG", "C.UTF-8")
        .arg("-V"));
    assert!(stdout(&o).contains("decompiler"), "{}", stdout(&o));

    // Errors localize too: LANG=zh + a bad option → the Chinese message
    // ONLY — one line, nothing on stdout.
    let o = run(ddc()
        .env_remove("DDC_LANG")
        .env("LANG", "zh_CN.UTF-8")
        .arg("--bogus"));
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("未知选项"), "{}", stderr(&o));
    assert_eq!(stdout(&o), "");
}

#[test]
fn language_chain_priority_and_neutral_tags() {    // LC_ALL outranks LANG (POSIX priority — a stated en wins over zh).
    let o = run(ddc()
        .env_remove("DDC_LANG")
        .env("LC_ALL", "en_US.UTF-8")
        .env("LANG", "zh_CN.UTF-8")
        .arg("-V"));
    assert!(stdout(&o).contains("decompiler"), "{}", stdout(&o));

    // C states no language: the chain walks on to LANG.
    let o = run(ddc()
        .env_remove("DDC_LANG")
        .env("LC_ALL", "C")
        .env("LANG", "zh_CN.UTF-8")
        .arg("-V"));
    assert!(stdout(&o).contains("反编译器"), "{}", stdout(&o));
    let o = run(ddc()
        .env_remove("DDC_LANG")
        .env("LC_ALL", "POSIX")
        .env("LANG", "zh_TW")
        .arg("-V"));
    assert!(stdout(&o).contains("反编译器"), "{}", stdout(&o));

    // LANGUAGE is a priority list: the first entry decides.
    let o = run(ddc()
        .env_remove("DDC_LANG")
        .env("LANGUAGE", "zh:en")
        .env_remove("LANG")
        .arg("-V"));
    assert!(stdout(&o).contains("反编译器"), "{}", stdout(&o));
    let o = run(ddc()
        .env_remove("DDC_LANG")
        .env("LANGUAGE", "fr:zh")
        .env_remove("LANG")
        .arg("-V"));
    assert!(stdout(&o).contains("decompiler"), "{}", stdout(&o));

    // BCP-47 style zh-Hans and a typo'd DDC_LANG (falls through to LANG).
    let o = run(ddc()
        .env_remove("DDC_LANG")
        .env("LANG", "zh-Hans")
        .arg("-V"));
    assert!(stdout(&o).contains("反编译器"), "{}", stdout(&o));
    let o = run(ddc()
        .env("DDC_LANG", "fr")
        .env("LANG", "zh_CN.UTF-8")
        .arg("-V"));
    assert!(stdout(&o).contains("反编译器"), "{}", stdout(&o));
}

/// Windows, no locale variables at all (plain cmd.exe / PowerShell):
/// the message language follows the user's UI language. The expectation
/// is derived from the same kernel32 call the binary consults — this
/// verifies the wiring end to end on whatever locale the runner has.
#[test]
#[cfg(windows)]
fn windows_locale_fallback_matches_ui_language() {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
    }
    let zh = unsafe { GetUserDefaultUILanguage() } & 0x3ff == 0x04;
    let o = run(ddc()
        .env_remove("DDC_LANG")
        .env_remove("LC_ALL")
        .env_remove("LC_MESSAGES")
        .env_remove("LANG")
        .env_remove("LANGUAGE")
        .arg("-V"));
    let want = if zh { "反编译器" } else { "decompiler" };
    assert!(stdout(&o).contains(want), "{}", stdout(&o));
}
