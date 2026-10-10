//! End-to-end fixture: a real d8-built DEX → Java source.

use ddc_dec::{top_level_classes, ClassOptions, DexPool};
use ddc_dex::DexFile;

fn pool() -> std::sync::Arc<DexPool> {
    let bytes = std::fs::read("tests/fixtures/hello.dex").unwrap();
    let mut pool = DexPool::new();
    pool.add_dex(DexFile::parse(bytes).unwrap());
    std::sync::Arc::new(pool)
}

#[test]
fn decompiles_greeter() {
    let pool = pool();
    let greeter = pool.get("Greeter").expect("Greeter class");
    let out = ddc_dec::classdec::decompile_class(
        &pool,
        greeter,
        &ClassOptions::default(),
        &std::sync::Mutex::new(Vec::new()),
    )
    .map_err(|e| anyhow::anyhow!("{:#}", e))
    .unwrap();
    assert!(out.contains("class Greeter {"), "header missing:\n{}", out);
    assert!(
        out.contains("private final java.lang.String name;"),
        "field missing:\n{}",
        out
    );
    // Provenance header: tool + version + origin image. The fixture pool
    // uses the default "dex 0" label; NO timestamp — outputs stay
    // byte-stable across runs for diffing.
    assert!(
        out.starts_with("// Decompiled by https://github.com/ejfkdev/ddc "),
        "tool header missing:\n{}",
        out
    );
    assert!(
        out.contains("// From: dex 0 (DEX "),
        "From header missing:\n{}",
        out
    );
    // The StringBuilder concat folds to the source expression.
    assert!(
        out.contains("return \"hi \" + this.name;"),
        "concat not folded:\n{}",
        out
    );
    assert!(!out.contains("StringBuilder"), "builder leaked:\n{}", out);
}

#[test]
fn decompiles_hello_main() {
    let pool = pool();
    let hello = pool.get("Hello").expect("Hello class");
    let out = ddc_dec::classdec::decompile_class(
        &pool,
        hello,
        &ClassOptions::default(),
        &std::sync::Mutex::new(Vec::new()),
    )
    .map_err(|e| anyhow::anyhow!("{:#}", e))
    .unwrap();
    // Constructor folding + call nesting across d8's temp registers.
    assert!(
        out.contains("System.out.println(new Greeter(\"world\").greet());"),
        "main not decompiled cleanly:\n{}",
        out
    );
    // The idiom pass folds the static counter update to `counter++`.
    assert!(
        out.contains("counter++;"),
        "static field update missing:\n{}",
        out
    );
}

#[test]
fn top_level_enumeration() {
    let pool = pool();
    let names = top_level_classes(&pool);
    assert_eq!(names, vec!["Greeter".to_string(), "Hello".to_string()]);
}

#[test]
fn array_multiconsume_materializes_once() {
    // d8 pattern: `static byte[] s = new byte[4]; s[0]=9; s[3]=7` keeps
    // ONE array in a register across the sput and the apuys. The old
    // lifter left the new-array view pending after the sput and emitted
    // a FRESH allocation per aput (`(new byte[4])[0] = 9` ×2 plus the
    // sput's own) — three arrays where the bytecode has one, and the
    // field ended up all-zero. One allocation, one local, element writes
    // on that local; the local's label carries the array type.
    let bytes = std::fs::read("tests/fixtures/arrinit.dex").unwrap();
    let mut pool = DexPool::new();
    pool.add_dex(DexFile::parse(bytes).unwrap());
    let pool = std::sync::Arc::new(pool);
    let cls = pool.get("ArrInit").expect("ArrInit class");
    let out = ddc_dec::classdec::decompile_class(
        &pool,
        cls,
        &ClassOptions::default(),
        &std::sync::Mutex::new(Vec::new()),
    )
    .map_err(|e| anyhow::anyhow!("{:#}", e))
    .unwrap();
    // cure's decl_assign_merge/array-literal normalization: the
    // declaration-position `new byte[] {…}` becomes the equivalent
    // shorthand initializer `{…}` (assignments keep the explicit form).
    assert!(
        out.contains("byte[] v0 = {1, 2, 3, 4"),
        "fill-array-data literal:\n{}",
        out
    );
    assert!(
        out.contains("sparse = v0"),
        "sput of the array local:\n{}",
        out
    );
    // f99f290's array-store coercion renders the primitive narrowing cast
    // on element writes (`(byte) 9` — redundant for a constant, required
    // for non-constant values; the char[] TimeUtils family).
    assert!(
        out.contains("v0[0] = (byte) 9;"),
        "element write on the local:\n{}",
        out
    );
    assert!(
        out.contains("v0[3] = (byte) 7;"),
        "second element write:\n{}",
        out
    );
    // Exactly one new byte[4] allocation for the sparse array.
    let count = out.matches("new byte[4]").count();
    assert_eq!(count, 1, "allocation count for sparse:\n{}", out);
}
