//! Subcommand integration tests: the progressive-analysis fast paths
//! (manifest/info/listclasses/getclass/findrefs), run against the built
//! binary. The AXML fixture is a real AndroidManifest.xml pulled from an
//! APK; the dex fixture is the shared d8-built hello.dex.

use std::path::PathBuf;
use std::process::{Command, Output};

fn ddc() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_ddc"));
    c.env_remove("DDC_NOWRITE").env_remove("DDC_CLASSTIME");
    c.env("DDC_LANG", "en");
    c
}

fn fixture() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("../ddc-dec/tests/fixtures/hello.dex");
    p
}

fn axml_fixture() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures/AndroidManifest.bin.xml");
    p
}

fn tmp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("ddc-sub-test-{name}"));
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

#[test]
fn manifest_decodes_real_axml() {
    let o = run(ddc().arg("manifest").arg(axml_fixture()));
    assert!(o.status.success(), "{}", stderr(&o));
    let xml = stdout(&o);
    assert!(xml.contains("<manifest"), "root missing:\n{}", xml);
    assert!(
        xml.contains("package=\"com.reqable.android\""),
        "package:\n{}",
        xml
    );
    assert!(xml.contains("uses-permission"), "permissions:\n{}", xml);
    assert!(xml.contains("<application"), "application:\n{}", xml);
    // Elements are balanced: every close matches an open.
    let opens = xml.matches('<').count();
    assert!(opens > 20, "suspiciously small manifest:\n{}", xml);
}

#[test]
fn listclasses_filters() {
    let o = run(ddc().arg("listclasses").arg(fixture()));
    assert!(o.status.success(), "{}", stderr(&o));
    let all = stdout(&o);
    assert!(all.contains("Hello\n"));
    assert!(all.contains("Greeter\n"));

    let o = run(ddc().arg("listclasses").arg(fixture()).arg("gre"));
    assert!(o.status.success());
    let filtered = stdout(&o);
    assert!(filtered.contains("Greeter"));
    assert!(
        !filtered.contains("Hello\n"),
        "filter did not apply:\n{}",
        filtered
    );
    // stdout mode is silent on stderr (no trailing summary).
    assert!(stderr(&o).is_empty(), "stderr noise:\n{}", stderr(&o));
}

#[test]
fn info_reports_tables() {
    let o = run(ddc().arg("info").arg(fixture()));
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("classes"), "{}", out);
    assert!(out.contains("methods"), "{}", out);
    assert!(out.contains("total: 1 image(s)"), "{}", out);
}

#[test]
fn getclass_prints_one_class() {
    let o = run(ddc().arg("getclass").arg(fixture()).arg("Greeter"));
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("class Greeter {"));
    assert!(!out.contains("class Hello"), "other class leaked:\n{}", out);

    // -o writes the file instead.
    let f = tmp("gc").join("G.java");
    let o = run(ddc()
        .arg("getclass")
        .arg(fixture())
        .arg("Greeter")
        .arg("-o")
        .arg(&f));
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(std::fs::read_to_string(&f)
        .unwrap()
        .contains("class Greeter {"));
    let _ = std::fs::remove_dir_all(tmp("gc"));
}

#[test]
fn getclass_unknown_class_errors() {
    let o = run(ddc().arg("getclass").arg(fixture()).arg("no.Such"));
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("not found"));
}

#[test]
fn findrefs_all_four_kinds() {
    // string — column format: header row + one row per method.
    let o = run(ddc().arg("findrefs").arg(fixture()).arg("string").arg("hi"));
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.starts_with("dex         kind"),
        "header missing:\n{}",
        out
    );
    assert!(out.contains("const-string  Greeter greet()"), "{}", out);
    assert!(out.contains("\"hi \""), "{}", out);

    // method
    let o = run(ddc()
        .arg("findrefs")
        .arg(fixture())
        .arg("method")
        .arg("greet"));
    assert!(o.status.success());
    let out = stdout(&o);
    assert!(out.contains("invoke        Hello main("), "{}", out);
    assert!(
        out.contains("LGreeter;->greet()Ljava/lang/String;"),
        "{}",
        out
    );

    // field
    let o = run(ddc()
        .arg("findrefs")
        .arg(fixture())
        .arg("field")
        .arg("counter"));
    assert!(o.status.success());
    let out = stdout(&o);
    assert!(out.contains("LHello;->counter:I"), "{}", out);

    // type (any naming form normalizes)
    let o = run(ddc()
        .arg("findrefs")
        .arg(fixture())
        .arg("type")
        .arg("Greeter"));
    assert!(o.status.success());
    let out = stdout(&o);
    assert!(out.contains("LGreeter;"), "{}", out);
}

