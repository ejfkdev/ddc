//! Per-method decompilation pipeline:
//! DEX code units → CFG → register→IR lifting (fixpoint with merge phis) →
//! `jdc-core` structuring → statement conversion → refinement passes.

use jdc_core::FxHashMap as HashMap;

use ddc_dex::insn::InsnKind;
use jdc_core::convert::Converter;
use jdc_core::ir::build::BlockResult;
use jdc_core::ir::expr::{Expr, TypeRef};
use jdc_core::ir::stmt::Stmt;
use jdc_core::structure::{reverse_postorder, Structurer};
use jdc_core::types::{JavaType, MethodDescriptor};
use jdc_core::var::VarTable;

use crate::cfg::DexCfg;
use crate::lift::{entry_regs, Lifter, MethodEnv, OutState, Reg};
use crate::passes;
use crate::{DexPool, PoolClass, PoolMethod};

pub struct MethodBody {
    pub body: Stmt,
    pub vt: VarTable,
    pub desc: MethodDescriptor,
}

/// Catch types for the ranges a block handles (drives move-exception typing).
/// Debug-env reads, cached: `env::var` is an environ lock+scan — a
/// per-METHOD cost across 716k methods shows up as __NSGetEnviron in
/// profiles. Once per process is the right granularity.
fn trace_on() -> bool {
    static T: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *T.get_or_init(|| std::env::var("DDC_TRACE").is_ok())
}

fn walk_budget_override() -> Option<u64> {
    static T: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *T.get_or_init(|| {
        std::env::var("DDC_WALKBUDGET")
            .ok()
            .and_then(|v| v.parse().ok())
    })
}

fn walk_work_override() -> Option<u64> {
    static T: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *T.get_or_init(|| {
        std::env::var("DDC_WALKWORK")
            .ok()
            .and_then(|v| v.parse().ok())
    })
}

fn handler_types_for(cfg: &DexCfg, bid: usize) -> Vec<Option<std::sync::Arc<str>>> {
    cfg.blocks[bid]
        .handlers
        .iter()
        .filter_map(|&ri| cfg.exc_ranges.get(ri).map(|r| r.catch_type.clone()))
        .collect()
}

