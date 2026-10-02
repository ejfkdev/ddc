//! Class-level rendering: header, fields (with static initializers),
//! constructors, methods and nested member classes.

use jdc_core::emit::{escape_string, format_float, Printer};
use jdc_core::types::JavaType;
use jdc_core::Ctx;

use crate::access::*;
use crate::PoolField;
use crate::ctx::{java_type_to_generic, DexCtx};
use crate::method::decompile_method;
use crate::{desc_type, DexPool, PoolClass, PoolMethod, StaticValue};
use ddc_dex::insn::InsnKind;

#[derive(Debug, Clone)]
pub struct ClassOptions {
    /// Prefix each file with a provenance comment.
    pub provenance: bool,
}

impl Default for ClassOptions {
    fn default() -> Self {
        ClassOptions { provenance: true }
    }
}

/// Decompile one class (plus nested member classes) to a Java source file.
///
/// Classes with large method bodies run in a monitored thread with a
/// deadline: pathological CFGs can drive the shared structurer's walk into
/// an exponential exploration that never returns. On timeout the class is
/// reported failed and the thread is abandoned (reaped at process exit).
/// One registered monitored decompile: the receiver the worker polls at
/// the tail, the class name (for diagnostics), and the deadline counted
/// from the spawn.
pub type PendingMonitor = (
    std::sync::mpsc::Receiver<Result<String, String>>,
    String,
    std::time::Instant,
);

pub fn decompile_class(
    pool: &std::sync::Arc<DexPool>,
    class: &PoolClass,
    opts: &ClassOptions,
    pending: &std::sync::Mutex<Vec<PendingMonitor>>,
) -> anyhow::Result<String> {
    if class_is_risky(pool, class) {
        // Detached monitored thread: the CALLER registers the receiver and
        // moves on (awaiting happens at the end of the run) — a spinning
        // pathological method no longer stalls its worker.
        let pool2 = pool.clone();
        let cls = class.clone();
        let opts2 = opts.clone();
        let (tx, rx) = std::sync::mpsc::channel::<Result<String, String>>();
        let name = class.name.clone();
        // 10s: pure insurance — the structurer's walk budgets and the
        // ladder giant short-circuit bound pathological methods
        // deterministically now (weixin cdp/l1: 4.2s single-class), but
        // full-run worker contention can ~2× that; the old 5s dropped
        // l1's file entirely (every referrer cannot-finds).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let _ = std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(move || {
                let r = decompile_class_impl(&pool2, &cls, &opts2).map_err(|e| format!("{:#}", e));
                let _ = tx.send(r);
            });
        pending.lock().unwrap().push((rx, name, deadline));
        return Err(anyhow::anyhow!(
            "deferred to monitored thread (pathological-CFG guard)"
        ));
    }
    decompile_class_impl(pool, class, opts)
}

/// Any method in the exponential-walk danger zone → run under the
/// deadline. Calibrated between real workloads: routine R8 classes top out
/// near 11k insns (reqable's biggest `<clinit>`); the exponential hangs
/// (weibo's gson TypeAdapters) sit at 24k+.
fn class_is_risky(pool: &DexPool, class: &PoolClass) -> bool {
    // Header peek only — decoding every body here would double the APK's
    // total decode work.
    for m in class.all_methods() {
        if m.code_off == 0 {
            continue;
        }
        if let Some(dex) = pool.dex(m.dex_idx) {
            if let Some((_regs, insns)) = dex.code_stats(m.code_off) {
                if insns > 16_000 {
                    return true;
                }
            }
        }
    }
    false
}

fn decompile_class_impl(
    pool: &DexPool,
    class: &PoolClass,
    opts: &ClassOptions,
) -> anyhow::Result<String> {
    let ctx = DexCtx::new(pool, class);
    // Size the class buffer up front: corpus-average classes render to
    // ~1KB per method — without the reserve the String doubles through
    // 4-6 realloc+copy rounds per class (~2× the final size in memmove).
    // Cap the reserve: weixin's monster classes (hundreds of methods)
    // would reserve megabytes of untouched capacity per class — large
    // blocks take mimalloc's commit/purge path (madvise churn showed up
    // in profiles). 256KB covers the corpus-average class dozens of
    // times over; bigger classes pay a few extra doublings.
    let mut out = String::with_capacity(
        (class.all_methods().count() * 1024).min(256 * 1024) + 512,
    );
    if opts.provenance {
        static BANNER: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        out.push_str(BANNER.get_or_init(|| {
            format!(
                "// Decompiled by https://github.com/ejfkdev/ddc {}\n",
                env!("CARGO_PKG_VERSION")
            )
        }));
        // Provenance: which input image this class was lifted from (jadx's
        // `loaded from: classes.dex` pattern). Byte-stable across runs —
        // deliberately NO timestamp so outputs diff cleanly. Direct
        // push_str: the per-class format! temporaries were 3 allocations
        // × every class in the corpus.
        if let Some(dex) = pool.dex(class.dex_idx) {
            if let Some(label) = pool.dex_labels.get(class.dex_idx) {
                out.push_str("// From: ");
                out.push_str(label);
                out.push_str(" (DEX ");
                out.push_str(&dex.version);
                out.push_str(")\n");
            }
        }
    }
    if let Some(src) = &class.source_file {
        out.push_str("// Source file: ");
        out.push_str(src);
        out.push('\n');
    }
    if class.is_synthetic() {
        out.push_str("// synthetic\n");
    }
    // The DISPLAY package: the root-package relocation rule moves the
    // default-package cohort under a synthetic package, and the emitted
    // `package` line + file path must agree with the renamed refs.
    // Every pre-existing rename rule keeps the package, so this is a
    // no-op for them.
    let own_display_full = crate::apply_class_rename(&class.name);
    let (pkg, _) = split_name(&own_display_full);
    // Per-package caches below are keyed by the ORIGINAL package — the
    // relocated cohort's same-package set is still the root one.
    let pkg_lookup: &str = match crate::root_pkg_display() {
        Some(rp) if rp == pkg => "",
        _ => pkg.as_str(),
    };
    // Two-pass import assembly: the body renders FIRST (into a temp
    // buffer) with the obscured render state installed — expression
    // positions DISCOVER additional obscured refs on the fly — then the
    // file assembles as provenance + package + imports + body. The one
    // extra body copy is the price of correct import placement.
    let shadow = inherited_field_shadows(pool, class);
    let mut obscured_map = compute_obscured_renders(pool, class, &shadow);
    // Same-package simple-name collisions: an import shadows every
    // same-package use of that simple in this file — drop those from
    // the map (their refs stay qualified, erroring honestly).
    // Arc BORROW of the package's simple-name set + a small overlay for
    // this file's own additions ($-tail extras, own field names). The
    // old unconditional deep copy cost one full set per class —
    // WhatsApp's ~10k-class package paid it twice (the second in the
    // obscured-state install), the #2/#3 CPU frames on its profile.
    static EMPTY_BLOCKED: std::sync::OnceLock<std::sync::Arc<jdc_core::FxHashSet<String>>> =
        std::sync::OnceLock::new();
    let blocked_base: std::sync::Arc<jdc_core::FxHashSet<String>> = pool
        .package_simples()
        .get(pkg_lookup)
        .cloned()
        .unwrap_or_else(|| {
            std::sync::Arc::clone(EMPTY_BLOCKED.get_or_init(|| {
                std::sync::Arc::new(jdc_core::FxHashSet::default())
            }))
        });
    // Rare JLS 7.5.1 narrowing: sibling simples the file never
    // references stop blocking — materialized as a REMOVAL overlay.
    let mut blocked_removed: jdc_core::FxHashSet<String> = jdc_core::FxHashSet::default();
    let mut blocked_extra: jdc_core::FxHashSet<String> = jdc_core::FxHashSet::default();
    // JLS 7.5.1 narrowing: importing simple `n` legally shadows the
    // same-package sibling throughout THIS file — it only breaks refs
    // the file itself makes to that sibling. Drop sibling simples the
    // file never references (exhaustive family scan: metadata + raw
    // insn refs), unblocking the import route for field-shadowed
    // package quals (lark pq/e: field `e2` captures `e2.c.b(..)` —
    // "变量 c" ×47 — while sibling pq.c is never used). The scan runs
    // ONLY when an obscured candidate is sibling-blocked.
    // Scan gate: an obscured ref can only exist when some in-scope
    // field name (or the own simple) equals a PACKAGE first segment —
    // and the candidates include BODY refs (lark pq/e's `e2.c.b(..)`
    // is invoke-static-only, invisible to the metadata map), so the
    // gate must not key off obscured_map.
    {
        let own_simple0 = class.name.rsplit(['/', '$']).next().unwrap_or("");
        let can_obscure = shadow.iter().any(|f| pool.package_simples().contains_key(f))
            || (!own_simple0.is_empty()
                && pool.package_simples().contains_key(own_simple0));
        if can_obscure && !blocked_base.is_empty() {
            let used = sibling_ref_simples(pool, class);
            if !used.is_empty() {
                blocked_removed.extend(
                    blocked_base
                        .iter()
                        .filter(|b| !used.contains(*b))
                        .map(|b| b.to_string()),
                );
            }
        }
    }
    // Blocked names must include the RENAMED displays of same-package
    // classes too: refs and imports render through the rename registry,
    // so an import matching a DISPLAY (not the raw simple) shadows that
    // class's in-file uses exactly the same (weixin n0/y0 displays as
    // y02 while h72 declares a raw class y0 — the raw-only blocked set
    // let the y02 import through on one side and killed it on the
    // other, stranding 622 bare `y02` refs).
    // Renamed-display blocking: a ZERO-COPY view into the process-wide
    // per-package cache (built once from the rename registry — iterating
    // `blocked` through apply_class_rename per FILE was O(files ×
    // package): WhatsApp's ~10k-class package X cost 1,269 cumulative
    // worker-seconds, decompile 24s → 134s; per-file clones of the
    // extension set still cost 184s).
    static PKG_RENAMED_DISPLAYS: std::sync::OnceLock<
        jdc_core::FxHashMap<String, jdc_core::FxHashSet<String>>,
    > = std::sync::OnceLock::new();
    let blocked_renamed: Option<&'static jdc_core::FxHashSet<String>> = {
        let cache = PKG_RENAMED_DISPLAYS.get_or_init(|| {
            let mut m: jdc_core::FxHashMap<String, jdc_core::FxHashSet<String>> =
                jdc_core::FxHashMap::default();
            if let Some(ren) = jdc_core::rename::with_renames(|r| r.clone()) {
                for (raw, disp) in ren.iter() {
                    if raw == disp {
                        continue;
                    }
                    let pkg_key = raw.rfind('/').map(|i| &raw[..i]).unwrap_or("");
                    let tail = disp
                        .rsplit(['/', '$'])
                        .next()
                        .unwrap_or(disp.as_str())
                        .to_string();
                    m.entry(pkg_key.to_string()).or_default().insert(tail);
                }
            }
            m
        });
        cache.get(pkg_lookup)
    };
    // In-file member shadowing: the own class's FIELDS and its family's
    // nested-class tails are in scope inside the body and shadow any
    // single-type import of the same simple (JLS 6.4.1/6.4.2) — an
    // obscured simple colliding with them cannot be imported.
    {
        let own_raw = class.name.rsplit('/').next().unwrap_or("");
        let prefix = format!("{}$", own_raw);
        let extras: Vec<String> = blocked_base
            .iter()
            .filter(|b| b.starts_with(&prefix))
            .filter_map(|b| b.rsplit('$').next().map(|t| t.to_string()))
            .collect();
        blocked_extra.extend(extras);
        for f in class.static_fields.iter().chain(class.instance_fields.iter()) {
            blocked_extra.insert(crate::classdec::java_ident(&f.name).into_owned());
        }
    }
    obscured_map.retain(|_internal, simple| {
        !blocked_extra.contains(simple)
            && (blocked_removed.contains(simple)
                || !blocked_base.contains(simple))
            && !blocked_renamed.is_some_and(|e| e.contains(simple))
    });
    // Display-simple collisions cannot coexist as single-type imports:
    // importing simple `n` while THIS file declares `n` is a JLS 7.5.1
    // error, and two imports of the same simple make every use of it
    // ambiguous (weibo `class a2 extends a2` with `import a.a.b.c.a2`
    // + `import a.e.a.a.a2` — both shadow-renamed from package-`a`
    // classes named `a`). Drop every colliding entry (own-display hits
    // drop ALL claimants; multi-internal simples keep the least
    // internal for determinism) — dropped refs fall back to the
    // qualified display render, which resolves against the renamed
    // output files.
    let own_display_internal = crate::apply_class_rename(&class.name);
    let own_disp_simple = own_display_internal
        .rsplit(['/', '$'])
        .next()
        .unwrap_or("")
        .to_string();
    {
        let mut by_simple: std::collections::BTreeMap<&str, Vec<&String>> =
            std::collections::BTreeMap::new();
        for (internal, simple) in obscured_map.iter() {
            by_simple.entry(simple.as_str()).or_default().push(internal);
        }
        let mut drop: Vec<String> = Vec::new();
        for (simple, mut internals) in by_simple {
            if simple == own_disp_simple.as_str() {
                drop.extend(internals.into_iter().cloned());
            } else if internals.len() > 1 {
                internals.sort();
                drop.extend(internals.into_iter().skip(1).cloned());
            }
        }
        for d in &drop {
            obscured_map.remove(d);
        }
    }
    set_obscured_state(
        class.name.clone(),
        obscured_map.clone(),
        std::sync::Arc::clone(&blocked_base),
        blocked_removed.clone(),
        blocked_extra.clone(),
        blocked_renamed,
        shadow,
    );
    let mut body_buf = String::with_capacity(out.capacity() / 2);
    let body_res = emit_class_body(pool, class, &ctx, opts, &mut body_buf, 0);
    let recorded = take_recorded_and_clear();
    body_res?;
    if !pkg.is_empty() {
        out.push('\n');
        out.push_str("package ");
        let dotted = pkg.replace('/', ".");
        out.push_str(&sanitize_fq(&dotted));
        out.push_str(";\n");
        // Imports: the metadata map plus the expression-level
        // discoveries, pool-known only (an import of an unknown class
        // is a hard error), display-renamed, sorted for determinism.
        let mut import_set: jdc_core::FxHashSet<String> =
            obscured_map.keys().cloned().collect();
        for r in recorded {
            if pool.get(&r).is_some() {
                // The import renders under the RENAMED display — the
                // blocked check must use the same name the refs render.
                // Framework-shadow stubs render (and import) their RAW
                // framework FQN — the reference means the platform class.
                let display = if crate::is_fw_shadow(&r) {
                    std::borrow::Cow::Borrowed(r.as_str())
                } else {
                    crate::apply_class_rename(&r)
                };
                let simple = display.rsplit(['/', '$']).next().unwrap_or("");
                if !simple.is_empty()
                    && !blocked_extra.contains(simple)
                    && (blocked_removed.contains(simple)
                        || !blocked_base.contains(simple))
                    && !blocked_renamed.is_some_and(|e| e.contains(simple))
                {
                    import_set.insert(r);
                }
            }
        }
        let mut imports: Vec<String> = import_set
            .iter()
            .map(|internal| {
                let renamed = if crate::is_fw_shadow(internal) {
                    std::borrow::Cow::Borrowed(internal.as_str())
                } else {
                    crate::apply_class_rename(internal)
                };
                crate::classdec::dotted(&renamed)
            })
            .collect();
        imports.sort();
        // Body-discovered (recorded) refs joined after the pre-emission
        // dedupe — re-check at render: one import per display simple,
        // none against this file's own display (JLS 7.5.1). A dropped
        // recorded ref's body render is ambiguous regardless; skipping
        // its import at least avoids the duplicate-import error.
        let own_display_dotted = crate::classdec::dotted(&own_display_internal);
        let own_dot_simple = own_display_dotted.rsplit('.').next().unwrap_or("");
        let mut seen_simples: jdc_core::FxHashSet<String> =
            jdc_core::FxHashSet::default();
        for display in imports {
            let simple = display.rsplit('.').next().unwrap_or("");
            if simple == own_dot_simple || !seen_simples.insert(simple.to_string()) {
                continue;
            }
            out.push_str(&format!("import {};\n", display));
        }
    }
    out.push('\n');
    out.push_str(&body_buf);
    Ok(out)
}

/// Render the class header, fields, methods and nested member classes into
/// `out` (used at top level and recursively for nested members).
// `opts` is only consumed by the nested-member recursion below — that
// is its purpose (propagating emission options into inline children).
/// One enum constant: the source identifier plus constructor
/// arguments beyond the compiler-mandated `(String name, int ordinal)`.
use jdc_core::ir::expr::{ConstVal, Expr};
use jdc_core::ir::stmt::Stmt;
struct EnumConst {
    #[allow(dead_code)]
    field: String,
    name: String,
    extra_args: Vec<Expr>,
}

/// Collect enum constants for a true `enum` rendering. Every ACC_ENUM
/// static field must be initialized in `<clinit>` by
/// `Self.field = new Self("NAME", ordinal, ...)` — the shape javac/d8
/// always emit. Returns None (caller falls back to the desugared
/// `/* enum */ class` form) when anything is missing: R8 variance,
/// constant-specific bodies (the field holds an anonymous subclass), or
/// a <clinit> that failed to decompile.
/// Cached DDC_DBG_ENUM gate (env reads must not sit in per-class paths).
fn dbg_enum() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| std::env::var_os("DDC_DBG_ENUM").is_some())
}

fn dbg_enum_none(ln: u32) -> Option<(Vec<EnumConst>, crate::method::MethodBody)> {
    if dbg_enum() {
        eprintln!("[enum-none] line {}", ln);
    }
    None
}