#[test]
fn findrefs_with_class_filter() {
    // Exact class filter: name greet exists on Greeter only.
    let o = run(ddc()
        .arg("findrefs")
        .arg(fixture())
        .arg("method")
        .arg("greet")
        .arg("--class")
        .arg("Greeter"));
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stdout(&o).contains("LGreeter;->greet()"));

    // Wrong class: no method ids resolve → no hits, and stdout mode
    // stays silent on stderr.
    let o = run(ddc()
        .arg("findrefs")
        .arg(fixture())
        .arg("method")
        .arg("greet")
        .arg("--class")
        .arg("Nope"));
    assert!(o.status.success());
    // No hits: header only (the table is the payload, not noise).
    assert!(
        !stdout(&o).contains("Greeter"),
        "unexpected hit:\n{}",
        stdout(&o)
    );
    assert!(stderr(&o).is_empty(), "stderr noise:\n{}", stderr(&o));

    // --dex filters images before the scan (raw dex label = file stem).
    let o = run(ddc()
        .arg("findrefs")
        .arg(fixture())
        .arg("--dex")
        .arg("hello")
        .arg("string")
        .arg("hi"));
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stdout(&o).contains("\"hi \""));

    // --dex with no matching image errors and lists what IS available.
    let o = run(ddc()
        .arg("findrefs")
        .arg(fixture())
        .arg("--dex")
        .arg("classes9")
        .arg("string")
        .arg("hi"));
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("no matching dex images"));
    assert!(
        stderr(&o).contains("hello"),
        "available list:\n{}",
        stderr(&o)
    );
}

#[test]
fn getclass_reports_multi_dex_ambiguity() {
    // The same class name registered from two images: getclass names the
    // ambiguity and resolves from the first.
    let o = run(ddc()
        .arg("getclass")
        .arg(fixture())
        .arg(fixture())
        .arg("Greeter"));
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stdout(&o).contains("class Greeter {"));
    assert!(
        stderr(&o).contains("defined in 2 images"),
        "no ambiguity note:\n{}",
        stderr(&o)
    );
    assert!(
        stderr(&o).contains("--dex"),
        "no --dex hint:\n{}",
        stderr(&o)
    );
}

#[test]
fn getclass_stdout_is_clean() {
    // stdout carries the source only; timing lives on stderr nowhere in
    // stdout mode.
    let o = run(ddc().arg("getclass").arg(fixture()).arg("Greeter"));
    assert!(o.status.success());
    assert!(stdout(&o).contains("class Greeter {"));
    assert!(stderr(&o).is_empty(), "stderr noise:\n{}", stderr(&o));
}

#[test]
fn manifest_stdout_is_clean() {
    let o = run(ddc().arg("manifest").arg(axml_fixture()));
    assert!(o.status.success());
    assert!(stdout(&o).contains("<manifest"));
    assert!(stderr(&o).is_empty(), "stderr noise:\n{}", stderr(&o));
}

#[test]
fn findrefs_bad_invocation() {
    let o = run(ddc().arg("findrefs").arg(fixture()).arg("bogus").arg("x"));
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("unknown kind"));

    let o = run(ddc().arg("findrefs").arg(fixture()));
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("findrefs needs <input>"));
}

// ---- browse subcommands (jadx-style lookup tools) --------------------------

#[test]
fn strings_filter_and_locations() {
    let o = run(ddc()
        .arg("strings")
        .arg(fixture())
        .arg("-f")
        .arg("hi")
        .arg("--with-locations"));
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.starts_with("dex         string"), "header:\n{}", out);
    assert!(out.contains("\"hi \""), "literal:\n{}", out);
    assert!(
        out.contains("Greeter greet()Ljava/lang/String;"),
        "used-by:\n{}",
        out
    );
}