/// Decompile one method. `Err` only for malformed input; unsupported
/// constructs degrade to comments inside the statement tree.
pub fn decompile_method(
    pool: &DexPool,
    class: &PoolClass,
    m: &PoolMethod,
) -> Result<Option<MethodBody>, String> {
    let t0 = std::time::Instant::now();
    if m.is_abstract_or_native() {
        return Ok(None);
    }
    let desc = m
        .parsed_desc()
        .ok_or_else(|| format!("bad descriptor {}", m.desc))?;
    let dex = pool.dex(m.dex_idx).ok_or("missing dex image")?;
    let dex = &*dex;
    let Some(mut code) = dex.code_at(m.code_off) else {
        return Ok(None);
    };
    let insn_count = code.insns.len();
    let param_names = dex.parameter_names(m.debug_info_off);
    // Pool-cached Arc class names: the closure ran dex.class_name
    // (fresh String) per exception range of every method.
    let cfg = DexCfg::build(&mut code, &|ty| pool.type_name_arc(m.dex_idx, ty));
    // Method-wide final-read set for the alloc-inline decisions (the
    // per-block computation mistook a block-tail read for the
    // generation's last read — cross-block alloc consumers minted
    // never-assigned int locals).
    let method_final_reads = crate::lift::compute_final_reads(&cfg);
    let n = cfg.blocks.len();
    let env = MethodEnv {
        pool,
        di: m.dex_idx,
        dex,
        code: &code,
        class_name: class.name.clone(),
        method_name: m.name.clone(),
        desc: desc.clone(),
        debug_locals: dex.debug_locals(m.debug_info_off),
        is_static: m.is_static(),
        code_units: cfg.code_units,
    };
    let mut vt = VarTable::default();
    let entry = entry_regs(&mut vt, &env, &param_names);
    if n == 0 {
        return Ok(None);
    }
    // Pathological inputs (R8-merged model classes, obfuscated switch
    // cascades) can explode the copied-tail rendering; guard up front.
    if insn_count > 30_000 || n > 8_000 {
        return Ok(Some(stub_body(
            format!(
                "$DDC: method too large to decompile ({} insns, {} blocks)",
                insn_count, n
            ),
            desc,
        )));
    }

    // SSA Phase 0 census (read-only, DDC_SSA_REPORT-gated).
    if crate::ssa_census::enabled() {
        crate::ssa_census::census_method(&cfg, &env);
    }

    // Visiting order: reverse postorder, then any stragglers (so every block
    // is built at least once even when unreachable).
    let core_for_order = cfg.to_core();
    let universe: jdc_core::FxHashSet<usize> = (0..n).collect();
    let mut order = reverse_postorder(&core_for_order, core_for_order.entry, &universe);
    {
        let mut seen: jdc_core::FxHashSet<usize> = order.iter().copied().collect();
        for b in &cfg.blocks {
            if seen.insert(b.id) {
                order.push(b.id);
            }
        }
    }

    // Handler blocks: entry state approximated by the try-entry state (the
    // dominant pattern: catch reads values established before the try).
    let mut handler_entry_block: HashMap<usize, usize> = HashMap::default();
    for b in &cfg.blocks {
        if b.handlers.is_empty() {
            continue;
        }
        let mut best: Option<(u32, usize)> = None;
        for &ri in &b.handlers {
            if let Some(r) = cfg.exc_ranges.get(ri) {
                if best.map(|(s, _)| r.start < s).unwrap_or(true) {
                    if let Some(eb) = cfg.block_at(r.start) {
                        best = Some((r.start, eb));
                    }
                }
            }
        }
        if let Some((_, eb)) = best {
            handler_entry_block.insert(b.id, eb);
        }
    }

    // ---- fixpoint: register states across blocks ----
    let mut results: Vec<BlockResult> = Vec::with_capacity(n);
    results.resize_with(n, || BlockResult {
        stmts: vec![],
        out_stack: vec![],
        term: jdc_core::ir::build::Term::Return(None),
    });
    let mut out_states: Vec<Option<OutState>> = vec![None; n];
    // Stable (block, register) → var ids across worklist rebuilds: keeps
    // re-lifts id-deterministic so the fixpoint CONVERGES instead of
    // cascading (was: 30M block-lifts on weibo, ~60 per block).
    let mut stable_vars: HashMap<(usize, u16, u64), u32> = HashMap::default();
    // Method feature flags OR-merged from every block lift: gates
    // post-lift passes that can only match if the bytecode contained the
    // feature (most methods contain none).
    let mut mflags = crate::lift::MethodFlags::none();
    // Try-entry INPUT snapshots (for handler-block approximation) — only
    // try-entry starts (rare) keep their input state; no persistent
    // per-block copies.
    let mut try_entry_snapshots: HashMap<usize, Vec<Reg>> = HashMap::default();
    let mut built_once = vec![false; n];
    let mut errors: HashMap<usize, String> = HashMap::default();
    // (merge block, register) → phi var id.
    let mut phis: HashMap<(usize, u16), u32> = HashMap::default();
    // pred → (write_pc, phi var, value) materializations.

    let entry_is_handler = handler_entry_block.contains_key(&0);

    // Per-visit costs hoisted out of the worklist loop: the try-entry scan
    // (all exc ranges × a binary search per range) and the handler catch
    // types (String clones) re-ran on EVERY visit — visits average 2.5×
    // blocks on real code, far more on monsters.
    let try_entry_flags: Vec<bool> = (0..n)
        .map(|b| {
            !cfg.blocks[b].handlers.is_empty()
                || cfg
                    .exc_ranges
                    .iter()
                    .any(|r| cfg.block_at(r.start) == Some(b))
        })
        .collect();
    let handler_types: Vec<Vec<Option<std::sync::Arc<str>>>> =
        (0..n).map(|b| handler_types_for(&cfg, b)).collect();

    // Worklist fixpoint with version stamps: a block rebuilds only when the
    // OUT-state versions of its inputs changed. This avoids the previous
    // every-round full pass, which deep-cloned and deep-compared the whole
    // register state (Expr trees) of every block — the dominant cost of
    // large methods.
    let mut ver: Vec<u64> = vec![0; n]; // out-state version per block
    let mut built_ver: Vec<u64> = vec![u64::MAX; n]; // input signature when built
                                                     // input signature: entry=0; otherwise XOR/sum of (pred, ver[pred]) pairs.
    fn input_sig(cfg: &DexCfg, bid: usize, ver: &[u64]) -> u64 {
        let mut h: u64 = 1;
        for &p in &cfg.blocks[bid].pred {
            h = h
                .wrapping_mul(31)
                .wrapping_add(p as u64)
                .wrapping_mul(31)
                .wrapping_add(ver[p]);
        }
        // Handler blocks also depend on their try-entry block's input.
        h
    }
    let mut queue: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    let mut queued: Vec<bool> = vec![false; n];
    for &bid in &order {
        queue.push_back(bid);
        queued[bid] = true;
    }
    // merge bid → [(pred, write_pc, phi var, value)] — re-recorded whole on
    // each visit of the merge block (stale rounds must not accumulate).
    let mut appends: HashMap<usize, Vec<(usize, u32, u32, Expr)>> = HashMap::default();
    let mut total_visits: usize = 0;
    let visit_cap: usize = 64 * n + 256;

    while let Some(bid) = queue.pop_front() {
        queued[bid] = false;
        total_visits += 1;
        if total_visits > visit_cap {
            CAP_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            CAP_INSNS.fetch_add(
                insn_count as u64 * 1000 + n as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
            break; // degenerate oscillation guard
        }
        let is_handler = handler_entry_block.contains_key(&bid);
        // 1. Signature gate FIRST: a visit whose input versions are
        //    unchanged skips the (clone-heavy) input-state computation
        //    entirely — that clone was the fixpoint's dominant cost.
        let sig = input_sig(&cfg, bid, &ver)
            ^ (if is_handler {
                let eb = handler_entry_block[&bid];
                (eb as u64) << 32
            } else {
                0
            });
        let need_build = !built_once[bid] || built_ver[bid] != sig;
        if !need_build {
            // Handler blocks depend on their try-entry block's INPUT —
            // when that entry is re-visited, the handler is re-queued; a
            // skipped visit here means the recorded state is current.
            continue;
        }
        // 2. Input register state (only on rebuild).
        let ins: Vec<Reg> = if bid == cfg.entry {
            if cfg.blocks[0].pred.is_empty() || entry_is_handler {
                entry.clone()
            } else {
                // A loop back into pc 0: params are one side of the merge.
                // Sides are BORROWED (cloning each pred's state here was a
                // top fixpoint cost).
                let mut sides: Vec<&[Reg]> = vec![entry.as_slice()];
                for p in cfg.blocks[bid].pred.clone() {
                    if let Some(o) = &out_states[p] {
                        sides.push(o.regs.as_slice());
                    }
                }
                let merged =
                    merge_states(&mut vt, &mut phis, bid, &sides, &env.debug_locals, cfg.blocks[bid].start);
                // The entry side's phi contributions become leading
                // assignments in the entry block (virtual pred). The entry
                // block may be re-visited by the worklist — each visit
                // replaces (not appends) its records.
                let mut recs: Vec<(usize, u32, u32, Expr)> = Vec::new();
                for (r, st) in entry.iter().enumerate() {
                    if let Some(&phi) = phis.get(&(bid, r as u16)) {
                        let value = match st {
                            Reg::Live(v) if *v == phi => continue,
                            Reg::Live(v) => Expr::Local {
                                var: *v,
                                ty: vt.var(*v).ty.clone(),
                            },
                            Reg::Pending(e) | Reg::PendingCall(e) => e.clone(),
                            Reg::Undef | Reg::WideHi => continue,
                        };
                        recs.push((usize::MAX, 0, phi, value));
                    }
                }
                appends.insert(bid, recs);
                merged
            }
        } else if is_handler {
            let eb = handler_entry_block[&bid];
            try_entry_snapshots
                .get(&eb)
                .cloned()
                .unwrap_or_else(|| vec![Reg::Undef; env.code.registers_size as usize])
        } else {
            // Borrowed sides; the single-pred fast path clones once (the
            // lifter needs an owned copy anyway).
            let mut sides: Vec<&[Reg]> = Vec::new();
            for p in cfg.blocks[bid].pred.clone() {
                if let Some(o) = &out_states[p] {
                    sides.push(o.regs.as_slice());
                }
            }
            if sides.is_empty() {
                vec![Reg::Undef; env.code.registers_size as usize]
            } else if sides.len() == 1 {
                sides[0].to_vec()
            } else {
                merge_states(
                    &mut vt,
                    &mut phis,
                    bid,
                    &sides,
                    &env.debug_locals,
                    cfg.blocks[bid].start,
                )
            }
        };

        // 3. Phi materialization records for predecessors — recomputed for
        //    this block only; later pred rebuilds re-visit the merge and
        //    refresh them (the last visit wins).
        if phis_for_merge(bid, &phis) {
            let mut recs: Vec<(usize, u32, u32, Expr)> = Vec::new();
            record_appends(&cfg, bid, &out_states, &phis, &vt, &mut recs);
            // Entry-merge visits also carry the entry-side contributions —
            // preserve those (recorded above when bid == entry).
            if bid == cfg.entry {
                if let Some(prev) = appends.get(&bid) {
                    for (p, pc, v, e) in prev.iter() {
                        if *p == usize::MAX {
                            recs.push((*p, *pc, *v, e.clone()));
                        }
                    }
                }
            }
            appends.insert(bid, recs);
        }

        // 4. Build (signature known changed). The lifter CONSUMES `ins`
        //    (moved, not cloned); try-entry snapshots preserve the input
        //    state for the handler blocks approximating from it.
        built_once[bid] = true;
        built_ver[bid] = sig;
        if try_entry_flags[bid] {
            try_entry_snapshots.insert(bid, ins.clone());
        }
        BUILD_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        INSN_LIFTED.fetch_add(
            cfg.block_ins(&cfg.blocks[bid]).len() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        let htypes = &handler_types[bid];
        let block_ins = cfg.block_ins(&cfg.blocks[bid]);
        let lifter = Lifter::new(&env, &mut vt, ins, bid, &mut stable_vars, &mut mflags);
        let rebuilt = match lifter.build_block(block_ins, htypes, method_final_reads.clone()) {
            Ok((r, out)) => {
                results[bid] = r;
                let changed = match &out_states[bid] {
                    Some(prev) => prev.regs != out.regs,
                    None => true,
                };
                out_states[bid] = Some(out);
                changed
            }
            Err(msg) => {
                results[bid] = BlockResult {
                    stmts: vec![Stmt::Comment(format!("$DDC-BLOCK-ERROR: {msg}"))],
                    out_stack: vec![],
                    term: jdc_core::ir::build::Term::Return(None),
                };
                out_states[bid] = Some(OutState {
                    regs: vec![Reg::Undef; env.code.registers_size as usize],
                    write_pc: vec![0; env.code.registers_size as usize],
                });
                errors.insert(bid, msg);
                false
            }
        };
        if rebuilt {
            ver[bid] += 1;
            // Successors re-check their input signature.
            for &succ in &cfg.blocks[bid].succ {
                if !queued[succ] && succ < n {
                    queue.push_back(succ);
                    queued[succ] = true;
                }
            }
            // Handler blocks watch their try-entry block's INPUT — cheap
            // approximation: also queue the handler of every range this
            // block belongs to.
            if is_handler {
                let eb = handler_entry_block[&bid];
                if !queued[eb] && eb < n {
                    queue.push_back(eb);
                    queued[eb] = true;
                }
            }
            // Try-entry inputs: when this block feeds a handler entry via a
            // try range, queue that handler.
            for &ri in &cfg.blocks[bid].handlers {
                if let Some(r) = cfg.exc_ranges.get(ri) {
                    if let Some(hb) = cfg.block_at(r.handler) {
                        if !queued[hb] && hb < n {
                            queue.push_back(hb);
                            queued[hb] = true;
                        }
                    }
                }
            }
        }
    }

    // Entry-side phi contributions: carried as declaration-with-init and
    // prepended to the method body AFTER conversion (the entry block may be
    // a loop header — its statements re-execute per iteration).
    // Entry-side phi contributions (the virtual pred): captured for the
    // method-top declarations; the REAL preds' records stay in `appends`
    // for the commit loop (removing the whole entry key would lose the
    // loop-body assignments — the phis would collapse to their entry
    // values).
    let mut entry_phi_inits: Vec<(u32, Expr)> = Vec::new();
    if let Some(recs) = appends.get_mut(&cfg.entry) {
        let mut entry_side: Vec<(u32, Expr)> = Vec::new();
        recs.retain(|(p, _, v, e)| {
            if *p == usize::MAX {
                entry_side.push((*v, e.clone()));
                false
            } else {
                true
            }
        });
        entry_side.sort_by_key(|(v, _)| *v);
        entry_phi_inits = entry_side;
    }

    // Commit phi materializations: each pred assigns its outgoing value to
    // the merge var, ordered by the pc of the instruction that produced it.
    // Values that REFERENCE a sibling phi are snapshotted into a fresh temp
    // first: register rotations (`a = b; b = a % b`) would otherwise read
    // the already-reassigned phi (stale capture).
    // Regroup: merge-keyed records → per-pred statement lists.
    //
    // Deterministic order: `adds.sort_by_key(pc)` below is a STABLE sort,
    // so same-pc records (one jump feeding several merge vars) keep
    // their push order — iterating `appends` in map order made that
    // order hasher-dependent (SipHash seeds per process: output was NOT
    // byte-stable across runs; 243/5579 files wobbled on reqable).
    // Sorted block ids here + sorted preds below pin every tie.
    if std::env::var("DDC_DBG_PHI").is_ok() {
        let mut ks: Vec<_> = phis.keys().copied().collect();
        ks.sort();
        for (b, r) in ks {
            eprintln!("[phi] merge={} reg={} var={} name={}", b, r, phis[&(b, r)], vt.var(phis[&(b, r)]).name);
        }
        let mut mo: Vec<usize> = appends.keys().copied().collect();
        mo.sort_unstable();
        for bid in mo {
            for (p, pc, v, e) in &appends[&bid] {
                eprintln!("[append] merge={} pred={} pc={} var={}({}) val={:?}", bid, p, pc, v, vt.var(*v).name, e);
            }
        }
    }
    let mut merge_order: Vec<usize> = appends.keys().copied().collect();
    merge_order.sort_unstable();
    let mut per_pred: HashMap<usize, Vec<(u32, u32, Expr)>> = HashMap::default();
    for bid in merge_order {
        for (p, pc, v, e) in &appends[&bid] {
            per_pred.entry(*p).or_default().push((*pc, *v, e.clone()));
        }
    }
    let mut pred_order: Vec<usize> = per_pred.keys().copied().collect();
    pred_order.sort_unstable();
    for p in pred_order {
        let mut adds = per_pred.remove(&p).unwrap();
        if p == usize::MAX {
            continue;
        }
        adds.sort_by_key(|(pc, _, _)| *pc);
        let term_is_exit = matches!(
            results[p].term,
            jdc_core::ir::build::Term::Return(_) | jdc_core::ir::build::Term::Throw(_)
        );
        if term_is_exit {
            continue;
        }
        let phi_set: std::collections::HashSet<u32> = adds.iter().map(|(_, v, _)| *v).collect();
        let mut snapshots: Vec<Stmt> = Vec::new();
        // (phi, value) pairs with snapshot temps substituted.
        let mut emitted: Vec<(u32, Expr)> = Vec::new();
        for (_, v, e) in &adds {
            let mut e = e.clone();
            let mut refs_phi = false;
            visit_phi_refs(&e, &phi_set, &mut refs_phi);
            if refs_phi {
                // Snapshot: emit `temp = value` BEFORE any phi assignment.
                let ty = e.type_ref();
                let id = vt.vars.len() as u32;
                let name = format!("v{}", id);
                vt.vars.push(jdc_core::var::VarInfo {
                    id,
                    slot: u16::MAX,
                    name,
                    ty: ty.clone(),
                    is_param: false,
                    range_start: 0,
                    range_end: u16::MAX,
                    synthetic_name: true,
                });
                snapshots.push(Stmt::LocalDef {
                    var: id,
                    init: Some(e),
                    is_final: false,
                    force_type: true,
                });
                e = Expr::Local { var: id, ty };
            }
            emitted.push((*v, e));
        }
        for st in snapshots {
            results[p].stmts.push(st);
        }
        for (v, e) in emitted {
            results[p].stmts.push(Stmt::ExprStmt(Expr::Assign {
                target: Box::new(Expr::Local {
                    var: v,
                    ty: vt.var(v).ty.clone(),
                }),
                op: jdc_core::ir::expr::AssignOp::Plain,
                value: Box::new(e),
            }));
        }
    }

    let t_fix = t0;
    phase_hit_n(0, n, t_fix);
    // ---- single-block fast path ----
    // A lone basic block is straight-line code (any branch/switch/handler
    // splits the graph): no merges, no phis, no control flow to structure.
    // Bypassing the structurer + converter + the control-flow passes for
    // these (over half of R8-produced methods) cuts the dominant cost.
    if n == 1 && !entry_is_handler && cfg.blocks[0].pred.is_empty() {
        let r = std::mem::replace(
            &mut results[0],
            BlockResult {
                stmts: vec![],
                out_stack: vec![],
                term: jdc_core::ir::build::Term::Goto,
            },
        );
        let mut body = r.stmts;
        match r.term {
            jdc_core::ir::build::Term::Return(Some(e)) => {
                body.push(Stmt::Return(Some(e)));
            }
            jdc_core::ir::build::Term::Return(None) => {}
            jdc_core::ir::build::Term::Throw(e) => body.push(Stmt::Throw(e)),
            _ => {}
        }
        let mut body = Stmt::Block(body);
        passes::fused_expr_rewrites(&mut body, &vt);
        if mflags.has_sb() {
            passes::fold_string_builders(&mut body, &vt);
        }
        passes::forward_single_use(&mut body, &vt);
        passes::cleanup(&mut body);
        passes::inline_accessors(&mut body, pool, &class.name);
        passes::infer_types(&mut vt, &mut body, &desc.ret, &env);
        passes::fix_ref_null_assigns(&vt, &mut body);
        passes::fix_null_sentinels(&mut body, &vt, &desc.ret);
        passes::split_generations(&mut vt, &mut body, pool);
        passes::insert_object_narrowing_casts(&vt, &mut body, pool, &desc.ret);
        passes::fix_field_owner_downcasts(&mut body, &vt, pool);
        passes::fix_shadowed_super_fields(&mut body, &vt, pool, &class.name);
        passes::fix_incomparable_equality(&mut body, &vt, pool);
        passes::fix_primitive_assign_casts(&vt, &mut body, &desc.ret);
        passes::fix_primitive_arg_bridges(&mut body, &vt, pool);
        passes::fix_bool_xor(&mut body, &vt, matches!(desc.ret, JavaType::Boolean));
        passes::fix_int_operand_bridges(&mut body, &vt);
        passes::fix_ref_array_null_consts(&mut body);
        passes::idiom_compounds(&mut body, &vt);
        passes::rescue_primitive_receivers(&mut body, &vt, pool);
        passes::fix_this0_owners(&mut body, &class.name);
        passes::apply_local_names(&mut vt, &body);
    passes::deshadow_locals(&mut vt, pool, &class.name, &body);
        passes::remove_kotlin_checks(&mut body);
        passes::rewrite_kotlin_facades(&mut body, pool);
        passes::platform_constants(&mut body);
        passes::drop_dead_locals(&mut body);
    passes::drop_dead_raw_news(&mut body);
    passes::drop_empty_finallies(&mut body);
    passes::drop_pure_value_stmts(&mut body);
    passes::mark_field_owner_concrete(&mut vt, &mut body);
    passes::ensure_declared(&mut body, &vt);
    passes::rescue_arg_return_swaps(&mut body, &vt, pool, &desc.ret);
    // AFTER the declaration hoisting: ensure_declared inserts bare
    // top-of-method declarations, and a local declaration ahead of the
    // super() call is still "super must be first statement".
    if &*m.name == "<init>" {
        // Boolean value-diamond folds ahead of the delegation machinery:
        // a branched bridge ctor whose else arm COMPUTES the defaulted
        // boolean (`v=false; if(x!=null){u=x.m(); v=false; if(u){v=true}}`)
        // is Dirty for split_branch — folded to `v = x!=null && x.m()`
        // (interleaved forwarding inlines the impure `u` the fold
        // exposes) the arm is linear defs and merge_at lifts the this()
        // (lark MmCreateAudioRequest, this-not-first ×456 family).
        passes::fold_bool_value_diamonds(&mut body, &vt);
        passes::fold_value_diamonds(&mut body, &vt);
        passes::fix_ctor_this_aliases(&mut body, &vt);
        // Vendor hotfix ctor guards wrap the delegation inside an If and
        // fire for ENUM ctors too (alipay InstantRun fallback enums are
        // the whole 1,436 ctor-not-first population) — run before the
        // enum/regular split, and before the classdec trace-param store
        // injection whose delegation-first probe must see the real head.
        passes::strip_ctor_hotfix_guards(&mut body);
        if class.is_enum() {
            passes::fold_enum_default_arg_bridge(&mut body);
            // Strip the implicit Enum.<init> super() ONLY for the enum
            // BASE (super is java/lang/Enum or Object — a true `enum`
            // decl or the fallback `/* enum */ class`, both get an
            // implicit Object/Enum super). An enum CONSTANT SUBCLASS
            // (j$6, super = the enum base j) renders as a plain
            // `class j$6 extends j`; its ctor MUST forward
            // `super(name, ordinal, ..)` or the implicit no-arg super()
            // fails ("对于j(没有参数), 找不到合适的构造器", weibo jsoup
            // TokeniserState ×134).
            let const_subclass = class
                .super_name
                .as_ref()
                .map(|s| s != "java/lang/Object" && s != "java/lang/Enum")
                .unwrap_or(false);
            if !const_subclass {
                passes::strip_enum_ctor_super(&mut body);
            }
        } else {
            passes::fix_ctor_delegation_arg_defs(&mut body);
            passes::fix_ctor_conditional_super(&mut body);
            passes::fix_ctor_super_first(&mut body, &vt);
            passes::dedupe_ctor_delegations(&mut body);
            passes::strip_secondary_delegations(&mut body);
            passes::hoist_branch_delegations(&mut body, &vt);
            passes::extract_branched_delegation_helper(&mut body, &vt, pool, class, &desc.to_string());
        }
        // The delegation merges/hoists can leave prelude decls dead
        // (their defs were inlined into the merged call) — the dropper
        // ran BEFORE the ctor passes, so re-run it on their output.
        passes::drop_dead_locals(&mut body);
    }
        passes::strip_trailing_void_return(&mut body);
        passes::cleanup(&mut body);
        passes::invert_empty_thens(&mut body);
        passes::fold_short_circuits(&mut body);
        passes::resolve_dangling_gotos(&mut body);
    passes::dedupe_multicatch(&mut body, pool);
    passes::strip_phantom_field_writes(&mut body, pool);
    passes::init_bare_decls(&mut body, &vt);
    passes::final_dead_assigns(&mut body);
        if !errors.is_empty() {
            passes::prepend_comment(
                &mut body,
                format!("$DDC: {} block(s) failed to decompile", errors.len()),
            );
        }
        phase_hit_n(2, n, t_fix);
        record_bucket(n, t0);
        return Ok(Some(MethodBody { body, vt, desc }));
    }

    // ---- structure + convert ----
    // jcdc's copy-budget degradation: a rendering whose statement tree
    // explodes past the guard retries at a halved copy budget (the
    // structurer's tail-copying is the exponential mechanism).
    const TREE_GUARD: usize = 20_000;
    let core_cfg = cfg.to_core();
    #[allow(unused_assignments)]
    let mut body: Option<Stmt> = None;
    let mut prev_size: Option<usize> = None;
    // Copy budget scales with graph size: many-block methods are where the
    // exponential tail-copy blowups live (weibo: 389 methods >100 blocks
    // = 40% of all CPU at budget 512). Starting lower converges without
    // the 6-round halving.
    let mut budget: u32 = if n > 100 {
        64
    } else if n > 20 {
        128
    } else {
        512
    };
    // Shared once-per-method artifacts: the O(groups²) exception-group
    // scan and the full-graph dominator tree used to run TWICE per
    // method (Structurer + Converter) and once more per copy-budget
    // retry — monster classes (gson adapters) spent the bulk of their
    // time there.
    let groups = jdc_core::structure::group_exceptions_with(&core_cfg, Some(&results));
    let dom_universe: jdc_core::FxHashSet<usize> = (0..core_cfg.blocks.len()).collect();
    let dom = jdc_core::structure::compute_dominators(&core_cfg, &dom_universe, core_cfg.entry);
    if trace_on() {
        eprintln!("[mshape] {}.{} blocks={}", class.name, m.name, n);
    }
    // Deterministic giant-method stub: at ~2.1k+ CFG blocks a single
    // structuring walk can burn 9s+ of wall time (weibo Gson doRead:
    // 2,179/3,230 blocks — per-visit dominator-pass cost scales far
    // faster than the Σ universe.len() charge model can bound, and the
    // spread across CFG shapes defeats any charge calibration). Healthy
    // methods below the line (JsonUserInfoTypeAdapter, 1,997 blocks)
    // structure fully, so stubbing beats both alternatives: burning the
    // class watchdog DROPS the whole file (every referrer cannot-finds)
    // and the old wall-clock deadline flipped output nondeterministically
    // under worker contention.
    if n >= 2_100 {
        return Ok(Some(stub_body(
            format!(
                "$DDC: method too large ({} blocks) — body elided for determinism",
                n
            ),
            desc,
        )));
    }
    #[cfg(feature = "visit-stats")]
    {
        jdc_core::structure::reset_visit_stats();
    }
    // Walk-visit budget, proportional to the block count: walk() runs
    // roughly once per structured scope for sane CFGs, so 40n+512 is
    // generous headroom — but the EXPONENTIAL explorations (Telegram
    // SendMessagesHelper family: deep if/loop/try nesting whose scope
    // count multiplies) consume visits geometrically and get cut to a
    // Goto, which terminates the walk. Without this a single <16k-insn
    // method can recurse for effectively forever while the worker (and
    // the process) never finishes — Telegram's full run wrote every file
    // yet hung for 300+ seconds on 25 such classes.
    let walk_budget: u64 = walk_budget_override().unwrap_or(8 * n as u64 + 128);
    // WORK budget (Σ universe.len() per visit): deterministic bound on
    // total walk cost — the count budget above under-bounds per-visit
    // cost for big-block exponential explorations (Telegram family),
    // which the wall-clock deadline used to catch nondeterministically.
    // A visit's REAL cost grows with the universe (dominator-style
    // passes: weibo doRead n=2179 burned 15µs per charge unit vs 5µs at
    // n=1533), so a flat charge budget still lets giants run 9s+ per
    // rung and trip the class watchdog (dropped files — every referrer
    // cannot-finds — are strictly worse than walk-cut degradation).
    let walk_work_budget: u64 = walk_work_override()
        .unwrap_or(600_000 + 128 * n as u64);
    // No wall-clock deadline here: the work budget above bounds walk
    // cost deterministically, and the old 1500ms guard flipped
    // borderline giants (weixin vm/a0.n, 28.5k nodes) between full body
    // and exploded stub run-to-run under worker contention. The
    // class-level 5s watchdog in classdec remains as the hang safety
    // net for pathological CFGs.
    loop {
        jdc_core::structure::set_budget_override(Some(budget));
        jdc_core::structure::set_walk_visit_budget(Some(walk_budget));
        jdc_core::structure::set_walk_work_budget(Some(walk_work_budget));
        let mut st = Structurer::with_shared_groups(
            &core_cfg,
            &results,
            &groups,
            Default::default(),
            Default::default(),
        );
        let tw0 = std::time::Instant::now();
        let region = st.structure_method();
        let tw1 = std::time::Instant::now();
        let mut converter = Converter::with_precomputed_ref(&core_cfg, &results, &groups, &dom);
        // The dump needs `region` AFTER convert consumed it — clone only
        // then; the hot path pays nothing.
        let dump_ir = std::env::var("DDC_DUMP_IR").ok().filter(|f| m.name.contains(f));
        let saved_region = dump_ir.as_deref().map(|_| region.clone());
        let candidate = converter.convert(region);
        if let (Some(_), Some(r)) = (dump_ir.as_deref(), saved_region) {
            eprintln!("[region-dump] {}.{}\n{:#?}", class.name, m.name, r);
            eprintln!("[stmt-dump] {}.{}\n{:#?}", class.name, m.name, candidate);
        }
        let tw2 = std::time::Instant::now();
        if trace_on() {
            eprintln!(
                "[rung] {}.{} budget={} walk={:?} convert={:?}",
                class.name, m.name, budget, tw1 - tw0, tw2 - tw1
            );
        }
        jdc_core::structure::set_budget_override(None);
        jdc_core::structure::set_walk_visit_budget(None);
        jdc_core::structure::set_walk_work_budget(None);
        let size = {
            let mut c = 0usize;
            passes::count_stmts_deep(&candidate, &mut c);
            c
        };
        if trace_on() {
            eprintln!("[guard] {}.{} size={} budget={}", class.name, m.name, size, budget);
        }
        if size <= TREE_GUARD {
            body = Some(candidate);
            break;
        }
        // Giant short-circuit: deep in the ladder (budget ≤ 64), a tree
        // still 3× over the guard will not shrink under further copy-budget
        // halving (weixin cdp/l1.w: 72,782 nodes at 64, ~2.8s of walk PER
        // rung) — stub now instead of burning two more rungs and tripping
        // the classdec 5s watchdog, which DROPS the whole file (every
        // referrer cannot-finds — strictly worse than one stubbed body).
        // Deterministic: driven by measured size, not wall-clock.
        if budget <= 64 && size > 3 * TREE_GUARD {
            return Ok(Some(stub_body(
                format!(
                    "$DDC: statement tree exploded ({} nodes) — pathological CFG, body elided",
                    size
                ),
                desc,
            )));
        }
        // No-progress short-circuit: the copy budget only bounds tail
        // duplication — if halving it did not shrink the tree at all,
        // further rungs cannot either (weixin vm/a0.n sits at ~28.5k
        // across every rung, burning a full work-budget walk each).
        // Deterministic, and saves the giant methods ~3 rungs of walk.
        if budget <= 32 && prev_size.is_some_and(|p| size >= p) {
            return Ok(Some(stub_body(
                format!(
                    "$DDC: statement tree exploded ({} nodes) — pathological CFG, body elided",
                    size
                ),
                desc,
            )));
        }
        prev_size = Some(size);
        if budget <= 16 {
            return Ok(Some(stub_body(
                format!(
                    "$DDC: statement tree exploded ({} nodes) even at copy budget 16",
                    size
                ),
                desc,
            )));
        }
        budget /= 2;
    }
    #[cfg(feature = "visit-stats")]
    if trace_on() && n > 3 {
        let used = jdc_core::structure::walk_visits_consumed();
        eprintln!(
            "[visits] {}.{} blocks={} consumed={} ratio={:.1}",
            class.name,
            m.name,
            n,
            used,
            used as f64 / n as f64
        );
    }
    phase_hit_n(1, n, t_fix);
    let mut body = body.unwrap();
    if !entry_phi_inits.is_empty() {
        let head: Vec<Stmt> = entry_phi_inits
            .into_iter()
            .map(|(v, e)| Stmt::LocalDef {
                var: v,
                init: Some(e),
                is_final: false,
                force_type: true,
            })
            .collect();
        body = match body {
            Stmt::Block(v) => {
                let mut nv = head;
                nv.extend(v);
                Stmt::Block(nv)
            }
            other => {
                let mut nv = head;
                nv.push(other);
                Stmt::Block(nv)
            }
        };
    }

    passes::bind_catches(&mut body, &mut vt);
    passes::prune_unreachable(&mut body);
    // One traversal for cmp residuals + null compares + const-first flips
    // (three separate full-tree walks before).
    passes::fused_expr_rewrites(&mut body, &vt);
    if mflags.has_sb() {
        passes::fold_string_builders(&mut body, &vt);
    }
    passes::ternary_fold(&mut body);
    passes::drop_empty_finallies(&mut body);
    passes::drop_pure_value_stmts(&mut body);
    if mflags.has_monitor() {
        passes::fold_synchronized(&mut body);
    }
    passes::forward_single_use(&mut body, &vt);
    passes::cleanup(&mut body);
    passes::inline_accessors(&mut body, pool, &class.name);
    passes::infer_types(&mut vt, &mut body, &desc.ret, &env);
    passes::fix_ref_null_assigns(&vt, &mut body);
    passes::fix_null_sentinels(&mut body, &vt, &desc.ret);
    passes::split_generations(&mut vt, &mut body, pool);
    // Post-booleanize re-split, gated on a nonzero conversion count:
    // conversions expose register reuse across boolean/numeric kinds no
    // earlier pass could see (`int v150` copying converted `boolean
    // v131` — weixin ConstraintLayout ×1.4k); with nothing converted
    // the fixpoint walk is pure decomp-time cost (the weixin 16→24s
    // regression).
    // Booleanize ↔ split fixpoint, bounded, ENDING on a booleanize:
    // each split MINTS fresh generation vars that inherit the stale int
    // type while receiving boolean values (`int v264_g160_g7_g3 =
    // !v200` with `!= 0` reads — weixin bool→int assign family, 2.8k
    // lines). Only a FOLLOWING booleanize retypes them and folds their
    // `v != 0` reads; a trailing split would leave the folds undone.
    {
        let rb = matches!(desc.ret, JavaType::Boolean);
        let mut rounds = 0;
        loop {
            let c = passes::booleanize(&mut vt, &mut body, rb);
            rounds += 1;
            if c == 0 || rounds >= 3 {
                break;
            }
            passes::split_generations(&mut vt, &mut body, pool);
        }
    }
    if matches!(
        desc.ret,
        JavaType::Int | JavaType::Long | JavaType::Short | JavaType::Byte | JavaType::Float | JavaType::Double
    ) {
        passes::fix_int_returns(&vt, &mut body);
    }
    if matches!(desc.ret, JavaType::Boolean) {
        passes::fix_bool_returns(&vt, &mut body);
    }
    passes::insert_object_narrowing_casts(&vt, &mut body, pool, &desc.ret);
    passes::fix_field_owner_downcasts(&mut body, &vt, pool);
    passes::fix_shadowed_super_fields(&mut body, &vt, pool, &class.name);
    passes::fix_incomparable_equality(&mut body, &vt, pool);
    passes::fix_primitive_assign_casts(&vt, &mut body, &desc.ret);
    passes::fix_primitive_arg_bridges(&mut body, &vt, pool);
    passes::fix_bool_xor(&mut body, &vt, matches!(desc.ret, JavaType::Boolean));
    passes::fix_int_operand_bridges(&mut body, &vt);
    passes::fix_ref_array_null_consts(&mut body);
    passes::idiom_compounds(&mut body, &vt);
    passes::rescue_primitive_receivers(&mut body, &vt, pool);
    passes::fix_this0_owners(&mut body, &class.name);
    passes::apply_local_names(&mut vt, &body);
    passes::deshadow_locals(&mut vt, pool, &class.name, &body);
    passes::remove_kotlin_checks(&mut body);
    passes::rewrite_kotlin_facades(&mut body, pool);
    passes::platform_constants(&mut body);
    passes::drop_dead_locals(&mut body);
    passes::drop_dead_raw_news(&mut body);
    passes::mark_field_owner_concrete(&mut vt, &mut body);
    passes::ensure_declared(&mut body, &vt);
    passes::rescue_arg_return_swaps(&mut body, &vt, pool, &desc.ret);
    // AFTER the declaration hoisting: ensure_declared inserts bare
    // top-of-method declarations, and a local declaration ahead of the
    // super() call is still "super must be first statement".
    if &*m.name == "<init>" {
        // Boolean value-diamond folds ahead of the delegation machinery:
        // a branched bridge ctor whose else arm COMPUTES the defaulted
        // boolean (`v=false; if(x!=null){u=x.m(); v=false; if(u){v=true}}`)
        // is Dirty for split_branch — folded to `v = x!=null && x.m()`
        // (interleaved forwarding inlines the impure `u` the fold
        // exposes) the arm is linear defs and merge_at lifts the this()
        // (lark MmCreateAudioRequest, this-not-first ×456 family).
        passes::fold_bool_value_diamonds(&mut body, &vt);
        passes::fold_value_diamonds(&mut body, &vt);
        passes::fix_ctor_this_aliases(&mut body, &vt);
        // Vendor hotfix ctor guards wrap the delegation inside an If and
        // fire for ENUM ctors too (alipay InstantRun fallback enums are
        // the whole 1,436 ctor-not-first population) — run before the
        // enum/regular split, and before the classdec trace-param store
        // injection whose delegation-first probe must see the real head.
        passes::strip_ctor_hotfix_guards(&mut body);
        if class.is_enum() {
            passes::fold_enum_default_arg_bridge(&mut body);
            // Strip the implicit Enum.<init> super() ONLY for the enum
            // BASE (super is java/lang/Enum or Object — a true `enum`
            // decl or the fallback `/* enum */ class`, both get an
            // implicit Object/Enum super). An enum CONSTANT SUBCLASS
            // (j$6, super = the enum base j) renders as a plain
            // `class j$6 extends j`; its ctor MUST forward
            // `super(name, ordinal, ..)` or the implicit no-arg super()
            // fails ("对于j(没有参数), 找不到合适的构造器", weibo jsoup
            // TokeniserState ×134).
            let const_subclass = class
                .super_name
                .as_ref()
                .map(|s| s != "java/lang/Object" && s != "java/lang/Enum")
                .unwrap_or(false);
            if !const_subclass {
                passes::strip_enum_ctor_super(&mut body);
            }
        } else {
            passes::fix_ctor_delegation_arg_defs(&mut body);
            passes::fix_ctor_conditional_super(&mut body);
            passes::fix_ctor_super_first(&mut body, &vt);
            passes::dedupe_ctor_delegations(&mut body);
            passes::strip_secondary_delegations(&mut body);
            passes::hoist_branch_delegations(&mut body, &vt);
            passes::extract_branched_delegation_helper(&mut body, &vt, pool, class, &desc.to_string());
        }
        // The delegation merges/hoists can leave prelude decls dead
        // (their defs were inlined into the merged call) — the dropper
        // ran BEFORE the ctor passes, so re-run it on their output.
        passes::drop_dead_locals(&mut body);
    }
    if &*m.name == "<clinit>" {
        passes::strip_clinit_returns(&mut body);
    }
    passes::strip_trailing_void_return(&mut body);
    passes::cleanup(&mut body);
    // Short-circuit folding last: the diamond shapes are final only
    // after cleanup merges singleton blocks.
    passes::invert_empty_thens(&mut body);
    passes::fold_short_circuits(&mut body);
    passes::resolve_dangling_gotos(&mut body);
    passes::dedupe_multicatch(&mut body, pool);
    passes::strip_phantom_field_writes(&mut body, pool);
    passes::init_bare_decls(&mut body, &vt);
    passes::final_dead_assigns(&mut body);

    if std::env::var("DDC_DBG_PHI").is_ok() {
        for (id, v) in vt.vars.iter().enumerate() {
            if matches!(v.name.as_str(), "p02x" | "p03x" | "linkedList10" | "linkedList" | "p0x") {
                eprintln!("[vt-final] v{} name={} ty={} slot={} synth={}", id, v.name, v.ty.erased(), v.slot, v.synthetic_name);
            }
        }
    }
    if !errors.is_empty() {
        passes::prepend_comment(
            &mut body,
            format!("$DDC: {} block(s) failed to decompile", errors.len()),
        );
    }

    phase_hit_n(2, n, t_fix);
    record_bucket(n, t0);
    Ok(Some(MethodBody { body, vt, desc }))
}

static PHASE_MICROS: [std::sync::atomic::AtomicU64; 5] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];
/// Phase × bucket attribution (5 phases × 5 buckets, row-major).
static PHASE_BUCKET_MICROS: [std::sync::atomic::AtomicU64; 25] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];
static PHASE_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn bucket_of(n: usize) -> usize {
    if n <= 1 {
        0
    } else if n <= 5 {
        1
    } else if n <= 20 {
        2
    } else if n <= 100 {
        3
    } else {
        4
    }
}