fn collect_enum_constants(
    pool: &DexPool,
    class: &PoolClass,
    ctx: &DexCtx<'_>,
) -> Option<(Vec<EnumConst>, crate::method::MethodBody)> {
    let _ = ctx;
    let const_fields: Vec<&PoolField> = class
        .static_fields
        .iter()
        .filter(|f| f.access & crate::access::ACC_ENUM != 0)
        .collect();
    // No ACC_ENUM constant fields: R8 drops them ALL when nothing
    // outside reads them (weixin sp2/ji — only the synthetic $VALUES
    // array survives). An ACC_ENUM CLASS still promotes: the synthetic-
    // constant scan below lifts the constants out of the array literal.
    // Non-enum classes have no business here.
    if const_fields.is_empty() && class.access & crate::access::ACC_ENUM == 0 {
        return dbg_enum_none(line!());
    }
    // JLS 8.9.2: a restored true-enum ctor may not read the enum's own
    // static fields (javac rejects even the QUALIFIED `E.field` form).
    // Meituan Robust injects a hotpatch guard reading
    // `changeQuickRedirect` into every instrumented ctor — legal in
    // bytecode (injected after javac), and strip_robust_enum_ctor_guard
    // removes it from the restored source. A self-static sget of any
    // OTHER type makes restoration inexpressible → stay on the fallback.
    for m in class.all_methods() {
        if &*m.name != "<init>" {
            continue;
        }
        let dex = pool.dex(m.dex_idx)?;
        let ci = dex.code_at(m.code_off)?;
        for ins in &ci.insns {
            if let ddc_dex::insn::InsnKind::SGet { field_idx, .. } = ins.kind {
                let fr = dex.field(field_idx);
                if dex.class_name(fr.class_idx) == class.name.as_str()
                    && dex.type_name(fr.type_idx) != "Lcom/meituan/robust/ChangeQuickRedirect;"
                {
                    return dbg_enum_none(line!());
                }
            }
        }
    }
    let clinit = class.all_methods().find(|m| &*m.name == "<clinit>")?;
    let mut body = decompile_method(pool, class, clinit).ok().flatten()?;
    // The structurer can leave the constant-build sequence inside a
    // nested Stmt::Block (jd1.u: stmt4 wraps all 26 ctors beside a
    // trailing loop) — the passes below index a FLAT statement list.
    if let Stmt::Block(vs) = &mut body.body {
        crate::passes::flatten_top_blocks(vs);
    }
    if dbg_enum() {
        if let Stmt::Block(vs) = &body.body {
            for (i, st) in vs.iter().enumerate() {
                let d = format!("{:?}", st);
                eprintln!("[enum-stmt {}] {}", i, d.chars().take(220).collect::<String>());
            }
        }
    }
    crate::passes::strip_clinit_hotfix_guard(&mut body.body);

    // Meituan Robust hotpatch guard (weibo's lifecycle enums — a big
    // slice of the 1,137 fallback population): the ENTIRE constant-build
    // sequence lives in one branch of a top-level
    // `if (PatchProxy.isSupportClinit(..)) { .. } else { .. }`, so the
    // flat scans below see nothing and the enum falls back (its dex
    // `Enum.valueOf(X.class,..)` then fails — X is no Enum subtype,
    // weibo ×853). Hoist the constant-carrying branch to the top level,
    // but only when the top level itself has no constant defs (an
    // ordinary clinit must not be touched, and a mixed shape is beyond
    // the flat-scan contract). Restoration semantics already normalize
    // the clinit (constants move to the header, inits drop); the
    // dispatch shim is inexpressible in a true enum regardless.
    if let Stmt::Block(vs) = &mut body.body {
        let self_cls: &str = class.name.as_str();
        let top_def = |st: &Stmt| -> bool {
            let v = match st {
                Stmt::LocalDef { init: Some(e), .. } => e,
                Stmt::ExprStmt(Expr::Assign { value, .. }) => value,
                _ => return false,
            };
            matches!(v, Expr::New { cls, args, .. }
                if cls.as_ref() == self_cls && args.len() >= 2)
        };
        if !vs.iter().any(top_def) {
            let branch_news = |st: &Stmt| -> usize {
                let mut n = 0usize;
                crate::passes::visit_all_exprs(st, &mut |x| {
                    if let Expr::New { cls, args, .. } = x {
                        if cls.as_ref() == self_cls && args.len() >= 2 {
                            n += 1;
                        }
                    }
                });
                n
            };
            let mut pick: Option<(usize, bool)> = None;
            for (i, st) in vs.iter().enumerate() {
                if let Stmt::If { then_stmt, else_stmt, .. } = st {
                    let t = branch_news(then_stmt) > 0;
                    let e = else_stmt
                        .as_ref()
                        .map(|s2| branch_news(s2) > 0)
                        .unwrap_or(false);
                    if t != e {
                        pick = Some((i, t));
                        break;
                    }
                }
            }
            if let Some((i, then_is_const)) = pick {
                let st = std::mem::replace(&mut vs[i], Stmt::Block(Vec::new()));
                if let Stmt::If { then_stmt, else_stmt, .. } = st {
                    let keep = if then_is_const {
                        Some(then_stmt)
                    } else {
                        else_stmt
                    };
                    if let Some(bx) = keep {
                        let inner = match *bx {
                            Stmt::Block(v) => v,
                            other => vec![other],
                        };
                        vs.splice(i..=i, inner);
                        // The branch can be double-wrapped (Block[Block[..]]);
                        // the passes below index a FLAT list.
                        crate::passes::flatten_top_blocks(vs);
                    }
                }
            }
        }
    }

    // R8/d8 split the constant build across an intermediate local:
    //   Self v0 = new Self("NAME", i, ...);
    //   a = v0;                       // sput to the ACC_ENUM field
    //   Self[] v5 = new Self[n]; v5[k] = v0; ...; $VALUES = v5;
    // Pass 1 registers each intermediate (or direct) new; pass 2 binds
    // them to the ACC_ENUM fields; references to the locals are then
    // rewritten into constant identifiers so the $VALUES build keeps
    // compiling once the definitions drop.
    use std::collections::HashMap;
    let mut const_name: Vec<String> = Vec::new();
    let mut const_field: Vec<String> = Vec::new();
    let mut const_extra: Vec<Vec<Expr>> = Vec::new();
    // Declared ordinal per constant (the ctor's int arg) — the merge at
    // the end orders field-bound and synthetic constants by it and
    // demands a dense 0..n run (Java derives ordinals from declaration
    // position).
    let mut const_ord: Vec<i64> = Vec::new();
    let mut var_of: HashMap<u32, usize> = HashMap::default(); // local id -> const idx
    let mut drop_stmts: Vec<usize> = Vec::new();

    // Collect (immutable borrows) first; the mutable passes come after.
    if !matches!(&body.body, Stmt::Block(_)) { return dbg_enum_none(line!()); }

    // Pass 1: definitions. (immutable borrow; rewrite comes later)
    // Rolling reaching-defs of clinit locals for resolving enum-ctor
    // extra args (resolve_enum_arg); `tainted` flips at the first
    // non-straight-line statement — past it, linear-scan defs are no
    // longer provable and Local args reject the enum mode.
    let mut defs: HashMap<u32, &Expr> = HashMap::default();
    let mut tainted = false;
    let self_name: std::sync::Arc<str> = class.name.as_str().into();
    let self_ty =
        jdc_core::ir::expr::TypeRef::J(JavaType::Object(class.name.as_str().into()));
    // A REUSED intermediate local: `v = new Self(..); A = v; v = new
    // Self(..); B = v;` — the first def is a LocalDef, the rest are
    // Assigns to the same var. Register every def site; pass 2 resolves
    // a local sput to the def site CURRENT at that statement (rolling),
    // not to the var (Kotlin EnumEntries enums, RegexOption: 8→7 count
    // mismatch used to abort the whole promotion).
    let mut def_site: HashMap<usize, (u32, usize)> = HashMap::default();
    for (i, st) in match &body.body {
        Stmt::Block(v) => v.iter().enumerate(),
        _ => return dbg_enum_none(line!()),
    } {
        // (var, New) for both def shapes: LocalDef and Assign-to-local.
        let def_shape: Option<(u32, &Expr)> = match st {
            Stmt::LocalDef {
                var,
                init: Some(e),
                ..
            } => Some((*var, e)),
            Stmt::ExprStmt(Expr::Assign { target, value, .. }) => {
                match (&**target, &**value) {
                    (Expr::Local { var, .. }, e) => Some((*var, e)),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some((var, Expr::New { cls: ncls, args, .. })) = def_shape {
            if ncls.as_ref() == class.name && args.len() >= 2 {
                if let (Expr::Const(ConstVal::Str(n)), Expr::Const(ConstVal::Int(ord0))) =
                    (&args[0], &args[1])
                {
                    if java_ident(n).as_ref() != &**n || n.is_empty() { return dbg_enum_none(line!()); }
                    var_of.insert(var, const_name.len());
                    def_site.insert(i, (var, const_name.len()));
                    const_ord.push(*ord0 as i64);
                    const_name.push(n.to_string());
                    const_field.push(String::new());
                    const_extra.push(resolve_enum_extras(
                        &args[2..],
                        &defs,
                        tainted,
                        &var_of,
                        &const_name,
                        &self_name,
                        &self_ty,
                    )?);
                    drop_stmts.push(i);
                }
            }
        }
        track_def(st, &mut defs, &mut tainted);
    }

    // Pass 2: sputs to the ACC_ENUM fields (direct new or intermediate).
    // Local sputs resolve through the ROLLING def map (updated at each
    // def-site statement) — a reused `v` must bind to the constant
    // defined most recently, not to the var's first registration.
    let mut defs2: HashMap<u32, &Expr> = HashMap::default();
    let mut tainted2 = false;
    let mut cur_def: HashMap<u32, usize> = HashMap::default();
    for (i, st) in match &body.body {
        Stmt::Block(v) => v.iter().enumerate(),
        _ => return dbg_enum_none(line!()),
    } {
        if let Some((var, idx)) = def_site.get(&i) {
            cur_def.insert(*var, *idx);
        }
        if let Stmt::ExprStmt(Expr::Assign { target, value, .. }) = st {
            if let Expr::Field {
                cls,
                name: fname,
                is_static: true,
                ..
            } = &**target
            {
                if cls.as_ref() != class.name {
                    continue;
                }
                // The lifter emits field refs THROUGH field_display — an
                // obscuring-renamed const field (te/b's field `b` shares
                // the class name → displays `b7`) arrives here already
                // renamed; the raw-name compare silently skipped the sput
                // and the enum fell back to `/* enum */ class` with the
                // synthetic ctor stripped — every `new b("STARS", 0)`
                // then failed 「无法将构造器应用到给定类型」 (Telegram 603
                // errors / 430 files). Match on the display name; the
                // promoted constant declares under it, which is exactly
                // what every pool reference resolves through.
                let bound = const_fields.iter().any(|f| {
                    f.name.as_ref() == &**fname
                        || jdc_core::rename::field_display(
                            class.name.as_str(),
                            &f.name,
                            &f.desc,
                        )
                        .is_some_and(|d| d == &**fname)
                });
                if !bound {
                    continue;
                }
                if const_field.iter().any(|f| !f.is_empty() && f == &**fname) { return dbg_enum_none(line!()); // duplicate assignment
                }
                let idx = match &**value {
                    Expr::Local { var, .. } => cur_def.get(var).copied()?,
                    Expr::New { cls: ncls, args, .. }
                        if ncls.as_ref() == class.name && args.len() >= 2 =>
                    {
                        if let (Expr::Const(ConstVal::Str(n)), Expr::Const(ConstVal::Int(ord0))) =
                            (&args[0], &args[1])
                        {
                            if java_ident(n).as_ref() != &**n || n.is_empty() { return dbg_enum_none(line!()); }
                            let idx = const_name.len();
                            const_ord.push(*ord0 as i64);
                            const_name.push(n.to_string());
                            const_field.push(String::new());
                            const_extra.push(resolve_enum_extras(
                                &args[2..],
                                &defs2,
                                tainted2,
                                &var_of,
                                &const_name,
                                &self_name,
                                &self_ty,
                            )?);
                            idx
                        } else {
                            return dbg_enum_none(line!());
                        }
                    }
                    _ => return dbg_enum_none(line!()),
                };
                const_field[idx] = fname.to_string();
                drop_stmts.push(i);
            }
        }
        track_def(st, &mut defs2, &mut tainted2);
    }
    // Own the reaching-def values: defs2 borrows body.body, and the
    // synthetic-constant scan below mutates the $VALUES array in place.
    let defs2_owned: std::collections::HashMap<u32, Expr> = defs2
        .iter()
        .map(|(k, v)| (*k, (*v).clone()))
        .collect();
    drop(defs2);
    let defs2: std::collections::HashMap<u32, &Expr> =
        defs2_owned.iter().map(|(k, v)| (*k, v)).collect();

    // Every ACC_ENUM field bound, every intermediate matched.
    // Every ACC_ENUM FIELD must be bound to a constant. The converse
    // need not hold: R8 drops the FIELD of a constant nothing reads
    // externally while the $VALUES array keeps it (weixin lite/api/n's
    // ON_DESTROY — a pass-1 local with no pass-2 sput). Field-less
    // constants keep their ctor-string source name.
    if const_field.iter().filter(|f| !f.is_empty()).count() != const_fields.len() {
    {
            if dbg_enum() {
                eprintln!("[enum-gate677] const_fields={:?} const_field={:?} const_name={:?} const_ord={:?}",
                    const_fields.iter().map(|f| f.name.to_string()).collect::<Vec<_>>(),
                    const_field, const_name, const_ord);
            }
            return dbg_enum_none(line!());
        }
    }
    // Obfuscated enums rename the ACC_ENUM FIELD (d/e/f) while the ctor's
    // name STRING keeps the source identifier — the promoted constant
    // must be declared under the FIELD name: every reference in the
    // pool resolves through it. Using the name string broke all
    // cross-file references (weixin +2.9k when newly-promoted enums
    // swapped d/e/f for TEXT_ENTER_EDITING/…).
    for i in 0..const_name.len() {
        let field = &const_field[i];
        if !field.is_empty() && field != &const_name[i] {
            let id = java_ident(field);
            if id.is_empty() || id.as_ref() != field.as_str() {
                return dbg_enum_none(line!());
            }
            const_name[i] = field.clone();
        }
    }
    {
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::default();
        if !const_name.iter().all(|c| seen.insert(c.as_str())) { return dbg_enum_none(line!()); }
    }

    // Synthetic constants: entries built INLINE inside the $VALUES
    // array (`new Self("NAME", ord, ..)` with no ACC_ENUM field — R8
    // drops the field when nothing outside reads it; weixin u2/i's
    // "CONSTANT"). Promote them so the header declares the constant:
    // the raw `new` in the array is "无法实例化枚举类", and every
    // constant after the gap carries a SHIFTED ordinal (e was 2 in the
    // dex, renders as position 1).
    let mut synth: Vec<(i64, String, Vec<Expr>)> = Vec::new();
    {
        let self_cls: &str = class.name.as_str();
        if let Stmt::Block(vs) = &mut body.body {
            for st in vs.iter_mut() {
                crate::passes::walk_stmt_exprs(st, &mut |e| {
                    crate::passes::deep_rewrite(e, &mut |x| {
                        let Expr::NewArray { elem, init: Some(list), .. } = x else {
                            return;
                        };
                        if elem.erased() != JavaType::Object(self_cls.into()) {
                            return;
                        }
                        for slot in list.iter_mut() {
                            let (n, ord, rest) = match slot {
                                Expr::New { cls, args, raw: false, .. }
                                    if cls.as_ref() == self_cls && args.len() >= 2 =>
                                {
                                    match (&args[0], &args[1]) {
                                        (
                                            Expr::Const(ConstVal::Str(sv)),
                                            Expr::Const(ConstVal::Int(o)),
                                        ) => (sv.to_string(), *o as i64, args[2..].to_vec()),
                                        _ => continue,
                                    }
                                }
                                _ => continue,
                            };
                            if java_ident(&n).as_ref() != n.as_str() || n.is_empty() {
                                continue;
                            }
                            if const_name.contains(&n)
                                || synth.iter().any(|(_, s2, _)| *s2 == n)
                            {
                                continue;
                            }
                            let Some(extras) = resolve_enum_extras(
                                &rest,
                                &defs2,
                                tainted2,
                                &var_of,
                                &const_name,
                                &self_name,
                                &self_ty,
                            ) else {
                                continue;
                            };
                            *slot = Expr::Field {
                                owner: None,
                                cls: self_name.clone(),
                                name: std::sync::Arc::from(n.as_str()),
                                ty: self_ty.clone(),
                                is_static: true,
                            };
                            synth.push((ord, n, extras));
                        }
                    });
                });
            }
        }
    }

    // Pass 3: rewrite references to the intermediate locals into the
    // constant identifiers (static-field reads on Self). POSITION-AWARE:
    // a REUSED local must read as the constant defined most recently at
    // that statement — a var-level map made every `arr[k] = v` in the
    // $VALUES build reference the LAST constant (weixin +2.9k when this
    // regressed values() contents).
    {
        let mut cur: HashMap<u32, usize> = HashMap::default();
        if let Stmt::Block(vs) = &mut body.body {
            for (i, st) in vs.iter_mut().enumerate() {
                if let Some((var, idx)) = def_site.get(&i) {
                    cur.insert(*var, *idx);
                }
                crate::passes::walk_stmt_exprs(st, &mut |e| {
                    crate::passes::deep_rewrite(e, &mut |x| {
                        if let Expr::Local { var, .. } = x {
                            if let Some(&idx) = cur.get(var) {
                                let name: std::sync::Arc<str> =
                                    std::sync::Arc::from(const_name[idx].as_str());
                                *x = Expr::Field {
                                    owner: None,
                                    cls: self_name.clone(),
                                    name,
                                    ty: self_ty.clone(),
                                    is_static: true,
                                };
                            }
                        }
                    });
                });
            }
        }
    }

    // Pass 4: drop the definitions and their sputs.
    if let Stmt::Block(vs) = &mut body.body {
        let drop_set: std::collections::HashSet<usize> =
            drop_stmts.iter().copied().collect();
        vs.retain(|_| true);
        let mut idx = 0usize;
        vs.retain(|_| {
            let keep = !drop_set.contains(&idx);
            idx += 1;
            keep
        });
    }
    // The resolved args consumed the clinit locals' readers; prune the
    // now-dead defs from the remnant (impure inits survive as bare
    // expression statements — drop_dead_locals' standard contract).
    crate::passes::drop_dead_locals(&mut body.body);

    let mut merged: Vec<(i64, EnumConst)> = const_ord
        .into_iter()
        .zip(
            const_field
                .into_iter()
                .zip(const_name)
                .zip(const_extra)
                .map(|((field, name), extra_args)| EnumConst {
                    field,
                    name,
                    extra_args,
                }),
        )
        .collect();
    for (ord, name, extra_args) in synth {
        merged.push((
            ord,
            EnumConst {
                field: name.clone(),
                name,
                extra_args,
            },
        ));
    }
    if merged.is_empty() {
        return dbg_enum_none(line!()); // nothing extracted — keep the desugared form
    }
    merged.sort_by_key(|(o, _)| *o);
    // Ordinals must be UNIQUE and ASCENDING; gaps are legal — R8 drops
    // unused constants wholesale (weixin lite/api/n starts at ordinal
    // 1). Java derives ordinal() from declaration position, so a gap
    // shifts every later constant. Pad each gap with a synthetic
    // constant (`_r<k>`) to keep positions faithful — only when no
    // constant carries extra ctor args (a pad cannot invent them).
    // The synthetic values()/valueOf() the true-enum render suppresses
    // then see the pad (values() drift vs the dex $VALUES array — the
    // compilable-and-ordinal-faithful side of the trade).
    let has_extras = merged.iter().any(|(_, c)| !c.extra_args.is_empty());
    let mut member_taken: jdc_core::FxHashSet<String> = merged
        .iter()
        .map(|(_, c)| c.name.clone())
        .chain(
            class
                .static_fields
                .iter()
                .chain(class.instance_fields.iter())
                .map(|f| f.name.to_string()),
        )
        .chain(class.all_methods().map(|m| m.name.to_string()))
        .collect();
    let mut padded: Vec<EnumConst> = Vec::with_capacity(merged.len());
    let mut expect: i64 = 0;
    let mut pad_k = 0u32;
    for (ord, c) in merged {
        if ord < expect {
            return dbg_enum_none(line!()); // duplicate/colliding ordinals — unfaithful
        }
        if ord > expect {
            // Extras-bearing ctors: a pad may borrow an adjacent
            // constant's literal extras ONLY when every ctor of the
            // class is benign (no branch, no throw, no non-<init>
            // invoke — pure param stores), so the invented value
            // cannot change runtime behavior beyond a dead constant's
            // stored field (jd1.u: gap at ordinal 7 + int extra arg
            // aborted the whole promotion — 92 files of broken
            // Enum.valueOf). Non-benign ctors keep the abort.
            let tmpl: Vec<Expr> = if has_extras {
                if !enum_ctors_benign(pool, class) {
                    return dbg_enum_none(line!()); // cannot synthesize the missing ctor args
                }
                match padded.last() {
                    Some(prev) => prev.extra_args.clone(),
                    None => c.extra_args.clone(),
                }
            } else {
                Vec::new()
            };
            while expect < ord {
                let pname = loop {
                    let cand = format!("_r{pad_k}");
                    pad_k += 1;
                    if !member_taken.contains(&cand) {
                        member_taken.insert(cand.clone());
                        break cand;
                    }
                };
                padded.push(EnumConst {
                    field: pname.clone(),
                    name: pname,
                    extra_args: tmpl.clone(),
                });
                expect += 1;
            }
        }
        expect = ord + 1;
        padded.push(c);
    }
    let out = padded;
    Some((out, body))
}

/// True when every `<init>` of the class is a pure param-store: no
/// branch, no throw, no switch, no goto, and no invoke other than a
/// constructor call (the super delegation). A benign ctor cannot
/// validate or leak its arguments, so gap-pad constants may borrow a
/// neighbor's literal extras without observable effect.
fn enum_ctors_benign(pool: &DexPool, class: &PoolClass) -> bool {
    let mut checked = 0usize;
    for m in class.all_methods() {
        if &*m.name != "<init>" {
            continue;
        }
        checked += 1;
        let Some(dex) = pool.dex(m.dex_idx) else {
            return false;
        };
        let Some(ci) = dex.code_at(m.code_off) else {
            return false;
        };
        for ins in &ci.insns {
            use ddc_dex::insn::InsnKind;
            match ins.kind {
                InsnKind::If { .. }
                | InsnKind::Throw { .. }
                | InsnKind::PackedSwitch { .. }
                | InsnKind::SparseSwitch { .. }
                | InsnKind::Goto { .. } => return false,
                InsnKind::Invoke { method_idx, .. } => {
                    let mr = dex.method(method_idx);
                    if dex.string(mr.name_idx) != "<init>" {
                        return false;
                    }
                }
                _ => {}
            }
        }
    }
    checked > 0
}

/// Update the rolling reaching-def map for enum-arg resolution. Only
/// flat single-target definitions keep the tracking sound; any control
/// flow taints it (a linear scan can no longer prove WHICH definition
/// reaches later capture sites).
fn track_def<'e>(
    st: &'e Stmt,
    defs: &mut std::collections::HashMap<u32, &'e Expr>,
    tainted: &mut bool,
) {
    match st {
        Stmt::LocalDef { var, init, .. } => match init {
            Some(e) => {
                defs.insert(*var, e);
            }
            None => {
                defs.remove(var);
            }
        },
        Stmt::ExprStmt(Expr::Assign { target, value, op, .. }) => {
            if let Expr::Local { var, .. } = &**target {
                if matches!(op, jdc_core::ir::expr::AssignOp::Plain) {
                    defs.insert(*var, value);
                } else {
                    defs.remove(var);
                }
            }
        }
        // Plain expression statements (calls) don't define locals.
        Stmt::ExprStmt(_) => {}
        _ => *tainted = true,
    }
}

/// Resolve every enum-ctor extra arg to a self-contained expression, or
/// reject the enum mode (None). R8 reuses ONE register across all
/// constant constructions — revenuecat's LogIntent builds 11 of 12
/// emoji lists through the same `list` local, reassigned between the
/// `new Self(.., list)` sites — and enum constants render OUTSIDE the
/// clinit where no local is in scope: an unresolved `Local` used to
/// print as the vt-dummy name (`DEBUG(var0)` — per-constant
/// cannot-find). Each Local is replaced by its reaching pure definition
/// (recursively) or, when it names an earlier constant's intermediate,
/// by a static-field reference to that constant. Any expression shape
/// WITHOUT locals passes through untouched (enum args may be arbitrary
/// expressions — BinOp/Cast/Method/New — each renders exactly once per
/// constant, so no duplication concern applies). Only an unresolvable
/// Local (missing/tainted def, depth > 4) rejects the whole enum
/// detection; the class then falls back to plain-field rendering, which
/// always compiles.
fn resolve_enum_extras(
    extras: &[Expr],
    defs: &std::collections::HashMap<u32, &Expr>,
    tainted: bool,
    var_of: &std::collections::HashMap<u32, usize>,
    const_name: &[String],
    self_name: &std::sync::Arc<str>,
    self_ty: &jdc_core::ir::expr::TypeRef,
) -> Option<Vec<Expr>> {
    extras
        .iter()
        .map(|e| {
            let mut out = e.clone();
            let mut fail = false;
            resolve_locals_in(
                &mut out, defs, tainted, var_of, const_name, self_name, self_ty, 0,
                &mut fail,
            );
            if fail {
                None
            } else {
                Some(out)
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn resolve_locals_in(
    e: &mut Expr,
    defs: &std::collections::HashMap<u32, &Expr>,
    tainted: bool,
    var_of: &std::collections::HashMap<u32, usize>,
    const_name: &[String],
    self_name: &std::sync::Arc<str>,
    self_ty: &jdc_core::ir::expr::TypeRef,
    depth: u32,
    fail: &mut bool,
) {
    crate::passes::deep_rewrite(e, &mut |x| {
        if let Expr::Local { var, .. } = x {
            // An earlier constant's intermediate: a static-field read of
            // that constant (declared above — backward reference, legal
            // in enum ctor args).
            if let Some(&ci) = var_of.get(var) {
                if let Some(nm) = const_name.get(ci) {
                    *x = Expr::Field {
                        owner: None,
                        cls: self_name.clone(),
                        name: std::sync::Arc::from(nm.as_str()),
                        ty: self_ty.clone(),
                        is_static: true,
                    };
                    return;
                }
            }
            if tainted || depth > 4 {
                *fail = true;
                return;
            }
            let Some(d) = defs.get(var).copied() else {
                *fail = true;
                return;
            };
            let mut sub = d.clone();
            resolve_locals_in(
                &mut sub, defs, tainted, var_of, const_name, self_name, self_ty,
                depth + 1, fail,
            );
            if *fail {
                return;
            }
            *x = sub;
        }
    });
}

/// Render enum-constant constructor arguments via the shared expression
/// emitter (the VarTable is irrelevant for argument printing — no local
/// names appear — but the API requires one).
fn render_enum_args(ctx: &DexCtx<'_>, pool: &DexPool, args: &[Expr], out: &mut String) {
    let dummy_vt = jdc_core::var::VarTable::default();
    let mut p = Printer::new(ctx, &dummy_vt);
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        p.expr(a, 0, out);
    }
    let _ = pool;
}

/// Synthesize `throw new AbstractMethodError()` stubs for abstract
/// interface methods a CONCRETE class fails to implement. R8 tree-shakes
/// an interface method whose call sites it proved unreachable, leaving a
/// concrete class ART accepts but javac rejects ("X不是抽象的, 并且未覆盖
/// 抽象方法…"). jadx reproduces the same incomplete class; the stub
/// restores compilability AND matches ART semantics exactly (invoking a
/// stripped abstract method throws AbstractMethodError). `provided` is
/// computed from the DEX method table (not the render), so a method ddc
/// merely failed to render is never double-declared, and a same-erasure
/// return-type-specialized sibling (Family B: void `invoke` beside the
/// Object bridge) counts as provided → no colliding stub.
struct ParsedStub {
    ret: JavaType,
    args: Vec<JavaType>,
}

/// Ctor argument signatures to mirror on an empty-shell class whose
/// ancestor chain ends at a Throwable-family FRAMEWORK class — read
/// from the embedded API database (the hand-written 26-name table it
/// replaced could not cover app-specific chains and hard-coded the
/// sig set instead of the real one). Private ctors are skipped: the
/// bridge `super(..)` would not compile against them (Throwable's
/// 4-arg suppression ctor is private).
fn throwable_ctor_sigs(fw: &str) -> Option<Vec<String>> {
    if !crate::fwdb::is_subtype(fw, "java/lang/Throwable") {
        return None;
    }
    let mut out: Vec<String> = Vec::new();
    crate::fwdb::for_each_method(fw, |d, f| {
        if d.starts_with("<init>(")
            && f & crate::fwdb::MF_STATIC == 0
            && f & crate::fwdb::MF_PRIVATE == 0
            && f & crate::fwdb::MF_UNKNOWN == 0
        {
            let hi = d.find(')').unwrap_or(d.len());
            let lo = d.find('(').map(|i| i + 1).unwrap_or(0);
            out.push(d[lo..hi].to_string());
        }
    });
    out.sort();
    out.dedup();
    Some(out)
}

fn synth_missing_interface_stubs(
    pool: &DexPool,
    class: &PoolClass,
    out: &mut String,
    depth: usize,
    emitted_any: &mut bool,
) {
    use crate::access::*;
    // Concrete classes only: interfaces / abstract classes / annotations
    // legitimately leave methods unimplemented.
    if class.access & (ACC_INTERFACE | ACC_ABSTRACT | ACC_ANNOTATION) != 0 {
        return;
    }
    fn argsig(d: &str) -> &str {
        let lo = d.find('(').map(|i| i + 1).unwrap_or(0);
        let hi = d.find(')').unwrap_or(d.len());
        &d[lo..hi]
    }
    // Framework interface method lists come from the embedded API
    // database (fwdb) — see the lookup below.

    // Transitive interface closure — seeded from the WHOLE superclass
    // chain, not just the class's own `implements`: weixin rd extends
    // abstract k0 implements ey2.a, R8 stripped rd's onAttach, and the
    // old own-interfaces-only seed saw an empty requirement (×51
    // weixin a.onAttach family). Pool interfaces contribute their
    // method lists; framework interfaces consult the table above.
    // (name, argsig) -> the unique declaring desc, or None when two
    // ancestors declare the same erasure with DIFFERENT descriptors —
    // a Java-inexpressible diamond where any synthesized stub would
    // itself fail "无法覆盖/无法实现" (weibo +1016 regression when the
    // first-wins desc was stubbed against a conflicting sibling).
    let mut required: jdc_core::FxHashMap<(String, String), Option<(String, String)>> =
        jdc_core::FxHashMap::default();
    let mut defaults: jdc_core::FxHashSet<(String, String)> = jdc_core::FxHashSet::default();
    let mut stack: Vec<String> = Vec::new();
    {
        let mut cur: Option<&PoolClass> = Some(class);
        let mut hops = 0;
        while let Some(c) = cur {
            hops += 1;
            if hops > 64 {
                break;
            }
            stack.extend(c.interfaces.iter().cloned());
            let mut hit_fw = false;
            cur = c.super_name.as_ref().and_then(|s| {
                if s == "java/lang/Object" {
                    None
                } else {
                    let pc = pool.get(s);
                    if pc.is_none() {
                        hit_fw = true;
                    }
                    pc
                }
            });
            if hit_fw {
                // A FRAMEWORK superclass contributes methods the pool
                // walk cannot see: the class may already provide (or be
                // barred from overriding — Context.getString is final)
                // any requirement, and a stub here collided ×669 weibo.
                // No stubs past the framework boundary.
                return;
            }
        }
    }
    let mut seen: jdc_core::FxHashSet<String> = jdc_core::FxHashSet::default();
    while let Some(iname) = stack.pop() {
        if !seen.insert(iname.clone()) {
            continue;
        }
        let Some(ic) = pool.get(&iname) else {
            // Framework interface: the embedded API database provides
            // its abstract instance methods with real access flags
            // (abstract, not static, not default) — the hand-written
            // 13-interface table this replaced covered only classics.
            if crate::fwdb::class_flags(&iname) & crate::fwdb::CF_INTERFACE != 0 {
                crate::fwdb::for_each_method(&iname, |d, f| {
                    if f & crate::fwdb::MF_ABSTRACT == 0
                        || f & crate::fwdb::MF_STATIC != 0
                        || f & crate::fwdb::MF_UNKNOWN != 0
                        || d.starts_with("<init>")
                    {
                        return;
                    }
                    let hi = d.find('(').unwrap_or(d.len());
                    let mn = d[..hi].to_string();
                    let md = d[hi..].to_string();
                    let key = (mn.clone(), argsig(&md).to_string());
                    match required.entry(key) {
                        std::collections::hash_map::Entry::Vacant(v) => {
                            v.insert(Some((mn, md)));
                        }
                        std::collections::hash_map::Entry::Occupied(mut o) => {
                            if o.get().as_ref().is_some_and(|(_, d0)| *d0 != md) {
                                o.insert(None);
                            }
                        }
                    }
                });
            }
            continue;
        };
        for m in ic.all_methods() {
            if m.is_static() || &*m.name == "<clinit>" || &*m.name == "<init>" {
                continue;
            }
            let key = (m.name.to_string(), argsig(&m.desc).to_string());
            if m.code_off == 0 {
                let mdesc = m.desc.to_string();
                match required.entry(key) {
                    std::collections::hash_map::Entry::Vacant(v) => {
                        v.insert(Some((m.name.to_string(), mdesc)));
                    }
                    std::collections::hash_map::Entry::Occupied(mut o) => {
                        if o.get().as_ref().is_some_and(|(_, d)| *d != mdesc) {
                            o.insert(None);
                        }
                    }
                }
            } else {
                defaults.insert(key);
            }
        }
        stack.extend(ic.interfaces.iter().cloned());
    }
    if required.is_empty() {
        return;
    }
    // Methods provided by the class + its concrete superclass chain, plus
    // the java/lang/Object publics that satisfy common requirements.
    let mut provided: jdc_core::FxHashSet<(String, String)> = jdc_core::FxHashSet::default();
    provided.insert(("equals".into(), "Ljava/lang/Object;".into()));
    provided.insert(("hashCode".into(), String::new()));
    provided.insert(("toString".into(), String::new()));
    // (name, desc) pairs whose erasure twin exists elsewhere in the
    // chain with a DIFFERENT desc — stub-hostile shapes (see below).
    let mut chain_sigs: jdc_core::FxHashMap<(String, String), jdc_core::FxHashSet<String>> =
        jdc_core::FxHashMap::default();
    let mut cur: Option<&PoolClass> = Some(class);
    while let Some(c) = cur {
        for m in c.all_methods() {
            if !m.is_static() && &*m.name != "<init>" && &*m.name != "<clinit>" {
                chain_sigs
                    .entry((m.name.to_string(), argsig(&m.desc).to_string()))
                    .or_default()
                    .insert(m.desc.to_string());
            }
        }
        for m in c.all_methods() {
            // A method PROVIDES the implementation if it is not abstract:
            // bytecode (code_off != 0) OR native (JNI body, code_off == 0
            // but ACC_NATIVE — `…ToNative` methods satisfy their interface
            // and must not be stubbed twice).
            if m.access & ACC_ABSTRACT == 0 && !m.is_static() {
                // A RENAMED method no longer provides its raw (name,
                // argsig): impostor overrides (return type satisfying no
                // ancestor declaration) and erasure-clash losers render
                // under their registry display, and the ancestor
                // requirement falls to the stub below — matching ART,
                // where the interface proto never resolved either.
                let renamed = jdc_core::rename::field_display(&c.name, &m.name, &m.desc)
                    .is_some();
                if !renamed {
                    provided.insert((m.name.to_string(), argsig(&m.desc).to_string()));
                }
            }
        }
        cur = c.super_name.as_ref().and_then(|s| {
            if s == "java/lang/Object" {
                None
            } else {
                pool.get(s)
            }
        });
    }
    let mut chain_conflict: jdc_core::FxHashSet<(String, String)> =
        jdc_core::FxHashSet::default();
    for ((name, _argsig), descs) in chain_sigs.iter() {
        if descs.len() > 1 {
            for d in descs {
                chain_conflict.insert((name.clone(), d.clone()));
            }
        }
    }
    let mut missing: Vec<(String, String)> = Vec::new();
    for (key, m) in &required {
        let Some(m) = m else { continue }; // conflicting ancestor descs
        if defaults.contains(key) || provided.contains(key) {
            continue;
        }
        // Kotlin bridge variant (return kotlin/Unit beside the real
        // boolean/object twin a framework interface declares): stubbing
        // it renders `java.lang.Void addAll(Collection)` that neither
        // implements the twin nor serves its callers — the poisoned
        // return type cascades through every call site (weibo +703
        // regression). The honest 未覆盖 error for the twin stays.
        if m.1.ends_with(")Lkotlin/Unit;") {
            continue;
        }
        // A chain method sharing the erasure but NOT the descriptor
        // (abstract super with a different return, an impostor the
        // rename pass left because it satisfies another ancestor):
        // the stub would clash with it — skip; the class keeps the
        // single honest 未覆盖 error instead of gaining a 无法覆盖 one.
        if chain_conflict.contains(&(m.0.clone(), m.1.clone())) {
            continue;
        }
        missing.push(m.clone());
    }
    missing.sort();
    let ind = "    ".repeat(depth + 1);
    for (mname, mdesc) in missing {
        let lo = mdesc.find('(').map(|i| i + 1).unwrap_or(0);
        let hi = mdesc.find(')').unwrap_or(mdesc.len());
        let ret_s = &mdesc[hi + 1..];
        let args_v = split_arg_descs(&mdesc[lo..hi]);
        let d = ParsedStub { ret: crate::desc_type(ret_s), args: args_v };
        if *emitted_any {
            out.push('\n');
        }
        out.push_str(&ind);
        out.push_str("public ");
        out.push_str(&type_name(pool, &d.ret));
        out.push(' ');
        out.push_str(&java_ident(&mname));
        out.push('(');
        for (i, a) in d.args.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&type_name(pool, a));
            out.push_str(&format!(" p{}", i + 1));
        }
        out.push_str(") {\n");
        out.push_str(&ind);
        out.push_str("    throw new AbstractMethodError();\n");
        out.push_str(&ind);
        out.push_str("}\n");
        *emitted_any = true;
    }
}

/// Synthetic storage fields backing name()/ordinal() on a FALLBACK enum
/// (a plain class cannot inherit java.lang.Enum's). The enum ctor's
/// (String, int) trace params feed them (injected in emit_method).
const ENUM_NAME_FIELD: &str = "$ddcName";
const ENUM_ORD_FIELD: &str = "$ddcOrdinal";

/// True when the class renders as a fallback enum BASE: ACC_ENUM,
/// restoration failed (enum_consts None — caller-gated), super is the
/// Enum root. Constant subclasses inherit the synthesized members.
fn fallback_enum_base(class: &PoolClass) -> bool {
    class.is_enum()
        && class
            .super_name
            .as_ref()
            .map(|s| s == "java/lang/Enum" || s == "java/lang/Object")
            .unwrap_or(true)
}

/// Full gate for the synthetic Enum surface: fallback base WITH the
/// trace ctor and no `$ddc*` field-name clash. The ctor injection in
/// emit_method, the member synthesis and the dex-valueOf suppression
/// must all agree, or fields/ctor/renders desynchronize.
fn enum_synth_active(class: &PoolClass) -> bool {
    fallback_enum_base(class)
        && enum_trace_ctor(class)
        && !class
            .static_fields
            .iter()
            .chain(class.instance_fields.iter())
            .any(|f| &*f.name == ENUM_NAME_FIELD || &*f.name == ENUM_ORD_FIELD)
}

/// Does the class have the compiler-generated `(String, int, ..)` enum
/// ctor whose leading params feed the synthetic name/ordinal fields?
fn enum_trace_ctor(class: &PoolClass) -> bool {
    class.all_methods().any(|m| {
        &*m.name == "<init>"
            && m.parsed_desc()
                .map(|d| {
                    matches!(d.args.first(), Some(JavaType::Object(s))
                        if s.as_ref() == "java/lang/String")
                        && matches!(d.args.get(1), Some(JavaType::Int))
                })
                .unwrap_or(false)
    })
}

/// Synthesize the java.lang.Enum member surface a FALLBACK enum base
/// lacks (it renders as a plain class — `extends Enum` is illegal in
/// source): storage fields `$ddcName`/`$ddcOrdinal` (fed by the ctor
/// trace-param injection in emit_method), `name()`, `ordinal()`,
/// `valueOf(String)` and `compareTo`, plus `values()` when R8 stripped
/// the dex copy. Kills the illegal dex `Enum.valueOf(X.class, ..)`
/// ("无法将类 Enum<E>中的方法 valueOf应用到给定类型" — weixin ×1,307,
/// weibo ×477) and makes ordinal() EXACT (the interim values()-scan
/// misread enums whose $VALUES carries register-reuse duplicates,
/// protobuf ub). Purely additive: the dex never declares these members
/// on the enum class (they are Enum inheritances), so no collision —
/// except the dex valueOf, suppressed at the method loop.
fn synth_enum_members(
    pool: &DexPool,
    class: &PoolClass,
    out: &mut String,
    depth: usize,
    emitted_any: &mut bool,
) {
    use crate::access::*;
    if !fallback_enum_base(class) {
        return;
    }
    let self_ty = JavaType::Object(class.name.as_str().into());
    let self_name = type_name(pool, &self_ty);
    let self_arr = JavaType::Array(Box::new(self_ty));
    let arr_name = type_name(pool, &self_arr);
    let traced = enum_synth_active(class);
    let has_values = class.all_methods().any(|m| {
        &*m.name == "values"
            && m.access & ACC_STATIC != 0
            && m.parsed_desc()
                .map(|d| d.ret == self_arr && d.args.is_empty())
                .unwrap_or(false)
    });
    let ind = "    ".repeat(depth + 1);
    let push_line = |out: &mut String, l: &str| {
        out.push_str(&ind);
        out.push_str(l);
        out.push('\n');
    };
    // values(): from the $VALUES array field when the dex copy is gone.
    if !has_values {
        let want = format!("[L{};", class.name);
        if let Some(vf) = class.static_fields.iter().find(|f| &*f.desc == want.as_str()) {
            if *emitted_any {
                out.push('\n');
            }
            *emitted_any = true;
            push_line(out, &format!("public static {} values() {{", arr_name));
            push_line(
                out,
                &format!("    return ({}) {}.clone();", arr_name, java_ident(&vf.name)),
            );
            push_line(out, "}");
        }
    }
    if !traced {
        return; // no (String,int) ctor: fields would stay default — keep
                // the scan-based ordinal only (below) and the dex valueOf.
    }
    if *emitted_any {
        out.push('\n');
    }
    *emitted_any = true;
    push_line(out, &format!("private java.lang.String {};", ENUM_NAME_FIELD));
    push_line(out, &format!("private int {};", ENUM_ORD_FIELD));
    out.push('\n');
    push_line(out, &format!(
        "public java.lang.String name() {{\n{}    return this.{};\n{}}}",
        ind, ENUM_NAME_FIELD, ind
    ));
    out.push('\n');
    push_line(out, &format!(
        "public int ordinal() {{\n{}    return this.{};\n{}}}",
        ind, ENUM_ORD_FIELD, ind
    ));
    out.push('\n');
    push_line(out, &format!(
        "public int compareTo({0} o) {{\n{1}    return this.{2} - o.{2};\n{1}}}",
        self_name, ind, ENUM_ORD_FIELD
    ));
    out.push('\n');
    push_line(out, &format!(
        "public static {0} valueOf(java.lang.String s) {{\n{1}    for ({0} v : values()) {{\n{1}        if (v.name().equals(s)) {{\n{1}            return v;\n{1}        }}\n{1}    }}\n{1}    throw new java.lang.IllegalArgumentException(s);\n{1}}}",
        self_name, ind
    ));
}

/// Scan-based ordinal() fallback for fallback enums WITHOUT the trace
/// ctor (rare): position in values(). Kept separate so synth_enum_members
/// can bail after values() synthesis.
fn synth_enum_ordinal_scan(
    pool: &DexPool,
    class: &PoolClass,
    out: &mut String,
    depth: usize,
    emitted_any: &mut bool,
) {
    if !fallback_enum_base(class) || enum_trace_ctor(class) {
        return;
    }
    if class
        .all_methods()
        .any(|m| &*m.name == "ordinal" && &*m.desc == "()I")
    {
        return;
    }
    let self_arr = JavaType::Array(Box::new(JavaType::Object(class.name.as_str().into())));
    let arr_name = type_name(pool, &self_arr);
    let ind = "    ".repeat(depth + 1);
    if *emitted_any {
        out.push('\n');
    }
    *emitted_any = true;
    out.push_str(&ind);
    out.push_str("public int ordinal() {\n");
    out.push_str(&ind);
    out.push_str(&format!("    {} vs = values();\n", arr_name));
    out.push_str(&ind);
    out.push_str("    for (int i = 0; i < vs.length; i++) {\n");
    out.push_str(&ind);
    out.push_str("        if (vs[i] == this) {\n");
    out.push_str(&ind);
    out.push_str("            return i;\n");
    out.push_str(&ind);
    out.push_str("        }\n");
    out.push_str(&ind);
    out.push_str("    }\n");
    out.push_str(&ind);
    out.push_str("    return 0;\n");
    out.push_str(&ind);
    out.push_str("}\n");
}

/// Strip the Meituan Robust hotpatch guard from a RESTORED enum ctor:
/// `Object[] v3 = {name, ordinal}; Class[] v4 = ..; boolean s =
/// PatchProxy.isSupport(v3, this, changeQuickRedirect, ..); if (s) {
/// PatchProxy.accessDispatch(..); return; }`. The guard reads the enum's
/// own static field — bytecode-legal (Robust instruments after javac),
/// source-illegal (JLS 8.9.2, both simple AND qualified forms). It is
/// dead code for decompiled output (no patch is ever loaded). Removal is
/// surgical: statements containing a PatchProxy call, the locals feeding
/// its arguments, their defs and element writes — a fixpoint over the
/// top-level list. collect_enum_constants' dex gate already refused
/// restoration when self-static reads go beyond the redirect field.
fn strip_robust_enum_ctor_guard(body: &mut Stmt) {
    let is_patchy = |e: &Expr| {
        matches!(e, Expr::Method { cls, name, .. }
            if cls.contains("PatchProxy")
                && (&**name == "isSupport"
                    || &**name == "accessDispatch"
                    || &**name == "proxy"
                    || &**name == "isSupportClinit"
                    || &**name == "accessDispatchClinit"))
    };
    let Stmt::Block(vs) = body else { return };
    if !vs.iter().any(|st| {
        let mut h = false;
        crate::passes::visit_all_exprs(st, &mut |x| {
            if is_patchy(x) {
                h = true;
            }
        });
        h
    }) {
        return;
    }
    let mut dead: jdc_core::FxHashSet<u32> = jdc_core::FxHashSet::default();
    // Locals read inside the PatchProxy call args of a statement (the
    // pack arrays) — dead once the statement goes.
    let patchy_arg_locals = |st: &Stmt, out: &mut jdc_core::FxHashSet<u32>| {
        crate::passes::visit_all_exprs(st, &mut |x| {
            if let Expr::Method { args, .. } = x {
                if is_patchy(x) {
                    for a in args {
                        let mut aa = a.clone();
                        crate::passes::deep_rewrite(&mut aa, &mut |y| {
                            if let Expr::Local { var, .. } = y {
                                out.insert(*var);
                            }
                        });
                    }
                }
            }
        });
    };
    for _ in 0..4 {
        let before = vs.len();
        let mut i = 0usize;
        while i < vs.len() {
            let action = {
                let st = &vs[i];
                match st {
                    Stmt::LocalDef { var, .. } if dead.contains(var) => Some((*var, false)),
                    Stmt::LocalDef { var, init: Some(e), .. } => {
                        let mut has_patchy = false;
                        let mut ec = e.clone();
                        crate::passes::deep_rewrite(&mut ec, &mut |x| {
                            if is_patchy(x) {
                                has_patchy = true;
                            }
                        });
                        if has_patchy {
                            Some((*var, true))
                        } else {
                            None
                        }
                    }
                    Stmt::If { .. } => {
                        let mut h = false;
                        crate::passes::visit_all_exprs(st, &mut |x| {
                            if is_patchy(x) {
                                h = true;
                            }
                        });
                        if h {
                            Some((u32::MAX, true))
                        } else {
                            None
                        }
                    }
                    // `v3[0] = str;` / `v3 = ..;` writes to a dead pack.
                    Stmt::ExprStmt(Expr::Assign { target, .. }) => {
                        let mut tgt_dead = false;
                        let mut tc = target.clone();
                        crate::passes::deep_rewrite(&mut tc, &mut |x| {
                            if let Expr::Local { var, .. } = x {
                                if dead.contains(var) {
                                    tgt_dead = true;
                                }
                            }
                        });
                        if tgt_dead {
                            Some((u32::MAX, false))
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            };
            if let Some((var, collect_args)) = action {
                if collect_args {
                    patchy_arg_locals(&vs[i], &mut dead);
                }
                if var != u32::MAX {
                    dead.insert(var);
                }
                vs.remove(i);
            } else {
                i += 1;
            }
        }
        if vs.len() == before {
            break;
        }
    }
}

#[allow(clippy::only_used_in_recursion)]
fn emit_class_body(
    pool: &DexPool,
    class: &PoolClass,
    ctx: &DexCtx<'_>,
    opts: &ClassOptions,
    out: &mut String,
    depth: usize,
) -> anyhow::Result<()> {
    let ind = indent(depth);
    // The class's own header must use the RENAMED display name — the
    // ctor path (is_init) already renames; a header declaring `class a`
    // while the file/ctor say `a_2` leaves the ctor looking like a
    // method with no return type (MinisApp's Y2.a vs y2.a package
    // case-collision: 2,518 javac parse errors).
    let cname = crate::apply_class_rename(&class.name);
    let (_, simple) = split_name(&cname);
    // A class emitted as its OWN top-level file (depth 0) must declare a
    // flat `$` name — `class Outer.Inner` is not declarable at file
    // scope. An INLINE nested member (depth > 0) declares its own
    // segment inside the parent's body.
    let simple = if depth == 0 {
        simple.to_string()
    } else {
        // R8 names can END in `$`: an empty last `$`-segment would
        // render `class  {` — keep the whole simple name.
        let seg = simple.rsplit('$').next().unwrap_or(&simple);
        if seg.is_empty() {
            simple.to_string()
        } else {
            seg.to_string()
        }
    };
    // A class NAMED `var`-style (taobao ships `tb.var`) cannot be
    // declared — restricted contextual type names escape here and in
    // the ctor, file name, and every type reference.
    let simple = if is_restricted_type_name(&simple) {
        format!("_{simple}")
    } else {
        simple
    };
    let is_iface = class.is_interface();
    let is_enum = class.is_enum();

    let mut head = String::new();
    let a = class.access;
    if a & ACC_PUBLIC != 0 || widen_class(&class.name) {
        head.push_str("public ");
    }
    // Inline nested members need their `static` (interfaces/annotations
    // are implicitly static; a missing `static` on a member class makes
    // every `new Report(...)` site an "outer instance required" error).
    if depth > 0 && !is_iface && ctx.nested_is_static(&class.name) {
        head.push_str("static ");
    }
    if a & ACC_FINAL != 0 && !is_enum {
        head.push_str("final ");
    }
    if a & ACC_ABSTRACT != 0 && !is_iface {
        head.push_str("abstract ");
    }
    if a & ACC_ANNOTATION != 0 {
        head.push('@');
    }
    let mut enum_consts: Option<(Vec<EnumConst>, crate::method::MethodBody)> = if is_enum {
        collect_enum_constants(pool, class, ctx)
    } else {
        None
    };
    if let Some(ecs) = &enum_consts {
        // True `enum` declaration: constants render in the header, the
        // desugared boilerplate (const fields, their <clinit> inits —
        // stripped by strip_enum_const_inits — and the ACC_ENUM flags)
        // disappears. R8-renamed values()/valueOf() stay (they do not
        // collide with the compiler-generated ones); javac-named ones
        // are skipped at the method loop below.
        head.push_str("enum ");
        head.push_str(&sanitize_ref(&simple));
        let _ = ecs;
    } else if is_enum {
        // An enum with constant-specific bodies carries ACC_ABSTRACT —
        // `abstract final` is an illegal modifier combination; abstract
        // (already pushed above) suppresses the hardcoded final.
        let abstract_ = a & ACC_ABSTRACT != 0;
        head.push_str(if abstract_ {
            "/* enum */ class "
        } else {
            "/* enum */ final class "
        });
        head.push_str(&sanitize_ref(&simple));
    } else if is_iface {
        head.push_str("interface ");
        head.push_str(&sanitize_ref(&simple));
    } else {
        head.push_str("class ");
        head.push_str(&sanitize_ref(&simple));
    }
    // The obscured-super import was emitted at the package line; the
    // clause must render the SIMPLE name (the qualified form binds to
    // the class itself even with the import present).
    let render_super = |sup: &String| -> String { print_class_name(pool, sup) };
    if is_iface {
        // A @interface cannot declare extends at all (JLS 9.6): the dex
        // interface table lists java/lang/annotation/Annotation for
        // every annotation type — rendering it produced "对于
        // @interfaces, 不允许 'extends'" (weibo 1100).
        let is_annot = a & ACC_ANNOTATION != 0;
        if !class.interfaces.is_empty() && !is_annot {
            head.push_str(" extends ");
            head.push_str(&join_dotted(pool, &class.interfaces));
        }
    } else {
        if let Some(sup) = &class.super_name {
            // Suppress `extends` for TRUE `enum` declarations (they
            // implicitly extend java.lang.Enum) and for the enum BASE in
            // fallback form (super IS java/lang/Enum — `class X extends
            // Enum` is illegal). But an ENUM CONSTANT SUBCLASS rendered
            // in fallback (`/* enum */ class j$1`, ACC_ENUM, super = the
            // enum base) MUST keep `extends j`: without it the constant
            // loses its subtype relation and every `field = new j$1(..)`
            // fails ("j$1无法转换为j", weibo jsoup TokeniserState ×172).
            let suppress_extends =
                is_enum && (enum_consts.is_some() || sup == "java/lang/Enum");
            if sup != "java/lang/Object" && !suppress_extends {
                head.push_str(" extends ");
                head.push_str(&render_super(sup));
            }
        }
        if !class.interfaces.is_empty() {
            head.push_str(if is_iface {
                " extends "
            } else {
                " implements "
            });
            head.push_str(&join_dotted(pool, &class.interfaces));
        }
    }

    out.push_str(&ind);
    out.push_str(&head);
    out.push_str(" {\n");

    // True-enum constant list: the constants lead the body, ahead of
    // any remaining fields.
    if let Some((ecs, _)) = &enum_consts {
        for (i, ec) in ecs.iter().enumerate() {
            out.push_str(&indent(depth + 1));
            out.push_str(&java_ident(&ec.name));
            if !ec.extra_args.is_empty() {
                let mut line = String::from("(");
                render_enum_args(ctx, pool, &ec.extra_args, &mut line);
                line.push(')');
                out.push_str(&line);
            }
            if i + 1 < ecs.len() {
                out.push_str(",\n");
            } else {
                out.push_str(";\n");
            }
        }
        out.push('\n');
    }

    // Fields.
    // A static final whose dex-encoded static value is OVERWRITTEN by
    // the clinit must render as a blank final: the dex encoded array
    // value is just the class-load default (ART lets <clinit> reassign
    // static finals), but Java forbids touching an initialized final
    // ("无法为 static final 变量 $stable 分配值" — Compose `$stable = 0`
    // + clinit `= 8`; WalletBaseUI HARDCODE_* = 0 + clinit computed
    // value; the original null-valued shape: Kotlin `object` INSTANCE,
    // okio SegmentPool). Scan the clinit's raw SPuts once.
    let clinit_sputs: Option<jdc_core::FxHashSet<(std::sync::Arc<str>, std::sync::Arc<str>)>> =
        // Gate: only classes with a VALUED final static can hit the
        // conflict — skip the clinit decode otherwise (decoding it for
        // every class cost seconds on 98k-class corpora).
        (|| {
            let has_valued_final = class.static_fields.iter().enumerate().any(|(i, f)| {
                f.access & crate::access::ACC_FINAL != 0
                    && class.static_values.get(i).is_some()
            });
            if !has_valued_final {
                return None;
            }
            let m = class.all_methods().find(|m| &*m.name == "<clinit>")?;
            let dex = pool.dex(m.dex_idx)?;
            let ci = dex.code_at(m.code_off)?;
            let mut set: jdc_core::FxHashSet<(
                std::sync::Arc<str>,
                std::sync::Arc<str>,
            )> = jdc_core::FxHashSet::default();
            for ins in &ci.insns {
                if let InsnKind::SPut { field_idx, .. } = ins.kind {
                    let fid = dex.field(field_idx);
                    // clinit only writes own-class fields here; match
                    // (name, type) against the field list below.
                    set.insert((
                        std::sync::Arc::from(dex.string(fid.name_idx)),
                        std::sync::Arc::from(dex.type_name(fid.type_idx)),
                    ));
                }
            }
            Some(set)
        })();
    let mut field_emitted = false;
    for (i, f) in class.static_fields.iter().enumerate() {
        // Enum constant fields became the header list above.
        if enum_consts.is_some() && f.access & crate::access::ACC_ENUM != 0 {
            continue;
        }
        if field_emitted || !class.instance_fields.is_empty() {
            out.push('\n');
        }
        field_emitted = true;
        // A blank-final: static final, clinit-assigned. Interfaces are
        // excluded — their fields are implicitly `public static final`
        // and MUST carry an initializer.
        let blank_final = f.access & crate::access::ACC_FINAL != 0
            && !class.is_interface()
            && clinit_sputs
                .as_ref()
                .is_some_and(|s| {
                    s.contains(&(
                        f.name.clone(),
                        f.desc.clone(),
                    ))
                });
        emit_field(
            pool,
            f,
            if blank_final {
                None
            } else {
                class.static_values.get(i)
            },
            &class.name,
            out,
            depth + 1,
            true,
            class.is_interface(),
        );
    }
    if !class.instance_fields.is_empty() && !class.static_fields.is_empty() {
        out.push('\n');
    }
    for (i, f) in class.instance_fields.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        emit_field(pool, f, None, &class.name, out, depth + 1, false, false);
    }

    // Methods.
    // Duplicated (name, parameter-types) pairs in one class are javac
    // "method is already defined" errors (weibo: 11.5k hits). Source
    // cannot declare them, so exactly one survives: covariant bridges
    // (`Object get(int)` beside `ByteString get(int)` — the compiler
    // GENERATES bridges), plus R8 output that lost the bridge flag
    // (okio Buffer.clone, interface getView re-declarations). A
    // non-bridge method outranks a bridge for the same signature
    // (bridge bodies are delegation stubs); first occurrence otherwise.
    // Never drop a signature outright — the last method standing for a
    // key is always rendered.
    // Keyed on the DISPLAY name: a member-renamed method (return-type-
    // only collisions on isolated classes) no longer collides with its
    // sibling — both render, each caller resolving through the registry.
    fn sig_key<'m>(cls: &str, renames_on: bool, m: &'m PoolMethod) -> (&'m str, &'m str) {
        let d: &'m str = &m.desc;
        let lo = d.find('(').map(|i| i + 1).unwrap_or(0);
        let hi = d.find(')').unwrap_or(d.len());
        let n = if renames_on {
            jdc_core::rename::field_display(cls, &m.name, d).unwrap_or(&*m.name)
        } else {
            &*m.name
        };
        (n, &d[lo..hi])
    }
    let renames_on = jdc_core::rename::member_rename_active();
    let suppress_dex_valueof = class.is_enum()
        && enum_consts.is_none()
        && enum_synth_active(class);
    let methods: Vec<&PoolMethod> = class.all_methods().collect();
    let mut claim: jdc_core::FxHashMap<(&str, &str), usize> = jdc_core::FxHashMap::default();
    for (i, m) in methods.iter().enumerate() {
        let key = sig_key(&class.name, renames_on, m);
        match claim.get(&key) {
            Some(&j)
                if m.access & crate::access::ACC_BRIDGE == 0
                    && methods[j].access & crate::access::ACC_BRIDGE != 0 =>
            {
                claim.insert(key, i);
            }
            None => {
                claim.insert(key, i);
            }
            _ => {}
        }
    }
    // NOTE: a same-signature bridge is already retired by the claim map
    // above. An ERASURE-shaped bridge (SAM/variance: params are the
    // Object-erased types of a sibling's) must NOT be skipped: with raw
    // (non-generic) interface rendering the bridge is exactly what
    // satisfies the interface — skipping it turned every Kotlin lambda
    // class into "不是抽象的, 并且未覆盖…invoke(Object,Object)" (reqable
    // +17), while the "对invoke的引用不明确" it appeared to fix was a
    // missing-kotlin-classpath cascade, not a ddc bug.
    let mut emitted_any = !class.static_fields.is_empty() || !class.instance_fields.is_empty();
    for (i, m) in methods.iter().enumerate() {
        if &*m.name == "<clinit>" {
            continue; // rendered after the fields
        }
        if claim.get(&sig_key(&class.name, renames_on, m)) != Some(&i) {
            continue;
        }
        if bridge_shadowed_by_ancestor(pool, class, m) {
            continue;
        }
        // Fallback enum base: the dex valueOf body is
        // `Enum.valueOf(X.class, str)` — illegal once X is a plain class
        // (T is not bounded by Enum). Suppress it; synth_enum_members
        // renders the values()-scan replacement.
        if suppress_dex_valueof
            && m.is_static()
            && &*m.name == "valueOf"
            && m.parsed_desc().map(|d| {
                d.args.len() == 1
                    && matches!(d.args[0], JavaType::Object(ref sn)
                        if sn.as_ref() == "java/lang/String")
                    && d.ret == JavaType::Object(class.name.as_str().into())
            }).unwrap_or(false)
        {
            continue;
        }
        // True-enum rendering: javac auto-generates values()/valueOf()
        // — the original-named ones must not re-declare (R8-renamed
        // copies stay, they do not collide).
        if let Some((ecs, _)) = &enum_consts {
            let d = m.parsed_desc();
            let self_arr = d.as_ref().map(|d| d.ret == JavaType::Array(Box::new(JavaType::Object(class.name.as_str().into()))));
            let self_ret = d.as_ref().map(|d| d.ret == JavaType::Object(class.name.as_str().into()));
            let no_args = d.as_ref().map(|d| d.args.is_empty()).unwrap_or(false);
            let one_str = d
                .as_ref()
                .map(|d| d.args.len() == 1 && matches!(d.args[0], JavaType::Object(ref n) if n.as_ref() == "java/lang/String"))
                .unwrap_or(false);
            let static_ = m.is_static();
            if static_ && no_args && self_arr == Some(true) && &*m.name == "values" {
                continue;
            }
            if static_ && one_str && self_ret == Some(true) && &*m.name == "valueOf" {
                continue;
            }
            // Enum constructors must be private in source form.
            let m_owned: Option<PoolMethod> = if &*m.name == "<init>" {
                let mut c = (*m).clone();
                c.access = (c.access & !(crate::access::ACC_PUBLIC | crate::access::ACC_PROTECTED | crate::access::ACC_PRIVATE)) | crate::access::ACC_PRIVATE;
                Some(c)
            } else {
                None
            };
            let m_ref: &PoolMethod = m_owned.as_ref().unwrap_or(m);
            let mark = out.len();
            if emitted_any {
                out.push('\n');
            }
            if emit_method(pool, class, ctx, m_ref, depth + 1, true, out)? {
                emitted_any = true;
            } else {
                out.truncate(mark);
            }
            let _ = ecs;
            continue;
        }
        let mark = out.len();
        if emitted_any {
            out.push('\n');
        }
        if emit_method(pool, class, ctx, m, depth + 1, enum_consts.is_some(), out)? {
            emitted_any = true;
        } else {
            out.truncate(mark);
        }
    }
    // Inherited-ctor bridges: dex method refs resolve through the
    // hierarchy, so `new C(args)` against a class C with NO declared
    // <init> legally targets the SUPERCLASS ctor (weixin tenpay
    // `new m(map)` — m declares nothing, i.<init>(HashMap) does). Java
    // has no inherited constructors: without a bridge, C's implicit
    // default ctor is the only one and every arg-carrying construction
    // fails ("无法将类 m中的构造器 m应用到给定类型; 需要: 没有参数").
    // Mirror the nearest ctor-declaring ancestor's public/protected
    // ctors as thin `super(..)` delegations.
    if !class.is_interface()
        && enum_consts.is_none()
        && !class.all_methods().any(|m| &*m.name == "<init>")
    {
        if let Some(mut sup) = class.super_name.clone() {
            loop {
                if sup == "java/lang/Object" {
                    break;
                }
                let Some(sc) = pool.get(&sup) else { break };
                let ctors: Vec<&PoolMethod> = sc
                    .all_methods()
                    .filter(|m| &*m.name == "<init>")
                    .collect();
                if !ctors.is_empty() {
                    for sm in ctors {
                        if sm.access & crate::access::ACC_PRIVATE != 0
                            || (sm.access
                                & (crate::access::ACC_PUBLIC | crate::access::ACC_PROTECTED)
                                == 0)
                        {
                            continue; // private/package-private: no legal bridge
                        }
                        let Some(d) = sm.parsed_desc() else { continue };
                        let mods = if sm.access & crate::access::ACC_PUBLIC != 0 {
                            "public "
                        } else {
                            "protected "
                        };
                        if emitted_any {
                            out.push('\n');
                        }
                        out.push_str(&format!("    {}", "    ".repeat(depth)));
                        out.push_str(mods);
                        out.push_str(&sanitize_ref(&simple));
                        out.push('(');
                        let mut names = Vec::with_capacity(d.args.len());
                        for (i, a) in d.args.iter().enumerate() {
                            if i > 0 {
                                out.push_str(", ");
                            }
                            out.push_str(&type_name(pool, a));
                            let nm = format!("p{}", i + 1);
                            out.push(' ');
                            out.push_str(&nm);
                            names.push(nm);
                        }
                        out.push_str(") {\n");
                        out.push_str(&format!("    {}    super({});\n", "    ".repeat(depth), names.join(", ")));
                        out.push_str(&format!("    {}}}\n", "    ".repeat(depth)));
                        emitted_any = true;
                    }
                    break;
                }
                match &sc.super_name {
                    Some(n) => sup = n.clone(),
                    None => break,
                }
            }
        }
    }

    // Referenced-but-undeclared inherited-ctor bridges. The block above
    // covers a class with NO declared ctor; this covers a class that
    // DECLARES ctors but is missing one that dex callers reach THROUGH
    // it. Dex resolves a `<init>` method-ref up the superclass chain, so
    // a subclass `super(2, recv, owner, name, sig, flags)` may target
    // `m.<init>(int,Object,Class,String,String,int)` that `m`
    // (FunctionReferenceImpl) does NOT declare — its super `l` does.
    // Java has no inherited ctors, so without a bridge on `m` every such
    // subclass fails ("无法将类 m中的构造器 m应用到给定类型"). Mirror each
    // referenced sig C lacks from the nearest ancestor declaring it, as a
    // thin `super(..)` delegation. Ref-gated (only sigs actually called
    // through C) so ordinary classes gain nothing.
    if !class.is_interface()
        && enum_consts.is_none()
        && class.all_methods().any(|m| &*m.name == "<init>")
    {
        if let Some(refs) = pool.ctor_ref_sigs(&class.name) {
            fn arg_sig(d: &str) -> &str {
                let lo = d.find('(').map(|i| i + 1).unwrap_or(0);
                let hi = d.find(')').unwrap_or(d.len());
                &d[lo..hi]
            }
            let declared: jdc_core::FxHashSet<&str> = class
                .all_methods()
                .filter(|m| &*m.name == "<init>")
                .map(|m| arg_sig(&m.desc))
                .collect();
            let mut missing: Vec<&String> = refs
                .iter()
                .filter(|s| !declared.contains(s.as_str()))
                .collect();
            missing.sort();
            for sig in missing {
                // Nearest ancestor declaring a public/protected <init>
                // with this exact arg signature.
                let mut sup = class.super_name.clone();
                let mut found: Option<&PoolMethod> = None;
                while let Some(s) = sup {
                    if s == "java/lang/Object" {
                        break;
                    }
                    let Some(sc) = pool.get(&s) else { break };
                    if let Some(sm) = sc.all_methods().find(|m| {
                        &*m.name == "<init>"
                            && arg_sig(&m.desc) == sig.as_str()
                            && m.access & crate::access::ACC_PRIVATE == 0
                            && m.access
                                & (crate::access::ACC_PUBLIC | crate::access::ACC_PROTECTED)
                                != 0
                    }) {
                        found = Some(sm);
                        break;
                    }
                    sup = sc.super_name.clone();
                }
                let Some(sm) = found else {
                    // The walk terminated at java/lang/Object — which
                    // always declares <init>()V, so an empty-sig ref
                    // through this class resolved there dex-wise. The
                    // Java implicit default ctor is gone (the class
                    // declares other ctors); mirror it as an explicit
                    // public bridge (AgentMenuTask$Request →
                    // AppBrandProxyUIProcessTask$ProcessRequest:
                    // "需要: Parcel 找到: 没有参数", weixin 205).
                    if sig.is_empty() {
                        if emitted_any {
                            out.push('\n');
                        }
                        out.push_str(&format!("    {}", "    ".repeat(depth)));
                        out.push_str("public ");
                        out.push_str(&sanitize_ref(&simple));
                        out.push_str("() {\n");
                        out.push_str(&format!(
                            "    {}    super();\n",
                            "    ".repeat(depth)
                        ));
                        out.push_str(&format!("    {}}}\n", "    ".repeat(depth)));
                        emitted_any = true;
                    }
                    continue;
                };
                let Some(d) = sm.parsed_desc() else { continue };
                let mods = if sm.access & crate::access::ACC_PUBLIC != 0 {
                    "public "
                } else {
                    "protected "
                };
                if emitted_any {
                    out.push('\n');
                }
                out.push_str(&format!("    {}", "    ".repeat(depth)));
                out.push_str(mods);
                out.push_str(&sanitize_ref(&simple));
                out.push('(');
                let mut names = Vec::with_capacity(d.args.len());
                for (i, a) in d.args.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&type_name(pool, a));
                    let nm = format!("p{}", i + 1);
                    out.push(' ');
                    out.push_str(&nm);
                    names.push(nm);
                }
                out.push_str(") {\n");
                out.push_str(&format!(
                    "    {}    super({});\n",
                    "    ".repeat(depth),
                    names.join(", ")
                ));
                out.push_str(&format!("    {}}}\n", "    ".repeat(depth)));
                emitted_any = true;
            }
        }
    }

    // Framework-ancestor ctor bridge. The two blocks above mirror POOL
    // ancestors; both stop at a framework (off-pool) superclass. A class
    // whose DIRECT superclass is a framework type (in android.jar) that
    // declares a ctor the class lacks also needs a bridge — `super(args)`
    // resolves against the jar. Only the DIRECT-super-is-framework case is
    // safe: an intermediate pool ancestor with no matching ctor would
    // break the `super()` chain (that ancestor needs its own bridge, which
    // its own refs may not trigger). Covers weixin g3 extends
    // org.xml.sax.SAXException (`new g3(String)` → SAXException(String)),
    // the custom-exception / Parcelable family the pool walk can't reach.
    if !class.is_interface() && enum_consts.is_none() {
        let direct_fw = class
            .super_name
            .as_ref()
            .map(|s| s != "java/lang/Object" && pool.get(s).is_none())
            .unwrap_or(false);
        if direct_fw {
            if let Some(refs) = pool.ctor_ref_sigs(&class.name) {
                fn arg_sig(d: &str) -> &str {
                    let lo = d.find('(').map(|i| i + 1).unwrap_or(0);
                    let hi = d.find(')').unwrap_or(d.len());
                    &d[lo..hi]
                }
                let declared: jdc_core::FxHashSet<&str> = class
                    .all_methods()
                    .filter(|m| &*m.name == "<init>")
                    .map(|m| arg_sig(&m.desc))
                    .collect();
                let mut missing: Vec<&String> = refs
                    .iter()
                    .filter(|s| !declared.contains(s.as_str()))
                    .collect();
                missing.sort();
                for sig in missing {
                    // Never bridge the empty sig here: a no-ctor class
                    // gets Java's implicit default, and a class WITH
                    // ctors is covered by the referenced-but-undeclared
                    // block's Object-walk tail (emitting in both was
                    // `已在类 b0中定义了构造器 b0()` ×6 weixin).
                    if sig.is_empty() {
                        continue;
                    }
                    let args = split_arg_descs(sig);
                    if emitted_any {
                        out.push('\n');
                    }
                    out.push_str(&format!("    {}", "    ".repeat(depth)));
                    out.push_str("public ");
                    out.push_str(&sanitize_ref(&simple));
                    out.push('(');
                    let mut names = Vec::with_capacity(args.len());
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        out.push_str(&type_name(pool, a));
                        let nm = format!("p{}", i + 1);
                        out.push(' ');
                        out.push_str(&nm);
                        names.push(nm);
                    }
                    out.push_str(") {\n");
                    out.push_str(&format!(
                        "    {}    super({});\n",
                        "    ".repeat(depth),
                        names.join(", ")
                    ));
                    out.push_str(&format!("    {}}}\n", "    ".repeat(depth)));
                    emitted_any = true;
                }
            }
        }
    }

    // Throwable-family ctor bridge for empty-shell exception classes
    // (see throwable_ctor_sigs). Walks the pool chain to the nearest
    // FRAMEWORK ancestor, stopping at any pool ancestor that DECLARES
    // ctors (the mirror block above covers that shape) — so chains of
    // empty shells (C extends P extends Error) each get the bridges and
    // every super(..) resolves against its parent's bridge.
    if !class.is_interface()
        && enum_consts.is_none()
        && !class.all_methods().any(|m| &*m.name == "<init>")
    {
        let mut fw_anc: Option<&str> = None;
        if let Some(sup) = class.super_name.as_deref() {
            let mut cur = sup;
            let mut hops = 0;
            loop {
                hops += 1;
                if hops > 32 || cur == "java/lang/Object" {
                    break;
                }
                match pool.get(cur) {
                    Some(sc) => {
                        if sc.all_methods().any(|m| &*m.name == "<init>") {
                            break;
                        }
                        match sc.super_name.as_deref() {
                            Some(n) => cur = n,
                            None => break,
                        }
                    }
                    None => {
                        fw_anc = Some(cur);
                        break;
                    }
                }
            }
        }
        if let Some(sigs) = fw_anc.and_then(throwable_ctor_sigs) {
            // The framework-ancestor bridge above already emitted every
            // sig the dex method-ref table attributes to this class —
            // emitting the table copy too was `已在类 z中定义了构造器
            // z(String)` (+60 weixin dup-def). The empty sig is exempt
            // there, so the table keeps it.
            let ref_sigs = pool.ctor_ref_sigs(&class.name);
            for sig in &sigs {
                if !sig.is_empty()
                    && ref_sigs.is_some_and(|rs| rs.contains(sig))
                {
                    continue;
                }
                let args = split_arg_descs(sig);
                if emitted_any {
                    out.push('\n');
                }
                out.push_str(&format!("    {}", "    ".repeat(depth)));
                out.push_str("public ");
                out.push_str(&sanitize_ref(&simple));
                out.push('(');
                let mut names = Vec::with_capacity(args.len());
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&type_name(pool, a));
                    let nm = format!("p{}", i + 1);
                    out.push(' ');
                    out.push_str(&nm);
                    names.push(nm);
                }
                out.push_str(") {\n");
                out.push_str(&format!(
                    "    {}    super({});\n",
                    "    ".repeat(depth),
                    names.join(", ")
                ));
                out.push_str(&format!("    {}}}\n", "    ".repeat(depth)));
                emitted_any = true;
            }
        }
    }

    // Static resolver helpers extracted from branched-delegation ctors
    // (passes::extract_branched_delegation_helper) — the computation the
    // Java first-statement rule cannot host inline.
    for h in crate::passes::take_ctor_helpers(&class.name) {
        if emitted_any {
            out.push('\n');
        }
        out.push_str(&indent(depth + 1));
        out.push_str("private static ");
        out.push_str(&type_name(pool, &h.ret));
        out.push(' ');
        out.push_str(&h.name);
        out.push('(');
        for (i, (ty, nm)) in h.params.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&type_name(pool, ty));
            out.push(' ');
            out.push_str(&java_ident(nm));
        }
        out.push_str(") {\n");
        let p = Printer::new(ctx, &h.vt);
        let text = p.with_indent(depth + 2).into_string(&h.body);
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            out.push_str(line);
            out.push('\n');
        }
        out.push_str(&indent(depth + 1));
        out.push_str("}\n");
        emitted_any = true;
    }

    // Synthetic relay ctors for FULL-mode branched delegations
    // (passes::extract_branched_delegation_helper): the public ctor
    // renders `this(resolve$X(params))`; this unpacks the carrier
    // array into the one legal first-statement delegation. Primitives
    // unbox through their wrappers (the array boxes them on pack).
    // ONE relay per class: the (Object[]) erasure is class-unique. Two
    // Full-mode ctors in one class (rare; each worker rewrites its own
    // body independently) resolve deterministically — sorted by origin
    // desc, the least emits and the others keep the honest
    // ctor-missing error. No duplicate-declaration risk.
    for (ri, (_desc, r)) in crate::passes::take_ctor_relays(&class.name)
        .into_iter()
        .enumerate()
    {
        if ri > 0 {
            continue;
        }
        if emitted_any {
            out.push('\n');
        }
        // Same depth rule as emit_class_body's header: a depth-0 class
        // declares its FLAT `$` name (the ctor must match it — weixin
        // ModularizingPkgRetrieverContract$SharedIPCCallArgs rendered a
        // tail-named relay ctor: "方法声明无效" parse-abort); an INLINE
        // nested member declares its last segment.
        let disp = crate::apply_class_rename(&class.name);
        let (_, simple) = split_name(&disp);
        let ctor_name = if depth == 0 {
            simple.clone()
        } else {
            let seg = simple.rsplit('$').next().unwrap_or("");
            if seg.is_empty() {
                simple.clone()
            } else {
                seg.to_string()
            }
        };
        out.push_str(&indent(depth + 1));
        out.push_str("private ");
        out.push_str(&java_ident(&ctor_name));
        out.push_str("(Object[] h$relay) {\n");
        out.push_str(&indent(depth + 2));
        out.push_str(if r.is_super { "super(" } else { "this(" });
        for (i, t) in r.formals.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let slot = format!("h$relay[{}]", i);
            match t {
                JavaType::Boolean => out.push_str(&format!("((Boolean) {}).booleanValue()", slot)),
                JavaType::Byte => out.push_str(&format!("((Byte) {}).byteValue()", slot)),
                JavaType::Char => out.push_str(&format!("((Character) {}).charValue()", slot)),
                JavaType::Short => out.push_str(&format!("((Short) {}).shortValue()", slot)),
                JavaType::Int => out.push_str(&format!("((Integer) {}).intValue()", slot)),
                JavaType::Float => out.push_str(&format!("((Float) {}).floatValue()", slot)),
                JavaType::Long => out.push_str(&format!("((Long) {}).longValue()", slot)),
                JavaType::Double => out.push_str(&format!("((Double) {}).doubleValue()", slot)),
                other => {
                    out.push('(');
                    out.push_str(&type_name(pool, other));
                    out.push_str(") ");
                    out.push_str(&slot);
                }
            }
        }
        out.push_str(");\n");
        out.push_str(&indent(depth + 1));
        out.push_str("}\n");
        emitted_any = true;
    }

    // Missing-abstract-method stubs (R8 tree-shook an interface method a
    // concrete class no longer implements; javac rejects the incomplete
    // class). Synthesized last so they sit after the real members.
    synth_missing_interface_stubs(pool, class, out, depth, &mut emitted_any);

    // Fallback enum base: recover the Enum member surface the
    // plain-class rendering lacks (name/ordinal/valueOf/compareTo +
    // synthetic storage fields fed by the ctor injection).
    if class.is_enum() && enum_consts.is_none() {
        synth_enum_members(pool, class, out, depth, &mut emitted_any);
        synth_enum_ordinal_scan(pool, class, out, depth, &mut emitted_any);
    }

    // Static initializer. INTERFACES cannot carry a `static { }` block in
    // Java — their clinit only assigns constants, which static_values (or
    // the `= null` default) already render as field initializers; skip
    // the block entirely.
    let skip_clinit = class.is_interface();
    if let Some((_, clinit_body)) = enum_consts.take() {
        // True-enum <clinit>: the constant assignments are already gone;
        // render the remainder (the $VALUES array build) directly.
        // Skip an empty remainder (all statements were constant inits).
        let empty = match &clinit_body.body {
            Stmt::Block(v) => v.iter().all(|s| matches!(s, Stmt::Block(b) if b.is_empty())),
            _ => false,
        };
        if !empty {
            if emitted_any {
                out.push('\n');
            }
            out.push_str(&indent(depth + 1));
            out.push_str("static {\n");
            let p = Printer::new(ctx, &clinit_body.vt);
            let text = p.with_indent(depth + 2).into_string(&clinit_body.body);
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                out.push_str(line);
                out.push('\n');
            }
            out.push_str(&indent(depth + 1));
            out.push_str("}\n");
            emitted_any = true;
        }
    } else if let Some(clinit) = (!skip_clinit)
        .then(|| class.all_methods().find(|m| &*m.name == "<clinit>"))
        .flatten()
    {
        let mark = out.len();
        if emitted_any {
            out.push('\n');
        }
        if emit_method(pool, class, ctx, clinit, depth + 1, false, out)? {
            emitted_any = true;
        } else {
            out.truncate(mark);
        }
    }

    // Nested member classes (clean `$` tails only; anonymous/local/lambda
    // classes are emitted as their own top-level files by the driver).
    for nested in nested_members(pool, class, ctx) {
        if emitted_any {
            out.push('\n');
        }
        let nested_ctx = DexCtx::new(pool, nested);
        out.push('\n');
        emit_class_body(pool, nested, &nested_ctx, opts, out, depth + 1)?;
        emitted_any = true;
    }

    out.push_str(&ind);
    out.push_str("}\n");
    Ok(())
}