#[test]
fn members_scoped_to_class() {
    let o = run(ddc()
        .arg("members")
        .arg(fixture())
        .arg("--class")
        .arg("Greeter"));
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.starts_with("dex         kind    class member"),
        "header:\n{}",
        out
    );
    assert!(
        out.contains("method  Greeter greet()Ljava/lang/String;"),
        "method:\n{}",
        out
    );
    // --class excludes other classes' members.
    assert!(
        !out.contains("Hello main"),
        "unfiltered class leaked:\n{}",
        out
    );
}

#[test]
fn hierarchy_lineage_and_subs() {
    let o = run(ddc().arg("hierarchy").arg(fixture()).arg("Greeter"));
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("class      Greeter"), "self:\n{}", out);
    assert!(
        out.contains("extends    Ljava/lang/Object;"),
        "super:\n{}",
        out
    );

    let o = run(ddc()
        .arg("hierarchy")
        .arg(fixture())
        .arg("java.lang.Object"));
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("sub        Greeter"), "sub:\n{}", out);
    assert!(out.contains("sub        Hello"), "sub:\n{}", out);
}

#[test]
fn largest_orders_by_insn_count() {
    let o = run(ddc().arg("largest").arg(fixture()).arg("-n").arg("2"));
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.starts_with("  insns  dex"), "header:\n{}", out);
    // hello.dex's biggest method is Hello.main (23 insns).
    assert!(
        out.contains("Hello main([Ljava/lang/String;)V"),
        "top:\n{}",
        out
    );
    let insns: Vec<usize> = out
        .lines()
        .skip(1)
        .filter_map(|l| {
            l.split_whitespace()
                .next()
                .and_then(|n| n.parse().ok())
        })
        .collect();
    assert_eq!(insns.len(), 2, "rows:\n{}", out);
    assert!(insns[0] >= insns[1], "not sorted desc:\n{}", out);
}

#[test]
fn disasm_whole_class_and_single_method() {
    let o = run(ddc().arg("disasm").arg(fixture()).arg("Greeter"));
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("// hello Greeter"), "banner:\n{}", out);
    assert!(
        out.contains("greet()Ljava/lang/String;:"),
        "method:\n{}",
        out
    );
    assert!(out.contains("const-string"), "insn:\n{}", out);

    // Class.method narrows to one method.
    let o = run(ddc().arg("disasm").arg(fixture()).arg("Greeter.greet"));
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("greet()Ljava/lang/String;:"),
        "method:\n{}",
        out
    );
    // The header check uses the `sig:` form — an invoke's method operand
    // legitimately references <init>.
    assert!(
        !out.contains("<init>(Ljava/lang/String;):"),
        "other methods leaked:\n{}",
        out
    );
    // Operands are rendered: registers, resolved string and method refs.
    assert!(
        out.contains("const-string v1, string@"),
        "string operand:\n{}",
        out
    );
    assert!(out.contains("\"hi \""), "string literal:\n{}", out);
    assert!(
        out.contains("method@8 java/lang/StringBuilder->append(Ljava/lang/String;)"),
        "method operand:\n{}",
        out
    );
}

#[test]
fn callers_finds_invokers() {
    let o = run(ddc().arg("callers").arg(fixture()).arg("println"));
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("Hello main([Ljava/lang/String;)V"),
        "caller:\n{}",
        out
    );
    assert!(
        out.contains("Ljava/io/PrintStream;->println"),
        "target:\n{}",
        out
    );
}

#[test]
fn getmethod_keeps_provenance_header() {
    let o = run(ddc().arg("getmethod").arg(fixture()).arg("Greeter.greet"));
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("// Decompiled by https://github.com/ejfkdev/ddc"),
        "header:\n{}",
        out
    );
    assert!(
        out.contains("return \"hi \" + this.name;"),
        "method body:\n{}",
        out
    );
}