pub(crate) fn phase_hit_n(i: usize, n: usize, t: std::time::Instant) {
    if !PHASE_ON.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let us = t.elapsed().as_micros() as u64;
    PHASE_MICROS[i].fetch_add(us, std::sync::atomic::Ordering::Relaxed);
    PHASE_BUCKET_MICROS[i * 5 + bucket_of(n)].fetch_add(us, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn phase_hit(i: usize, t: std::time::Instant) {
    if !PHASE_ON.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    PHASE_MICROS[i].fetch_add(
        t.elapsed().as_micros() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// Enable phase timers (DDC_PHASES=1).
pub fn phases_enable() {
    PHASE_ON.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// (fixpoint, structure+convert, passes, print, total) in micros.
pub fn phases_dump() -> [u64; 5] {
    let mut out = [0u64; 5];
    for i in 0..5 {
        out[i] = PHASE_MICROS[i].load(std::sync::atomic::Ordering::Relaxed);
    }
    out
}

/// Per-bucket phase micros: 5 phases × 5 buckets, row-major.
pub fn phases_bucket_dump() -> [[u64; 5]; 5] {
    let mut out = [[0u64; 5]; 5];
    for i in 0..5 {
        for b in 0..5 {
            out[i][b] = PHASE_BUCKET_MICROS[i * 5 + b].load(std::sync::atomic::Ordering::Relaxed);
        }
    }
    out
}

static CAP_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static CAP_INSNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

static BUILD_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static INSN_LIFTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

static BUCKET_COUNT: [std::sync::atomic::AtomicU64; 5] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];
static BUCKET_MICROS: [std::sync::atomic::AtomicU64; 5] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Dump build/insn counters.
pub fn dump_builds() -> (u64, u64) {
    (
        BUILD_COUNT.load(std::sync::atomic::Ordering::Relaxed),
        INSN_LIFTED.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// (cap-hit methods, sum of insns*1000+blocks for cap-hit methods).
pub fn dump_caps() -> (u64, u64) {
    (
        CAP_HITS.load(std::sync::atomic::Ordering::Relaxed),
        CAP_INSNS.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// Dump DDC_BUCKETS statistics (global atomics).
pub fn dump_buckets() -> [(u64, u64); 5] {
    let mut out = [(0u64, 0u64); 5];
    for i in 0..5 {
        out[i] = (
            BUCKET_COUNT[i].load(std::sync::atomic::Ordering::Relaxed),
            BUCKET_MICROS[i].load(std::sync::atomic::Ordering::Relaxed),
        );
    }
    out
}

/// Dominator-recompute counters from jdc-core (perf diagnostics).
pub fn dom_counters() -> (u64, u64) {
    (
        jdc_core::structure::DOM_CALLS.load(std::sync::atomic::Ordering::Relaxed),
        jdc_core::structure::DOM_BLOCKS.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// Enable dominator-recompute counters (diagnostics; jdc-core gate).
pub fn dom_counters_enable() {
    jdc_core::structure::set_dom_counters(true);
}

/// A minimal body carrying a diagnostic comment.
fn stub_body(msg: String, desc: MethodDescriptor) -> MethodBody {
    MethodBody {
        body: Stmt::Block(vec![Stmt::Comment(msg)]),
        vt: VarTable::default(),
        desc,
    }
}

/// True when any phi belongs to merge block `bid`.
fn phis_for_merge(bid: usize, phis: &HashMap<(usize, u16), u32>) -> bool {
    phis.keys().any(|(b, _)| *b == bid)
}

/// Phi materialization records for `bid`'s predecessors (pred-tagged).
fn record_appends(
    cfg: &DexCfg,
    bid: usize,
    out_states: &[Option<OutState>],
    phis: &HashMap<(usize, u16), u32>,
    vt: &jdc_core::var::VarTable,
    out: &mut Vec<(usize, u32, u32, Expr)>,
) {
    for p in cfg.blocks[bid].pred.clone() {
        let Some(o) = &out_states[p] else { continue };
        for (r, st) in o.regs.iter().enumerate() {
            if let Some(&phi) = phis.get(&(bid, r as u16)) {
                let value = match st {
                    Reg::Live(v) if *v == phi => continue,
                    Reg::Live(v) => Expr::Local {
                        var: *v,
                        ty: vt.var(*v).ty.clone(),
                    },
                    Reg::Pending(e) | Reg::PendingCall(e) => e.clone(),
                    Reg::Undef | Reg::WideHi => continue,
                };
                out.push((p, o.write_pc.get(r).copied().unwrap_or(0), phi, value));
            }
        }
    }
}

#[allow(unused)]
fn record_bucket(n: usize, t0: std::time::Instant) {
    // (Perf: env::var is a lock+lookup — per-METHOD cost on 716k-method
    // runs; resolve once via OnceLock.)
    static BUCKETS_ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*BUCKETS_ON.get_or_init(|| std::env::var("DDC_BUCKETS").is_ok()) {
        return;
    }
    let bucket = if n <= 1 {
        0
    } else if n <= 5 {
        1
    } else if n <= 20 {
        2
    } else if n <= 100 {
        3
    } else {
        4
    };
    BUCKET_COUNT[bucket].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    BUCKET_MICROS[bucket].fetch_add(
        t0.elapsed().as_micros() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// True when the expression references any of the phi vars.
fn visit_phi_refs(e: &Expr, phis: &std::collections::HashSet<u32>, hit: &mut bool) {
    if *hit {
        return;
    }
    if let Expr::Local { var, .. } = e {
        if phis.contains(var) {
            *hit = true;
        }
        return;
    }
    let mut kids: Vec<&Expr> = Vec::new();
    match e {
        Expr::Un { e, .. } | Expr::Cast { e, .. } | Expr::InstanceOf { e, .. } => kids.push(e),
        Expr::Bin { l, r, .. } => {
            kids.push(l);
            kids.push(r);
        }
        Expr::Cond { c, t, f } => {
            kids.push(c);
            kids.push(t);
            kids.push(f);
        }
        Expr::Assign { target, value, .. } => {
            kids.push(target);
            kids.push(value);
        }
        Expr::PreIncDec { e, .. } | Expr::PostIncDec { e, .. } => kids.push(e),
        Expr::Field { owner: Some(o), .. } => kids.push(o),
        Expr::Method {
            owner: Some(o),
            args,
            ..
        } => {
            kids.push(o);
            kids.extend(args.iter());
        }
        Expr::ArrayIndex { array, index } => {
            kids.push(array);
            kids.push(index);
        }
        Expr::New { args, .. } => kids.extend(args.iter()),
        Expr::NewArray { dims, init, .. } => {
            kids.extend(dims.iter());
            if let Some(v) = init {
                kids.extend(v.iter());
            }
        }
        Expr::NewMultiArray { dims, .. } => kids.extend(dims.iter()),
        _ => {}
    }
    for k in kids {
        visit_phi_refs(k, phis, hit);
        if *hit {
            return;
        }
    }
}

/// Merge several predecessor register states: pass identical values
/// through, otherwise materialize into a phi var for `(block, register)`.
/// Debug-info name covering `(pc, slot)`, sanitized (shared by the
/// lifter and the merge path).
fn debug_name_at(
    locals: &[ddc_dex::DebugLocal],
    slot: u16,
    pc: u32,
    ty: &jdc_core::types::JavaType,
) -> Option<String> {
    if locals.is_empty() {
        return None;
    }
    let hit = locals.iter().find(|l| {
        l.reg == slot
            && pc >= l.start
            && pc < l.end
            && l.ty.as_deref().map(|d| &crate::desc_type(d) == ty).unwrap_or(true)
    })?;
    let n = crate::classdec::java_ident(&hit.name);
    if n.is_empty() {
        None
    } else {
        Some(n.into_owned())
    }
}

fn merge_states(
    vt: &mut VarTable,
    phis: &mut HashMap<(usize, u16), u32>,
    bid: usize,
    sides: &[&[Reg]],
    debug_locals: &[ddc_dex::DebugLocal],
    merge_pc: u32,
) -> Vec<Reg> {
    let len = sides.iter().map(|s| s.len()).max().unwrap_or(0);
    let mut out = vec![Reg::Undef; len];
    for (r, slot) in out.iter_mut().enumerate() {
        let mut all_eq = true;
        let mut first: Option<&Reg> = None;
        for s in sides {
            let v = s.get(r);
            match (first, v) {
                (None, v) => first = v,
                (Some(f), Some(v)) if f == v => {}
                _ => all_eq = false,
            }
        }
        if all_eq {
            *slot = first.cloned().unwrap_or(Reg::Undef);
            continue;
        }
        // Diverged: phi var.
        if std::env::var("DDC_DBG_PHI").is_ok() {
            let sides_dbg: Vec<String> = sides
                .iter()
                .map(|s| match s.get(r) {
                    Some(Reg::Live(v)) => format!("L{}({})", v, vt.var(*v).ty.erased()),
                    Some(Reg::Pending(e) | Reg::PendingCall(e)) => format!("P({})", e.type_ref().erased()),
                    _ => "U".into(),
                })
                .collect();
            eprintln!("[phi-div] merge={} reg={} sides={:?}", bid, r, sides_dbg);
        }
        let key = (bid, r as u16);
        let phi = match phis.get(&key) {
            Some(&v) => v,
            None => {
                let ty = join_side_types(vt, sides, r);
                // Loop-carried values are usually THE source local (the
                // `char[] out` / `int i` of a loop): name the phi from the
                // debug range covering the merge block's head. When a var
                // of that name+type already exists, REUSE it — per-block
                // copies of one source local degrade to out/out2/out3
                // suffix soup, and one register+type = one Java local is
                // exactly Dalvik semantics.
                if let Some(n) = debug_name_at(debug_locals, r as u16, merge_pc, &ty.erased()) {
                    let unified = vt
                        .vars
                        .iter()
                        .find(|v| {
                            v.slot == r as u16
                                && !v.synthetic_name
                                && v.name == n
                                && v.ty.erased() == ty.erased()
                        })
                        .map(|v| v.id);
                    let id = match unified {
                        Some(id) => id,
                        None => {
                            let id = vt.vars.len() as u32;
                            vt.vars.push(jdc_core::var::VarInfo {
                                id,
                                slot: r as u16,
                                name: n,
                                ty,
                                is_param: false,
                                range_start: 0,
                                range_end: u16::MAX,
                                synthetic_name: false,
                            });
                            while vt.by_slot.len() <= r {
                                vt.by_slot.push(Vec::new());
                            }
                            vt.by_slot[r].push((0, u16::MAX, id));
                            id
                        }
                    };
                    phis.insert(key, id);
                    id
                } else {
                    let id = vt.vars.len() as u32;
                    vt.vars.push(jdc_core::var::VarInfo {
                        id,
                        slot: r as u16,
                        name: format!("v{}", id),
                        ty,
                        is_param: false,
                        range_start: 0,
                        range_end: u16::MAX,
                        synthetic_name: true,
                    });
                    while vt.by_slot.len() <= r {
                        vt.by_slot.push(Vec::new());
                    }
                    vt.by_slot[r].push((0, u16::MAX, id));
                    phis.insert(key, id);
                    id
                }
            }
        };
        *slot = Reg::Live(phi);
    }
    out
}

/// Type of a phi var: join of the non-null side values' erased types.
fn join_side_types(vt: &VarTable, sides: &[&[Reg]], r: usize) -> TypeRef {
    let mut ty: Option<JavaType> = None;
    for s in sides {
        let st = s.get(r).cloned().unwrap_or(Reg::Undef);
        let e = match st {
            Reg::Pending(e) | Reg::PendingCall(e) => e,
            Reg::Live(v) => Expr::Local {
                var: v,
                ty: vt.var(v).ty.clone(),
            },
            _ => continue,
        };
        let t = e.type_ref().erased();
        ty = Some(match ty {
            None => t,
            Some(prev) => join_types(&prev, &t),
        });
    }
    TypeRef::J(ty.unwrap_or(JavaType::Int))
}

fn join_types(a: &JavaType, b: &JavaType) -> JavaType {
    if a == b {
        return a.clone();
    }
    let num = |t: &JavaType| {
        matches!(
            t,
            JavaType::Boolean
                | JavaType::Byte
                | JavaType::Char
                | JavaType::Short
                | JavaType::Int
                | JavaType::Long
                | JavaType::Float
                | JavaType::Double
        )
    };
    if num(a) && num(b) {
        // Numeric promotion.
        return match (a, b) {
            (JavaType::Double, _) | (_, JavaType::Double) => JavaType::Double,
            (JavaType::Float, _) | (_, JavaType::Float) => JavaType::Float,
            (JavaType::Long, _) | (_, JavaType::Long) => JavaType::Long,
            _ => JavaType::Int,
        };
    }
    if let (JavaType::Object(x), JavaType::Object(y)) = (a, b) {
        if x == y {
            return a.clone();
        }
        // Char/int ternaries join to int; otherwise objects join to Object.
        return JavaType::Object("java/lang/Object".into());
    }
    JavaType::Object("java/lang/Object".into())
}

/// True when the block's terminator exits (used by callers to skip appends).
#[allow(dead_code)]
fn is_exit_term(term: &jdc_core::ir::build::Term) -> bool {
    matches!(
        term,
        jdc_core::ir::build::Term::Return(_) | jdc_core::ir::build::Term::Throw(_)
    )
}

#[allow(dead_code)]
fn unused(_: &InsnKind) {}