/// Member classes of `class` (direct children by the `$` chain / member
/// annotations), excluding anonymous / local / lambda shapes.
fn nested_members<'a>(
    pool: &'a DexPool,
    class: &'a PoolClass,
    ctx: &DexCtx<'_>,
) -> Vec<&'a PoolClass> {
    let mut out = Vec::new();
    for name in pool.children_of(&class.name) {
        let Some(pc) = pool.get(name) else { continue };
        // Only a REAL `outer$tail` name is an inline member: the
        // children index also carries annotation-derived outers
        // (EnclosingClass) with no naming relationship to the child
        // (obfuscated apps pair a 1-char name with a long enclosing
        // descriptor) — the blind slice panicked there (alipay
        // `a.a.a.a.c`, exposed once children_of actually returned data).
        let Some(rest) = name
            .strip_prefix(class.name.as_str())
            .and_then(|t| t.strip_prefix('$'))
        else {
            continue;
        };
        // The SAME predicate top_level_classes uses for the standalone
        // decision: a non-clean member tail ($ExternalSynthetic flat
        // units, hyphen markers, digit-led anonymous/local shapes) is
        // emitted as its own file — inlining it here too DOUBLE-EMITS
        // the class, and the nested copy renders corrupt (weibo:
        // 12,152 nested `ExternalSyntheticLambdaN` chimeras across
        // 4,316 files; Recorder's nested copy mixed a sibling's
        // captures — `int f$0` + Builder.setSource — 无法取消引用int).
        if !crate::clean_member_tail(rest) {
            continue;
        }
        if ctx.find_outer(name).as_deref() != Some(class.name.as_str()) {
            continue;
        }
        out.push(pc);
    }
    out
}

