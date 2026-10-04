//! Register-machine expression lifting: Dalvik instructions → jdc-core IR.
//!
//! The register file is modeled as per-register *views* of values:
//! * `Live(v)` — the register equals local `v`, whose defining statement was
//!   already emitted (SSA-ish: every materialization allocates a fresh var,
//!   so a `Local` reference can never go stale);
//! * `Pending(e)` — the register holds the expression `e`, whose store is
//!   deferred so consumers can nest it (`return a + b`);
//! * `PendingCall(e)` — the value of a just-executed invoke /
//!   filled-new-array whose statement has not been emitted yet: consumed
//!   exactly once (a materialization, a term, or a discarded-value
//!   statement), never duplicated;
//! * `WideHi` — the high half of a J/D pair.
//!
//! At block exit the terminator is built FIRST (terms may carry pending
//! call expressions: `return foo();`), then unconsumed calls become
//! statements and impure pendings materialize, so cross-block states only
//! carry pure values, locals, and open `new`/`new-array` views.

use ddc_dex::insn::{ArithOp, CmpKind, CmpOp, Insn, InsnKind, InvokeKind, Payload};
use ddc_dex::{CodeItem, DexFile};
use jdc_core::ir::build::{has_side_effects, BlockResult, SwitchTargets, Term};
use jdc_core::ir::expr::{AssignOp, BinOp, ConcatPart, ConstVal, Expr, TypeRef, UnOp};
use jdc_core::ir::stmt::Stmt;
use jdc_core::types::{JavaType, MethodDescriptor};
use jdc_core::var::{VarInfo, VarTable};

use crate::DexPool;

pub type BResult<T> = Result<T, String>;

/// Cheap per-method feature flags accumulated by the lifter: which
/// post-lift passes could possibly match anything. Most methods use none of
/// these — gating their passes skips full-tree analysis walks.
#[derive(Clone, Copy, Default, PartialEq)]
pub struct MethodFlags(u8);

impl MethodFlags {
    const SB: u8 = 1;
    const MONITOR: u8 = 2;
    const CMP: u8 = 4;
    pub fn none() -> MethodFlags {
        MethodFlags(0)
    }
    pub fn with_sb(self) -> MethodFlags {
        MethodFlags(self.0 | Self::SB)
    }
    pub fn with_monitor(self) -> MethodFlags {
        MethodFlags(self.0 | Self::MONITOR)
    }
    pub fn with_cmp(self) -> MethodFlags {
        MethodFlags(self.0 | Self::CMP)
    }
    pub fn has_sb(self) -> bool {
        self.0 & Self::SB != 0
    }
    pub fn has_monitor(self) -> bool {
        self.0 & Self::MONITOR != 0
    }
    pub fn has_cmp(self) -> bool {
        self.0 & Self::CMP != 0
    }
    pub fn merge(self, o: MethodFlags) -> MethodFlags {
        MethodFlags(self.0 | o.0)
    }
}

/// Everything one method body needs from its DEX image.
pub struct MethodEnv<'a> {
    pub pool: &'a DexPool,
    /// Image index of `dex` in the pool (keys the pool's ref caches).
    pub di: usize,
    pub dex: &'a DexFile,
    pub code: &'a CodeItem,
    pub class_name: String,
    pub method_name: std::sync::Arc<str>,
    pub desc: MethodDescriptor,
    /// Debug-info local table (empty for release/stripped builds):
    /// real source names per (register, pc-range).
    pub debug_locals: Vec<ddc_dex::DebugLocal>,
    pub is_static: bool,
    /// Total code units — the CFG owns the insns stream, so the lifter reads
    /// the extent from here instead of `code.insns` (hollowed after the
    /// CFG build moved it).
    pub code_units: u32,
}

impl<'a> MethodEnv<'a> {
    pub fn type_name(&self, idx: u32) -> String {
        self.dex.type_name(idx).to_string()
    }
    pub fn java_type(&self, idx: u32) -> JavaType {
        // Shared per-image parse (was desc-reparse per instruction).
        (*self.pool.type_java(self.di, idx)).clone()
    }
    pub fn type_name_arc(&self, idx: u32) -> std::sync::Arc<str> {
        self.pool.type_name_arc(self.di, idx)
    }
    pub fn field_ref(
        &self,
        idx: u32,
    ) -> (
        std::sync::Arc<str>,
        std::sync::Arc<str>,
        JavaType,
    ) {
        let f = self.dex.field(idx);
        let owner = std::sync::Arc::from(self.dex.class_name(f.class_idx).as_str());
        let raw = self.dex.string(f.name_idx);
        // Colliding obfuscated field names (synthetic outer ref renamed
        // to a one-char name beside a real field) resolve differently per
        // site in source — run the shared rename registry so every
        // reference prints the same display name as the declaration.
        let name = if jdc_core::rename::owner_has_renames(&owner) {
            jdc_core::rename::field_display(&owner, raw, self.dex.type_name(f.type_idx))
                .map(std::sync::Arc::from)
                .unwrap_or_else(|| std::sync::Arc::from(raw))
        } else {
            std::sync::Arc::from(raw)
        };
        (owner, name, self.java_type(f.type_idx))
    }
    pub fn method_ref(
        &self,
        idx: u32,
    ) -> (
        std::sync::Arc<str>,
        std::sync::Arc<str>,
        std::sync::Arc<MethodDescriptor>,
    ) {
        // Was: N param Strings + format! + descriptor RE-PARSE per invoke
        // (6-10 allocations per call instruction). All three components
        // are shared table entries now — refcount bumps.
        let m = self.dex.method(idx);
        let owner = std::sync::Arc::from(self.dex.class_name(m.class_idx).as_str());
        let desc = self.pool.proto_desc_parsed(self.di, m.proto_idx);
        // Sanitizer-collapsed method names (whole `ERROR_中文` families on
        // weibo render one `ERROR________`) need the shared member-rename
        // registry so declarations and call sites agree. Cold path only:
        // clean corpora never rebuild the descriptor string.
        let name = if jdc_core::rename::owner_has_renames(&owner) {
            let raw = self.dex.string(m.name_idx);
            jdc_core::rename::field_display(&owner, raw, &desc.to_string())
                .map(std::sync::Arc::from)
                .unwrap_or_else(|| std::sync::Arc::from(raw))
        } else {
            std::sync::Arc::from(self.dex.string(m.name_idx))
        };
        (owner, name, desc)
    }
}

/// Per-register value view (see module docs).
#[derive(Debug, Clone, PartialEq)]
pub enum Reg {
    Undef,
    Live(u32),
    Pending(Expr),
    PendingCall(Expr),
    WideHi,
}

/// Block-exit register state (machine-neutral view plus origin tracking).
#[derive(Debug, Clone)]
pub struct OutState {
    pub regs: Vec<Reg>,
    /// Per register: the pc of the instruction that produced its value
    /// (orders merge materializations by original instruction order).
    pub write_pc: Vec<u32>,
}

/// The registers an instruction READS as operands (receivers, arguments,
/// move sources — destination registers excluded). Drives the block
/// lookahead that decides when an allocation view must materialize.
pub(crate) fn src_regs(kind: &InsnKind) -> Vec<u16> {
    let mut out: Vec<u16> = Vec::with_capacity(4);
    let mut push = |r: u16| out.push(r);
    match kind {
        InsnKind::Nop
        | InsnKind::ReturnVoid
        | InsnKind::Unknown
        | InsnKind::NewInstance { .. }
        | InsnKind::SGet { .. } => {}
        InsnKind::Move { src, .. }
        | InsnKind::Un { src, .. }
        | InsnKind::Return { src }
        | InsnKind::ArrayLength { src, .. } => push(*src),
        InsnKind::MonitorEnter { reg }
        | InsnKind::MonitorExit { reg }
        | InsnKind::Throw { reg }
        | InsnKind::CheckCast { reg, .. }
        | InsnKind::FillArrayData { reg, .. }
        | InsnKind::PackedSwitch { reg, .. }
        | InsnKind::SparseSwitch { reg, .. } => push(*reg),
        InsnKind::MoveResult { .. } | InsnKind::MoveException { .. } => {}
        InsnKind::Const { .. }
        | InsnKind::ConstClass { .. }
        | InsnKind::ConstString { .. }
        | InsnKind::ConstMethodHandle { .. }
        | InsnKind::ConstMethodType { .. } => {}
        InsnKind::InstanceOf { src, .. } => {
            push(*src);
        }
        InsnKind::NewArray { size, .. } => {
            push(*size);
        }
        InsnKind::FilledNewArray { regs, .. } | InsnKind::InvokeCustom { regs, .. } => {
            for r in regs {
                push(*r);
            }
        }
        InsnKind::Goto { .. } => {}
        InsnKind::Cmp { a, b, .. } | InsnKind::Bin { a, b, .. } => {
            push(*a);
            push(*b);
        }
        InsnKind::BinLit { a, .. } => push(*a),
        InsnKind::If { a, b, z, .. } => {
            push(*a);
            if !*z {
                push(*b);
            }
        }
        InsnKind::AGet { array, index, .. } => {
            push(*array);
            push(*index);
        }
        InsnKind::APut {
            value,
            array,
            index,
            ..
        } => {
            push(*value);
            push(*array);
            push(*index);
        }
        InsnKind::IGet { obj, .. } => {
            push(*obj);
        }
        InsnKind::IPut { value, obj, .. } => {
            push(*value);
            push(*obj);
        }
        InsnKind::SPut { value, .. } => {
            push(*value);
        }
        InsnKind::Invoke { regs, .. } => {
            for r in regs {
                push(*r);
            }
        }
    }
    out
}

/// Registers an instruction READS as operands.
/// Registers an instruction DEFINES (new value generations).
fn dst_regs(kind: &InsnKind) -> Vec<u16> {
    let mut out: Vec<u16> = Vec::with_capacity(2);
    let mut push = |r: u16| out.push(r);
    match kind {
        InsnKind::Move { dst, .. }
        | InsnKind::MoveResult { dst }
        | InsnKind::MoveException { dst }
        | InsnKind::Const { dst, .. }
        | InsnKind::ConstClass { dst, .. }
        | InsnKind::ConstString { dst, .. }
        | InsnKind::ConstMethodHandle { dst, .. }
        | InsnKind::ConstMethodType { dst, .. }
        | InsnKind::NewInstance { dst, .. }
        | InsnKind::NewArray { dst, .. }
        | InsnKind::InstanceOf { dst, .. }
        | InsnKind::ArrayLength { dst, .. }
        | InsnKind::Cmp { dst, .. }
        | InsnKind::AGet { dst, .. }
        | InsnKind::IGet { dst, .. }
        | InsnKind::SGet { dst, .. }
        | InsnKind::Un { dst, .. }
        | InsnKind::Bin { dst, .. }
        | InsnKind::BinLit { dst, .. } => push(*dst),
        _ => {}
    }
    out
}