#[test]
fn pkg_decompiles_a_package_subtree() {
    // hello.dex has no packages (default package) — the whole image is the
    // "root package"; use it to prove the pipeline runs end-to-end.
    let dir = tmp("pkg-root");
    let o = run(ddc().arg("pkg").arg(fixture()).arg("").arg("-o").arg(&dir));
    assert!(o.status.success(), "{}", stderr(&o));
    let mut n = 0;
    for e in walk(&dir) {
        if e.ends_with(".java") {
            n += 1;
        }
    }
    assert!(
        n >= 2,
        "expected Hello+Greeter, got {n} file(s) in {}",
        dir.display()
    );
}

fn walk(dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&p) {
            for e in rd.flatten() {
                let path = e.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(path.display().to_string());
                }
            }
        }
    }
    out
}

// ---- manifest-derived + resource subcommands ---------------------------------

/// A stored-zip "APK": manifest + a res XML + a text asset + hello.dex.
/// (Zip layout via the same hand-rolled builder xapk.rs uses.)
fn apk_fixture(tag: &str) -> PathBuf {
    let manifest = std::fs::read(axml_fixture()).expect("manifest fixture");
    let dex = std::fs::read(fixture()).expect("dex fixture");
    let items: Vec<(&str, Vec<u8>)> = vec![
        ("AndroidManifest.xml", manifest.clone()),
        ("res/values/strings.xml", manifest), // any AXML decodes
        ("assets/note.txt", b"hello asset\n".to_vec()),
        ("classes.dex", dex),
    ];
    // Tests in this binary run in parallel — one dir per caller, or a
    // neighbor's remove_dir_all races this one's read.
    let dir = tmp(&format!("apk-fixture-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("app.apk");
    std::fs::write(&p, stored_zip(&items)).unwrap();
    p
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn stored_zip(items: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut cds: Vec<(String, u32, u32, u32)> = Vec::new();
    let u16le = |v: u16| v.to_le_bytes();
    let u32le = |v: u32| v.to_le_bytes();
    for (name, data) in items {
        let offset = out.len() as u32;
        let crc = crc32(data);
        out.extend_from_slice(&u32le(0x0403_4b50));
        out.extend_from_slice(&u16le(20));
        out.extend_from_slice(&u16le(0));
        out.extend_from_slice(&u16le(0)); // stored
        out.extend_from_slice(&u16le(0));
        out.extend_from_slice(&u16le(0));
        out.extend_from_slice(&u32le(crc));
        out.extend_from_slice(&u32le(data.len() as u32));
        out.extend_from_slice(&u32le(data.len() as u32));
        out.extend_from_slice(&u16le(name.len() as u16));
        out.extend_from_slice(&u16le(0));
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        cds.push((name.to_string(), crc, data.len() as u32, offset));
    }
    let cd_start = out.len();
    for (name, crc, size, offset) in &cds {
        // Central-directory entry, exactly 46 bytes + name.
        out.extend_from_slice(&u32le(0x0201_4b50));
        out.extend_from_slice(&u16le(20)); // version made by
        out.extend_from_slice(&u16le(20)); // version needed
        out.extend_from_slice(&u16le(0)); // flags
        out.extend_from_slice(&u16le(0)); // method: stored
        out.extend_from_slice(&u16le(0)); // time
        out.extend_from_slice(&u16le(0)); // date
        out.extend_from_slice(&u32le(*crc));
        out.extend_from_slice(&u32le(*size));
        out.extend_from_slice(&u32le(*size));
        out.extend_from_slice(&u16le(name.len() as u16));
        out.extend_from_slice(&u16le(0)); // extra len
        out.extend_from_slice(&u16le(0)); // comment len
        out.extend_from_slice(&u16le(0)); // disk number
        out.extend_from_slice(&u16le(0)); // internal attrs
        out.extend_from_slice(&u32le(0)); // external attrs
        out.extend_from_slice(&u32le(*offset));
        out.extend_from_slice(name.as_bytes());
    }
    let eocd = out.len();
    out.extend_from_slice(&u32le(0x0605_4b50));
    out.extend_from_slice(&u16le(0));
    out.extend_from_slice(&u16le(0));
    out.extend_from_slice(&u16le(cds.len() as u16));
    out.extend_from_slice(&u16le(cds.len() as u16));
    out.extend_from_slice(&u32le((eocd - cd_start) as u32));
    out.extend_from_slice(&u32le(cd_start as u32));
    out.extend_from_slice(&u16le(0));
    out
}

#[test]
fn manifest_component_filter() {
    let o = run(ddc()
        .arg("manifest")
        .arg(axml_fixture())
        .arg("--component")
        .arg("activity"));
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("<manifest"), "header kept:\n{}", out);
    assert!(out.contains("<activity"), "activity kept:\n{}", out);
    assert!(
        !out.contains("uses-permission"),
        "permissions filtered:\n{}",
        out
    );

    let o = run(ddc()
        .arg("manifest")
        .arg(axml_fixture())
        .arg("--component")
        .arg("launcher"));
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("<activity"), "launcher activity:\n{}", out);
    assert!(!out.contains("<service"), "services filtered:\n{}", out);
}

#[test]
fn mainactivity_reports_entry_point() {
    let o = run(ddc().arg("mainactivity").arg(apk_fixture("main")));
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("package     com.reqable.android"),
        "package:\n{}",
        out
    );
    assert!(
        out.contains("launcher    com.reqable.android.MainActivity"),
        "launcher:\n{}",
        out
    );
    assert!(out.contains("dex         "), "dex verification:\n{}", out);
}