// Eight parameters are all load-bearing (pool, field, static value,
// owner class, output, depth, staticness, interface-init requirement);
// bundling them into a struct would obscure each call site.
#[allow(clippy::too_many_arguments)]
fn emit_field(
    pool: &DexPool,
    f: &crate::PoolField,
    init: Option<&StaticValue>,
    f_class: &str,
    out: &mut String,
    depth: usize,
    is_static: bool,
    require_init: bool,
) {
    let ind = indent(depth);
    let mut line = String::new();
    let a = f.access;
    if a & ACC_PUBLIC != 0 || widen_field(f_class, &f.name) {
        line.push_str("public ");
    } else if a & ACC_PRIVATE != 0 {
        line.push_str("private ");
    } else if a & ACC_PROTECTED != 0 {
        line.push_str("protected ");
    }
    if is_static {
        line.push_str("static ");
    }
    if a & ACC_FINAL != 0 && !unfinal_field(f_class, &f.name) {
        line.push_str("final ");
    }
    if a & ACC_SYNTHETIC != 0 {
        line.push_str("/* synthetic */ ");
    }
    if a & ACC_TRANSIENT_HINT != 0 {
        line.push_str("transient ");
    }
    if a & ACC_VOLATILE_HINT != 0 {
        line.push_str("volatile ");
    }
    let ty = type_name(pool, &desc_type(&f.desc));
    line.push_str(&ty);
    line.push(' ');
    let fname = jdc_core::rename::field_display(f_class, &f.name, &f.desc).unwrap_or(&*f.name);
    // javac reserves `$assertionsDisabled`; the emit side prints every
    // REFERENCE under the private alias ASSERT_FIELD, so the
    // DECLARATION must carry the same alias or the field is 找不到符号
    // at each `if (!$jcdcAssertionsDisabled)` guard (QQ jbox2d Vec2
    // ×166).
    let fname: std::borrow::Cow<str> = if fname == "$assertionsDisabled" {
        std::borrow::Cow::Borrowed(jdc_core::analysis::ASSERT_FIELD)
    } else {
        std::borrow::Cow::Borrowed(fname)
    };
    line.push_str(&java_ident(&fname));
    let mut rendered = None;
    if let Some(v) = init {
        rendered = render_static_value(pool, v, f_class);
    }
    if rendered.is_none() && require_init {
        // Interface fields MUST have an initializer in Java; the dex may
        // not carry a static_values entry for a compile-time-constant the
        // compiler folded away. Keep it compilable.
        let default = match desc_type(&f.desc) {
            JavaType::Boolean => "false",
            JavaType::Byte | JavaType::Short | JavaType::Char | JavaType::Int => "0",
            JavaType::Long => "0L",
            JavaType::Float => "0.0F",
            JavaType::Double => "0.0",
            _ => "null",
        };
        rendered = Some(default.to_string());
    }
    if let Some(text) = rendered {
        // A long field holding an `Int(i64)` static value needs the `L`
        // suffix: without it the literal is an int and overflows
        // (`long d = -6343169151696340687` failed javac).
        let text = if matches!(desc_type(&f.desc), JavaType::Long)
            && text.chars().all(|c| c.is_ascii_digit() || c == '-')
        {
            format!("{text}L")
        } else {
            text
        };
        line.push_str(" = ");
        line.push_str(&text);
    }
    line.push(';');
    out.push_str(&ind);
    out.push_str(&line);
    out.push('\n');
}