/// The (pc, reg) reads that are the final read of the register's current
/// generation: walking the register's events in pc order, a read is
/// final when no other read of the same register precedes its next WRITE
/// (a write starts a new generation — the old value is dead there).
pub(crate) fn compute_final_reads(
    cfg: &crate::cfg::DexCfg,
) -> std::sync::Arc<jdc_core::FxHashSet<(u32, u16)>> {
    use jdc_core::FxHashMap as HashMap;
    // reg → (pc, is_read) events in EXECUTION order, not pc order.
    //
    // pc order interleaves exception handlers into the normal flow:
    // their move-exception definitions sat BETWEEN a protected block's
    // read and the reads of the blocks that actually follow it, and the
    // "next event is a write ⇒ final read" rule consumed allocation
    // views at reads that still had live uses (weixin a03/j.get: the
    // handler's `move-exception v0` at 0024 shadowed the iput read at
    // 001c; the New view got consumed there, the create path's phi
    // value became Undef and the method returned the stale pre-write
    // read). Reverse postorder over NORMAL edges matches execution;
    // exception-only and dead blocks are appended after — their own
    // events still participate, they just no longer interrupt.
    let order = cfg.rpo_order();
    let mut events: HashMap<u16, Vec<(u32, bool)>> = HashMap::default();
    for &bid in &order {
        for i in cfg.block_ins(&cfg.blocks[bid]) {
            for r in src_regs(&i.kind) {
                events.entry(r).or_default().push((i.pc, true));
            }
            for r in dst_regs(&i.kind) {
                events.entry(r).or_default().push((i.pc, false));
            }
        }
    }
    let mut out = jdc_core::FxHashSet::default();
    for (reg, evs) in events {
        let mut dedup: Vec<(u32, bool)> = Vec::with_capacity(evs.len());
        for (pc, is_read) in evs {
            if dedup.last() != Some(&(pc, is_read)) {
                dedup.push((pc, is_read));
            }
        }
        for (i, &(pc, is_read)) in dedup.iter().enumerate() {
            if !is_read {
                continue;
            }
            // Final read of the current generation: no other read before
            // the register's next write.
            let mut final_ = true;
            if let Some(&(_, is_read2)) = dedup.get(i + 1) {
                if is_read2 {
                    final_ = false;
                }
            }
            if final_ {
                out.insert((pc, reg));
            }
        }
    }
    std::sync::Arc::new(out)
}

pub struct Lifter<'a> {
    pub env: &'a MethodEnv<'a>,
    pub vt: &'a mut VarTable,
    /// Owning block id (stable-var registry key).
    block_id: usize,
    /// (block, register) → var id, shared across REBUILDS: a re-lift of
    /// the same block with the same input reuses the same materialized
    /// variables, so the output state is id-stable. Without this, every
    /// rebuild allocated fresh ids, the out-state compared unequal, and
    /// the worklist cascaded re-queuing until the visit cap (~60
    /// rebuilds per block, 30M lifts on weibo).
    stable: &'a mut jdc_core::FxHashMap<(usize, u16, u64), u32>,
    regs: Vec<Reg>,
    write_pc: Vec<u32>,
    stmts: Vec<Stmt>,
    pending_call: Option<Expr>,
    code_units: u32,
    /// Current instruction's pc (drives the allocation materialization
    /// lookahead below).
    cur_pc: u32,
    /// (pc, register) reads that are the FINAL read of the register's
    /// current value generation: no other read before the register's
    /// next write. An allocation view (post-fold `New` / `NewArray`)
    /// may only be inlined at a final read; a register reused for a
    /// call result and read again (greet → move-result v0 → println(v0))
    /// is a new generation, not another use of the allocation.
    final_read: std::sync::Arc<jdc_core::FxHashSet<(u32, u16)>>,
    /// Method-level feature flags, merged in place as features are seen
    /// (build_block consumes the lifter, so the flags must escape via a
    /// shared reference rather than a field read afterwards).
    mflags: &'a mut MethodFlags,
    /// pcs of field reads whose value view MUST materialize at its
    /// defining instruction: a later same-field write in this block would
    /// otherwise kill the view while it is still pending, and the use-site
    /// re-render would read the field AFTER the write (y5/e$b.a:
    /// `iget v6,this.h.f; iput this.h=null; invoke …(v6)` rendered
    /// `this.h = null; j((String) this.h.f, …)` — runtime NPE).
    force_mat: jdc_core::FxHashSet<u32>,
}