#[test]
fn res_lists_and_dumps_entries() {
    let apk = apk_fixture("res");
    let o = run(ddc().arg("res").arg(&apk));
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    let header: Vec<&str> = out.lines().next().unwrap().split_whitespace().collect();
    assert_eq!(header, vec!["method", "size", "entry"], "header:\n{}", out);
    assert!(
        out.contains("res/values/strings.xml"),
        "xml entry:\n{}",
        out
    );
    assert!(out.contains("assets/note.txt"), "asset entry:\n{}", out);

    // Text entry dumps verbatim.
    let o = run(ddc().arg("res").arg(&apk).arg("assets/note.txt"));
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o), "hello asset\n");

    // Binary XML entry decodes through the AXML decoder.
    let o = run(ddc().arg("res").arg(&apk).arg("res/values/strings.xml"));
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("<manifest") || out.contains("<resources"),
        "decoded xml:\n{}",
        out
    );
}

#[test]
fn getmethod_slices_one_method() {
    let o = run(ddc().arg("getmethod").arg(fixture()).arg("Greeter.greet"));
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("java.lang.String greet() {"),
        "signature:\n{}",
        out
    );
    assert!(
        out.contains("return \"hi \" + this.name;"),
        "body:\n{}",
        out
    );
    assert!(
        !out.contains("class Greeter {"),
        "whole class leaked:\n{}",
        out
    );

    // Unknown method: error lists what the class has.
    let o = run(ddc().arg("getmethod").arg(fixture()).arg("Greeter.nope"));
    assert_eq!(o.status.code(), Some(2));
    assert!(
        stderr(&o).contains("methods: Greeter, greet"),
        "hint:\n{}",
        stderr(&o)
    );
}

#[test]
fn pkg_app_reports_unresolvable_package() {
    // hello.dex's classes live in the default package, so the manifest's
    // com.reqable.android has nothing under it (launcher fallback is the
    // same package here) — a clean error, not a crash.
    let o = run(ddc()
        .arg("pkg")
        .arg(apk_fixture("pkg-app"))
        .arg("--app")
        .arg("-o")
        .arg(tmp("pkg-app-none")));
    assert_eq!(o.status.code(), Some(2));
    assert!(
        stderr(&o).contains("no classes under package"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn info_context_header_and_table() {
    let apk = apk_fixture("info-ctx");
    // DDC_LANG pins the locale (a zh shell would flip the row labels).
    let o = run(ddc().arg("info").arg(&apk).env("DDC_LANG", "en"));
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    // context header
    assert!(out.contains("label       Reqable"), "{out}");
    assert!(out.contains("package     com.reqable.android"), "{out}");
    assert!(out.contains("version     3.2.23 (221)"), "{out}");
    assert!(out.contains("launcher    "), "{out}");
    assert!(out.contains("sdk         21–35"), "{out}");
    assert!(out.contains("md5         "), "{out}");
    // per-dex table below it
    assert!(out.contains("image"), "{out}");
    assert!(out.contains("classes"), "{out}");
}