// DEX field access_flags bits 0x40/0x80 are bridge/varargs for METHODS and
// volatile(0x40)/transient(0x80) for fields.
pub const ACC_VOLATILE_HINT: u32 = 0x40;
pub const ACC_TRANSIENT_HINT: u32 = 0x80;

fn render_static_value(pool: &DexPool, v: &StaticValue, owner: &str) -> Option<String> {
    Some(match v {
        StaticValue::Int(i) => i.to_string(),
        StaticValue::Float(f) => format_float(*f as f64, true),
        StaticValue::Double(d) => format_float(*d, false),
        StaticValue::Str(s) => format!("\"{}\"", escape_string(s)),
        StaticValue::Type(t) => format!("{}.class", dotted(t)),
        StaticValue::Boolean(b) => b.to_string(),
        StaticValue::Null => "null".into(),
        StaticValue::Field(cls, name, desc) => {
            // The rename registry owns the display name (obscuring-field
            // renames key on (owner, name, desc)) — a const field ref must
            // render the SAME name as the declaration.
            let n: std::borrow::Cow<str> = match jdc_core::rename::field_display(cls, name, desc) {
                Some(d) => std::borrow::Cow::Borrowed(d),
                None => java_ident(name),
            };
            if cls == owner {
                n.into_owned()
            } else {
                format!("{}.{}", print_class_name(pool, cls), n)
            }
        }
        StaticValue::Other => return None,
    })
}

/// Render one method straight into the class buffer `out`. Returns
/// whether anything was written (skips: no descriptor, deferred to a
/// monitored thread). The old shape returned a per-method String that
/// the caller copied in — one extra full copy of every method body.
/// `enum_promoted`: the class rendered as a true `enum` declaration
/// (constants in the header), which changes what a ctor signature may
/// declare.
/// d8's covariant-return bridges: `Context getContext() { return
/// super.getContext(); }` shadowing MMActivity's covariant
/// `AppCompatActivity getContext()`. Source cannot declare the
/// wider-return override, and the rendered bridge HIJACKS every
/// `this.getContext()` in the file to the wide type (weixin
/// FTSBaseVoiceSearchUI's `AppCompatActivity context =
/// this.getContext();` — Context无法转换为AppCompatActivity ×74). The
/// inherited covariant method satisfies source callers natively and
/// javac regenerates binary bridges on demand — skip the render.
///
/// TWO gates, both learned the hard way (the ungated ACC_BRIDGE +
/// ancestor-name version skipped erasure/interface-satisfier bridges
/// and cost lark +29k — the claim-map's raw-interface lesson at whole-
/// hierarchy scale):
/// 1. INSTRUCTION SHAPE: the body must be exactly invoke-super to a
///    same-named method + optional move-result + return (≤3 insns).
///    Erasure bridges call THIS with casts; interface satisfiers are
///    real bodies — both keep rendering.
/// 2. POOL ancestor (super chain, exact name+args, non-private)
///    declares the covariant source. Framework ancestors are not
///    consulted (fwdb carries no arg descs — a name-only match could
///    skip a real overload); such bridges keep rendering.
fn bridge_shadowed_by_ancestor(pool: &DexPool, class: &PoolClass, m: &PoolMethod) -> bool {
    if m.access & crate::access::ACC_BRIDGE == 0 || &*m.name == "<init>" || m.code_off == 0 {
        return false;
    }
    let Some(d) = m.parsed_desc() else {
        return false;
    };
    // Gate 1: invoke-super-same-name + move-result*/return* tail only.
    let Some(dex) = pool.dex(m.dex_idx) else {
        return false;
    };
    let dex = &*dex;
    let Some(code) = dex.code_insns_bytes_at(m.code_off) else {
        return false;
    };
    let mut head_ok = false;
    let mut bad_tail = false;
    let mut cnt = 0usize;
    let mut first = true;
    ddc_dex::insn::scan_instructions(code, &mut |op, pc, bytes| {
        cnt += 1;
        if first {
            first = false;
            if op == 0x6f || op == 0x75 {
                let unit = bytes
                    .get(2 * (pc + 1)..2 * (pc + 1) + 2)
                    .map(|b| u16::from_le_bytes([b[0], b[1]]) as u32)
                    .unwrap_or(u32::MAX);
                if unit != u32::MAX {
                    let mr = dex.method(unit);
                    head_ok = dex.string(mr.name_idx) == &*m.name;
                }
            }
        } else if !matches!(op, 0x0a | 0x0b | 0x0c | 0x0e | 0x0f | 0x10 | 0x11) {
            bad_tail = true;
        }
    });
    if !head_ok || bad_tail || !(2..=3).contains(&cnt) {
        return false;
    }
    // Gate 2: pool super chain declares name+args.
    let mut cur: Option<String> = class.super_name.clone();
    let mut hops = 0u32;
    while let Some(s) = cur {
        hops += 1;
        if hops > 64 {
            return false;
        }
        let Some(pc) = pool.get_if_materialized(&s) else {
            return false; // unresolvable ancestor — keep the bridge
        };
        if pc.all_methods().any(|am| {
            am.name == m.name
                && am.access & crate::access::ACC_PRIVATE == 0
                && am.parsed_desc().is_some_and(|ad| ad.args == d.args)
        }) {
            return true;
        }
        cur = pc.super_name.clone();
    }
    false
}