impl<'a> Lifter<'a> {
    pub fn new(
        env: &'a MethodEnv<'a>,
        vt: &'a mut VarTable,
        in_regs: Vec<Reg>,
        block_id: usize,
        stable: &'a mut jdc_core::FxHashMap<(usize, u16, u64), u32>,
        mflags: &'a mut MethodFlags,
    ) -> Lifter<'a> {
        let n = env.code.registers_size as usize;
        let n = n.max(in_regs.len()).max(1);
        let mut regs = in_regs;
        regs.resize(n, Reg::Undef);
        Lifter {
            env,
            vt,
            regs,
            write_pc: vec![0; n],
            stmts: Vec::new(),
            block_id,
            stable,
            pending_call: None,
            code_units: env.code_units,
            cur_pc: 0,
            final_read: std::sync::Arc::new(jdc_core::FxHashSet::default()),
            mflags,
            force_mat: jdc_core::FxHashSet::default(),
        }
    }

    // -- variable creation ---------------------------------------------------

    /// Cheap 64-bit type fingerprint for the stable-var key: hashing the
    /// JavaType itself (string bytes + tuple) ran on EVERY register
    /// materialization; the fingerprint is one pass with no clones
    /// (`erased()` deep-clones Array chains). G is unreachable in DEX
    /// lift (no generic signatures) — a coarse tag suffices.
    fn ty_key(ty: &TypeRef) -> u64 {
        use std::hash::{Hash, Hasher};
        fn jt_key(j: &JavaType, h: &mut jdc_core::fx::FxHasher) {
            match j {
                JavaType::Void => 1u64.hash(h),
                JavaType::Boolean => 2u64.hash(h),
                JavaType::Byte => 3u64.hash(h),
                JavaType::Char => 4u64.hash(h),
                JavaType::Short => 5u64.hash(h),
                JavaType::Int => 6u64.hash(h),
                JavaType::Float => 7u64.hash(h),
                JavaType::Long => 8u64.hash(h),
                JavaType::Double => 9u64.hash(h),
                JavaType::Object(n) => {
                    10u64.hash(h);
                    n.hash(h);
                }
                JavaType::Array(inner) => {
                    11u64.hash(h);
                    jt_key(inner, h);
                }
            }
        }
        let mut h = jdc_core::fx::FxHasher::default();
        match ty {
            TypeRef::J(j) => jt_key(j, &mut h),
            TypeRef::G(_) => 99u64.hash(&mut h),
        }
        h.finish()
    }

    fn fresh_var(&mut self, slot: u16, ty: TypeRef) -> u32 {
        // Keyed by TYPE as well as (block, slot): d8 reuses one register
        // for values of different types (`v1: byte[]` then `v1: String`
        // in the same method — the lab package's Main.run). A Java local
        // cannot change type — the old (block, slot) key reused one var
        // and then RETYPED it in place, emitting `byte[] v1 = …; v1 =
        // Obf.s(v1, 90);` (String into byte[]) and kin all over the
        // corpus. Distinct types mint distinct vars; rebuild stability
        // (the worklist-convergence fix) holds because the key is a pure
        // function of the instruction stream.
        let key = (self.block_id, slot, Self::ty_key(&ty));
        if let Some(&id) = self.stable.get(&key) {
            return id;
        }
        // Real source name from the debug-info local table when a range
        // covers (pc, slot); otherwise the synthetic `vN`. apply_local_
        // names never renames non-synthetic vars and de-duplicates the
        // (legal-in-source, illegal-in-flat-Java) same-name scopes.
        if let Some(name) = self.debug_name(slot, &ty) {
            // Unify same register+type+name vars across blocks: the debug
            // table says they are ONE source local, and per-block copies
            // degrade to `out`/`out2`/`out3` suffix soup after de-dup.
            // Dalvik registers are method-scoped, so one Java local shared
            // by every write site is exactly register semantics.
            let tkey = Self::ty_key(&ty);
            if let Some(id) = self
                .vt
                .vars
                .iter()
                .find(|v| {
                    v.slot == slot
                        && !v.synthetic_name
                        && v.name == name
                        && Self::ty_key(&v.ty) == tkey
                })
                .map(|v| v.id)
            {
                self.stable.insert(key, id);
                return id;
            }
            let id = self.vt.vars.len() as u32;
            self.stable.insert(key, id);
            self.push_var(id, slot, name, ty, false);
            return id;
        }
        let id = self.vt.vars.len() as u32;
        let name = format!("v{}", id);
        self.stable.insert(key, id);
        self.push_var(id, slot, name, ty, true);
        id
    }

    fn push_var(&mut self, id: u32, slot: u16, name: String, ty: TypeRef, synthetic: bool) {
        self.vt.vars.push(VarInfo {
            id,
            slot,
            name,
            ty,
            is_param: false,
            range_start: 0,
            range_end: u16::MAX,
            synthetic_name: synthetic,
        });
        while self.vt.by_slot.len() <= slot as usize {
            self.vt.by_slot.push(Vec::new());
        }
        self.vt.by_slot[slot as usize].push((0, u16::MAX, id));
    }

    /// The source-level name covering `(cur_pc, slot)` in the debug
    /// local table, sanitized into a legal Java identifier. Linear scan:
    /// the table is empty in release APKs (early-out) and small in
    /// debug builds.
    fn debug_name(&self, slot: u16, ty: &TypeRef) -> Option<String> {
        let locals = &self.env.debug_locals;
        if locals.is_empty() {
            return None;
        }
        let want = ty.erased();
        let covers = |pc: u32| {
            locals.iter().find(|l| {
                l.reg == slot
                    && pc >= l.start
                    && pc < l.end
                    // Type gate: register reuse mints vars of a DIFFERENT
                    // type inside the same range (`new-array v0, v0` reads
                    // the old v0 as an int size inside the new v0's [C
                    // range) — naming that junk var stole the real name.
                    && l.ty.as_deref().map(|d| crate::desc_type(d) == want).unwrap_or(true)
            })
        };
        // Materializations at block merges run BEFORE any instruction of
        // the block (cur_pc is stale/zero) — fall back to the pc where
        // the register's current value was BORN (write_pc), which is
        // what the debug range actually describes.
        let hit = covers(self.cur_pc).or_else(|| {
            let wpc = *self.write_pc.get(slot as usize)?;
            if wpc == 0 {
                None
            } else {
                covers(wpc)
            }
        })?;
        let name = crate::classdec::java_ident(&hit.name);
        if name.is_empty() {
            return None;
        }
        Some(name.into_owned())
    }

    fn local_expr(&self, v: u32) -> Expr {
        Expr::Local {
            var: v,
            ty: self.vt.var(v).ty.clone(),
        }
    }

    /// Registers still holding a move-object alias of the folded
    /// construction in `r`. The ctor fold writes one `new C(args)` view
    /// into every copy of the allocation (keyed by the origin write_pc
    /// the Move handler propagates from the new-instance site): one
    /// copy being consumed or materialized must retire the whole set —
    /// a surviving copy would re-emit the construction.
    fn folded_new_aliases(&self, r: u16) -> Vec<usize> {
        let Some(w) = self.write_pc.get(r as usize).copied() else {
            return Vec::new();
        };
        (0..self.regs.len())
            .filter(|&rr| {
                rr != r as usize
                    && self.write_pc.get(rr).copied() == Some(w)
                    && matches!(self.regs[rr], Reg::Pending(Expr::New { raw: false, .. }))
            })
            .collect()
    }

    // -- register access -----------------------------------------------------

    /// Read for nesting into another expression (clones the value).
    #[track_caller]
    fn read_nest(&mut self, r: u16) -> Expr {
        match self.regs.get(r as usize).cloned().unwrap_or(Reg::Undef) {
            Reg::Undef | Reg::WideHi => {
                // Reading a register never written on this path (or beyond
                // the frame): a fresh (never-assigned) local keeps the
                // output compilable.
                let ty = TypeRef::J(JavaType::Int);
                let v = self.fresh_var(r, ty);
                if (r as usize) < self.regs.len() {
                    self.regs[r as usize] = Reg::Live(v);
                }
                self.local_expr(v)
            }
            Reg::Live(v) => self.local_expr(v),
            Reg::Pending(e) => {
                // An allocation view (`NewArray`, or a post-fold `New`)
                // may be emitted inline only when THIS read is the
                // register's final one: any later read would re-emission
                // a FRESH allocation (`sput v0, sparse; aput v2, v0, v1`
                // printed three distinct `new byte[4]`; a discarded-
                // result StringBuilder chain printed one `new
                // StringBuilder()` per append). Materialize when a later
                // read exists; inline the last use, keeping
                // `foo(new Bar())` nesting. `New { raw: true }` is
                // always exempt: the constructor fold consumes the raw
                // view at the invoke-direct site.
                //
                // A folded construction ALIASED into a second register
                // (move-object) counts as a later read too — one `new
                // C(args)` cannot be inlined at one alias and survive as
                // a pending copy on the other. Materialize so both
                // aliases share the single construction var.
                //
                // RAW pendings are no longer exempt from materialization
                // either (only from the fold path, which reads the
                // register state directly): when the ctor invoke never
                // arrives (init lost to a degraded region), every read
                // used to clone a FRESH `new Q.d()` — multi-allocation
                // semantic corruption rendered as `new Q.d().d2 = v35;`
                // temp-write shapes (uuyc ctor-mismatch ×1.4k).
                let alias_needs_share = matches!(&e, Expr::New { raw: false, .. })
                    && self.folded_new_aliases(r).iter().any(|&rr| {
                        self.final_read
                            .iter()
                            .any(|&(pc2, r2)| r2 == rr as u16 && pc2 >= self.cur_pc)
                    });
                let alloc = alias_needs_share
                    || matches!(&e, Expr::NewArray { .. })
                    || (matches!(&e, Expr::New { .. })
                        && !self.final_read.contains(&(self.cur_pc, r)));
                if alloc {
                    let v = self.materialize(r);
                    self.local_expr(v)
                } else {
                    // Inline AND consume: this is the register's final
                    // read in the block, yet leaving the Pending view in
                    // place made the block-exit materialization emit the
                    // SAME allocation a second time (`this.last = new
                    // Report(..); new Report(..);` — a duplicated
                    // observable side effect; lab package Guard.report).
                    // `New { raw: true }` is EXEMPT: the ctor fold reads
                    // the raw view at the invoke-direct site and must
                    // still find it (consuming it there broke every
                    // constructor fold).
                    if matches!(&e, Expr::NewArray { .. })
                        || matches!(&e, Expr::New { raw: false, .. })
                    {
                        let aliases = if matches!(&e, Expr::New { raw: false, .. }) {
                            self.folded_new_aliases(r)
                        } else {
                            Vec::new()
                        };
                        if let Some(slot) = self.regs.get_mut(r as usize) {
                            *slot = Reg::Undef;
                        }
                        // Consuming the last use retires the aliased
                        // copies too (none of them is read again — the
                        // share branch above materialized otherwise).
                        for rr in aliases {
                            self.regs[rr] = Reg::Undef;
                        }
                    }
                    e
                }
            }
            Reg::PendingCall(_) => {
                let v = self.materialize(r);
                self.local_expr(v)
            }
        }
    }

    /// Read for a context that evaluates exactly once (term payloads): the
    /// value is consumed, not cloned — otherwise the block-exit
    /// materialization would emit it a second time.
    fn read_term(&mut self, r: u16) -> Expr {
        match self.regs.get(r as usize).cloned().unwrap_or(Reg::Undef) {
            Reg::PendingCall(e) | Reg::Pending(e) => {
                // Terms terminate the block: no later read can follow, so a
                // folded construction consumed here retires its aliased
                // copies as well.
                let aliases = if matches!(&e, Expr::New { raw: false, .. }) {
                    self.folded_new_aliases(r)
                } else {
                    Vec::new()
                };
                if (r as usize) < self.regs.len() {
                    self.regs[r as usize] = Reg::Undef;
                }
                for rr in aliases {
                    self.regs[rr] = Reg::Undef;
                }
                self.pending_call = None;
                e
            }
            _ => self.read_nest(r),
        }
    }

    fn write(&mut self, r: u16, e: Expr, pc: u32, wide: bool) {
        if r as usize >= self.regs.len() {
            return;
        }
        self.write_pc[r as usize] = pc;
        // A view that a later same-field write in this block kills must
        // materialize HERE (at the bytecode read position), or the use
        // re-renders the field read past the write.
        if self.force_mat.contains(&pc) {
            let e = value_of_cmp(&e);
            let v = self.materialize_value(r, e);
            self.regs[r as usize] = Reg::Live(v);
            if wide {
                self.mark_wide_hi(r + 1, pc);
            }
            return;
        }
        // Loop-carried Pending trees grow multiplicatively across fixpoint
        // rounds (merge clones the whole tree per side per visit); past
        // this size the register materializes, which caps every state at
        // a bounded expression over stable locals.
        if expr_size(&e) > 32 {
            self.write_pc[r as usize] = pc;
            let v = self.materialize_value(r, e);
            if (r as usize) < self.regs.len() {
                self.regs[r as usize] = Reg::Live(v);
            }
            if wide {
                self.mark_wide_hi(r + 1, pc);
            }
            return;
        }
        self.regs[r as usize] = Reg::Pending(e);
        if wide {
            self.mark_wide_hi(r + 1, pc);
        }
    }

    /// Materialize WITHOUT consuming the register slot's caller contract:
    /// emit `v = e` and return the var id (used by the size cap).
    fn materialize_value(&mut self, r: u16, e: Expr) -> u32 {
        let e = value_of_cmp(&e);
        let ty = e.type_ref();
        // fresh_var is type-keyed: a retyped register gets a FRESH var,
        // so no in-place retyping here (each var keeps its mint type and
        // its LocalDef label always matches its init).
        let v = self.fresh_var(r, ty);
        self.stmts.push(Stmt::LocalDef {
            var: v,
            init: Some(e),
            is_final: false,
            force_type: true,
        });
        v
    }

    /// Write a value whose evaluation already happened (invoke /
    /// filled-new-array results): consumed exactly once.
    fn write_call_result(&mut self, r: u16, e: Expr, pc: u32) {
        if r as usize >= self.regs.len() {
            return;
        }
        self.write_pc[r as usize] = pc;
        self.regs[r as usize] = Reg::PendingCall(e);
    }

    fn mark_wide_hi(&mut self, r: u16, pc: u32) {
        if (r as usize) < self.regs.len() {
            self.regs[r as usize] = Reg::WideHi;
            self.write_pc[r as usize] = pc;
        }
    }

    /// Materialize every pending view whose expression tree reads the
    /// given field anywhere (top-level, receiver chain, or nested call
    /// arguments): the write about to land invalidates the view, and a
    /// later use would re-render the read at its own position — past the
    /// write. Materializing HERE puts the temp before the write, strictly
    /// closer to the bytecode's read position than any use site.
    fn materialize_views_mentioning(
        &mut self,
        cls: &std::sync::Arc<str>,
        name: &std::sync::Arc<str>,
        is_static: bool,
    ) {
        let mut stale: Vec<u16> = Vec::new();
        for (i, r) in self.regs.iter().enumerate() {
            match r {
                Reg::Pending(e) | Reg::PendingCall(e)
                    if expr_mentions_field(e, cls, name, is_static) =>
                {
                    stale.push(i as u16);
                }
                _ => {}
            }
        }
        for r in stale {
            self.materialize(r);
        }
    }

    /// Emit `v = expr` and mark the register `Live(v)`.
    fn materialize(&mut self, r: u16) -> u32 {
        // TAKE the slot's expression (single consumer: this materialization
        // replaces the register with Live(v)) — the old `.cloned()` deep-
        // cloned the whole pending Expr and then dropped the original.
        let cur = std::mem::replace(
            self.regs.get_mut(r as usize).unwrap_or(&mut Reg::Undef),
            Reg::Undef,
        );
        let e = match cur {
            Reg::Pending(e) | Reg::PendingCall(e) => e,
            Reg::Live(v) => {
                if (r as usize) < self.regs.len() {
                    self.regs[r as usize] = Reg::Live(v);
                }
                return v;
            }
            // Undef (a value flowing in from an unanalyzed path): a fresh
            // local, NOT Const 0 — `0.new a()` was not even valid Java.
            _ => {
                let v = self.fresh_var(r, TypeRef::J(JavaType::Object("java/lang/Object".into())));
                self.stmts.push(Stmt::LocalDef {
                    var: v,
                    init: None,
                    is_final: false,
                    force_type: false,
                });
                if (r as usize) < self.regs.len() {
                    self.regs[r as usize] = Reg::Live(v);
                }
                return v;
            }
        };
        // cmp sentinels stored as values become library compare calls.
        let e = value_of_cmp_owned(e);
        let ty = e.type_ref();
        // fresh_var is type-keyed: a retyped register gets a FRESH var,
        // so no in-place retyping here (each var keeps its mint type and
        // its LocalDef label always matches its init).
        let v = self.fresh_var(r, ty);
        self.stmts.push(Stmt::LocalDef {
            var: v,
            init: Some(e),
            is_final: false,
            force_type: true,
        });
        if (r as usize) < self.regs.len() {
            self.regs[r as usize] = Reg::Live(v);
        }
        // A folded construction materialized from ONE alias binds every
        // alias to the same var — the copies would otherwise re-emit the
        // construction at their own read (or orphan at block exit).
        for rr in self.folded_new_aliases(r) {
            self.regs[rr] = Reg::Live(v);
        }
        v
    }

    /// Discard an unconsumed call result (invoke without move-result).
    fn drop_pending_call(&mut self) {
        if let Some(e) = self.pending_call.take() {
            let e = value_of_cmp(&e);
            if has_side_effects(&e) {
                self.stmts.push(Stmt::ExprStmt(e));
            }
        }
    }

    // -- helpers -------------------------------------------------------------

    fn owner_expr(&self, obj: Expr, cls: &str) -> Option<Box<Expr>> {
        if cls == self.env.class_name && matches!(obj, Expr::This) {
            return None;
        }
        Some(Box::new(obj))
    }

    fn arith_op(op: ArithOp) -> BinOp {
        match op {
            ArithOp::Add => BinOp::Add,
            ArithOp::Sub => BinOp::Sub,
            ArithOp::Mul => BinOp::Mul,
            ArithOp::Div => BinOp::Div,
            ArithOp::Rem => BinOp::Rem,
            ArithOp::And => BinOp::And,
            ArithOp::Or => BinOp::Or,
            ArithOp::Xor => BinOp::Xor,
            ArithOp::Shl => BinOp::Shl,
            ArithOp::Shr => BinOp::Shr,
            ArithOp::Ushr => BinOp::Ushr,
        }
    }

    fn cmp_op(op: CmpOp) -> BinOp {
        match op {
            CmpOp::Eq => BinOp::Eq,
            CmpOp::Ne => BinOp::Ne,
            CmpOp::Lt => BinOp::Lt,
            CmpOp::Ge => BinOp::Ge,
            CmpOp::Gt => BinOp::Gt,
            CmpOp::Le => BinOp::Le,
        }
    }

    fn cmp_sentinel(kind: CmpKind, l: Expr, r: Expr) -> Expr {
        let name = match kind {
            CmpKind::CmplF => "\0cmpl-float",
            CmpKind::CmpgF => "\0cmpg-float",
            CmpKind::CmplD => "\0cmpl-double",
            CmpKind::CmpgD => "\0cmpg-double",
            CmpKind::CmpJ => "\0cmp-long",
        };
        Expr::Invokedynamic {
            name: name.into(),
            desc: MethodDescriptor {
                args: vec![],
                ret: JavaType::Int,
            },
            args: vec![l, r],
            bsm_text: String::new(),
            bsm_static_args: vec![],
        }
    }

    // -- the main loop ---------------------------------------------------------

    /// Lift one block. `handler_types` gives catch types for ranges this
    /// block handles (for move-exception typing).
    pub fn build_block(
        mut self,
        ins: &[Insn],
        handler_types: &[Option<std::sync::Arc<str>>],
        final_read: std::sync::Arc<jdc_core::FxHashSet<(u32, u16)>>,
    ) -> BResult<(BlockResult, OutState)> {
        let payloads = &self.env.code.payloads;
        // METHOD-wide final-read set (computed once by the caller over
        // the full instruction stream). The block-scoped computation
        // this replaced marked a block's LAST read of a register as
        // final even when LATER BLOCKS read the same generation: the
        // alloc inlined-and-consumed at the block tail, and the
        // downstream reads minted fresh never-assigned int locals
        // (`new c(..).e = new HashMap();` + `v28.e` with `int v28` —
        // weixin tf5/e, the dominant 无法取消引用int shape once the
        // obscuring cascades stopped suppressing it).
        self.final_read = final_read;
        // Field views a later same-field write in this block kills: the
        // register holds the READ expression as a pending view; the write
        // only replaces the written register, so the stale view re-renders
        // at its use (past the write). Materialize such views at their
        // DEFINING pc — the temp local is exactly the source's temporary.
        {
            let mut field_writes: jdc_core::FxHashMap<
                (std::sync::Arc<str>, std::sync::Arc<str>, bool),
                Vec<u32>,
            > = jdc_core::FxHashMap::default();
            for ins in ins {
                match &ins.kind {
                    InsnKind::IPut { field_idx, .. } | InsnKind::SPut { field_idx, .. } => {
                        let (cls, name, _) = self.env.field_ref(*field_idx);
                        let is_static = matches!(ins.kind, InsnKind::SPut { .. });
                        field_writes
                            .entry((cls, name, is_static))
                            .or_default()
                            .push(ins.pc);
                    }
                    _ => {}
                }
            }
            if !field_writes.is_empty() {
                for ins in ins {
                    let (_dst, field_idx, is_static) = match &ins.kind {
                        InsnKind::IGet { dst, field_idx, .. } => (*dst, *field_idx, false),
                        InsnKind::SGet { dst, field_idx } => (*dst, *field_idx, true),
                        _ => continue,
                    };
                    let (cls, name, _) = self.env.field_ref(field_idx);
                    let killed = field_writes
                        .get(&(cls, name, is_static))
                        .is_some_and(|ws| ws.iter().any(|w| *w > ins.pc));
                    if killed {
                        self.force_mat.insert(ins.pc);
                    }
                }
            }
        }
        for ins in ins {
            self.cur_pc = ins.pc;
            match &ins.kind {
                InsnKind::Nop => {}
                InsnKind::Unknown => {
                    return Err(format!(
                        "unsupported opcode {:#04x} ({}) at pc {}",
                        ins.op,
                        ddc_dex::insn::op_name(ins.op),
                        ins.pc
                    ));
                }
                InsnKind::Move { dst, src } => {
                    // Impure / call values are consumed once: materialize the
                    // source so both registers share one evaluation. A raw
                    // `new-instance` view is exempt (as at block exit): the
                    // ctor fold consumes it at the invoke-direct site, and
                    // the invoke-range receiver shuffle (`new-instance v0;
                    // move-object v5, v0; invoke/range {v5..}`) must keep it
                    // pending — materializing here emitted an empty `new C()`
                    // while the real construction orphaned at the receiver
                    // (Kotlin default-arg ctors: every enum constant got an
                    // empty shell).
                    let src_state = self.regs.get(*src as usize).cloned().unwrap_or(Reg::Undef);
                    if matches!(src_state, Reg::PendingCall(_))
                        || matches!(&src_state, Reg::Pending(e)
                            if has_side_effects(e) && !matches!(e, Expr::New { raw: true, .. }))
                    {
                        self.materialize(*src);
                    }
                    let st = self.regs.get(*src as usize).cloned().unwrap_or(Reg::Undef);
                    let wide = reg_is_wide(&st, self.vt);
                    if (*dst as usize) < self.regs.len() {
                        self.regs[*dst as usize] = st;
                        self.write_pc[*dst as usize] =
                            self.write_pc.get(*src as usize).copied().unwrap_or(0);
                    }
                    if wide {
                        self.mark_wide_hi(*dst + 1, ins.pc);
                    }
                }
                InsnKind::MoveResult { dst } => {
                    if let Some(e) = self.pending_call.take() {
                        let wide =
                            matches!(e.type_ref().erased(), JavaType::Long | JavaType::Double);
                        self.write_call_result(*dst, e, ins.pc);
                        if wide {
                            self.mark_wide_hi(*dst + 1, ins.pc);
                        }
                    } else {
                        // move-result without a pending call: fresh var.
                        let v = self.fresh_var(*dst, TypeRef::J(JavaType::Int));
                        if (*dst as usize) < self.regs.len() {
                            self.regs[*dst as usize] = Reg::Live(v);
                        }
                    }
                }
                InsnKind::MoveException { dst } => {
                    let ty = handler_types
                        .iter()
                        .flatten()
                        .next()
                        .map(|t| TypeRef::J(JavaType::Object(t.clone())))
                        .unwrap_or_else(|| {
                            TypeRef::J(JavaType::Object("java/lang/Throwable".into()))
                        });
                    let v = self.fresh_var(*dst, ty);
                    self.stmts.push(Stmt::LocalDef {
                        var: v,
                        init: None,
                        is_final: false,
                        force_type: true,
                    });
                    if (*dst as usize) < self.regs.len() {
                        self.regs[*dst as usize] = Reg::Live(v);
                    }
                }
                InsnKind::ReturnVoid
                | InsnKind::Return { .. }
                | InsnKind::Throw { .. }
                | InsnKind::Goto { .. }
                | InsnKind::PackedSwitch { .. }
                | InsnKind::SparseSwitch { .. }
                | InsnKind::If { .. } => {}
                InsnKind::Const { dst, val, wide } => {
                    let e = if *wide {
                        Expr::Const(ConstVal::Long(*val))
                    } else {
                        Expr::Const(ConstVal::Int(*val as i32))
                    };
                    self.write(*dst, e, ins.pc, *wide);
                }
                InsnKind::ConstString { dst, str_idx } => {
                    let s = std::sync::Arc::from(self.env.dex.string(*str_idx));
                    self.write(*dst, Expr::Const(ConstVal::Str(s)), ins.pc, false);
                }
                InsnKind::ConstClass { dst, type_idx } => {
                    let ty = TypeRef::J(self.env.java_type(*type_idx));
                    self.write(*dst, Expr::Const(ConstVal::ClassLit(ty)), ins.pc, false);
                }
                InsnKind::MonitorEnter { reg } => {
                    let e = self.read_nest(*reg);
                    *self.mflags = self.mflags.with_monitor();
                    self.stmts.push(Stmt::MonitorEnter(e));
                }
                InsnKind::MonitorExit { reg } => {
                    let e = self.read_nest(*reg);
                    *self.mflags = self.mflags.with_monitor();
                    self.stmts.push(Stmt::MonitorExit(e));
                }
                InsnKind::CheckCast { reg, type_idx } => {
                    let e = self.read_nest(*reg);
                    let ty = TypeRef::J(self.env.java_type(*type_idx));
                    self.write(*reg, Expr::Cast { ty, e: Box::new(e) }, ins.pc, false);
                }
                InsnKind::InstanceOf { dst, src, type_idx } => {
                    let e = self.read_nest(*src);
                    let ty = TypeRef::J(self.env.java_type(*type_idx));
                    self.write(*dst, Expr::InstanceOf { e: Box::new(e), ty }, ins.pc, false);
                }
                InsnKind::ArrayLength { dst, src } => {
                    let e = self.read_nest(*src);
                    let length = Expr::Field {
                        owner: Some(Box::new(e)),
                        cls: std::sync::Arc::from(""),
                        name: "length".into(),
                        ty: TypeRef::J(JavaType::Int),
                        is_static: false,
                    };
                    self.write(*dst, length, ins.pc, false);
                }
                InsnKind::NewInstance { dst, type_idx } => {
                    let cls = self.env.type_name_arc(*type_idx);
                    let ty = TypeRef::J(JavaType::Object(cls.clone()));
                    self.write(
                        *dst,
                        Expr::New {
                            cls,
                            ty,
                            args: vec![],
                            arg_tys: Vec::new(),
                            raw: true,
                        },
                        ins.pc,
                        false,
                    );
                }
                InsnKind::NewArray {
                    dst,
                    size,
                    type_idx,
                } => {
                    let elem_desc = self.env.type_name(*type_idx);
                    // The type id names the ARRAY type: strip one `[`.
                    let elem = crate::desc_type(elem_desc.trim_start_matches('['));
                    // `new-array vB, vC, T@` allocates ONE dimension (vC) of
                    // elements whose type is T@ with a single `[` stripped.
                    // When T@ is itself an array (`[[I` = int[][]), the extra
                    // `[` levels are trailing empty dims: `new int[size][]`.
                    // Counting only the first `[` as the sized dim and the
                    // rest as trailing_dims (elem keeps the fully-stripped
                    // base) — the old trim_start_matches('[') dropped every
                    // level, rendering int[][] as `new int[size]` and the
                    // element stores as `a[i] = (int[]) x` ("int[]无法转换
                    // 为int", 267 on reqable).
                    let n_brackets = elem_desc.chars().take_while(|&c| c == '[').count();
                    let trailing_dims = n_brackets.saturating_sub(1).min(u8::MAX as usize) as u8;
                    let dims = self.read_nest(*size);
                    self.write(
                        *dst,
                        Expr::NewArray {
                            elem: TypeRef::J(elem),
                            dims: vec![dims],
                            trailing_dims,
                            init: None,
                        },
                        ins.pc,
                        false,
                    );
                }
                InsnKind::FilledNewArray { regs, type_idx } => {
                    self.drop_pending_call();
                    let elem_desc = self.env.type_name(*type_idx);
                    let base = elem_desc.trim_start_matches('[');
                    // `trim_start_matches` strips EVERY leading `[` — the
                    // total depth must be counted back: a filled-new-array
                    // may create a MULTIdimensional array in one insn
                    // (`filled-new-array/range {40 rows}, [[I` — autoclaw
                    // so/P's static table; R8 emits this for 2-D literal
                    // rows). Losing the extra depth rendered `new int[]
                    // {…rows…}` (int[]无法转换为int ×787 on autoclaw) and
                    // the init form needs trailing_dims = depth − 1 (the
                    // printer emits trailing+1 bracket pairs).
                    let depth = elem_desc.len() - base.len();
                    let elem = crate::desc_type(base);
                    // A wide element (long/double) occupies TWO
                    // register slots per element — walking the cursor by
                    // the element width groups them correctly (reading
                    // every register minted fresh locals from the WideHi
                    // slots). Multi-dim elements are ARRAY REFERENCES
                    // (`[[J` fills `[J` refs) — never wide.
                    let wide = depth == 1 && elem.is_wide();
                    let mut args: Vec<Expr> = Vec::with_capacity(regs.len());
                    let mut it = regs.iter();
                    while let Some(&r) = it.next() {
                        args.push(self.read_nest(r));
                        if wide {
                            it.next();
                        }
                    }
                    self.pending_call = Some(Expr::NewArray {
                        elem: TypeRef::J(elem),
                        dims: vec![],
                        trailing_dims: depth.saturating_sub(1).min(255) as u8,
                        init: Some(args),
                    });
                }
                InsnKind::FillArrayData { reg, payload_pc } => {
                    if let Some(Payload::ArrayData {
                        elem_width,
                        size,
                        data,
                    }) = payloads.get(payload_pc)
                    {
                        let elems = payload_consts(*elem_width, *size, data);
                        let cur = self.regs.get(*reg as usize).cloned().unwrap_or(Reg::Undef);
                        if let Reg::Pending(Expr::NewArray { elem, init, .. }) = cur {
                            if init.is_none() {
                                self.write(
                                    *reg,
                                    Expr::NewArray {
                                        elem,
                                        dims: vec![Expr::Const(ConstVal::Int(*size as i32))],
                                        trailing_dims: 0,
                                        init: Some(elems),
                                    },
                                    ins.pc,
                                    false,
                                );
                                continue;
                            }
                        }
                        // Not the new-array+fill pattern: element stores.
                        let arr = self.read_nest(*reg);
                        for (i, e) in elems.into_iter().enumerate() {
                            let idx = Expr::Const(ConstVal::Int(i as i32));
                            let target = Expr::ArrayIndex {
                                array: Box::new(arr.clone()),
                                index: Box::new(idx),
                            };
                            self.stmts.push(Stmt::ExprStmt(Expr::Assign {
                                target: Box::new(target),
                                op: AssignOp::Plain,
                                value: Box::new(e),
                            }));
                        }
                    }
                }
                InsnKind::Cmp { dst, a, b, kind } => {
                    let l = self.read_nest(*a);
                    let r = self.read_nest(*b);
                    *self.mflags = self.mflags.with_cmp();
                    self.write(*dst, Self::cmp_sentinel(*kind, l, r), ins.pc, false);
                }
                InsnKind::AGet {
                    dst, array, index, ..
                } => {
                    let a = self.read_nest(*array);
                    let i = self.read_nest(*index);
                    self.write(
                        *dst,
                        Expr::ArrayIndex {
                            array: Box::new(a),
                            index: Box::new(i),
                        },
                        ins.pc,
                        false,
                    );
                }
                InsnKind::APut {
                    value,
                    array,
                    index,
                    ..
                } => {
                    self.drop_pending_call();
                    let v = self.read_nest(*value);
                    let a = self.read_nest(*array);
                    let i = self.read_nest(*index);
                    let target = Expr::ArrayIndex {
                        array: Box::new(a),
                        index: Box::new(i),
                    };
                    self.stmts.push(Stmt::ExprStmt(Expr::Assign {
                        target: Box::new(target),
                        op: AssignOp::Plain,
                        value: Box::new(v),
                    }));
                }
                InsnKind::IGet {
                    dst,
                    obj,
                    field_idx,
                } => {
                    self.drop_pending_call();
                    let (cls, name, ty) = self.env.field_ref(*field_idx);
                    let owner = self.read_nest(*obj);
                    let owner_opt = self.owner_expr(owner, &cls);
                    self.write(
                        *dst,
                        Expr::Field {
                            owner: owner_opt,
                            cls,
                            name,
                            ty: TypeRef::J(ty),
                            is_static: false,
                        },
                        ins.pc,
                        false,
                    );
                }
                InsnKind::IPut {
                    value,
                    obj,
                    field_idx,
                } => {
                    self.drop_pending_call();
                    let (cls, name, ty) = self.env.field_ref(*field_idx);
                    // Views that mention the written field ANYWHERE (the
                    // receiver chain counts: `this.h.e` is killed by
                    // `this.h = null`) go stale the moment the write lands;
                    // materialize them ahead of it so their use re-renders
                    // the pre-write value.
                    self.materialize_views_mentioning(&cls, &name, false);
                    let v = null_in_obj_ctx(self.read_nest(*value), &ty);
                    let owner = self.read_nest(*obj);
                    let owner_opt = self.owner_expr(owner, &cls);
                    let target = Expr::Field {
                        owner: owner_opt,
                        cls,
                        name,
                        ty: TypeRef::J(ty),
                        is_static: false,
                    };
                    self.stmts.push(Stmt::ExprStmt(Expr::Assign {
                        target: Box::new(target),
                        op: AssignOp::Plain,
                        value: Box::new(v),
                    }));
                }
                InsnKind::SGet { dst, field_idx } => {
                    self.drop_pending_call();
                    let (cls, name, ty) = self.env.field_ref(*field_idx);
                    self.write(
                        *dst,
                        Expr::Field {
                            owner: None,
                            cls,
                            name,
                            ty: TypeRef::J(ty),
                            is_static: true,
                        },
                        ins.pc,
                        false,
                    );
                }
                InsnKind::SPut { value, field_idx } => {
                    self.drop_pending_call();
                    let (cls, name, ty) = self.env.field_ref(*field_idx);
                    self.materialize_views_mentioning(&cls, &name, true);
                    let v = null_in_obj_ctx(self.read_nest(*value), &ty);
                    let target = Expr::Field {
                        owner: None,
                        cls,
                        name,
                        ty: TypeRef::J(ty),
                        is_static: true,
                    };
                    self.stmts.push(Stmt::ExprStmt(Expr::Assign {
                        target: Box::new(target),
                        op: AssignOp::Plain,
                        value: Box::new(v),
                    }));
                }
                InsnKind::Invoke {
                    kind,
                    regs,
                    method_idx,
                } => {
                    self.drop_pending_call();
                    self.do_invoke(*kind, regs, *method_idx, ins.pc)?;
                }
                InsnKind::InvokeCustom {
                    call_site_idx,
                    regs,
                } => {
                    self.drop_pending_call();
                    let e = self.build_invoke_custom(*call_site_idx, regs, ins.pc);
                    self.pending_call = Some(e);
                }
                InsnKind::Un {
                    dst,
                    src,
                    op,
                    from: _,
                    to,
                } => {
                    let e = self.read_nest(*src);
                    let ne = match op {
                        ddc_dex::insn::UnArith::Neg => Expr::Un {
                            op: UnOp::Neg,
                            e: Box::new(e),
                        },
                        ddc_dex::insn::UnArith::Not => Expr::Un {
                            op: UnOp::BitNot,
                            e: Box::new(e),
                        },
                        ddc_dex::insn::UnArith::Conv => Expr::Cast {
                            ty: TypeRef::J(primitive_type(*to)),
                            e: Box::new(e),
                        },
                    };
                    let wide = matches!(*to, 'J' | 'D');
                    self.write(*dst, ne, ins.pc, wide);
                }
                InsnKind::Bin { op, dst, a, b, ty } => {
                    let l = self.read_nest(*a);
                    let r = self.read_nest(*b);
                    let jt = primitive_type(*ty);
                    let e = Expr::Bin {
                        op: Self::arith_op(*op),
                        l: Box::new(l),
                        r: Box::new(r),
                        ty: Some(TypeRef::J(jt)),
                    };
                    let wide = matches!(*ty, 'J' | 'D');
                    self.write(*dst, e, ins.pc, wide);
                }
                InsnKind::BinLit {
                    op,
                    dst,
                    a,
                    lit,
                    rsub,
                } => {
                    let av = self.read_nest(*a);
                    let e = if *rsub {
                        Expr::Bin {
                            op: BinOp::Sub,
                            l: Box::new(Expr::Const(ConstVal::Int(*lit))),
                            r: Box::new(av),
                            ty: Some(TypeRef::J(JavaType::Int)),
                        }
                    } else {
                        Expr::Bin {
                            op: Self::arith_op(*op),
                            l: Box::new(av),
                            r: Box::new(Expr::Const(ConstVal::Int(*lit))),
                            ty: Some(TypeRef::J(JavaType::Int)),
                        }
                    };
                    self.write(*dst, e, ins.pc, false);
                }
                InsnKind::ConstMethodHandle { dst, handle_idx } => {
                    self.drop_pending_call();
                    let e = self.method_handle_expr(*handle_idx);
                    self.write(*dst, e, ins.pc, false);
                }
                InsnKind::ConstMethodType { dst, proto_idx } => {
                    self.drop_pending_call();
                    let proto = self.env.dex.proto(*proto_idx);
                    let desc = format!(
                        "({}){}",
                        self.env
                            .dex
                            .proto_params(*proto_idx)
                            .iter()
                            .map(|&t| self.env.dex.type_name(t))
                            .collect::<Vec<_>>()
                            .join(""),
                        self.env.dex.type_name(proto.return_type_idx)
                    );
                    self.write(*dst, Expr::Raw(desc), ins.pc, false);
                }
            }
        }

        // Terminator first: terms may consume PendingCall values directly.
        let term = self.block_term(ins);

        // Exit cleanup: unconsumed call → statement; impure pendings → vars.
        // Open `new`/`new-array` views stay pending (their consumer may live
        // in a later block).
        self.drop_pending_call();
        self.materialize_impure_at_exit();

        // Move (not clone): the lifter is consumed after build_block, and
        // these two clones — regs deep-copies boxed Exprs for any Pending
        // states — ran 3.7M times on weibo.
        let out = OutState {
            regs: std::mem::take(&mut self.regs),
            write_pc: std::mem::take(&mut self.write_pc),
        };
        Ok((
            BlockResult {
                stmts: std::mem::take(&mut self.stmts),
                out_stack: Vec::new(),
                term,
            },
            out,
        ))
    }

    fn do_invoke(
        &mut self,
        kind: InvokeKind,
        regs: &[u16],
        method_idx: u32,
        pc: u32,
    ) -> BResult<()> {
        let (cls, name, md) = self.env.method_ref(method_idx);
        if cls.as_ref() == "java/lang/StringBuilder" || cls.as_ref() == "java/lang/StringBuffer" {
            *self.mflags = self.mflags.with_sb();
        }
        let is_static = matches!(kind, InvokeKind::Static);
        let receiver_reg = if is_static {
            None
        } else {
            regs.first().copied()
        };
        let arg_regs: Vec<u16> = if is_static {
            regs.to_vec()
        } else {
            regs.iter().skip(1).copied().collect()
        };
        let mut args: Vec<Expr> = Vec::with_capacity(md.args.len());
        // A WIDE parameter (long/double) occupies TWO register slots in
        // the instruction's register list — indexing by PARAMETER
        // position shifted every argument after a wide one (weixin
        // yieldTransaction(JZ L) read the boolean from the long's HIGH
        // register and the signal from the boolean's — the root of the
        // call-argument int↔bool family and ~11k dangling `int vN`
        // reads). Advance the register cursor by the arg's width.
        let mut ri = 0usize;
        for at in md.args.iter() {
            let r = arg_regs.get(ri).copied().unwrap_or(0);
            args.push(null_in_obj_ctx(self.read_nest(r), at));
            ri += if at.is_wide() { 2 } else { 1 };
        }
        // Constructor call: fold `new C` receivers; this/super otherwise.
        // recv_expr is computed LAZILY (after the fold's early return):
        // reading the receiver through read_nest before the fold would
        // materialize a raw pending New into a local and the fold's
        // Pending-pattern check would never fire.
        if name.as_ref() == "<init>" && matches!(kind, InvokeKind::Direct) {
            let recv_reg = receiver_reg.unwrap_or(0);
            let recv_state = self
                .regs
                .get(recv_reg as usize)
                .cloned()
                .unwrap_or(Reg::Undef);
            if let Reg::Pending(Expr::New {
                raw: true, cls: nc, ..
            }) = &recv_state
            {
                // R8 constructor outlining / vertical class merging:
                // `new-instance Child` + `invoke-direct Parent-><init>`
                // — the child's trivial ctor was stripped and the verifier
                // allows the super-ctor call on the uninitialized
                // instance. The CONSTRUCTED class is the new-instance
                // type; folding on the invoke owner rendered
                // `new Hd.c(p2x)` against the abstract parent (uuyc
                // 抽象的无法实例化 ×2.2k). The inherited-ctor bridge in
                // classdec gives the child a matching `super(..)`
                // delegation, so `new Child(args)` compiles.
                if nc == &cls || self.env.pool.is_subtype(nc.as_ref(), cls.as_ref()) {
                    let folded = Expr::New {
                        cls: nc.clone(),
                        ty: TypeRef::J(JavaType::Object(nc.clone())),
                        arg_tys: md.args.clone(),
                        args,
                        raw: false,
                    };
                    // The raw pending may live in SEVERAL registers:
                    // `move-object` copies it for the invoke-range
                    // receiver shuffle. Writing only the receiver left
                    // the original register's raw view behind, so the
                    // following `sput v0` inlined an EMPTY `new C()`
                    // while the real construction orphaned at the
                    // receiver. Carry the fold to every alias, keyed by
                    // the origin write_pc the Move handler propagates.
                    let origin = self.write_pc.get(recv_reg as usize).copied();
                    let aliases: Vec<usize> = origin
                        .map(|w| {
                            (0..self.regs.len())
                                .filter(|&r| {
                                    r != recv_reg as usize
                                        && self.write_pc.get(r).copied() == Some(w)
                                        && matches!(
                                            &self.regs[r],
                                            Reg::Pending(Expr::New { raw: true, cls: c, .. })
                                                if c == nc
                                        )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    self.write(recv_reg, folded.clone(), pc, false);
                    // write()'s size cap may have materialized the
                    // construction into a local — share that var with the
                    // aliases instead of pending copies of the same
                    // allocation.
                    match self.regs.get(recv_reg as usize) {
                        Some(Reg::Live(v)) => {
                            let v = *v;
                            for r in aliases {
                                self.regs[r] = Reg::Live(v);
                            }
                        }
                        _ => {
                            for r in aliases {
                                self.regs[r] = Reg::Pending(folded.clone());
                                self.write_pc[r] = pc;
                            }
                        }
                    }
                    return Ok(());
                }
            }
            let recv_expr = receiver_reg.map(|r| self.read_nest(r));
            let owner_expr_v = recv_expr.unwrap_or(Expr::This);
            // Normalize the receiver: in an instance method the `this`
            // parameter is ALWAYS var 0 (entry_regs pushes it first).
            // read_nest hands back Local{var:0}, which the emitter's
            // is_this/lost_alloc checks do not recognize — a plain
            // `super()` arrived there as a non-this receiver and printed
            // `new Object();` (every default ctor in every app).
            let owner_expr_v = if !self.env.is_static
                && matches!(&owner_expr_v, Expr::Local { var: 0, .. })
            {
                Expr::This
            } else {
                owner_expr_v
            };
            let is_super = cls.as_ref() != self.env.class_name.as_str();
            let owner = if matches!(owner_expr_v, Expr::This) && is_super {
                None
            } else {
                Some(Box::new(owner_expr_v))
            };
            let call = Expr::Method {
                owner,
                cls,
                name: "<init>".into(),
                desc: md,
                args,
                is_static: false,
                is_interface: false,
                is_special: true,
                is_super,
                is_dynamic: false,
                type_args: vec![],
            };
            self.stmts.push(Stmt::ExprStmt(call));
            return Ok(());
        }

        let recv_expr = receiver_reg.map(|r| self.read_nest(r));
        let is_interface = matches!(kind, InvokeKind::Interface);
        // `super.` only when the receiver is THIS. R8/d8 desugar an
        // inner-class/lambda `outer.super.m()` into a STATIC synthetic
        // accessor `w(Outer p) { invoke-special p, Super.m() }` — a
        // nonvirtual call on a PARAMETER. The `super` keyword is illegal
        // there ("无法从静态上下文中引用非静态 变量 super", weixin 318 +
        // lark 176), and Java cannot express special dispatch on an
        // arbitrary instance, so a plain call on the receiver is the
        // only legal render.
        // The receiver may reach here as Expr::This, the Raw("\u{3}")
        // placeholder, or — most often — the `this` LOCAL (read_nest of
        // register v0; the VarInfo is named "this" and emit prints the
        // keyword). All three are the this-receiver; anything else (a
        // real parameter/local) is not.
        let recv_is_this = match &recv_expr {
            Some(Expr::This) => true,
            Some(Expr::Raw(t)) => t == "\u{3}" || t == "this",
            Some(Expr::Local { var, .. }) => {
                (*var as usize) < self.vt.vars.len() && self.vt.vars[*var as usize].name == "this"
            }
            _ => false,
        };
        let is_super = match kind {
            InvokeKind::Super | InvokeKind::Direct => {
                recv_is_this && cls.as_ref() != self.env.class_name.as_str()
            }
            _ => false,
        };
        let owner: Option<Box<Expr>> = match recv_expr {
            None => None,
            Some(o) => {
                if is_super && matches!(o, Expr::This) {
                    None
                } else {
                    self.owner_expr(o, &cls)
                }
            }
        };
        let is_dynamic = matches!(kind, InvokeKind::Polymorphic);
        let call = Expr::Method {
            owner,
            cls,
            name,
            desc: md,
            args,
            is_static,
            is_interface,
            is_special: matches!(kind, InvokeKind::Direct),
            is_super,
            is_dynamic,
            type_args: vec![],
        };
        self.pending_call = Some(call);
        Ok(())
    }

    /// Resolve a method-handle constant into a readable expression
    /// (`Cls::name` for method kinds).
    fn method_handle_expr(&mut self, handle_idx: u32) -> Expr {
        let Some(h) = self.env.dex.method_handle(handle_idx) else {
            return Expr::Raw(format!("methodHandle@{}", handle_idx));
        };
        if h.is_field {
            let f = self.env.dex.field(h.target_id);
            let cls = self.env.dex.class_name(f.class_idx);
            let name = self.env.dex.string(f.name_idx).to_string();
            return Expr::Raw(format!("{}::{}", cls.replace('/', "."), name));
        }
        let m = self.env.dex.method(h.target_id);
        let cls = self.env.dex.class_name(m.class_idx);
        let name = self.env.dex.string(m.name_idx).to_string();
        Expr::Raw(format!("{}::{}", cls.replace('/', "."), name))
    }

    /// Build the expression for an invoke-custom site.
    fn build_invoke_custom(&mut self, cs_idx: u32, regs: &[u16], pc: u32) -> Expr {
        let Some(cs) = self.env.dex.call_site(cs_idx).cloned() else {
            return Expr::Invokedynamic {
                name: format!("call-site#{}", cs_idx),
                desc: jdc_core::types::MethodDescriptor {
                    args: vec![],
                    ret: JavaType::Object("java/lang/Object".into()),
                },
                args: vec![],
                bsm_text: "unresolved".into(),
                bsm_static_args: vec![],
            };
        };
        let site_name = self.env.dex.string(cs.name_idx).to_string();
        let params: Vec<JavaType> = self
            .env
            .dex
            .proto_params(cs.proto_idx)
            .iter()
            .map(|&t| self.env.java_type(t))
            .collect();
        let ret = self
            .env
            .java_type(self.env.dex.proto(cs.proto_idx).return_type_idx);

        let (bs_cls, bs_name) = match self.env.dex.method_handle(cs.bootstrap_handle) {
            Some(h) if !h.is_field => {
                let m = self.env.dex.method(h.target_id);
                (
                    self.env.dex.class_name(m.class_idx),
                    self.env.dex.string(m.name_idx).to_string(),
                )
            }
            _ => (String::new(), String::new()),
        };

        // Dynamic arguments (the SAM parameters for lambdas). A wide
        // parameter occupies TWO register slots — index a walking cursor
        // by the param's width, not the param position (same bug class
        // as the invoke arg lift).
        let mut args: Vec<Expr> = Vec::with_capacity(params.len().min(regs.len()));
        let mut ri = 0usize;
        for at in params.iter() {
            if let Some(&r) = regs.get(ri) {
                args.push(self.read_nest(r));
            }
            ri += if at.is_wide() { 2 } else { 1 };
        }

        // StringConcatFactory: fold the recipe into `+`.
        if bs_name == "makeConcatWithConstants" {
            let recipe = cs
                .linker_args
                .iter()
                .find_map(|v| match v {
                    ddc_dex::annotations::EncodedValue::String(s) => {
                        Some(self.env.dex.string(*s).to_string())
                    }
                    _ => None,
                })
                .unwrap_or_default();
            return concat_from_recipe(&recipe, args);
        }

        // LambdaMetafactory: linker args are [samType (erased),
        // implMethod (handle), instantiatedMethodType]. The instantiated
        // type carries the source-level SAM parameters.
        let mut impl_m = None;
        let mut instantiated_proto: Option<u32> = None;
        if bs_name == "metafactory" || bs_name == "altMetafactory" {
            let mut seen_handle = false;
            for lv in &cs.linker_args {
                match lv {
                    ddc_dex::annotations::EncodedValue::MethodHandle(hi) => {
                        if !seen_handle {
                            seen_handle = true;
                            if let Some(h) = self.env.dex.method_handle(*hi) {
                                if !h.is_field {
                                    let m = self.env.dex.method(h.target_id);
                                    impl_m = Some((
                                        self.env.dex.class_name(m.class_idx).replace('/', "."),
                                        self.env.dex.string(m.name_idx).to_string(),
                                    ));
                                }
                            }
                        }
                    }
                    ddc_dex::annotations::EncodedValue::MethodType(p) if seen_handle => {
                        instantiated_proto = Some(*p);
                    }
                    _ => {}
                }
            }
        }
        let _ = pc;

        // Remaining registers are the captured values.
        let mut captures = Vec::new();
        for &r in regs.iter().skip(params.len().min(regs.len())) {
            captures.push(self.read_nest(r));
        }

        if let Some((impl_cls, impl_name)) = impl_m {
            if captures.is_empty() {
                // Non-capturing: render real Java syntax. A synthetic
                // `lambda$` impl is a lambda; anything else is a method
                // reference (`Cls::name`).
                if impl_name.starts_with("lambda$") {
                    // Source-level parameter count comes from the
                    // instantiated SAM type, not the site proto (which is
                    // the functional-interface factory view: `()Function`).
                    let n_params = match instantiated_proto {
                        Some(p) => self.env.dex.proto_params(p).len(),
                        None => params.len(),
                    };
                    let pnames: Vec<String> = (0..n_params).map(|i| format!("a{}", i)).collect();
                    return Expr::Raw(format!(
                        "({}) -> {}.{}({})",
                        pnames.join(", "),
                        impl_cls,
                        impl_name,
                        pnames.join(", ")
                    ));
                }
                return Expr::Raw(format!("{}::{}", impl_cls, impl_name));
            }
            // Capturing: the captures are real expressions; emit the
            // desugared call shape (impl applies SAM params then captures).
            let mut call_args = args;
            call_args.extend(captures);
            return Expr::Invokedynamic {
                name: format!("{} -> {}.{}", site_name, impl_cls, impl_name),
                desc: jdc_core::types::MethodDescriptor { args: params, ret },
                args: call_args,
                bsm_text: format!("{}::{}", bs_cls, bs_name),
                bsm_static_args: vec![],
            };
        }

        Expr::Invokedynamic {
            name: site_name,
            desc: jdc_core::types::MethodDescriptor { args: params, ret },
            args: captures,
            bsm_text: format!("{}::{}", bs_cls, bs_name),
            bsm_static_args: vec![],
        }
    }

    fn materialize_impure_at_exit(&mut self) {
        let n = self.regs.len();
        for r in 0..n {
            let st = self.regs[r].clone();
            let impure = match &st {
                Reg::PendingCall(_) => true,
                Reg::Pending(e) => {
                    has_side_effects(e)
                        && !matches!(e, Expr::New { raw: true, .. })
                        && !matches!(e, Expr::NewArray { init: None, .. })
                }
                _ => false,
            };
            if impure {
                self.materialize(r as u16);
            }
        }
    }

    /// Terminator for the block, from its last instruction.
    fn block_term(&mut self, ins: &[Insn]) -> Term {
        let Some(last) = ins.last() else {
            return Term::Goto;
        };
        match &last.kind {
            InsnKind::Goto { .. } => Term::Goto,
            InsnKind::ReturnVoid => Term::Return(None),
            InsnKind::Return { src } => {
                let v = null_in_obj_ctx(self.read_term(*src), &self.env.desc.ret);
                Term::Return(Some(v))
            }
            InsnKind::Throw { reg } => {
                // throw is always an object context.
                let mut v = self.read_term(*reg);
                if let Expr::Const(ConstVal::Int(0)) = v {
                    v = Expr::Const(ConstVal::Null);
                }
                Term::Throw(v)
            }
            InsnKind::If { op, a, b, z, .. } => {
                let cond = if *z {
                    let v = self.read_nest(*a);
                    let (l, r, realop) = jdc_core::ir::build::unfold_cmp(v, Self::cmp_op(*op));
                    Expr::Bin {
                        op: realop,
                        l: Box::new(l),
                        r: Box::new(r),
                        ty: Some(TypeRef::J(JavaType::Boolean)),
                    }
                } else {
                    let l = self.read_nest(*a);
                    let r = self.read_nest(*b);
                    Expr::Bin {
                        op: Self::cmp_op(*op),
                        l: Box::new(l),
                        r: Box::new(r),
                        ty: Some(TypeRef::J(JavaType::Boolean)),
                    }
                };
                Term::Cond { cond }
            }
            InsnKind::PackedSwitch { reg, payload_pc }
            | InsnKind::SparseSwitch { reg, payload_pc } => {
                let selector = self.read_term(*reg);
                let default = Some(last.pc + last.size);
                let targets = match self.env.code.payloads.get(payload_pc) {
                    Some(Payload::Packed { first_key, targets }) => SwitchTargets::Table {
                        low: *first_key,
                        targets: targets
                            .iter()
                            .map(|t| (last.pc as i64 + *t as i64) as u32)
                            .collect(),
                    },
                    Some(Payload::Sparse { pairs }) => SwitchTargets::Lookup {
                        pairs: pairs
                            .iter()
                            .map(|(k, t)| (*k, (last.pc as i64 + *t as i64) as u32))
                            .collect(),
                    },
                    _ => SwitchTargets::Lookup { pairs: vec![] },
                };
                Term::Switch {
                    selector,
                    targets,
                    default,
                }
            }
            // Fell off the analysis end (payload region after): goto.
            _ => {
                let next = last.pc + last.size;
                if next < self.code_units {
                    Term::Fallthrough
                } else {
                    Term::Goto
                }
            }
        }
    }
}

/// StringConcatFactory recipe → `+` parts: `\u{1}` slots consume the next
/// argument, `\u{2}` <len> <chars> emits literal text (rare), other bytes
/// are literal.
fn concat_from_recipe(recipe: &str, args: Vec<Expr>) -> Expr {
    use jdc_core::ir::expr::ConcatPart;
    let mut parts: Vec<ConcatPart> = Vec::new();
    let mut arg_i = 0usize;
    let mut const_buf = String::new();
    for c in recipe.chars() {
        match c {
            '\u{1}' => {
                if !const_buf.is_empty() {
                    parts.push(ConcatPart::Const(std::mem::take(&mut const_buf)));
                }
                if arg_i < args.len() {
                    parts.push(ConcatPart::Str(args[arg_i].clone()));
                    arg_i += 1;
                }
            }
            '\u{2}' => {
                // Length-prefixed literal: skip the length char and copy the
                // following chars until the next marker.
            }
            c => const_buf.push(c),
        }
    }
    if !const_buf.is_empty() {
        parts.push(ConcatPart::Const(const_buf));
    }
    Expr::StringConcat(parts)
}

/// Node count of an expression tree (caps loop-carried growth: a Pending
/// tree that grows across fixpoint rounds would otherwise reach GBs — each
/// merge clones the whole tree per side per visit).
fn expr_size(e: &Expr) -> usize {
    let mut n = 0usize;
    count_nodes(e, &mut n);
    n
}

/// Does the expression read the given field anywhere (receiver chains and
/// nested arguments included)?
fn expr_mentions_field(
    e: &Expr,
    cls: &std::sync::Arc<str>,
    name: &std::sync::Arc<str>,
    is_static: bool,
) -> bool {
    match e {
        Expr::Field {
            owner,
            cls: c,
            name: n,
            is_static: s,
            ..
        } => {
            (c == cls && n == name && *s == is_static)
                || owner
                    .as_deref()
                    .is_some_and(|o| expr_mentions_field(o, cls, name, is_static))
        }
        Expr::Un { e, .. } | Expr::Cast { e, .. } | Expr::InstanceOf { e, .. } => {
            expr_mentions_field(e, cls, name, is_static)
        }
        Expr::Bin { l, r, .. } => {
            expr_mentions_field(l, cls, name, is_static)
                || expr_mentions_field(r, cls, name, is_static)
        }
        Expr::Cond { c, t, f } => {
            expr_mentions_field(c, cls, name, is_static)
                || expr_mentions_field(t, cls, name, is_static)
                || expr_mentions_field(f, cls, name, is_static)
        }
        Expr::Assign { target, value, .. } => {
            expr_mentions_field(target, cls, name, is_static)
                || expr_mentions_field(value, cls, name, is_static)
        }
        Expr::PreIncDec { e, .. } | Expr::PostIncDec { e, .. } => {
            expr_mentions_field(e, cls, name, is_static)
        }
        Expr::Method {
            owner, args, ..
        } => {
            owner
                .as_deref()
                .is_some_and(|o| expr_mentions_field(o, cls, name, is_static))
                || args
                    .iter()
                    .any(|a| expr_mentions_field(a, cls, name, is_static))
        }
        Expr::ArrayIndex { array, index } => {
            expr_mentions_field(array, cls, name, is_static)
                || expr_mentions_field(index, cls, name, is_static)
        }
        Expr::New { args, .. } => args
            .iter()
            .any(|a| expr_mentions_field(a, cls, name, is_static)),
        Expr::NewArray { dims, init, .. } => {
            dims.iter().any(|d| expr_mentions_field(d, cls, name, is_static))
                || init
                    .as_ref()
                    .is_some_and(|v| v.iter().any(|x| expr_mentions_field(x, cls, name, is_static)))
        }
        Expr::NewMultiArray { dims, .. } => {
            dims.iter().any(|d| expr_mentions_field(d, cls, name, is_static))
        }
        Expr::StringConcat(parts) => parts.iter().any(|p| match p {
            ConcatPart::Str(e) => expr_mentions_field(e, cls, name, is_static),
            ConcatPart::Const(_) => false,
        }),
        _ => false,
    }
}

fn count_nodes(e: &Expr, n: &mut usize) {
    *n += 1;
    if *n > 512 {
        return; // cap the walk itself
    }
    match e {
        Expr::Un { e, .. } | Expr::Cast { e, .. } | Expr::InstanceOf { e, .. } => count_nodes(e, n),
        Expr::Bin { l, r, .. } => {
            count_nodes(l, n);
            count_nodes(r, n);
        }
        Expr::Cond { c, t, f } => {
            count_nodes(c, n);
            count_nodes(t, n);
            count_nodes(f, n);
        }
        Expr::Assign { target, value, .. } => {
            count_nodes(target, n);
            count_nodes(value, n);
        }
        Expr::PreIncDec { e, .. } | Expr::PostIncDec { e, .. } => count_nodes(e, n),
        Expr::Field { owner: Some(o), .. } => count_nodes(o, n),
        Expr::Method {
            owner: Some(o),
            args,
            ..
        } => {
            count_nodes(o, n);
            for a in args {
                count_nodes(a, n);
            }
        }
        Expr::ArrayIndex { array, index } => {
            count_nodes(array, n);
            count_nodes(index, n);
        }
        Expr::New { args, .. } => {
            for a in args {
                count_nodes(a, n);
            }
        }
        Expr::NewArray { dims, init, .. } => {
            for d in dims {
                count_nodes(d, n);
            }
            if let Some(v) = init {
                for x in v {
                    count_nodes(x, n);
                }
            }
        }
        Expr::NewMultiArray { dims, .. } => {
            for d in dims {
                count_nodes(d, n);
            }
        }
        Expr::StringConcat(parts) => {
            for p in parts {
                if let ConcatPart::Str(x) = p {
                    count_nodes(x, n);
                }
            }
        }
        _ => {}
    }
}

fn reg_is_wide(st: &Reg, vt: &VarTable) -> bool {
    match st {
        Reg::Pending(e) | Reg::PendingCall(e) => e.type_ref().erased().is_wide(),
        Reg::Live(v) => vt.var(*v).ty.erased().is_wide(),
        Reg::WideHi => true,
        Reg::Undef => false,
    }
}

/// d8 encodes `null` as `const/4 vN, 0`; the instruction kind does not
/// distinguish object contexts (return/return-object share one variant),
/// so the surrounding DESCRIPTOR type decides: a zero constant flowing
/// into a reference context is `null`, not `0` (rendering `return 0;`
/// from a String method — lab-package feedback).
fn null_in_obj_ctx(mut v: Expr, ctx_ty: &JavaType) -> Expr {
    if ctx_ty.is_reference() {
        if let Expr::Const(ConstVal::Int(0)) = v {
            v = Expr::Const(ConstVal::Null);
        }
    }
    v
}

/// Turn a stored cmp sentinel into a `Long.compare`-style call.
fn value_of_cmp(e: &Expr) -> Expr {
    value_of_cmp_owned(e.clone())
}

/// By-value form: single-consumer call sites (materialize) pass the owned
/// expression and skip the clone the shared `&Expr` form pays on the
/// non-sentinel fast path.
fn value_of_cmp_owned(e: Expr) -> Expr {
    // Borrow first; only rebuild when the sentinel shape matches (the
    // non-sentinel fast path returns the original with zero clones).
    if let Expr::Invokedynamic { name, args, .. } = &e {
        if name.starts_with('\0') && args.len() == 2 {
            let (cls, ty) = match name.as_str() {
                "\0cmp-long" => ("java/lang/Long", JavaType::Long),
                "\0cmpl-float" | "\0cmpg-float" => ("java/lang/Float", JavaType::Float),
                _ => ("java/lang/Double", JavaType::Double),
            };
            let desc = MethodDescriptor {
                args: vec![ty.clone(), ty],
                ret: JavaType::Int,
            };
            return Expr::Method {
                owner: None,
                cls: cls.into(),
                name: "compare".into(),
                desc: std::sync::Arc::new(desc),
                args: args.clone(),
                is_static: true,
                is_interface: false,
                is_special: false,
                is_super: false,
                is_dynamic: false,
                type_args: vec![],
            };
        }
    }
    e
}

pub fn primitive_type(c: char) -> JavaType {
    match c {
        'Z' => JavaType::Boolean,
        'B' => JavaType::Byte,
        'C' => JavaType::Char,
        'S' => JavaType::Short,
        'F' => JavaType::Float,
        'J' => JavaType::Long,
        'D' => JavaType::Double,
        _ => JavaType::Int,
    }
}

/// Constant elements from a fill-array-data payload, typed by width (the
/// consuming new-array's element type refines them later).
fn payload_consts(elem_width: u16, size: u32, data: &[u8]) -> Vec<Expr> {
    let n = (size as usize).min(data.len() / elem_width.max(1) as usize);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let off = i * elem_width as usize;
        let e = match elem_width {
            1 => Expr::Const(ConstVal::Int(data[off] as i8 as i32)),
            2 => {
                let v = i16::from_le_bytes([data[off], data[off + 1]]);
                Expr::Const(ConstVal::Int(v as i32))
            }
            4 => {
                let mut b = [0u8; 4];
                b.copy_from_slice(&data[off..off + 4]);
                Expr::Const(ConstVal::Int(i32::from_le_bytes(b)))
            }
            8 => {
                let mut b = [0u8; 8];
                b.copy_from_slice(&data[off..off + 8]);
                Expr::Const(ConstVal::Long(i64::from_le_bytes(b)))
            }
            _ => Expr::Const(ConstVal::Int(0)),
        };
        out.push(e);
    }
    out
}

/// Entry register state from parameters. Returns the register state.
pub fn entry_regs(vt: &mut VarTable, env: &MethodEnv, param_names: &[Option<String>]) -> Vec<Reg> {
    let code = env.code;
    let n = code.registers_size as usize;
    let mut regs = vec![Reg::Undef; n.max(1)];
    let ins = code.ins_size.min(code.registers_size) as usize;
    let mut reg = n.saturating_sub(ins);
    let mut pidx = 0;
    if !env.is_static && reg < n {
        // `this` occupies the first incoming register.
        let id = vt.vars.len() as u32;
        vt.vars.push(VarInfo {
            id,
            slot: reg as u16,
            name: "this".into(),
            ty: TypeRef::J(JavaType::Object(env.class_name.as_str().into())),
            is_param: true,
            range_start: 0,
            range_end: u16::MAX,
            synthetic_name: false,
        });
        while vt.by_slot.len() <= reg {
            vt.by_slot.push(Vec::new());
        }
        vt.by_slot[reg].push((0, u16::MAX, id));
        regs[reg] = Reg::Live(id);
        reg += 1;
        pidx += 1;
    }
    for (i, arg) in env.desc.args.iter().enumerate() {
        if reg >= n {
            break;
        }
        let dbg = param_names.get(i).and_then(|o| o.clone());
        let named = dbg.is_some();
        // Debug-info names are ARBITRARY DEX strings — obfuscated apps
        // ship params named `25` (reqable a4/e: rendered `25 = 25;`, a
        // javac parse error). Run them through the same deterministic
        // sanitizer as every other declared identifier.
        let name = dbg
            .map(|d| crate::classdec::java_ident(&d).into_owned())
            .unwrap_or_else(|| format!("p{}", pidx));
        let id = vt.vars.len() as u32;
        vt.vars.push(VarInfo {
            id,
            slot: reg as u16,
            name,
            ty: TypeRef::J(arg.clone()),
            is_param: true,
            range_start: 0,
            range_end: u16::MAX,
            synthetic_name: !named,
        });
        while vt.by_slot.len() <= reg {
            vt.by_slot.push(Vec::new());
        }
        vt.by_slot[reg].push((0, u16::MAX, id));
        regs[reg] = Reg::Live(id);
        reg += if arg.is_wide() { 2 } else { 1 };
        pidx += 1;
    }
    regs
}