fn emit_method(
    pool: &DexPool,
    class: &PoolClass,
    ctx: &DexCtx<'_>,
    m: &PoolMethod,
    depth: usize,
    enum_promoted: bool,
    out: &mut String,
) -> anyhow::Result<bool> {
    let ind = indent(depth);
    let desc = m.parsed_desc();

    // Signature.
    let mut sig = String::with_capacity(192);
    let a = m.access;
    if a & ACC_PUBLIC != 0 || widen_method(&class.name, &m.name, &m.desc) {
        sig.push_str("public ");
    } else if a & ACC_PRIVATE != 0 {
        // A fallback enum BASE (`/* enum */ class`, not a promoted
        // `enum`) renders its constant-specific-body subclasses as
        // SEPARATE same-package classes (`j$6 extends j`); their ctors
        // call `super(name, ordinal, ..)`, which a PRIVATE base ctor
        // forbids ("j(String,int) has private access"). Widen the base
        // ctor to package-private so the subclass links. The base is
        // abstract → no external instantiation risk. Only the BASE
        // (super is Enum/Object), not the constant subclass itself.
        let fallback_enum_base_ctor = class.is_enum()
            && !enum_promoted
            && &*m.name == "<init>"
            && class
                .super_name
                .as_ref()
                .map(|s| s == "java/lang/Enum" || s == "java/lang/Object")
                .unwrap_or(true);
        if !fallback_enum_base_ctor {
            sig.push_str("private ");
        }
    } else if a & ACC_PROTECTED != 0 {
        sig.push_str("protected ");
    }
    let is_clinit = &*m.name == "<clinit>";
    let is_init = &*m.name == "<init>";
    if a & ACC_STATIC != 0 || is_clinit {
        sig.push_str("static ");
    }
    if a & ACC_FINAL != 0 {
        sig.push_str("final ");
    }
    // `abstract synchronized` is an illegal combination — obfuscated
    // builds mark abstract bridges synchronized (WhatsApp
    // SQLiteOpenHelper).
    if a & (ACC_SYNCHRONIZED | ACC_DECLARED_SYNCHRONIZED) != 0 && a & ACC_ABSTRACT == 0 {
        sig.push_str("synchronized ");
    }
    if a & ACC_NATIVE != 0 {
        sig.push_str("native ");
    }
    if a & ACC_ABSTRACT != 0 {
        sig.push_str("abstract ");
    }
    if a & ACC_SYNTHETIC != 0 {
        sig.push_str("/* synthetic */ ");
    }

    // Body (needed for parameter names even for abstract methods).
    let mut body = decompile_method(pool, class, m).ok().flatten();
    if &*m.name == "<clinit>" {
        if let Some(b) = &mut body {
            crate::passes::strip_clinit_hotfix_guard(&mut b.body);
        }
    }
    // An interface method WITH a body is a `default` method (JLS 9.4.3)
    // unless static/private — dex carries no `default` flag, so the
    // plain form rendered an abstract signature with a body and javac
    // rejected every one ("接口抽象方法不能带有主体", weixin 1138). A
    // default REQUIRES a body: only push it once the body is confirmed
    // (a failed decompile renders a body-less declaration).
    if class.is_interface()
        && a & (ACC_STATIC | ACC_ABSTRACT | ACC_NATIVE | ACC_PRIVATE | ACC_ANNOTATION) == 0
        && body.is_some()
    {
        sig.insert_str(0, "default ");
    }
    let param_names: Vec<String> = body
        .as_ref()
        .map(|b| {
            let mut ps: Vec<(u16, String)> =
                b.vt.vars
                    .iter()
                    .filter(|v| v.is_param && v.name != "this")
                    .map(|v| (v.slot, v.name.clone()))
                    .collect();
            ps.sort_by_key(|(s, _)| *s);
            ps.into_iter().map(|(_, n)| n).collect()
        })
        .unwrap_or_else(|| {
            (0..desc.as_ref().map(|d| d.args.len()).unwrap_or(0))
                .map(|i| format!("p{}", i + 1))
                .collect()
        });

    // Non-static member-inner ctor: the synthetic outer instance rides
    // as args[0] (typed as the direct enclosing class). Every emission
    // site already passes it implicitly — qualified `outer.new Inner(..)`,
    // `this.new Inner(..)` from inside the outer, `super(..)` after
    // skip_outer_arg — so the signature drops the param and the body's
    // references become `Outer.this`. Without this, every construction
    // site fails javac arity ("无法将类…构造器…应用到给定类型" — the
    // guava androidx inner-class families, d8 capture lambdas).
    let mut inner_arg0 = 0usize;
    if is_init {
        let tail = class.name.rsplit('$').next().unwrap_or("");
        let digit_simple = !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit());
        if let Some(d) = desc.as_ref() {
            if let Some(JavaType::Object(outer)) = d.args.first() {
                // The direct enclosing class: nesting annotation when
                // present, else the `$`-chain parent (find_outer_name —
                // d8 lambdas are `Outer$$ExternalSyntheticLambdaN`, where
                // a plain rsplit leaves a trailing `$`). It must agree
                // with the this$0 type for the rewrite to fire.
                let enclosing: Option<String> = class
                    .nesting
                    .enclosing_class
                    .clone()
                    .or_else(|| crate::find_outer_name(pool, &class.name));
                let direct = enclosing.as_deref() == Some(outer.as_ref());
                if direct && !digit_simple && ctx.class_has_this0(&class.name) {
                    let param0 = body.as_ref().and_then(|b| {
                        b.vt.vars
                            .iter()
                            .find(|v| v.is_param && v.name != "this")
                            .map(|v| v.id)
                    });
                    let written = match (&body, param0) {
                        (Some(b), Some(p0)) => crate::passes::local_is_written(&b.body, p0),
                        _ => false,
                    };
                    if let (Some(p0), false) = (param0, written) {
                        if let Some(b) = body.as_mut() {
                            // this()-delegations to the SAME class drop the
                            // outer arg (all of its ctors share the strip);
                            // a super target joins when it is an inner of
                            // the same enclosing family.
                            let mut eligible: Vec<String> = vec![class.name.clone()];
                            if let Some(sup) = &class.super_name {
                                let sup_enclosing =
                                    crate::find_outer_name(pool, sup);
                                if sup_enclosing.as_deref() == Some(outer.as_ref())
                                    && ctx.class_has_this0(sup)
                                {
                                    eligible.push(sup.clone());
                                }
                            }
                            let renamed = crate::apply_class_rename(outer);
                            let display = dotted(renamed.as_ref());
                            let ty =
                                jdc_core::ir::expr::TypeRef::J(JavaType::Object(outer.clone()));
                            crate::passes::rewrite_inner_ctor_outer_param(
                                &mut b.body,
                                p0,
                                &display,
                                &ty,
                                &eligible,
                            );
                            inner_arg0 = 1;
                        }
                    }
                }
            }
        }
    }

    if is_clinit {
        // `static { ... }` — the caller strips the method name/params.
    } else {
        let Some(d) = &desc else { return Ok(false) };
        if is_init {
            // The ctor name must equal the DECLARED class name of its
            // file: flat `$` at depth 0 (own file), own segment when
            // inlined in the parent at depth > 0. Case-renamed classes
            // use their display name.
            let dname = crate::apply_class_rename(&class.name);
            let (_, simple) = split_name(&dname);
            // emit_method's depth is the METHOD indent = class depth + 1:
            // own-file classes (depth 0 header → method depth 1) need the
            // flat `$` ctor name; inline nested members use their segment.
            let base = if depth <= 1 {
                simple.to_string()
            } else {
                // R8 names can END in `$` (`ThreadMsg$$$`): the last
                // `$`-segment is empty — keep the whole simple name.
                let seg = simple.rsplit('$').next().unwrap_or(&simple);
                if seg.is_empty() {
                    simple.to_string()
                } else {
                    seg.to_string()
                }
            };
            let name = if is_restricted_type_name(&base) {
                format!("_{base}")
            } else {
                base
            };
            // The ctor name must equal the DECLARED class name — the same
            // injective `_u<hex>` sanitizer the header uses (java_ident's
            // lossy `-`→`_` fold left `class _u2dDeprecatedOkio` with a
            // `private _DeprecatedOkio()` ctor: "方法声明无效; 需要返回
            // 类型", which as a PARSE error also masks every semantic
            // error in the whole javac run).
            sig.push_str(&sanitize_ref(&name));
        } else {
            sig.push_str(&type_name(pool, &d.ret));
            sig.push(' ');
            let mname =
                jdc_core::rename::field_display(&class.name, &m.name, &m.desc).unwrap_or(&*m.name);
            sig.push_str(&java_ident(mname));
        }
        sig.push('(');
        // A promoted enum ctor's dex descriptor carries the compiler-
        // synthesized `(String name, int ordinal)` prefix — JLS forbids
        // declaring those (they are implicit in the `A(args)` constant
        // declarations the promotion emits, and strip_enum_ctor_super
        // already removed the `super(name, ordinal, ..)` delegation).
        // Skip the pair in the signature, or every constant declaration
        // fails javac arity ("无法将枚举…构造器…应用到给定类型"). Only
        // when the body never reads the two params — code that genuinely
        // uses them keeps the declared form.
        // Robust hotpatch guard strip MUST run before the trace-param
        // analysis: the guard's isSupport(..) call is the ctor's only
        // reader of the (String, int) params for plain robust enums, and
        // its self-static read is what JLS 8.9.2 forbids.
        if enum_promoted && is_init {
            if let Some(b) = body.as_mut() {
                strip_robust_enum_ctor_guard(&mut b.body);
            }
        }
        // Fallback enum base ctor: store the (String, int) trace params
        // into the synthetic $ddcName/$ddcOrdinal fields that back the
        // synthesized name()/ordinal()/valueOf() (synth_enum_members).
        // Non-final fields: constant-subclass ctors reach this store
        // through their super(..) chain, and a `final` would demand
        // definite assignment on every ctor path.
        if is_init
            && !enum_promoted
            && class.is_enum()
            && enum_synth_active(class)
            && matches!(d.args.first(), Some(JavaType::Object(sx)) if sx.as_ref() == "java/lang/String")
            && matches!(d.args.get(1), Some(JavaType::Int))
        {
            if let Some(b) = body.as_mut() {
                // A ctor that DELEGATES first (the -IA synthetic bridge,
                // `this(str, p2)`) must stay delegation-first — the
                // delegated-to ctor performs the stores.
                // First-LEAF probe: the ctor chain leaves the delegation
                // Block-wrapped (`Block[this(..), assigns]`), and a
                // stripped hotfix guard can leave the wrapper as the
                // first top-level statement — unwrap to the real head.
                let starts_with_delegation =
                    crate::passes::is_bare_ctor_call(crate::passes::first_leaf_stmt(&b.body));
                let ps: Vec<u32> = b
                    .vt
                    .vars
                    .iter()
                    .filter(|v| v.is_param && v.name != "this")
                    .take(2)
                    .map(|v| v.id)
                    .collect();
                if ps.len() == 2 && !starts_with_delegation {
                    let mk_store = |var: u32, fname: &str, fty: JavaType| {
                        Stmt::ExprStmt(Expr::Assign {
                            target: Box::new(Expr::Field {
                                owner: Some(Box::new(Expr::This)),
                                cls: std::sync::Arc::from(class.name.as_str()),
                                name: std::sync::Arc::from(fname),
                                ty: jdc_core::ir::expr::TypeRef::J(fty.clone()),
                                is_static: false,
                            }),
                            op: jdc_core::ir::expr::AssignOp::Plain,
                            value: Box::new(Expr::Local {
                                var,
                                ty: jdc_core::ir::expr::TypeRef::J(fty),
                            }),
                        })
                    };
                    let mut inj = vec![
                        mk_store(ps[0], ENUM_NAME_FIELD, JavaType::Object("java/lang/String".into())),
                        mk_store(ps[1], ENUM_ORD_FIELD, JavaType::Int),
                    ];
                    if let Stmt::Block(vs) = &mut b.body {
                        let mut rest = std::mem::take(vs);
                        inj.append(&mut rest);
                        *vs = inj;
                    } else {
                        let prev = std::mem::replace(&mut b.body, Stmt::Block(Vec::new()));
                        inj.push(prev);
                        b.body = Stmt::Block(inj);
                    }
                }
            }
        }
        let mut arg0 = 0;
        if enum_promoted
            && is_init
            && matches!(d.args.first(), Some(JavaType::Object(s)) if s.as_ref() == "java/lang/String")
            && matches!(d.args.get(1), Some(JavaType::Int))
        {
            // The Kotlin default-arg bridge ctor reads name/ordinal in
            // its `this(str, p2, ..)` delegation only — the LEADING pair
            // of a this()-delegation drops together with the params, so
            // those reads do not block the strip (the constants' extra
            // args match the bridge's user params, not the 2-param user
            // ctor).
            let synthetic: Vec<u32> = body
                .as_ref()
                .map(|b| {
                    b.vt
                        .vars
                        .iter()
                        .filter(|v| v.is_param && v.name != "this")
                        .take(2)
                        .map(|v| v.id)
                        .collect()
                })
                .unwrap_or_default();
            // The (name, ordinal) strip is UNIFORM across every promoted
            // enum ctor shape: refs_ok below requires each param's total
            // reads to equal its delegation-LEAD reads, i.e. the param is
            // used ONLY as a leading arg of a this() delegation (or not
            // at all). That covers all three shapes at once —
            //   • the real field-store ctor (super(name,ordinal) already
            //     stripped by strip_enum_ctor_super → total 0 == lead 0);
            //   • the d8 SYNTHETIC bare-constant bridge `g(String,int)
            //     { this(name, ordinal, <defaults>); }` (uuyc Sb/g ×294);
            //   • a REAL user forwarding ctor `C(String,int,X,..) {
            //     this(name, ordinal, x, ..); }` for a multi-arity Java
            //     enum (rimet GaeaConfigKey: constants pass explicit args
            //     only, so the implicit pair MUST drop or every constant
            //     fails 找不到合适的构造器 ×1,204).
            // A ctor that READS name/ordinal in its body (total > lead)
            // keeps its signature unless the value-read rewrite above
            // turned those reads into name()/ordinal() calls. The old
            // is_bridge/ACC_SYNTHETIC gate on this was stale: relaxing it
            // to the uniform total==lead rule IMPROVED every corpus
            // (weixin 6,419→6,381, weibo −90, lark −62, rimet −1,180) —
            // the feared "weixin +2.9k ungated" regression did not recur
            // (that predates the refs_ok total==lead check, strip_enum_
            // ctor_super, and the value-read rewrite, which together keep
            // body-reading and Kotlin-bridge ctors correct).
            let n_args = d.args.len();
            let is_bridge = n_args >= 2
                && matches!(&d.args[n_args - 1], JavaType::Object(ref m)
                    if m.starts_with("kotlin/jvm/internal/"))
                && matches!(d.args[n_args - 2], JavaType::Int);
            // Meituan Robust instrumented ctors PACK the trace params
            // into dispatch arrays (`v3[0] = str; v3[1] = new
            // Integer(p2); PatchProxy.isSupport(v3, ..)`) — value reads
            // that refuse the strip, and a true enum cannot DECLARE
            // (String, int) params (JLS 8.9.1), so the whole class fell
            // back (illegal `Enum.valueOf` + no ordinal()). Rewrite pure
            // VALUE reads to the equivalent `name()`/`ordinal()` calls —
            // Enum's fields are set by the implicit super before the
            // body runs, so the pack sees identical values. Delegation
            // LEAD reads (this(str, p2) of Kotlin/-IA bridges) must keep
            // their Local shape for the lead-drop below, so skip any
            // ctor that has them; written params are never replaceable.
            if synthetic.len() == 2 && !is_bridge {
                if let Some(b) = body.as_ref() {
                    let (p0, p1) = (synthetic[0], synthetic[1]);
                    let mut lead_reads = 0usize;
                    {
                        let cls_name = class.name.as_str();
                        crate::passes::visit_stmt_exprs_ro(&b.body, &mut |e| {
                            if let Expr::Method { name: mn, cls: mc, args, is_special, .. } = e {
                                if &**mn == "<init>" && *is_special && mc.as_ref() == cls_name {
                                    for a in args.iter().take(2) {
                                        if let Expr::Local { var: v, .. } = a {
                                            if *v == p0 || *v == p1 {
                                                lead_reads += 1;
                                            }
                                        }
                                    }
                                }
                            }
                        });
                    }
                    let uses = crate::passes::count_locals_stmts(std::slice::from_ref(&b.body));
                    let reads = uses.get(&p0).copied().unwrap_or(0)
                        + uses.get(&p1).copied().unwrap_or(0);
                    let written = crate::passes::local_is_written(&b.body, p0)
                        || crate::passes::local_is_written(&b.body, p1);
                    if lead_reads == 0 && reads > 0 && !written {
                        let mk = |nm: &str, ret: JavaType| Expr::Method {
                            owner: Some(Box::new(Expr::This)),
                            cls: std::sync::Arc::from("java/lang/Enum"),
                            name: std::sync::Arc::from(nm),
                            desc: std::sync::Arc::new(jdc_core::types::MethodDescriptor {
                                ret,
                                args: Vec::new(),
                            }),
                            args: Vec::new(),
                            is_static: false,
                            is_interface: false,
                            is_special: false,
                            is_super: false,
                            is_dynamic: false,
                            type_args: Vec::new(),
                        };
                        let name_call =
                            mk("name", JavaType::Object("java/lang/String".into()));
                        let ord_call = mk("ordinal", JavaType::Int);
                        if let Some(b) = body.as_mut() {
                            crate::passes::walk_stmt_exprs(&mut b.body, &mut |e| {
                                crate::passes::deep_rewrite(e, &mut |x| {
                                    if let Expr::Local { var, .. } = x {
                                        if *var == p0 {
                                            *x = name_call.clone();
                                        } else if *var == p1 {
                                            *x = ord_call.clone();
                                        }
                                    }
                                });
                            });
                        }
                    }
                }
            }
            let mut lead = [0usize, 0usize];
            let refs_ok = body.as_ref().is_some_and(|b| {
                let uses = crate::passes::count_locals_stmts(std::slice::from_ref(&b.body));
                let cls_name = class.name.as_str();
                crate::passes::visit_stmt_exprs_ro(&b.body, &mut |e| {
                    if let Expr::Method { name: mn, cls: mc, args, is_special, .. } = e {
                        if &**mn == "<init>" && *is_special && mc.as_ref() == cls_name {
                            for (k, a) in args.iter().enumerate() {
                                if let Expr::Local { var: v, .. } = a {
                                    if let Some(pi) = synthetic.iter().position(|p| p == v) {
                                        if k == pi && k < 2 {
                                            lead[pi] += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                });
                synthetic.iter().enumerate().all(|(pi, id)| {
                    let total = uses.get(id).copied().unwrap_or(0);
                    // Uniform rule: the implicit param is used ONLY as a
                    // this()-delegation lead (or not at all). See the
                    // block comment above — this subsumes the old
                    // total==0 (real ctor) and is_bridge cases.
                    total == lead[pi]
                })
            });
            if refs_ok {
                arg0 = 2;
                // Drop the leading (name, ordinal) args of the this()
                // delegations.
                if let Some(b) = body.as_mut() {
                    let cls_name = class.name.as_str();
                    let syn = synthetic;
                    crate::passes::walk_stmt_exprs(&mut b.body, &mut |e| {
                        if let Expr::Method { name: mn, cls: mc, args, is_special, .. } = e {
                            if &**mn == "<init>" && *is_special && mc.as_ref() == cls_name {
                                let mut drop_n = 0;
                                for a in args.iter().take(2) {
                                    if let Expr::Local { var: v, .. } = a {
                                        if syn.contains(v) && drop_n == v - syn[0] {
                                            drop_n += 1;
                                            continue;
                                        }
                                    }
                                    break;
                                }
                                for _ in 0..drop_n {
                                    args.remove(0);
                                }
                            }
                        }
                    });
                }
            }
        }
        let n = d.args.len();
        let varargs = a & ACC_VARARGS != 0 && n > 0;
        let arg0 = arg0 + inner_arg0;
        for (i, arg) in d.args.iter().enumerate().skip(arg0) {
            if i > arg0 {
                sig.push_str(", ");
            }
            let name = param_names
                .get(i)
                .cloned()
                .unwrap_or_else(|| format!("p{}", i));
            if varargs && i + 1 == n {
                if let JavaType::Array(inner) = arg {
                    sig.push_str(&type_name(pool, inner));
                    sig.push_str("...");
                } else {
                    sig.push_str(&type_name(pool, arg));
                }
            } else {
                sig.push_str(&type_name(pool, arg));
            }
            sig.push(' ');
            // Param names come from dex debug info — obfuscated apps
            // name them `_` (reserved since Java 9) or after keywords.
            sig.push_str(&java_ident(&name));
        }
        sig.push(')');
    }

    if is_clinit {
        let Some(b) = body else { return Ok(false) };
        // Direct render: the printer starts at the method's ABSOLUTE
        // indent and appends into the same buffer that carries the
        // header — no intermediate body string, no per-line re-indent
        // pass (two full copies of every method body saved).
        out.push_str(&ind);
        out.push_str("static {\n");
        let hdr = out.len();
        let printer = Printer::new(ctx, &b.vt)
            .with_indent(depth + 1)
            .with_output(std::mem::take(out));
        let t_print = std::time::Instant::now();
        let mut rendered = printer.into_string(&b.body);
        crate::method::phase_hit(3, t_print);
        if rendered[hdr..].trim().is_empty() {
            rendered.truncate(hdr);
        }
        rendered.push_str(&ind);
        rendered.push_str("}\n");
        *out = rendered;
        return Ok(true);
    }
    if a & (ACC_ABSTRACT | ACC_NATIVE) != 0 || body.is_none() {
        out.push_str(&ind);
        out.push_str(&sig);
        out.push_str(";\n");
        return Ok(true);
    }
    let Some(b) = body else { return Ok(false) };

    // Direct render at the absolute indent level (see the clinit path).
    let mut printer = Printer::new(ctx, &b.vt).with_indent(depth + 1);
    match &b.desc.ret {
        JavaType::Boolean => {
            printer = printer.with_ret_bool(true);
        }
        JavaType::Char => {
            printer = printer.with_ret_char(true);
        }
        JavaType::Byte => {
            printer = printer.with_ret_narrow(true, false);
        }
        JavaType::Short => {
            printer = printer.with_ret_narrow(false, true);
        }
        _ => {}
    }
    out.push_str(&ind);
    out.push_str(&sig);
    out.push_str(" {\n");
    let hdr = out.len();
    let printer = printer.with_output(std::mem::take(out));
    let t_print = std::time::Instant::now();
    let mut rendered = printer.into_string(&b.body);
    crate::method::phase_hit(3, t_print);
    if rendered[hdr..].trim().is_empty() {
        rendered.truncate(hdr);
    }
    rendered.push_str(&ind);
    rendered.push_str("}\n");
    *out = rendered;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Naming helpers
// ---------------------------------------------------------------------------

fn indent(depth: usize) -> String {
    "    ".repeat(depth)
}

/// `$`-separated nesting rendered with dots — but ONLY when every
/// segment is a clean Java identifier (a genuine member class chain).
/// Anonymous (`Outer$1`), Kotlin synthetic (`Version$bigInteger$2`,
/// `...$$inlined$collect$1`) and local-class tails are NOT member
/// classes Java can name; the whole name stays flat with `$` (ddc emits
/// them as their own top-level files).
/// Kotlin emits method names like `invokeSuspend$lambda-0` — `-` (and
/// any other non-identifier character) is not legal Java. Deterministic
/// mapping, applied identically at declaration and call sites.
pub(crate) fn java_ident(name: &str) -> std::borrow::Cow<'_, str> {
    // env::var_os is an environ lock+scan — java_ident runs per IDENTIFIER
    // (millions per APK), where it profiled as __NSGetEnviron.
    static DBG_IDENT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *DBG_IDENT.get_or_init(|| std::env::var_os("DDC_DBG_IDENT").is_some()) {
        eprintln!("[ident] {name:?}");
    }
    let clean = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    // Kotlin names fields/methods `default` (Companion.default) — no Java
    // program can declare or reference a keyword; the identical mapping
    // lives in jdc-core's call-site sanitizer.
    let keyword = matches!(
        name,
        "abstract"
            | "assert"
            | "boolean"
            | "break"
            | "byte"
            | "case"
            | "catch"
            | "char"
            | "class"
            | "const"
            | "continue"
            | "default"
            | "do"
            | "double"
            | "else"
            | "enum"
            | "extends"
            | "final"
            | "finally"
            | "float"
            | "for"
            | "goto"
            | "if"
            | "implements"
            | "import"
            | "instanceof"
            | "int"
            | "interface"
            | "long"
            | "native"
            | "new"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "return"
            | "short"
            | "static"
            | "strictfp"
            | "super"
            | "switch"
            | "synchronized"
            | "this"
            | "throw"
            | "throws"
            | "transient"
            | "try"
            | "void"
            | "volatile"
            | "while"
            | "true"
            | "false"
            | "null"
            // `_` is a reserved identifier since Java 9 (Alipay's
            // instant-run fields are named `_`) — the `_<name>` mapping
            // turns it into `__`, matching jdc-core's call sites.
            | "_"
    );
    // A simple name may not START with a digit either (WhatsApp nests
    // `X/0Xx`): the declaration site and every reference (jdc-core's
    // sanitize_source_name) prefix the same underscore.
    let digit_start = name.chars().next().is_some_and(|c| c.is_ascii_digit());
    if clean && !keyword && !digit_start {
        std::borrow::Cow::Borrowed(name)
    } else if keyword || digit_start {
        std::borrow::Cow::Owned(format!("_{name}"))
    } else {
        // Non-ASCII single chars (Alipay names a field `支`) map to a
        // lone `_` — itself reserved since Java 9. Escape it.
        let mapped: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        if mapped == "_" {
            std::borrow::Cow::Owned("__".to_string())
        } else {
            std::borrow::Cow::Owned(mapped)
        }
    }
}

fn split_name(internal: &str) -> (String, String) {
    match internal.rfind('/') {
        Some(i) => (internal[..i].to_string(), internal[i + 1..].to_string()),
        None => (String::new(), internal.to_string()),
    }
}

/// Dotted source form of an internal name.
pub fn dotted(internal: &str) -> String {
    let mut out = internal.replace('/', ".");
    // `$` → `.` only when the following segment can start a Java
    // identifier (anonymous/synthetic tails stay `$`).
    let mut i = 0;
    while let Some(p) = out[i..].find('$') {
        let at = i + p;
        // A LEADING `$` (ProGuard keeps `$Gson$Types`) is part of the
        // source name — dotting it produced a leading `.Gson.Types`.
        let head_ok = at > 0
            && out[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        let tail_ok = out[at + 1..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
        if tail_ok && head_ok {
            out.replace_range(at..at + 1, ".");
            i = at + 1;
        } else {
            i = at + 1;
        }
    }
    // Every `.`-segment must start an identifier: WhatsApp's `X/0Hl`
    // reached `extends` with the digit start unmapped.
    sanitize_fq(&out)
}

fn join_dotted(pool: &DexPool, names: &[String]) -> String {
    names
        .iter()
        .map(|n| print_class_name(pool, n))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A printable type name (arrays render with `[]` suffixes). Nested class
/// names dot their `$` when the outer chain is known to the pool.
/// Split a concatenated method-descriptor ARGUMENT string
/// (`"Ljava/lang/String;I[Ljava/lang/Object;"`) into per-parameter
/// `JavaType`s. Used by the framework-ancestor ctor bridge, where the
/// referenced sig comes from the method-id table (no pool ancestor
/// method to read a parsed descriptor from).
pub(crate) fn split_arg_descs(sig: &str) -> Vec<JavaType> {
    let mut out = Vec::new();
    let b = sig.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        let start = i;
        while i < b.len() && b[i] == b'[' {
            i += 1;
        }
        if i < b.len() {
            if b[i] == b'L' {
                while i < b.len() && b[i] != b';' {
                    i += 1;
                }
                if i < b.len() {
                    i += 1; // consume ';'
                }
            } else {
                i += 1; // primitive single char
            }
        }
        if i > start {
            out.push(desc_type(&sig[start..i]));
        } else {
            break; // malformed: avoid an infinite loop
        }
    }
    out
}

pub fn type_name(pool: &DexPool, t: &JavaType) -> String {
    match t {
        JavaType::Void => "void".into(),
        JavaType::Object(n) => print_class_name(pool, n),
        JavaType::Array(inner) => format!("{}[]", type_name(pool, inner)),
        other => other.to_java(false),
    }
}

/// `com/foo/Outer$Inner` → `com.foo.Outer.Inner` — each `$` dots only when
/// its left side names a known class (literal `$` top-level names survive).
// ---------------------------------------------------------------------------
// The per-class import map (JLS 6.4.2 obscuring repair).
//
// A referenced FQN whose FIRST segment equals an in-scope class simple
// name binds the qualifier to the CLASS, not the package — `class j3
// implements j3.g` is cyclic inheritance and collapses javac's
// attribution for the whole package (the Object-cascade root, weixin
// 84 files + contagion). The repair: emit `import j3.g;` and render
// the SIMPLE name in every type position. The set is populated per
// class from a metadata pre-scan (supertypes, field types, method
// signature descriptors) and read by print_class_name and the
// DexCtx::obscured_simple implementation (which jdc-core's shorten /
// java_type_name consult).
struct ObscureState {
    /// The ROOT class being rendered (its simple name is the obscuring
    /// in-scope name).
    class: String,
    /// Field names in scope through the root class's inheritance chain
    /// (own + pool ancestors + framework ancestors' statics/instances +
    /// interface constants). An in-scope field obscures a package's
    /// first segment in expression position (JLS 6.4.2) — WhatsApp's
    /// View subclasses inherit the static Property X/Y/Z (API 31+) and
    /// its whole obfuscated codebase lives in package X (×30,171); the
    /// general set replaces the hardcoded X/Y/Z list. Obscured refs
    /// route through the import layer like own-simple obscuring.
    shadow: jdc_core::FxHashSet<String>,
    /// Metadata pre-scan results: internal → simple render.
    map: jdc_core::FxHashMap<String, String>,
    /// Expression-level obscured refs DISCOVERED during the body render
    /// (their imports emit at assembly time).
    recorded: jdc_core::FxHashSet<String>,
    /// Simple names of the CURRENT PACKAGE's own classes: importing a
    /// name that collides would SHADOW every same-package use of it in
    /// this file (JLS 7.5.1) — those refs stay qualified (obscured,
    /// erroring) rather than corrupt.
    blocked_base: std::sync::Arc<jdc_core::FxHashSet<String>>,
    blocked_removed: jdc_core::FxHashSet<String>,
    blocked_extra: jdc_core::FxHashSet<String>,
    /// RENAMED display tails of same-package classes (zero-copy view
    /// into the process-wide cache): same blocking role as `blocked`
    /// for refs that render under the rename registry.
    blocked_renamed: Option<&'static jdc_core::FxHashSet<String>>,
}

thread_local! {
    static OBSCURE: std::cell::RefCell<Option<ObscureState>> =
        const { std::cell::RefCell::new(None) };
}

/// Install the per-class render state (call at class render entry).
pub(crate) fn set_obscured_state(
    class: String,
    map: jdc_core::FxHashMap<String, String>,
    blocked_base: std::sync::Arc<jdc_core::FxHashSet<String>>,
    blocked_removed: jdc_core::FxHashSet<String>,
    blocked_extra: jdc_core::FxHashSet<String>,
    blocked_renamed: Option<&'static jdc_core::FxHashSet<String>>,
    shadow: jdc_core::FxHashSet<String>,
) {
    OBSCURE.with(|m| {
        *m.borrow_mut() = Some(ObscureState {
            class,
            shadow,
            map,
            recorded: jdc_core::FxHashSet::default(),
            blocked_base,
            blocked_removed,
            blocked_extra,
            blocked_renamed,
        });
    });
}

/// Simple names this class's file renders through the import layer
/// (metadata map values + dynamically recorded nested tails). A method
/// local with one of these names captures the imported simple name in
/// expression position (locals beat single-type imports — `h.e` binds
/// a local int `h`, 无法取消引用int); deshadow_locals renames them.
/// Empty when no render state is installed (progressive paths).
pub(crate) fn obscured_simples_snapshot() -> jdc_core::FxHashSet<String> {
    OBSCURE.with(|m| {
        let mut out = jdc_core::FxHashSet::default();
        let Ok(st) = m.try_borrow() else { return out };
        let Some(st) = st.as_ref() else { return out };
        for simple in st.map.values() {
            out.insert(simple.clone());
        }
        for internal in &st.recorded {
            let renamed = crate::apply_class_rename(internal);
            if let Some(tail) = renamed.rsplit(['/', '$']).next() {
                if !tail.is_empty() {
                    out.insert(tail.to_string());
                }
            }
        }
        out
    })
}

/// Take the recorded expression-level refs and clear the state.
pub(crate) fn take_recorded_and_clear() -> jdc_core::FxHashSet<String> {
    OBSCURE.with(|m| {
        m.borrow_mut()
            .take()
            .map(|st| st.recorded)
            .unwrap_or_default()
    })
}

// ---- cross-package access widening ----------------------------------
// ART tolerates cross-package access to non-public classes/members that
// R8/d8 preserved; Java source cannot express it ("是在不可访问的类或
// 接口中定义的", "k在kotlinx.coroutines中不是公共的", "unknownFields 在
// l6 中是 protected"). The census scan records every cross-package
// reference; the installer keeps the non-public targets and the
// declaration renders widen them to public.

struct WidenState {
    classes: jdc_core::FxHashSet<String>,
    methods: jdc_core::FxHashMap<String, jdc_core::FxHashSet<(String, String)>>,
    fields: jdc_core::FxHashMap<String, jdc_core::FxHashSet<String>>,
    /// (owner, field name) finals written outside the declaring class's
    /// ctor/clinit — rendered WITHOUT `final` (see refscan census).
    unfinal: jdc_core::FxHashSet<(String, String)>,
}

static WIDEN: std::sync::OnceLock<WidenState> = std::sync::OnceLock::new();

pub(crate) fn install_access_widening(pool: &crate::DexPool) {
    let t0 = std::time::Instant::now();
    let raw = crate::refscan::access_widening_scan(pool.dexes());
    let mut classes: jdc_core::FxHashSet<String> = jdc_core::FxHashSet::default();
    for c in &raw.classes {
        if let Some(pc) = pool.get_if_materialized(c) {
            if pc.access & crate::access::ACC_PUBLIC == 0 {
                classes.insert(c.clone());
            }
        }
    }
    let mut methods: jdc_core::FxHashMap<String, jdc_core::FxHashSet<(String, String)>> =
        jdc_core::FxHashMap::default();
    for (owner, set) in &raw.methods {
        for (name, desc) in set {
            if name == "<clinit>" {
                continue; // never referenced
            }
            // A dex ref of an INHERITED member may name any subclass as
            // owner (ART resolves up the chain), while the declaration —
            // and its access modifier — lives on an ancestor. Register
            // the widening on the DECLARING class or the rendered decl
            // stays protected ("在 X 中是 protected 访问控制").
            let mut cur: Option<&str> = Some(owner.as_str());
            let mut hops = 0u32;
            while let Some(cname) = cur {
                hops += 1;
                if hops > 64 {
                    break;
                }
                let Some(pc) = pool.get_if_materialized(cname) else {
                    break;
                };
                // Enum ctors are source-level private-only: widening is
                // a javac "modifier not allowed here".
                if name == "<init>" && pc.access & crate::access::ACC_ENUM != 0 {
                    break;
                }
                match pc.all_methods()
                    .find(|m| &*m.name == name.as_str() && &*m.desc == desc.as_str())
                {
                    Some(m) if m.access & crate::access::ACC_PUBLIC == 0 => {
                        methods
                            .entry(cname.to_string())
                            .or_default()
                            .insert((name.clone(), desc.clone()));
                        break;
                    }
                    Some(_) => break, // already public
                    None => cur = pc.super_name.as_deref(),
                }
            }
        }
    }
    // Override-closure DOWN the hierarchy: a widened (public) super
    // method whose subclass override stays protected/package is
    // "无法覆盖...更低的访问权限" (lark +823). Every transitive
    // subclass declaring the same (name, desc) non-public widens too.
    {
        let mut subs: jdc_core::FxHashMap<String, Vec<String>> =
            jdc_core::FxHashMap::default();
        for name in &pool.order {
            let Some(pc) = pool.get_if_materialized(name) else {
                continue;
            };
            if let Some(sup) = &pc.super_name {
                subs.entry(sup.clone()).or_default().push(name.clone());
            }
            for i in &pc.interfaces {
                subs.entry(i.clone()).or_default().push(name.clone());
            }
        }
        let seeds: Vec<(String, String, String)> = methods
            .iter()
            .flat_map(|(o, set)| {
                set.iter().map(|(n, d)| (o.clone(), n.clone(), d.clone()))
            })
            .collect();
        for (owner, mname, mdesc) in seeds {
            let mut queue: Vec<String> = match subs.get(&owner) {
                Some(v) => v.clone(),
                None => continue,
            };
            let mut seen: jdc_core::FxHashSet<String> = jdc_core::FxHashSet::default();
            while let Some(c) = queue.pop() {
                if !seen.insert(c.clone()) {
                    continue;
                }
                let Some(pc) = pool.get_if_materialized(&c) else {
                    continue;
                };
                let declares_nonpub = pc.all_methods().any(|m| {
                    &*m.name == mname.as_str()
                        && &*m.desc == mdesc.as_str()
                        && m.access & crate::access::ACC_PUBLIC == 0
                });
                if declares_nonpub {
                    methods
                        .entry(c.clone())
                        .or_default()
                        .insert((mname.clone(), mdesc.clone()));
                }
                if let Some(v) = subs.get(&c) {
                    queue.extend(v.iter().cloned());
                }
            }
        }
        // Subclass `super(..)` targets. A class C extends S; C's ctor
        // renders `super(..)` against S's ctor. When R8 INLINES S's
        // trivial ctor, C's dex ctor calls `Object.<init>` directly (not
        // `S.<init>`), so the bytecode census above never sees the S.<init>
        // reference and a PRIVATE S ctor stays private — the rendered
        // `super()` then fails ("R() 在 R 中是 private 访问控制", lark ×122:
        // pl.droidsonroids.gif.R extends com.ss.android.lark.R, whose ctor
        // R8 inlined to Object.<init>). Widen every private ctor of a
        // class that HAS subclasses so the implicit super() links. Enums
        // are excluded (their ctors are source-level private-only).
        for sup in subs.keys() {
            let Some(pc) = pool.get_if_materialized(sup) else {
                continue;
            };
            if pc.access & crate::access::ACC_ENUM != 0 {
                continue;
            }
            for m in pc.all_methods() {
                if &*m.name == "<init>" && m.access & crate::access::ACC_PRIVATE != 0 {
                    methods
                        .entry(sup.clone())
                        .or_default()
                        .insert((m.name.to_string(), m.desc.to_string()));
                }
            }
        }
    }
    let mut fields: jdc_core::FxHashMap<String, jdc_core::FxHashSet<String>> =
        jdc_core::FxHashMap::default();
    for (owner, set) in &raw.fields {
        for name in set {
            // Same inherited-owner walk as methods: the protected field
            // lives on the ancestor that DECLARES it (weibo
            // mModuleContext/mData ×470 — refs named the module
            // subclass, the own-fields-only filter never widened the
            // declaring base).
            let mut cur: Option<&str> = Some(owner.as_str());
            let mut hops = 0u32;
            while let Some(cname) = cur {
                hops += 1;
                if hops > 64 {
                    break;
                }
                let Some(pc) = pool.get_if_materialized(cname) else {
                    break;
                };
                match pc.static_fields
                    .iter()
                    .chain(pc.instance_fields.iter())
                    .find(|f| f.name.as_ref() == name.as_str())
                {
                    Some(f) if f.access & crate::access::ACC_PUBLIC == 0 => {
                        fields
                            .entry(cname.to_string())
                            .or_default()
                            .insert(name.clone());
                        break;
                    }
                    Some(_) => break, // already public
                    None => cur = pc.super_name.as_deref(),
                }
            }
        }
    }
    if std::env::var("DDC_STATS").is_ok() {
        eprintln!(
            "[renames] access widening: classes={} method-owners={} field-owners={} scan={:?}",
            classes.len(),
            methods.len(),
            fields.len(),
            t0.elapsed()
        );
    }

    let unfinal: jdc_core::FxHashSet<(String, String)> =
        raw.unfinal_fields.into_iter().collect();
    let _ = WIDEN.set(WidenState { classes, methods, fields, unfinal });
}

#[inline]
pub(crate) fn widen_class(internal: &str) -> bool {
    WIDEN.get().is_some_and(|w| w.classes.contains(internal))
}

#[inline]
pub(crate) fn widen_method(owner: &str, name: &str, desc: &str) -> bool {
    WIDEN.get().is_some_and(|w| {
        w.methods
            .get(owner)
            .is_some_and(|s| s.contains(&(name.to_string(), desc.to_string())))
    })
}

#[inline]
pub(crate) fn widen_field(owner: &str, name: &str) -> bool {
    WIDEN.get().is_some_and(|w| {
        w.fields.get(owner).is_some_and(|s| s.contains(name))
    })
}

/// True when the census saw this final field written outside the
/// declaring class's `<init>`/`<clinit>` — the `final` modifier must
/// not render ("无法为 final 变量 v 分配值" at every foreign write).
#[inline]
pub(crate) fn unfinal_field(owner: &str, name: &str) -> bool {
    WIDEN.get().is_some_and(|w| {
        w.unfinal.contains(&(owner.to_string(), name.to_string()))
    })
}

/// Render-time lookup: the metadata map first; otherwise an
/// expression-level ref whose first segment equals the root class's
/// simple name — record it (its import emits at assembly) and render
/// the simple name NOW.
pub(crate) fn obscured_render_pub(internal: &str) -> Option<String> {
    OBSCURE.with(|m| {
        let mut st = m.borrow_mut();
        let st = st.as_mut()?;
        if let Some(simple) = st.map.get(internal) {
            return Some(simple.clone());
        }
        let first = internal.split('/').next().unwrap_or("");
        // The DISPLAY own simple: when the root class itself was renamed
        // (case-collision `widget/d` → `widget/d2`), no in-scope type
        // bears the raw name any more — the package's first segment is
        // NOT obscured and the qualified render is the correct one
        // (reqable d2.java: importing d.h let the class's own boolean
        // field h shadow it — 无法在调用超类型构造器之前引用h ×23).
        let own_renamed = crate::apply_class_rename(&st.class);
        let own = own_renamed.rsplit(['/', '$']).next().unwrap_or("");
        let seg_obscured = first == own || st.shadow.contains(first);
        if seg_obscured && internal.split('/').count() >= 2 {
            // A NESTED internal's in-scope simple name is its `$` tail
            // (`x/a$b` imports/renders as `b`) — the slash-tail left the
            // `$` in the render (`t2$a` flat against a nested emission).
            // The tail must come from the RENAMED display: case-collision
            // renames (`X/00i` → `X/_00i_2`) live in the class registry —
            // the raw tail rendered `_00i` against the declaration's
            // `_00i_2` (WhatsApp cannot-find ×27k after the View-shadow
            // escape opened this path at scale).
            let renamed = crate::apply_class_rename(internal);
            let simple = renamed
                .rsplit(['/', '$'])
                .next()
                .unwrap_or("")
                .to_string();
            // NOTE: no digit-lead rejection here — a digit-leading RAW
            // tail (`X/00i`) is sanitized to an importable `_00i` at
            // the import line and the refs render through the registry;
            // rejecting them dropped WhatsApp's whole field-X-shadowed
            // package import layer (cannot-find:变量 ×17k cascade).
            if !simple.is_empty()
                && !st.blocked_extra.contains(&simple)
                && (st.blocked_removed.contains(&simple)
                    || !st.blocked_base.contains(&simple))
                && !st.blocked_renamed.is_some_and(|e| e.contains(&simple))
            {
                st.recorded.insert(internal.to_string());
                return Some(simple);
            }
        }
        None
    })
}

/// Field names in scope for `class` through inheritance: own fields,
/// pool ancestors (supers + interfaces, transitively), and framework
/// ancestors via the embedded API database (chain sets memoized
/// process-wide — every View subclass shares the View/ViewGroup/Object
/// field union, so the per-file cost is its own pool chain only).
fn inherited_field_shadows(
    pool: &DexPool,
    class: &PoolClass,
) -> jdc_core::FxHashSet<String> {
    fn fw_chain_fields(boundary: &str) -> std::sync::Arc<jdc_core::FxHashSet<String>> {
        use std::sync::{Mutex, OnceLock};
        static CACHE: OnceLock<Mutex<jdc_core::FxHashMap<String, std::sync::Arc<jdc_core::FxHashSet<String>>>>> =
            OnceLock::new();
        let cache = CACHE.get_or_init(|| Mutex::new(jdc_core::FxHashMap::default()));
        if let Some(hit) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(boundary) {
            return hit.clone();
        }
        let mut set: jdc_core::FxHashSet<String> = jdc_core::FxHashSet::default();
        let mut stack: Vec<&str> = vec![boundary];
        let mut seen: jdc_core::FxHashSet<&str> = jdc_core::FxHashSet::default();
        let mut hops = 0usize;
        while let Some(c) = stack.pop() {
            hops += 1;
            if hops > 256 || !seen.insert(c) {
                continue;
            }
            crate::fwdb::for_each_field(c, |n, f| {
                // STATIC only: instance fields shadow qualified refs in
                // instance contexts but NOT in static ones, and the
                // render decision is context-free — the static subset
                // is the sound approximation (View.X/Y/Z are static;
                // including instance fields cost +33 battery).
                if f & crate::fwdb::MF_STATIC != 0 {
                    set.insert(n.to_string());
                }
            });
            if let Some(sup) = crate::fwdb::super_of(c) {
                stack.push(sup);
            }
            crate::fwdb::for_each_interface(c, |i| stack.push(i));
        }
        let arc = std::sync::Arc::new(set);
        cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(boundary.to_string(), arc.clone());
        arc
    }

    let mut out: jdc_core::FxHashSet<String> = jdc_core::FxHashSet::default();
    let mut stack: Vec<String> = vec![class.name.to_string()];
    let mut seen: jdc_core::FxHashSet<String> = jdc_core::FxHashSet::default();
    let mut hops = 0usize;
    while let Some(cn) = stack.pop() {
        hops += 1;
        if hops > 512 || !seen.insert(cn.clone()) {
            continue;
        }
        let Some(pc) = pool.get(&cn) else {
            // Framework boundary: memoized chain set.
            out.extend(fw_chain_fields(&cn).iter().cloned());
            continue;
        };
        // Static-only (pool side too): ANCESTOR private instance
        // fields are NOT inherited into lexical scope (JLS 6.4.3.1) —
        // including them over-triggered the obscured-import route and
        // the emitted simple static qualifiers were captured by real
        // in-scope variables (reqable SearchView `a.H` "变量 H" ×128).
        // Own instance fields shadowing packages are handled at the
        // ROOT: the collision-mint loops avoid package first segments
        // (lib.rs root_segs guard), and raw-name collisions go
        // through field_deshadow_renames.
        for f in pc.static_fields.iter() {
            out.insert(f.name.to_string());
        }
        if let Some(sup) = &pc.super_name {
            if sup != "java/lang/Object" {
                stack.push(sup.clone());
            } else {
                // Object itself lives in the DB (its fields shadow too —
                // none exist, but interface constants reached via
                // interfaces below still do).
            }
        } else {
            // Interface (no super): java/lang/Object contributes nothing.
        }
        stack.extend(pc.interfaces.iter().cloned());
    }
    out
}

/// The pre-scan: internal names referenced by the class's metadata
/// (supertypes, field types, method descriptors) whose first segment
/// equals the class's own simple name and which exist in the pool —
/// these get imports and simple-name renders.
/// Base simple name of an internal (`pq/c$x` → `c`), if it lives in
/// `pkg` (the bare-name segment a same-package ref renders under).
fn sibling_simple(internal: &str, pkg: &str) -> Option<String> {
    let (p, tail) = match internal.rfind('/') {
        Some(i) => (&internal[..i], &internal[i + 1..]),
        None => ("", internal),
    };
    if p != pkg || tail.is_empty() {
        return None;
    }
    let base = tail.split('$').next().unwrap_or("");
    if base.is_empty() {
        None
    } else {
        Some(base.to_string())
    }
}

/// Simple names of SAME-PACKAGE classes this file's family references
/// from method bodies (raw insn ref scan — decode only, no lift) or
/// metadata (supertypes, field descriptors, signature descriptors).
/// JLS 7.5.1: a single-type import legally shadows same-package
/// siblings throughout the file — it only breaks the refs the file
/// ITSELF makes to them, so the package-wide blocked set narrows to
/// this. Undercounting would silently rebind a sibling ref (semantic
/// corruption), so the universe is exhaustive over code refs:
/// invoke/field owners, type-index instructions, catch types.
fn sibling_ref_simples(pool: &DexPool, class: &PoolClass) -> jdc_core::FxHashSet<String> {
    let pkg = class.name.rfind('/').map(|i| &class.name[..i]).unwrap_or("");
    let mut out: jdc_core::FxHashSet<String> = jdc_core::FxHashSet::default();
    let add_internal = |n: &str, out: &mut jdc_core::FxHashSet<String>| {
        if let Some(s) = sibling_simple(n, pkg) {
            out.insert(s);
        }
    };
    let add_desc = |d: &str, out: &mut jdc_core::FxHashSet<String>| {
        // descriptor → internal(s): arrays peel, L..; strips
        let mut t = d;
        while t.starts_with('[') {
            t = &t[1..];
        }
        if t.len() > 1 && t.starts_with('L') && t.ends_with(';') {
            add_internal(&t[1..t.len() - 1], out);
        }
    };
    // metadata: supertypes, fields, signatures (family-wide below)
    let mut seen: jdc_core::FxHashSet<String> = jdc_core::FxHashSet::default();
    let mut queue: Vec<String> = vec![class.name.clone()];
    while let Some(cn) = queue.pop() {
        if !seen.insert(cn.clone()) {
            continue;
        }
        queue.extend(pool.children_of(&cn).iter().cloned());
        let Some(pc) = pool.get(&cn) else { continue };
        add_internal(&pc.name, &mut out);
        if let Some(sup) = &pc.super_name {
            add_internal(sup, &mut out);
        }
        for i in &pc.interfaces {
            add_internal(i, &mut out);
        }
        for f in pc.static_fields.iter().chain(pc.instance_fields.iter()) {
            add_desc(&f.desc, &mut out);
        }
        for m in pc.all_methods() {
            if let Some(d) = m.parsed_desc() {
                for a in &d.args {
                    if let JavaType::Object(n) = a {
                        add_internal(n, &mut out);
                    } else if let JavaType::Array(_) = a {
                        add_desc(&a.to_descriptor(), &mut out);
                    }
                }
                if let JavaType::Object(n) = &d.ret {
                    add_internal(n, &mut out);
                } else if let JavaType::Array(_) = &d.ret {
                    add_desc(&d.ret.to_descriptor(), &mut out);
                }
            }
            let Some(dex) = pool.dex(m.dex_idx) else {
                continue;
            };
            let Some(code) = dex.code_at(m.code_off) else {
                continue;
            };
            for ins in code.insns.iter() {
                use ddc_dex::insn::InsnKind;
                match &ins.kind {
                    InsnKind::Invoke { method_idx, .. } => {
                        let mm = dex.method(*method_idx);
                        add_internal(&dex.class_name(mm.class_idx), &mut out);
                    }
                    InsnKind::IGet { field_idx, .. }
                    | InsnKind::IPut { field_idx, .. }
                    | InsnKind::SGet { field_idx, .. }
                    | InsnKind::SPut { field_idx, .. } => {
                        let f = dex.field(*field_idx);
                        add_internal(&dex.class_name(f.class_idx), &mut out);
                    }
                    InsnKind::ConstClass { type_idx, .. }
                    | InsnKind::NewInstance { type_idx, .. }
                    | InsnKind::NewArray { type_idx, .. }
                    | InsnKind::FilledNewArray { type_idx, .. }
                    | InsnKind::CheckCast { type_idx, .. }
                    | InsnKind::InstanceOf { type_idx, .. } => {
                        add_desc(dex.type_name(*type_idx), &mut out);
                    }
                    _ => {}
                }
            }
            for h in code.handlers.iter() {
                for (tidx, _) in &h.catches {
                    add_desc(dex.type_name(*tidx), &mut out);
                }
            }
        }
    }
    // the file's OWN family names are not import-shadow risks for
    // themselves (own-simple drops happen later); keep them anyway —
    // a sibling simple equal to a family base only blocks when a
    // same-package OTHER class shares it, which is the true collision.
    out
}

fn compute_obscured_renders(
    pool: &DexPool,
    class: &PoolClass,
    shadow: &jdc_core::FxHashSet<String>,
) -> jdc_core::FxHashMap<String, String> {
    let own_renamed = crate::apply_class_rename(&class.name);
    let own_simple = own_renamed.rsplit(['/', '$']).next().unwrap_or("");
    if own_simple.is_empty() {
        return jdc_core::FxHashMap::default();
    }
    let mut refs: jdc_core::FxHashSet<String> = jdc_core::FxHashSet::default();
    for sup in class.interfaces.iter() {
        refs.insert(sup.clone());
    }
    if let Some(sup) = &class.super_name {
        refs.insert(sup.clone());
    }
    for f in class.static_fields.iter().chain(class.instance_fields.iter()) {
        if let JavaType::Object(n) = crate::desc_type(&f.desc) {
            refs.insert(n.to_string());
        }
    }
    for m in class.all_methods() {
        if let Some(d) = m.parsed_desc() {
            for a in d.args.iter() {
                if let JavaType::Object(n) = a {
                    refs.insert(n.to_string());
                }
            }
            if let JavaType::Object(n) = &d.ret {
                refs.insert(n.to_string());
            }
        }
    }
    let mut out: jdc_core::FxHashMap<String, String> = jdc_core::FxHashMap::default();
    for r in refs {
        // First segment == the class's own simple name: the qualified
        // render would be obscured. The name must exist in the pool for
        // the import to resolve. The KEY stays internal (renders look
        // refs up by internal name), but the import line and the
        // simple name use the RENAMED display — a collision-renamed
        // class renders under its display name and an import of the
        // internal name does not resolve.
        let first = r.split('/').next().unwrap_or("");
        let seg_obscured = first == own_simple || shadow.contains(first);
        if seg_obscured && r.split('/').count() >= 2 && pool.get(&r).is_some() {
            // Own-family refs (the class itself, its nested members)
            // render through member scope — an import would be redundant
            // (self-import) or shadow a same-package sibling named like
            // the tail (`import t.t2.a` hijacks every bare `a` that
            // meant sibling class t.a — lark t/t2).
            if r == class.name || r.starts_with(&format!("{}$", class.name)) {
                continue;
            }
            let display = crate::apply_class_rename(&r);
            let simple = display
                .rsplit(['/', '$'])
                .next()
                .unwrap_or("")
                .to_string();
            if !simple.is_empty() {
                out.insert(r.clone(), simple);
            }
        }
    }
    out
}

pub fn print_class_name(pool: &DexPool, internal: &str) -> String {
    if let Some(simple) = obscured_render_pub(internal) {
        return sanitize_ref(&simple);
    }
    let orig_internal: &str = internal;
    // Framework-shadow stub: the reference means the PLATFORM class —
    // render the raw FQN (only the stub's own FILE identity moved).
    let shadow_skip = crate::is_fw_shadow(internal);
    if std::env::var("DDC_DBG_FW").is_ok() && internal.contains("ImageDecoder") {
        eprintln!("[fw] print_class_name input={internal} skip={shadow_skip}");
    }
    let cow = if shadow_skip {
        std::borrow::Cow::Borrowed(internal)
    } else {
        crate::apply_class_rename(internal)
    };
    let internal: &str = &cow;
    // The pool indexes PRE-rename internals. The ddcroot ROOT-PACKAGE
    // RELOCATION renames `Foo$Bar` to `ddcroot/Foo$Bar` and the pool
    // lookups below must fall back to the pre-rename name, or the
    // class reads as off-pool — the framework-nesting fallback then
    // dots the `$` (`ddcroot.UserCustomStatusExtraParams.
    // CalendarAutomaticStatus` against a FLAT `...$...` declaration —
    // "不可见" ×138 on lark's R8-outer-deleted enum family). STRICTLY
    // the relocation (renamed == rp + "/" + orig): collision-renamed
    // nested families (weibo x0$a$b) keep their pre-existing
    // generic-loop treatment — a general orig fallback flipped their
    // `!pool.get(display)` dotting condition and regressed weibo
    // +483.
    let reloc_orig: Option<&str> = if orig_internal != internal {
        match crate::root_pkg_display() {
            Some(rp)
                if internal.len() > rp.len() + 1
                    && internal.starts_with(rp.as_str())
                    && internal.as_bytes()[rp.len()] == b'/'
                    && &internal[rp.len() + 1..] == orig_internal
                    && pool.get(orig_internal).is_some() =>
            {
                Some(orig_internal)
            }
            _ => None,
        }
    } else {
        None
    };
    let pool_name: &str = if pool.get(internal).is_some() {
        internal
    } else {
        reloc_orig.unwrap_or(internal)
    };
    // Flat EMISSION UNITS: a digit-tail member (anonymous / d8-lambda
    // shape, `Outer$lruCache$1`) is emitted as its own top-level file
    // whose simple name keeps every `$` — the `$` boundaries inside
    // that unit are part of the NAME, not nesting. References must use
    // the flat unit name; the generic loop below dotted the first
    // boundary (`LruCacheKt.lruCache$1`) against the declaration
    // `class LruCacheKt$lruCache$1` — every use of the type failed and
    // javac's attribution for the whole file collapsed (3627 pure-
    // cascade files on weibo).
    if pool_name.contains('$') && pool.get(pool_name).is_some() {
        if let Some(root) = emission_root(pool, pool_name) {
            if root.contains('$') {
                let below = &pool_name[root.len()..];
                if below.is_empty() {
                    // The unit itself: the flat name IS the reference.
                    return sanitize_ref(&internal.replace('/', "."));
                }
                let segs: Vec<&str> = below[1..].split('$').collect();
                if segs.iter().any(|s| s.is_empty()) {
                    // R8 tails like `ThreadMsg$$$` cannot be dotted at
                    // all — keep the whole name flat.
                    return sanitize_ref(&internal.replace('/', "."));
                }
                let root_cow = crate::apply_class_rename(&root);
                let mut out = root_cow.replace('/', ".");
                for s in segs {
                    out.push('.');
                    out.push_str(s);
                }
                return sanitize_ref(&out);
            }
        }
    }
    let mut out = String::new();
    // `known` must test the ACCUMULATED internal prefix, not the bare
    // inter-`$` segment: the per-segment shape checked `pool.get("a")`
    // for the second level of `s5/o$a$b`, missed, and rendered the
    // undeclarable reference `s5.o.a$b` (找不到符号) for every nested-
    // nested type.
    let mut off = 0usize;
    loop {
        let rest = &internal[off..];
        match rest.find('$') {
            Some(i) => {
                let seg = &rest[..i];
                let prefix = &internal[..off + i];
                let known = pool.get(prefix).is_some()
                    || jdc_core::rename::is_renamed_display(prefix)
                    // The FULL name is not a pool class (also under
                    // the ddcroot-relocation pre-rename form — the
                    // migration hides pool classes from the renamed
                    // lookup): this `$` cannot be a literal name
                    // (pool literal classes — an app's own
                    // `View$OnUnhandledKeyEventListener` — keep their
                    // `$` here AND at their declaration), so it can
                    // only be an external framework nesting boundary
                    // (`View$OnClickListener` → `.OnClickListener`).
                    || !(pool.get(internal).is_some() || reloc_orig.is_some());
                // The `$` may only become a nesting dot when the tail
                // segment STARTS a Java identifier: R8's desugared-
                // library names carry `$` inside PACKAGE paths
                // (`j$/util/...` dotted into `j..util`) and suffixes
                // like `Collection$-EL` or anonymous `RequestId$1`
                // cannot be dotted under any reading.
                //
                // Sanitizer-escaped tails DO dot for POOL classes: the
                // injective `_u<hex>` escape turns any non-ASCII (or
                // ASCII-punctuation) leading char into `_`-initial —
                // a valid identifier start — and the declaration side
                // renders the member that way (weibo's unicode-named
                // nested `a$Ꮺ`: decl `class a_u2dIA` inside a, ctor
                // params dotted `a.a_u2dIA`, but casts stayed flat
                // `a$a_u2dIA` — 找不到符号 类 ×~500). Digit and `$`
                // tails stay flat (emission-unit files keep the `$`;
                // framework binary names are unreachable either way).
                let tail_next = rest[i + 1..].chars().next();
                let tail_ok = match tail_next {
                    Some(c) if c.is_ascii_alphabetic() || c == '_' => true,
                    // `/` = a `$` INSIDE the package path (desugar's
                    // `j$/util/...` — dotting produced `j..util`,
                    // deepseek 非法'.' ×3,661); digit tails are flat
                    // emission units; `-`-led tails fail
                    // clean_member_tail and emit flat too; `$` tails
                    // ($$-orphans) keep the old behavior.
                    Some(c) if !c.is_ascii_digit() && c != '$' && c != '/' && c != '-' => {
                        pool.get(internal).is_some()
                    }
                    _ => false,
                };
                out.push_str(&seg.replace('/', "."));
                out.push_str(if known && tail_ok { "." } else { "$" });
                off += i + 1;
            }
            None => {
                out.push_str(&rest.replace('/', "."));
                return sanitize_ref(&out);
            }
        }
    }
}

/// The top-level EMISSION UNIT ancestor of a pool class: walking the
/// `$`-chain upward (outer_of), the first ancestor that is itself
/// emitted as its own compilation unit — a digit-tail rest marks a
/// flat boundary (see top_level_classes). Clean members render inline
/// in their outer, so only the unit's own name may carry `$`.
fn emission_root(pool: &DexPool, internal: &str) -> Option<String> {
    let mut cur = internal.to_string();
    loop {
        let outer = pool.outer_of(&cur)?;
        let rest = cur
            .strip_prefix(outer)
            .and_then(|t| t.strip_prefix('$'))
            .unwrap_or("");
        if !crate::clean_member_tail(rest) {
            return Some(cur);
        }
        cur = outer.to_string();
    }
}

/// Class-file names may contain characters Java source identifiers
/// cannot (`Collection$-EL`); the deterministic mapping matches the
/// declaration sites (java_ident).
/// Every `.`-segment of a fully-qualified name must start a Java
/// identifier: obfuscators emit `package do;` and `..badge.new..` paths.
///
/// PUBLIC on purpose: this is the single source of truth for the
/// declaration↔file-name mapping. The CLI writer used to keep its own
/// copy (`sanitize_file_seg`) which had drifted — it lacked the lone-`_`
/// escape below — so `l.֡` declared `class __` inside a file named
/// `_.java`. Callers reach it through `sanitize_seg`/`sanitize_internal`.
///
/// INJECTIVE on the characters, which is the property the writer needs.
/// The old mapping folded every non-identifier character to `_`, so
/// `l.᩻ܶ`, `l.᩻ۡ` and `l.֫᩷` all became `l.__`: on bin.mt.plus 22,636
/// distinct classes in package `l` collapsed onto three file names, the
/// writer's EEXIST branch overwrote them, and 22,633 sources were
/// destroyed with exit code 0. `_u<hex>` keeps them distinct, is
/// self-describing (it encodes the original code point) and costs the
/// decompiler nothing — the alternative, minting 22,636 registry
/// renames, makes every type reference allocate and took this sample
/// from 5.2 s to 26.8 s.
///
/// The only residual collisions are *lookalikes*: a literal class named
/// `_u1a7b` beside the class `᩻` (same code point), or the pre-existing
/// keyword form `_do` beside `do`. Those are detected and repaired
/// deterministically by `lossy_sanitize_renames`, which is why that pass
/// still exists.
pub fn sanitize_fq(dotted: &str) -> String {
    dotted
        .split('.')
        .map(sanitize_fq_seg)
        .collect::<Vec<_>>()
        .join(".")
}

/// One `.`-segment: escape, then repair identifier-position problems.
fn sanitize_fq_seg(seg: &str) -> String {
    // Ident-safe ASCII passes through unchanged (the overwhelmingly
    // common case, and the only one on the borrow-fast path); every
    // other character — non-ASCII, and ASCII punctuation such as the
    // `-` in `Collection$-EL` — becomes `_u<hex>`.
    let plain = seg
        .chars()
        .all(|c| c.is_ascii() && (c.is_ascii_alphanumeric() || c == '_' || c == '$'));
    let mut out = if plain {
        seg.to_string()
    } else {
        use std::fmt::Write as _;
        let mut s = String::with_capacity(seg.len() + 8);
        for c in seg.chars() {
            if c.is_ascii() && (c.is_ascii_alphanumeric() || c == '_' || c == '$') {
                s.push(c);
            } else {
                s.push_str("_u");
                let _ = write!(s, "{:x}", c as u32);
            }
        }
        s
    };
    // `out` is pure ASCII from here: the identifier-position repairs.
    if is_java_keyword_name(&out)
        || is_restricted_type_name(&out)
        || out.chars().next().is_some_and(|c| c.is_ascii_digit())
    {
        out.insert(0, '_');
    }
    // A lone `_` is a reserved IDENTIFIER since Java 9.
    if out == "_" {
        out = "__".to_string();
    }
    out
}

/// Restricted contextual TYPE names — legal as member/local names
/// (rt.jar compiles `var` locals), illegal in class declarations and
/// type references. Consulted only on CLASS-name paths.
pub(crate) fn is_restricted_type_name(s: &str) -> bool {
    matches!(s, "var" | "yield" | "record" | "sealed" | "permits")
}

fn is_java_keyword_name(s: &str) -> bool {
    matches!(
        s,
        "abstract"
            | "assert"
            | "boolean"
            | "break"
            | "byte"
            | "case"
            | "catch"
            | "char"
            | "class"
            | "const"
            | "continue"
            | "default"
            | "do"
            | "double"
            | "else"
            | "enum"
            | "extends"
            | "final"
            | "finally"
            | "float"
            | "for"
            | "goto"
            | "if"
            | "implements"
            | "import"
            | "instanceof"
            | "int"
            | "interface"
            | "long"
            | "native"
            | "new"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "return"
            | "short"
            | "static"
            | "strictfp"
            | "super"
            | "switch"
            | "synchronized"
            | "this"
            | "throw"
            | "throws"
            | "transient"
            | "try"
            | "void"
            | "volatile"
            | "while"
            | "true"
            | "false"
            | "null"
            // `_` is a reserved identifier since Java 9 (Alipay's
            // instant-run fields are named `_`) — the `_<name>` mapping
            // turns it into `__`, matching jdc-core's call sites.
            | "_"
    )
}

fn sanitize_ref(name: &str) -> String {
    sanitize_fq(name)
}

/// Dotted source form of an internal name.
pub fn dotted_pool(pool: &DexPool, internal: &str) -> String {
    print_class_name(pool, internal)
}

/// Unused import silencer.
#[allow(dead_code)]
fn _unused(_: &dyn Fn(&JavaType) -> jdc_core::types::GenericType) {
    let _ = java_type_to_generic;
}

#[cfg(test)]
mod sanitize_tests {
    use super::sanitize_fq;

    /// The property the file writer depends on: distinct class names must
    /// reach distinct output paths. The lossy predecessor of this mapping
    /// folded every non-identifier character to `_`, so 22,636 classes of
    /// `bin.mt.plus` (names built from Thai/Yi/Syriac code points) became
    /// three (`l.__`, `l.___`, `l._`) and all but three were overwritten.
    #[test]
    fn distinct_obfuscated_names_stay_distinct() {
        // The three names that collapsed onto `l/__.java` in the field.
        let names = ["l.᩻ܶ", "l.᩻ۡ", "l.֫᩷", "l.֡", "l.᩻᩶ۛ"];
        let mut mapped: Vec<String> = names.iter().map(|n| sanitize_fq(n)).collect();
        let before = mapped.len();
        mapped.sort();
        mapped.dedup();
        assert_eq!(mapped.len(), before, "sanitizer folded distinct names: {mapped:?}");
        assert!(
            mapped.iter().all(|m| m.is_ascii()),
            "sanitizer must stay ASCII: {mapped:?}"
        );
        // Self-describing: the escape carries the original code point.
        assert_eq!(sanitize_fq("l.᩻ܶ"), "l._u1a7b_u736");
    }

    /// Every output must be a legal Java identifier segment: no leading
    /// digit, no keyword, not the lone `_` reserved since Java 9.
    #[test]
    fn output_is_a_legal_identifier() {
        for (input, want) in [
            ("do", "_do"),
            ("_", "__"),
            ("0Xx", "_0Xx"),
            ("Collection$-EL", "Collection$_u2dEL"),
            ("᩻", "_u1a7b"),
            ("a", "a"),
            ("A$B", "A$B"),
            ("x_y", "x_y"),
        ] {
            assert_eq!(sanitize_fq(input), want, "input {input:?}");
        }
    }

    /// Documented residual: ident-safe ASCII passes through unchanged, so a
    /// literal `_u1a7b` collides with the single character U+1A7B. The
    /// collision is real — which is why `lossy_sanitize_renames` still owns
    /// a repair pass — and this test pins the shape the guard must catch.
    #[test]
    fn lookalike_collision_is_known_and_bounded() {
        assert_eq!(sanitize_fq("_u1a7b"), sanitize_fq("᩻"));
    }
}
