//! Post-convert refinement passes for DEX-lifted statement trees.
//!
//! The converter hands back a correct-but-raw tree; these passes restore the
//! source shapes a register machine loses: catch parameter binding,
//! copy-forwarding (single-use temporaries), if/else→ternary folding,
//! `StringBuilder` chain → `+` concatenation, `synchronized` recovery from
//! the d8 monitor pattern, erased-type inference, boolean and null
//! comparisons, and declaration hygiene.

// The tree-walker match arms intentionally mirror the statement grammar
// one level at a time; collapsing the nested `if let`s into outer match
// arms would trade per-arm clarity for lint silence.
#![allow(clippy::collapsible_match)]

use jdc_core::FxHashSet as HashSet;

// The tree-walker match arms intentionally mirror the statement grammar
// one level at a time; collapsing the nested `if let`s into outer match
// arms would trade per-arm clarity for lint silence.
use jdc_core::ir::build::has_side_effects;
use jdc_core::emit::is_java_keyword;
use jdc_core::ir::expr::{AssignOp, BinOp, ConcatPart, ConstVal, Expr, TypeRef, UnOp};
use jdc_core::ir::stmt::{CaseGroup, Catch, Stmt};
use jdc_core::types::{JavaType, MethodDescriptor};
use jdc_core::var::VarTable;

use crate::lift::MethodEnv;
use crate::{access, desc_type, DexPool};
use ddc_dex::insn::{Insn, InsnKind, InvokeKind};
use jdc_core::types::parse_method_descriptor;

// ---------------------------------------------------------------------------
// Generic tree walking
// ---------------------------------------------------------------------------

/// Deep expression rewrite over a statement tree.
pub fn rewrite_exprs<F: FnMut(&mut Expr)>(s: &mut Stmt, f: &mut F) {
    walk_stmt_exprs(s, f);
}

pub(crate) fn walk_stmt_exprs<F: FnMut(&mut Expr)>(s: &mut Stmt, f: &mut F) {
    match s {
        Stmt::Block(v) => {
            for x in v.iter_mut() {
                walk_stmt_exprs(x, f);
            }
        }
        Stmt::ExprStmt(e) | Stmt::Throw(e) | Stmt::MonitorEnter(e) | Stmt::MonitorExit(e) => {
            f(e);
        }
        Stmt::Return(Some(e)) => f(e),
        Stmt::LocalDef { init, .. } => {
            if let Some(e) = init {
                f(e);
            }
        }
        Stmt::If {
            cond,
            then_stmt,
            else_stmt,
        } => {
            f(cond);
            walk_stmt_exprs(then_stmt, f);
            if let Some(e) = else_stmt {
                walk_stmt_exprs(e, f);
            }
        }
        Stmt::While { cond, body } => {
            f(cond);
            walk_stmt_exprs(body, f);
        }
        Stmt::DoWhile { body, cond } => {
            walk_stmt_exprs(body, f);
            f(cond);
        }
        Stmt::For {
            init,
            cond,
            update,
            body,
        } => {
            for x in init.iter_mut() {
                walk_stmt_exprs(x, f);
            }
            if let Some(c) = cond {
                f(c);
            }
            for u in update.iter_mut() {
                f(u);
            }
            walk_stmt_exprs(body, f);
        }
        Stmt::ForEach { iterable, body, .. } => {
            f(iterable);
            walk_stmt_exprs(body, f);
        }
        Stmt::Switch {
            selector,
            cases,
            default,
            ..
        } => {
            f(selector);
            for c in cases {
                for x in c.body.iter_mut() {
                    walk_stmt_exprs(x, f);
                }
                if let Some(g) = &mut c.guard {
                    f(g);
                }
            }
            if let Some(d) = default {
                walk_stmt_exprs(d, f);
            }
        }
        Stmt::Try {
            body,
            catches,
            finally,
        } => {
            walk_stmt_exprs(body, f);
            for c in catches {
                walk_stmt_exprs(&mut c.body, f);
            }
            if let Some(fl) = finally {
                walk_stmt_exprs(fl, f);
            }
        }
        Stmt::TryWithResources {
            resources,
            body,
            catches,
            finally,
        } => {
            for x in resources.iter_mut() {
                walk_stmt_exprs(x, f);
            }
            walk_stmt_exprs(body, f);
            for c in catches {
                walk_stmt_exprs(&mut c.body, f);
            }
            if let Some(fl) = finally {
                walk_stmt_exprs(fl, f);
            }
        }
        Stmt::Assert { cond, msg } => {
            f(cond);
            if let Some(m) = msg {
                f(m);
            }
        }
        Stmt::Synchronized { lock, body } => {
            f(lock);
            walk_stmt_exprs(body, f);
        }
        Stmt::TernaryValue { e } => f(e),
        Stmt::Labeled { body, .. } => walk_stmt_exprs(body, f),
        Stmt::Goto(_) | Stmt::Label(_) | Stmt::Break(_) | Stmt::Continue(_) => {}
        _ => {}
    }
}

/// Immutable mirror of `walk_stmt_exprs`: read-only visitors must not
/// have to clone a statement tree just to obtain a `&mut` handle.
/// (count_locals_stmts cloned EVERY counted statement — 29k of 50k
/// Stmt::clone samples in corpus profiles, ~25% of total CPU.)
pub(crate) fn visit_stmt_exprs_ro<F: FnMut(&Expr)>(s: &Stmt, f: &mut F) {
    match s {
        Stmt::Block(v) => {
            for x in v {
                visit_stmt_exprs_ro(x, f);
            }
        }
        Stmt::ExprStmt(e) | Stmt::Throw(e) | Stmt::MonitorEnter(e) | Stmt::MonitorExit(e) => {
            f(e);
        }
        Stmt::Return(Some(e)) => f(e),
        Stmt::LocalDef { init: Some(e), .. } => f(e),
        Stmt::If {
            cond,
            then_stmt,
            else_stmt,
        } => {
            f(cond);
            visit_stmt_exprs_ro(then_stmt, f);
            if let Some(e) = else_stmt {
                visit_stmt_exprs_ro(e, f);
            }
        }
        Stmt::While { cond, body } => {
            f(cond);
            visit_stmt_exprs_ro(body, f);
        }
        Stmt::DoWhile { body, cond } => {
            visit_stmt_exprs_ro(body, f);
            f(cond);
        }
        Stmt::For {
            init,
            cond,
            update,
            body,
        } => {
            for x in init {
                visit_stmt_exprs_ro(x, f);
            }
            if let Some(c) = cond {
                f(c);
            }
            for u in update {
                f(u);
            }
            visit_stmt_exprs_ro(body, f);
        }
        Stmt::ForEach { iterable, body, .. } => {
            f(iterable);
            visit_stmt_exprs_ro(body, f);
        }
        Stmt::Switch {
            selector,
            cases,
            default,
            ..
        } => {
            f(selector);
            for c in cases {
                for x in &c.body {
                    visit_stmt_exprs_ro(x, f);
                }
                if let Some(g) = &c.guard {
                    f(g);
                }
            }
            if let Some(d) = default {
                visit_stmt_exprs_ro(d, f);
            }
        }
        Stmt::Try {
            body,
            catches,
            finally,
        } => {
            visit_stmt_exprs_ro(body, f);
            for c in catches {
                visit_stmt_exprs_ro(&c.body, f);
            }
            if let Some(fl) = finally {
                visit_stmt_exprs_ro(fl, f);
            }
        }
        Stmt::TryWithResources {
            resources,
            body,
            catches,
            finally,
            ..
        } => {
            for x in resources {
                visit_stmt_exprs_ro(x, f);
            }
            visit_stmt_exprs_ro(body, f);
            for c in catches {
                visit_stmt_exprs_ro(&c.body, f);
            }
            if let Some(fl) = finally {
                visit_stmt_exprs_ro(fl, f);
            }
        }
        Stmt::Assert { cond, msg } => {
            f(cond);
            if let Some(m) = msg {
                f(m);
            }
        }
        Stmt::Synchronized { lock, body } => {
            f(lock);
            visit_stmt_exprs_ro(body, f);
        }
        Stmt::TernaryValue { e } => f(e),
        Stmt::Labeled { body, .. } => visit_stmt_exprs_ro(body, f),
        _ => {}
    }
}

/// Count Local reads with the lost-alloc `<init>`-owner exclusion,
/// zero-clone: mirrors `strip_lost_alloc_owners` + deep_rewrite count
/// exactly (the strip replaced the owner subtree with Const(0), so the
/// owner's locals were never counted; nested exclusions cannot double-
/// skip because the excluded owner is never descended into).
fn count_expr_ro(e: &Expr, m: &mut jdc_core::FxHashMap<u32, usize>) {
    if let Expr::Local { var, .. } = e {
        *m.entry(*var).or_insert(0) += 1;
        return;
    }
    if let Expr::Method {
        name,
        is_special: true,
        owner: Some(o),
        args,
        ..
    } = e
    {
        if &**name == "<init>" && !matches!(**o, Expr::This) {
            for a in args {
                count_expr_ro(a, m);
            }
            return;
        }
    }
    for_each_child(e, &mut |c| count_expr_ro(c, m));
}

/// Visit every expression (immutable).
fn visit_exprs<F: FnMut(&Expr)>(e: &Expr, f: &mut F) {
    f(e);
    for_each_child(e, &mut |c| visit_exprs(c, f));
}

fn for_each_child<F: FnMut(&Expr)>(e: &Expr, f: &mut F) {
    match e {
        Expr::Un { e, .. } | Expr::Cast { e, .. } | Expr::InstanceOf { e, .. } => f(e),
        Expr::Bin { l, r, .. } => {
            f(l);
            f(r);
        }
        Expr::Cond { c, t, f: fe } => {
            f(c);
            f(t);
            f(fe);
        }
        Expr::Assign { target, value, .. } => {
            f(target);
            f(value);
        }
        Expr::PreIncDec { e, .. } | Expr::PostIncDec { e, .. } => f(e),
        Expr::Field { owner, .. } => {
            if let Some(o) = owner {
                f(o);
            }
        }
        Expr::Method { owner, args, .. } => {
            if let Some(o) = owner {
                f(o);
            }
            for a in args {
                f(a);
            }
        }
        Expr::ArrayIndex { array, index } => {
            f(array);
            f(index);
        }
        Expr::New { args, .. } => {
            for a in args {
                f(a);
            }
        }
        Expr::NewArray { dims, init, .. } => {
            for d in dims {
                f(d);
            }
            if let Some(v) = init {
                for x in v {
                    f(x);
                }
            }
        }
        Expr::NewMultiArray { dims, .. } => {
            for d in dims {
                f(d);
            }
        }
        Expr::StringConcat(parts) => {
            for p in parts {
                if let ConcatPart::Str(x) = p {
                    f(x);
                }
            }
        }
        Expr::Invokedynamic { args, .. } => {
            for a in args {
                f(a);
            }
        }
        Expr::AnonNew { args, .. } => {
            for a in args {
                f(a);
            }
        }
        _ => {}
    }
}

/// Mutable child walk (one level).
fn for_each_child_mut<F: FnMut(&mut Expr)>(e: &mut Expr, f: &mut F) {
    match e {
        Expr::Un { e, .. } | Expr::Cast { e, .. } | Expr::InstanceOf { e, .. } => f(e),
        Expr::Bin { l, r, .. } => {
            f(l);
            f(r);
        }
        Expr::Cond { c, t, f: fe } => {
            f(c);
            f(t);
            f(fe);
        }
        Expr::Assign { target, value, .. } => {
            f(target);
            f(value);
        }
        Expr::PreIncDec { e, .. } | Expr::PostIncDec { e, .. } => f(e),
        Expr::Field { owner, .. } => {
            if let Some(o) = owner {
                f(o);
            }
        }
        Expr::Method { owner, args, .. } => {
            if let Some(o) = owner {
                f(o);
            }
            for a in args.iter_mut() {
                f(a);
            }
        }
        Expr::ArrayIndex { array, index } => {
            f(array);
            f(index);
        }
        Expr::New { args, .. } => {
            for a in args.iter_mut() {
                f(a);
            }
        }
        Expr::NewArray { dims, init, .. } => {
            for d in dims.iter_mut() {
                f(d);
            }
            if let Some(v) = init {
                for x in v.iter_mut() {
                    f(x);
                }
            }
        }
        Expr::NewMultiArray { dims, .. } => {
            for d in dims.iter_mut() {
                f(d);
            }
        }
        Expr::StringConcat(parts) => {
            for p in parts {
                if let ConcatPart::Str(x) = p {
                    f(x);
                }
            }
        }
        Expr::Invokedynamic { args, .. } => {
            for a in args.iter_mut() {
                f(a);
            }
        }
        Expr::AnonNew { args, .. } => {
            for a in args.iter_mut() {
                f(a);
            }
        }
        _ => {}
    }
}

/// Deep mutable expression rewrite.
pub(crate) fn deep_rewrite<F: FnMut(&mut Expr)>(e: &mut Expr, f: &mut F) {
    f(e);
    for_each_child_mut(e, &mut |c| deep_rewrite(c, f));
}

/// `deep_rewrite` that respects WRITE positions: a `Local` appearing as
/// an assignment target or a `++`/`--` operand never reaches `f`.
/// Value-forwarding closures (`*x = value`) must use this — plain
/// deep_rewrite turned a forwarded `vX = 25` into `25 = 25` (target
/// replaced, and drop_defs then no longer recognized the statement, so
/// the garbage survived into the output; reqable a4/e). Compound
/// targets (`a[i] = …`, `o.f = …`) still recurse: owners and indices
/// ARE reads.
/// Read-only mirror of `deep_rewrite_reads` (same descent rules: a
/// plain Local assignment/inc-dec target is a WRITE and never reaches
/// `f`; compound targets still descend — owners and indices are reads).
fn visit_exprs_reads<F: FnMut(&Expr)>(e: &Expr, f: &mut F) {
    f(e);
    match e {
        Expr::Assign { target, value, .. } => {
            if !matches!(**target, Expr::Local { .. }) {
                visit_exprs_reads(target, f);
            }
            visit_exprs_reads(value, f);
        }
        Expr::PreIncDec { e: inner, .. } | Expr::PostIncDec { e: inner, .. } => {
            if !matches!(**inner, Expr::Local { .. }) {
                visit_exprs_reads(inner, f);
            }
        }
        _ => for_each_child(e, &mut |c| visit_exprs_reads(c, f)),
    }
}

fn deep_rewrite_reads<F: FnMut(&mut Expr)>(e: &mut Expr, f: &mut F) {
    f(e);
    match e {
        Expr::Assign { target, value, .. } => {
            if !matches!(**target, Expr::Local { .. }) {
                deep_rewrite_reads(target, f);
            }
            deep_rewrite_reads(value, f);
        }
        Expr::PreIncDec { e: inner, .. } | Expr::PostIncDec { e: inner, .. } => {
            if !matches!(**inner, Expr::Local { .. }) {
                deep_rewrite_reads(inner, f);
            }
        }
        _ => for_each_child_mut(e, &mut |c| deep_rewrite_reads(c, f)),
    }
}

fn collect_vars(e: &Expr, out: &mut HashSet<u32>) {
    visit_exprs(e, &mut |x| {
        if let Expr::Local { var, .. } = x {
            out.insert(*var);
        }
    });
}

/// Collect variable references (reads; `assignments` adds assignment
/// targets). Single-pass recursion — the previous shape wrapped a
/// self-recursive closure in `walk_all`, visiting every subtree twice over
/// (exponential in nesting depth).
fn stmt_collect_vars(s: &Stmt, out: &mut HashSet<u32>, assignments: bool) {
    match s {
        Stmt::Block(v) => {
            for x in v {
                stmt_collect_vars(x, out, assignments);
            }
        }
        Stmt::ExprStmt(Expr::Assign { target, value, .. }) => {
            if let Expr::Local { var, .. } = &**target {
                if assignments {
                    out.insert(*var);
                }
            } else {
                collect_vars(target, out);
            }
            collect_vars(value, out);
        }
        Stmt::ExprStmt(e) | Stmt::Throw(e) | Stmt::MonitorEnter(e) | Stmt::MonitorExit(e) => {
            collect_vars(e, out)
        }
        Stmt::Return(Some(e)) => collect_vars(e, out),
        Stmt::LocalDef { var, init, .. } => {
            if assignments {
                out.insert(*var);
            }
            if let Some(e) = init {
                collect_vars(e, out);
            }
        }
        Stmt::If {
            cond,
            then_stmt,
            else_stmt,
        } => {
            collect_vars(cond, out);
            stmt_collect_vars(then_stmt, out, assignments);
            if let Some(e) = else_stmt {
                stmt_collect_vars(e, out, assignments);
            }
        }
        Stmt::While { cond, body } => {
            collect_vars(cond, out);
            stmt_collect_vars(body, out, assignments);
        }
        Stmt::DoWhile { body, cond } => {
            stmt_collect_vars(body, out, assignments);
            collect_vars(cond, out);
        }
        Stmt::For {
            init,
            cond,
            update,
            body,
        } => {
            for x in init {
                stmt_collect_vars(x, out, assignments);
            }
            if let Some(c) = cond {
                collect_vars(c, out);
            }
            for u in update {
                collect_vars(u, out);
            }
            stmt_collect_vars(body, out, assignments);
        }
        Stmt::ForEach {
            var,
            iterable,
            body,
            ..
        } => {
            if assignments {
                out.insert(*var);
            }
            collect_vars(iterable, out);
            stmt_collect_vars(body, out, assignments);
        }
        Stmt::Switch {
            selector,
            cases,
            default,
            ..
        } => {
            collect_vars(selector, out);
            for c in cases {
                for x in &c.body {
                    stmt_collect_vars(x, out, assignments);
                }
            }
            if let Some(d) = default {
                stmt_collect_vars(d, out, assignments);
            }
        }
        Stmt::Try {
            body,
            catches,
            finally,
        } => {
            stmt_collect_vars(body, out, assignments);
            for c in catches {
                stmt_collect_vars(&c.body, out, assignments);
            }
            if let Some(f) = finally {
                stmt_collect_vars(f, out, assignments);
            }
        }
        Stmt::TryWithResources {
            resources,
            body,
            catches,
            finally,
        } => {
            for x in resources {
                stmt_collect_vars(x, out, assignments);
            }
            stmt_collect_vars(body, out, assignments);
            for c in catches {
                stmt_collect_vars(&c.body, out, assignments);
            }
            if let Some(f) = finally {
                stmt_collect_vars(f, out, assignments);
            }
        }
        Stmt::Synchronized { lock, body } => {
            collect_vars(lock, out);
            stmt_collect_vars(body, out, assignments);
        }
        Stmt::Assert { cond, msg } => {
            collect_vars(cond, out);
            if let Some(m) = msg {
                collect_vars(m, out);
            }
        }
        Stmt::Labeled { body, .. } => stmt_collect_vars(body, out, assignments),
        _ => {}
    }
}

pub(crate) fn walk_all<F: FnMut(&Stmt)>(s: &Stmt, f: &mut F) {
    f(s);
    match s {
        Stmt::Block(v) => {
            for x in v {
                walk_all(x, f);
            }
        }
        Stmt::If {
            then_stmt,
            else_stmt,
            ..
        } => {
            walk_all(then_stmt, f);
            if let Some(e) = else_stmt {
                walk_all(e, f);
            }
        }
        Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => walk_all(body, f),
        Stmt::For { init, body, .. } => {
            for x in init {
                walk_all(x, f);
            }
            walk_all(body, f);
        }
        Stmt::ForEach { body, .. } => walk_all(body, f),
        Stmt::Switch { cases, default, .. } => {
            for c in cases {
                for x in &c.body {
                    walk_all(x, f);
                }
            }
            if let Some(d) = default {
                walk_all(d, f);
            }
        }
        Stmt::Try {
            body,
            catches,
            finally,
        } => {
            walk_all(body, f);
            for c in catches {
                walk_all(&c.body, f);
            }
            if let Some(fl) = finally {
                walk_all(fl, f);
            }
        }
        Stmt::TryWithResources {
            resources,
            body,
            catches,
            finally,
        } => {
            for x in resources {
                walk_all(x, f);
            }
            walk_all(body, f);
            for c in catches {
                walk_all(&c.body, f);
            }
            if let Some(fl) = finally {
                walk_all(fl, f);
            }
        }
        Stmt::Synchronized { body, .. } | Stmt::Labeled { body, .. } => walk_all(body, f),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Passes
// ---------------------------------------------------------------------------

/// Bind catch parameters: the handler's first statement (a bare LocalDef
/// from move-exception) becomes the catch variable.
pub fn bind_catches(s: &mut Stmt, vt: &mut VarTable) {
    // Vars defined anywhere in the method (params + LocalDefs +
    // assign targets): a catch body reading a var OUTSIDE this set is
    // the unmaterialized move-exception register (the lifter minted it
    // without a defining statement).
    let mut defined: jdc_core::FxHashSet<u32> = jdc_core::FxHashSet::default();
    for v in &vt.vars {
        if v.is_param {
            defined.insert(v.id);
        }
    }
    collect_defined_locals(s, &mut defined);
    let reads_all = count_locals_stmts(std::slice::from_ref(s));
    // Vars with a real assignment (LocalDef-with-init or assign target):
    // a var only ever READ is either the unmaterialized move-exception
    // register or a bare declaration awaiting its hoisted assignment.
    let mut assigned: jdc_core::FxHashSet<u32> = jdc_core::FxHashSet::default();
    collect_assigned_locals(s, &mut assigned);
    let mut fallback_bound: Vec<u32> = Vec::new();
    bind_catches_walk(s, vt, &defined, &reads_all, &assigned, &mut fallback_bound);
    // The catch parameter IS the declaration now: drop the bare
    // `Throwable th;` hoists for vars the fallback bound.
    if !fallback_bound.is_empty() {
        remove_bare_decls(s, &fallback_bound);
    }
}

fn collect_defined_locals(s: &Stmt, out: &mut jdc_core::FxHashSet<u32>) {
    crate::passes::walk_all(s, &mut |st| {
        match st {
            Stmt::LocalDef { var, .. } => {
                out.insert(*var);
            }
            Stmt::ExprStmt(Expr::Assign { target, .. }) => {
                if let Expr::Local { var, .. } = &**target {
                    out.insert(*var);
                }
            }
            _ => {}
        }
    });
}

/// Vars with a bare `LocalDef{init: None}` declaration in `s` (dedup,
/// declaration order).
fn collect_bare_decls(s: &Stmt, out: &mut Vec<u32>) {
    walk_all(s, &mut |st| {
        if let Stmt::LocalDef { var, init: None, .. } = st {
            if !out.contains(var) {
                out.push(*var);
            }
        }
    });
}


/// First read of a local that is defined nowhere in the method (and is
/// not a parameter) inside the catch body — the unmaterialized
/// move-exception register.
/// Vars carrying a real assignment (init or write target).
fn collect_assigned_locals(s: &Stmt, out: &mut jdc_core::FxHashSet<u32>) {
    crate::passes::walk_all(s, &mut |st| {
        match st {
            Stmt::LocalDef { var, init: Some(_), .. } => {
                out.insert(*var);
            }
            Stmt::ExprStmt(Expr::Assign { target, .. }) => {
                if let Expr::Local { var, .. } = &**target {
                    out.insert(*var);
                }
            }
            _ => {}
        }
    });
}

/// First `throw <local>` var in the catch body that is never assigned
/// anywhere and is exception-typed — the unmaterialized move-exception.
fn first_thrown_unassigned_local(
    body: &Stmt,
    vt: &VarTable,
    assigned: &jdc_core::FxHashSet<u32>,
) -> Option<u32> {
    let mut found: Option<u32> = None;
    let c = body.clone();
    walk_all(&c, &mut |st| {
        if let Stmt::Throw(th) = st {
            if let Expr::Local { var, .. } = th {
                let v = *var;
                if !assigned.contains(&v) {
                    if let JavaType::Object(o) = vt.var(v).ty.erased() {
                        if o.as_ref() == "java/lang/Throwable" {
                            found = Some(v);
                        }
                    }
                }
            }
        }
    });
    found
}

/// Remove the first LEAF statement (mirror of first_leaf_stmt), pruning
/// leading blocks that became empty — the stored-def bind consumes the
/// move-exception decl wherever the region walk nested it.
fn remove_first_leaf(s: &mut Stmt) {
    if let Stmt::Block(v) = s {
        if let Some(f) = v.first_mut() {
            remove_first_leaf(f);
        }
        while matches!(v.first(), Some(Stmt::Block(b)) if b.is_empty()) {
            v.remove(0);
        }
    } else {
        *s = Stmt::Block(vec![]);
    }
}

/// Remove bare `LocalDef{var, init: None}` declarations for the given
/// vars (the catch parameter is their declaration now).
fn remove_bare_decls(s: &mut Stmt, vars: &[u32]) {
    let vars: jdc_core::FxHashSet<u32> = vars.iter().copied().collect();
    walk_mut_deep(s, &mut |st| {
        if let Stmt::Block(v) = st {
            v.retain(|x| {
                !matches!(x, Stmt::LocalDef { var, init: None, .. } if vars.contains(var))
            });
        }
    });
}

fn first_undefined_local_read(
    body: &Stmt,
    defined: &jdc_core::FxHashSet<u32>,
) -> Option<u32> {
    let mut found: Option<u32> = None;
    visit_stmt_exprs_ro(body, &mut |e| {
        if found.is_none() {
            visit_exprs(e, &mut |x| {
                if found.is_none() {
                    if let Expr::Local { var, .. } = x {
                        if !defined.contains(var) {
                            found = Some(*var);
                        }
                    }
                }
            });
        }
    });
    found
}

#[allow(clippy::too_many_arguments)]
fn bind_catches_walk(
    s: &mut Stmt,
    vt: &mut VarTable,
    defined: &jdc_core::FxHashSet<u32>,
    reads_all: &jdc_core::FxHashMap<u32, usize>,
    assigned: &jdc_core::FxHashSet<u32>,
    fallback_bound: &mut Vec<u32>,
) {
    match s {
        Stmt::Try {
            body,
            catches,
            finally,
        } => {
            bind_catches_walk(body, vt, defined, reads_all, assigned, fallback_bound);
            for c in catches.iter_mut() {
                if c.var == u32::MAX {
                    // Leaf-piercing head check: the region walk can wrap
                    // the handler body as Block[Block[…]] (weixin ri5/c
                    // thx rethrow family — the top-level `v.first()` saw
                    // a Block, all three bind paths missed, and the emit
                    // fell back to `catch (Throwable ignored)` beside a
                    // never-assigned `Throwable th;` read).
                    let stored = match first_leaf_stmt(c.body.as_ref()) {
                        // ONLY the bare move-exception decl (the
                        // lifter emits LocalDef{init:None} for it).
                        // A first stmt with a REAL init is handler
                        // computation — consuming it as the catch
                        // store deleted the def and hijacked its
                        // reads to the catch param (lark ih6/w's
                        // `v11 = Thread.currentThread()` →
                        // `catch (InterruptedException thread2) {
                        // thread2.interrupt(); }`; ch6/e cls4.
                        // getName; the exception-ignoring catch is
                        // the standard interrupt idiom).
                        Stmt::LocalDef { var, init: None, .. } => Some(*var),
                        // Phi-copy shape `v = exc` where exc is the
                        // unmaterialized move-exception register
                        // (defined nowhere in the method).
                        Stmt::ExprStmt(Expr::Assign { target, value, .. }) => {
                            match (&**target, &**value) {
                                (Expr::Local { var, .. }, Expr::Local { var: src, .. })
                                    if !defined.contains(src) =>
                                {
                                    Some(*var)
                                }
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    if let Some(v) = stored {
                        let exc = c
                            .exc
                            .first()
                            .cloned()
                            .unwrap_or_else(|| "java/lang/Throwable".into());
                        let slot = vt.var(v).slot;
                        let name = "e".to_string();
                        let new_var =
                            vt.add_catch_var(slot, name, TypeRef::J(JavaType::Object(exc)));
                        remove_first_leaf(c.body.as_mut());
                        rewrite_local_refs(c.body.as_mut(), v, new_var);
                        c.var = new_var;
                    } else if let Some(v) = first_undefined_local_read(c.body.as_ref(), defined)
                        .filter(|v| {
                            // Bind only a var read NOWHERE outside this
                            // catch: ensure_declared would hoist an
                            // outside-read var to a method local, and
                            // consuming it as the catch parameter would
                            // scope it too narrowly.
                            let in_catch = count_locals_stmts(std::slice::from_ref(c.body.as_ref()));
                            reads_all.get(v).copied().unwrap_or(0)
                                == in_catch.get(v).copied().unwrap_or(0)
                        })
                        .filter(|v| {
                            // And only an EXCEPTION-typed var: the
                            // move-exception registers carry the handler
                            // type — a plain local pending its hoisted
                            // declaration (StringBuilder sb) must not be
                            // consumed as the catch parameter (reqable
                            // amazon: the declaration vanished and its
                            // outer readers broke).
                            let ty = vt.var(*v).ty.erased();
                            let exc = c
                                .exc
                                .first()
                                .cloned()
                                .unwrap_or_else(|| "java/lang/Throwable".into());
                            matches!(&ty, JavaType::Object(o)
                                if o.as_ref() == exc.as_ref()
                                    || o.as_ref() == "java/lang/Throwable")
                        })
                    {
                        // The handler's first statement is not the
                        // move-exception def (the d8 synchronized pattern
                        // leads with MonitorExit): the register the lifter
                        // minted for move-exception is still READ in the
                        // body (`throw th;`) with no defining statement
                        // anywhere. Bind IT as the catch parameter —
                        // otherwise emit printed `catch (Throwable ignored)`
                        // over an undeclared `throw th` (definite-assignment
                        // failure, 6.4k weibo sites).
                        c.var = v;
                        fallback_bound.push(v);
                    } else if let Some(v) =
                        first_thrown_unassigned_local(c.body.as_ref(), vt, assigned)
                            .filter(|v| {
                                let in_catch =
                                    count_locals_stmts(std::slice::from_ref(c.body.as_ref()));
                                reads_all.get(v).copied().unwrap_or(0)
                                    == in_catch.get(v).copied().unwrap_or(0)
                            })
                            .filter(|v| {
                                // Same exception-type gate: the hoisted
                                // `Throwable th;` declares the var but the
                                // move-exception never assigned it.
                                let ty = vt.var(*v).ty.erased();
                                let exc = c
                                    .exc
                                    .first()
                                    .cloned()
                                    .unwrap_or_else(|| "java/lang/Throwable".into());
                                matches!(&ty, JavaType::Object(o)
                                    if o.as_ref() == exc.as_ref()
                                        || o.as_ref() == "java/lang/Throwable")
                            })
                    {
                        // Declared-never-assigned: `Throwable th;` hoisted
                        // by an earlier phase, the catch body's `throw th`
                        // its only use — the move-exception assignment was
                        // never materialized. Bind th as the catch
                        // parameter and drop the bare declaration.
                        c.var = v;
                        fallback_bound.push(v);
                    }
                }
                if c.var != u32::MAX {
                    // Second move-exception identity INSIDE the body: R8
                    // guards the handler's own close() with a nested
                    // try/finally and re-materializes the in-flight
                    // exception on its exceptional continuation as a
                    // fresh var (`Throwable th7; throw th7;` — weixin
                    // y5/b, the 327-site residue of the leaf-piercing
                    // fix). The twin is never ASSIGNED anywhere in the
                    // method (its register only ever receives
                    // move-exception) and the catch param IS its value —
                    // rewrite the twin's reads to the param and drop the
                    // bare decl. The never-assigned gate makes the
                    // rewrite flow-insensitive: every body read of the
                    // twin reads the exception. Twins read outside this
                    // catch are left alone (the param's scope ends here).
                    let in_catch =
                        count_locals_stmts(std::slice::from_ref(c.body.as_ref()));
                    let mut twins: Vec<u32> = Vec::new();
                    collect_bare_decls(c.body.as_ref(), &mut twins);
                    let mut swept: Vec<u32> = Vec::new();
                    for x in twins {
                        if x == c.var || assigned.contains(&x) {
                            continue;
                        }
                        if reads_all.get(&x).copied().unwrap_or(0)
                            != in_catch.get(&x).copied().unwrap_or(0)
                        {
                            continue;
                        }
                        if !matches!(vt.var(x).ty.erased(),
                            JavaType::Object(o) if o.as_ref() == "java/lang/Throwable")
                        {
                            continue;
                        }
                        rewrite_local_refs(c.body.as_mut(), x, c.var);
                        swept.push(x);
                    }
                    if !swept.is_empty() {
                        remove_bare_decls(c.body.as_mut(), &swept);
                    }
                }
                bind_catches_walk(&mut c.body, vt, defined, reads_all, assigned, fallback_bound);
            }
            if let Some(f) = finally {
                bind_catches_walk(f.as_mut(), vt, defined, reads_all, assigned, fallback_bound);
            }
        }
        Stmt::Block(v) => {
            for x in v.iter_mut() {
                bind_catches_walk(x, vt, defined, reads_all, assigned, fallback_bound);
            }
        }
        Stmt::If {
            then_stmt,
            else_stmt,
            ..
        } => {
            bind_catches_walk(then_stmt, vt, defined, reads_all, assigned, fallback_bound);
            if let Some(e) = else_stmt {
                bind_catches_walk(e, vt, defined, reads_all, assigned, fallback_bound);
            }
        }
        Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => bind_catches_walk(body, vt, defined, reads_all, assigned, fallback_bound),
        Stmt::For { init, body, .. } => {
            for x in init.iter_mut() {
                bind_catches_walk(x, vt, defined, reads_all, assigned, fallback_bound);
            }
            bind_catches_walk(body, vt, defined, reads_all, assigned, fallback_bound);
        }
        Stmt::ForEach { body, .. }
        | Stmt::Labeled { body, .. }
        | Stmt::Synchronized { body, .. } => bind_catches_walk(body, vt, defined, reads_all, assigned, fallback_bound),
        Stmt::Switch { cases, default, .. } => {
            for c in cases.iter_mut() {
                for x in c.body.iter_mut() {
                    bind_catches_walk(x, vt, defined, reads_all, assigned, fallback_bound);
                }
            }
            if let Some(d) = default {
                bind_catches_walk(d, vt, defined, reads_all, assigned, fallback_bound);
            }
        }
        _ => {}
    }
}

fn rewrite_local_refs(s: &mut Stmt, from: u32, to: u32) {
    rewrite_exprs(s, &mut |e| {
        deep_rewrite(e, &mut |x| {
            if let Expr::Local { var, .. } = x {
                if *var == from {
                    *var = to;
                }
            }
        });
    });
}

/// Flatten blocks, drop empty ones and trailing no-op statements.
pub fn cleanup(s: &mut Stmt) {
    jdc_core::ir::stmt::flatten(s);
    prune_dead_tails(s);
    strip_empty(s);
    merge_singleton_stmts(s);
}

/// JLS "cannot complete normally" pruning: drop statements after the
/// first hard terminator in any statement sequence — blocks, switch
/// case bodies (bare Vecs), for-inits, catch bodies. The 无法访问的语句
/// family: `continue; break;` inside switch cases and `return` after a
/// try{…return…}catch{…return…} (pdd h0/a, c20/d; ~5.9k sites across
/// pdd/cmb/jianying). Conservative: loops and switches never count as
/// terminators (no infinite-loop modeling), sequences still carrying
/// fallback Label/Goto statements stay untouched, and only
/// Return/Throw/Break/Continue plus If/Try/Labeled/Synchronized/Block
/// compositions of them terminate.
fn seq_terminates(s: &Stmt) -> bool {
    match s {
        Stmt::Return(_) | Stmt::Throw(_) | Stmt::Break(_) | Stmt::Continue(_) => {
            true
        }
        Stmt::Block(v) => v.iter().any(seq_terminates),
        Stmt::If {
            then_stmt,
            else_stmt,
            ..
        } => {
            seq_terminates(then_stmt)
                && else_stmt
                    .as_ref()
                    .map(|e| seq_terminates(e))
                    .unwrap_or(false)
        }
        Stmt::Try {
            body,
            catches,
            finally,
        }
        | Stmt::TryWithResources {
            body,
            catches,
            finally,
            ..
        } => {
            if let Some(f) = finally {
                if seq_terminates(f) {
                    return true;
                }
            }
            seq_terminates(body) && catches.iter().all(|c| seq_terminates(&c.body))
        }
        Stmt::Labeled { body, .. } | Stmt::Synchronized { body, .. } => {
            seq_terminates(body)
        }
        // A break-less infinite while/do-while never completes normally
        // either — jdc-core's label-aware analysis (the emit DEADEND
        // truncator uses it): statements after it are 无法访问的语句 the
        // same way (the bulk of the pruning residue: jianying 738).
        Stmt::While { .. } | Stmt::DoWhile { .. } => {
            jdc_core::analysis::dead_end_infinite_while(s)
        }
        _ => false,
    }
}

fn prune_seq(v: &mut Vec<Stmt>) {
    if v.iter()
        .any(|x| matches!(x, Stmt::Label(_) | Stmt::Goto(_)))
    {
        return;
    }
    if let Some(i) = v.iter().position(seq_terminates) {
        v.truncate(i + 1);
    }
}

pub fn prune_dead_tails(s: &mut Stmt) {
    match s {
        Stmt::Block(v) => {
            for x in v.iter_mut() {
                prune_dead_tails(x);
            }
            prune_seq(v);
        }
        Stmt::If {
            then_stmt,
            else_stmt,
            ..
        } => {
            prune_dead_tails(then_stmt);
            if let Some(e) = else_stmt {
                prune_dead_tails(e);
            }
        }
        Stmt::While { body, .. }
        | Stmt::DoWhile { body, .. }
        | Stmt::ForEach { body, .. }
        | Stmt::Labeled { body, .. }
        | Stmt::Synchronized { body, .. } => prune_dead_tails(body),
        Stmt::For { init, body, .. } => {
            for x in init.iter_mut() {
                prune_dead_tails(x);
            }
            prune_seq(init);
            prune_dead_tails(body);
        }
        Stmt::Switch {
            cases, default, ..
        } => {
            for c in cases {
                for x in c.body.iter_mut() {
                    prune_dead_tails(x);
                }
                prune_seq(&mut c.body);
            }
            if let Some(d) = default {
                prune_dead_tails(d);
            }
        }
        Stmt::Try {
            body,
            catches,
            finally,
        }
        | Stmt::TryWithResources {
            body,
            catches,
            finally,
            ..
        } => {
            prune_dead_tails(body);
            for c in catches {
                prune_dead_tails(&mut c.body);
            }
            if let Some(f) = finally {
                prune_dead_tails(f);
            }
        }
        _ => {}
    }
}

/// Drop assignments whose target field is a PHANTOM: the declaring class
/// is pool-materialized but does not declare the field (R8 inlines R-class
/// constants everywhere, strips the field declarations, and leaves the
/// `<clinit>` husk still sput-ing to them — QQ's R$anim/kj3.a husks:
/// class_data with 0 fields beside a clinit of phantom sget→sput pairs,
/// 7,774 找不到符号 in four R.java files alone). The dex statements are
/// dead by construction — executing them is a guaranteed NoSuchFieldError,
/// and R8 only keeps them because nothing triggers the husk's clinit — so
/// dropping them preserves every observable behavior while the faithful
/// render (`x = kj3.a.x;` against declared-nowhere names) cannot compile.
/// Conservative gates: only plain ExprStmt assignments whose TARGET is a
/// phantom static/instance field of a materialized class, and whose VALUE
/// is side-effect-free (const / local / phantom-field read / cast chain) —
/// a real-field read could trigger another class's clinit, and a call
/// could carry side effects, so those statements stay.
pub fn strip_phantom_field_writes(body: &mut Stmt, pool: &DexPool) {
    fn phantom(pool: &DexPool, cls: &str, name: &str) -> bool {
        // pool.get (on-demand materialize), not get_if_materialized:
        // lazy getclass runs must reach the same verdicts as full runs
        // (the RHS holder kj3/a is not pre-materialized there).
        let Some(pc) = pool.get(cls) else {
            return false;
        };
        // The IR Field ref carries the DISPLAY name (the lifter emits
        // through field_display) while the pool tables carry RAW names —
        // match both, or an obscured-renamed field (`h` → `h17` beside
        // class h) reads as phantom and its const-build assignment gets
        // dropped, breaking enum promotion (weixin z54/h fell back to
        // `/* enum */ class`, taking every `h[] → Enum[]` conversion
        // down with it — the +193 weixin battery regression).
        fn declares(pc: &crate::PoolClass, cls: &str, name: &str) -> bool {
            pc.static_fields
                .iter()
                .chain(pc.instance_fields.iter())
                .any(|f| {
                    f.name.as_ref() == name
                        || jdc_core::rename::field_display(cls, &f.name, &f.desc)
                            .is_some_and(|d| d == name)
                })
        }
        if declares(pc, cls, name) {
            return false;
        }
        // Field resolution walks the superclass chain, so "phantom" must
        // be PROVEN against the whole chain — a write to an inherited
        // field is real code (QQ com.tencent.gdtad.statistics.d inherits
        // `b` from super `e`; an own-fields-only verdict dropped the
        // legitimate `v10x.b = gdtAd2;` — silent data loss, latent until
        // the walk started descending into If.then_stmt). Off-pool
        // ancestors are enumerated through fwdb (framework fields like
        // View.mID are real); an ancestor NEITHER pool nor fwdb knows
        // (R8-stripped class) ends the proof — keep the write.
        let mut cur: Option<String> = pc.super_name.clone();
        let mut hops = 0u32;
        while let Some(s) = cur {
            hops += 1;
            if hops > 64 {
                return false;
            }
            if let Some(sup) = pool.get(&s) {
                if declares(sup, &s, name) {
                    return false;
                }
                cur = sup.super_name.clone();
            } else {
                if !crate::fwdb::exists(&s) {
                    return false;
                }
                let mut found = false;
                crate::fwdb::for_each_field(&s, |f, _| {
                    if f == name {
                        found = true;
                    }
                });
                if found {
                    return false;
                }
                // fwdb strips the Object super edge: the chain ends here.
                cur = crate::fwdb::super_of(&s).map(|x| x.to_string());
            }
        }
        true
    }
    fn droppable_value(pool: &DexPool, e: &Expr) -> bool {
        match e {
            Expr::Const(_) | Expr::Local { .. } => true,
            Expr::Field { cls, name, .. } => phantom(pool, cls, name),
            Expr::Cast { e, .. } => droppable_value(pool, e),
            _ => false,
        }
    }
    fn is_phantom_write(pool: &DexPool, st: &Stmt) -> bool {
        if let Stmt::ExprStmt(Expr::Assign { target, value, .. }) = st {
            if let Expr::Field { cls, name, .. } = &**target {
                return phantom(pool, cls, name) && droppable_value(pool, value);
            }
        }
        false
    }
    // walk_mut_deep over every Vec-holding node — the bare-Vec positions
    // (switch case bodies, For init, TWR resources) are the known blind
    // spot of Block-only recursion.
    walk_mut_deep(body, &mut |st| match st {
        Stmt::Block(v) => v.retain(|x| !is_phantom_write(pool, x)),
        Stmt::Switch { cases, .. } => {
            for c in cases.iter_mut() {
                c.body.retain(|x| !is_phantom_write(pool, x));
            }
        }
        Stmt::For { init, .. } => init.retain(|x| !is_phantom_write(pool, x)),
        Stmt::TryWithResources { resources, .. } => {
            resources.retain(|x| !is_phantom_write(pool, x))
        }
        _ => {}
    });
    if is_phantom_write(pool, body) {
        *body = Stmt::Block(vec![]);
    }
}

/// Late default-init for surviving bare declarations (`T v;`). Runs
/// AFTER every fold/ctor/hotfix matcher that keys on `init: None`
/// (bind_catches, strip_clinit_hotfix_guard, the folding prefix scans),
/// so nothing observes the initialized form until emission. Dex
/// registers are zero-initialized at method entry, so `= null/0/false`
/// is the FAITHFUL value for any read that reaches it before the first
/// write — and javac's definite assignment rejects the bare form on
/// partial paths (可能尚未初始化变量: pdd gi1/a obj12 assigned in two
/// branches and read after the merge; also turns the residual
/// move-exception twins into compilable — and register-faithful —
/// null reads instead of undefined ones).
pub fn init_bare_decls(body: &mut Stmt, vt: &VarTable) {
    walk_mut_deep(body, &mut |st| {
        if let Stmt::LocalDef { var, init, .. } = st {
            if init.is_some() {
                return;
            }
            let lit = match vt.var(*var).ty.erased() {
                JavaType::Boolean => "false",
                JavaType::Int
                | JavaType::Byte
                | JavaType::Short
                | JavaType::Char => "0",
                JavaType::Long => "0L",
                JavaType::Float => "0.0f",
                JavaType::Double => "0.0d",
                JavaType::Void => return,
                JavaType::Object(_) | JavaType::Array(_) => "null",
            };
            *init = Some(Expr::Raw(lit.to_string()));
        }
    });
}

fn strip_empty(s: &mut Stmt) {
    match s {
        Stmt::Block(v) => {
            v.retain(|x| !x.is_empty_block());
            for x in v.iter_mut() {
                strip_empty(x);
            }
        }
        Stmt::If {
            then_stmt,
            else_stmt,
            ..
        } => {
            strip_empty(then_stmt);
            if let Some(e) = else_stmt {
                strip_empty(e);
                if e.is_empty_block() {
                    *else_stmt = None;
                }
            }
        }
        Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => strip_empty(body),
        Stmt::For { init, body, .. } => {
            init.retain(|x| !x.is_empty_block());
            strip_empty(body);
        }
        Stmt::ForEach { body, .. } => strip_empty(body),
        Stmt::Switch { cases, default, .. } => {
            for c in cases {
                c.body.retain(|x| !x.is_empty_block());
            }
            if let Some(d) = default {
                strip_empty(d);
            }
        }
        Stmt::Try {
            body,
            catches,
            finally,
        } => {
            strip_empty(body);
            for c in catches {
                strip_empty(&mut c.body);
            }
            if let Some(f) = finally {
                strip_empty(f);
            }
        }
        Stmt::Synchronized { body, .. } | Stmt::Labeled { body, .. } => strip_empty(body),
        _ => {}
    }
}

fn merge_singleton_stmts(s: &mut Stmt) {
    if let Stmt::Block(v) = s {
        for x in v.iter_mut() {
            merge_singleton_stmts(x);
        }
    }
}

/// Drop statements after a definite terminator within a block.
pub fn prune_unreachable(s: &mut Stmt) {
    if let Stmt::Block(v) = s {
        let mut cut: Option<usize> = None;
        for (i, x) in v.iter().enumerate() {
            if matches!(x, Stmt::Return(_) | Stmt::Throw(_)) {
                cut = Some(i + 1);
                break;
            }
        }
        if let Some(c) = cut {
            if c < v.len() {
                v.truncate(c);
            }
        }
        for x in v.iter_mut() {
            prune_unreachable(x);
        }
    } else {
        walk_mut(s, &mut prune_unreachable);
    }
}

fn walk_mut<F: FnMut(&mut Stmt)>(s: &mut Stmt, f: &mut F) {
    match s {
        Stmt::Block(v) => {
            for x in v.iter_mut() {
                f(x);
            }
        }
        Stmt::If {
            then_stmt,
            else_stmt,
            ..
        } => {
            f(then_stmt);
            if let Some(e) = else_stmt {
                f(e);
            }
        }
        Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => f(body),
        Stmt::For { init, body, .. } => {
            for x in init.iter_mut() {
                f(x);
            }
            f(body);
        }
        Stmt::ForEach { body, .. } => f(body),
        Stmt::Switch { cases, default, .. } => {
            for c in cases {
                for x in c.body.iter_mut() {
                    f(x);
                }
            }
            if let Some(d) = default {
                f(d);
            }
        }
        Stmt::Try {
            body,
            catches,
            finally,
        } => {
            f(body);
            for c in catches {
                f(&mut c.body);
            }
            if let Some(fl) = finally {
                f(fl);
            }
        }
        Stmt::TryWithResources {
            resources,
            body,
            catches,
            finally,
        } => {
            for r in resources.iter_mut() {
                f(r);
            }
            f(body);
            for c in catches {
                f(&mut c.body);
            }
            if let Some(fl) = finally {
                f(fl);
            }
        }
        Stmt::Synchronized { body, .. } | Stmt::Labeled { body, .. } => f(body),
        _ => {}
    }
}

/// Count LocalDef statements (pipeline diagnostics).
pub fn count_localdefs(s: &Stmt, out: &mut usize) {
    if matches!(s, Stmt::LocalDef { .. }) {
        *out += 1;
    }
    walk_all(s, &mut |x| {
        if matches!(x, Stmt::LocalDef { .. }) {
            *out += 1;
        }
    });
}

/// `if (c) { } else { B }` → `if (!c) { B }` — the empty-then shape is
/// how the structurer lands an inverted diamond, and source never writes
/// it (jadx prints the positive form). negate() carries De Morgan so
/// composed conditions stay clean.
pub fn invert_empty_thens(s: &mut Stmt) {
    walk_mut_deep(s, &mut |st| {
        if let Stmt::If {
            cond,
            then_stmt,
            else_stmt,
        } = st
        {
            let then_empty = matches!(&**then_stmt, Stmt::Block(v) if v.is_empty());
            if then_empty {
                if let Some(els) = else_stmt.take() {
                    let c = std::mem::replace(cond, Expr::Const(ConstVal::Int(0)));
                    *cond = jdc_core::convert::negate(c);
                    *then_stmt = els;
                }
            }
        }
    });
}

/// Fold if-diamonds back into short-circuit conditions (DAD's
/// short_circuit_struct, statement level). The structurer emits nested
/// ifs for `a && b` / `a || b` bytecode diamonds; javac source almost
/// never nests them, and jadx folds all four shapes:
///   if (c1) { if (c2) {T} else {F} } else {F}  →  if (c1 && c2) {T} else {F}
///   if (c1) { if (c2) {F} else {T} } else {F}  →  if (c1 && !c2) {T} else {F}
///   if (c1) {T} else { if (c2) {T} else {F} }  →  if (c1 || c2) {T} else {F}
///   if (c1) {T} else { if (c2) {F} else {T} }  →  if (c1 || !c2) {T} else {F}
/// Branch identity is deep structural equality (Stmt: PartialEq);
/// iterate to a fixpoint so chained diamonds `(a && b) && c` collapse.
pub fn fold_short_circuits(s: &mut Stmt) {
    for _ in 0..8 {
        let mut changed = false;
        walk_mut_deep(s, &mut |st| {
            if try_fold_diamond(st) {
                changed = true;
            }
        });
        if !changed {
            break;
        }
    }
}

fn try_fold_diamond(st: &mut Stmt) -> bool {
    enum Shape {
        ThenAnd,
        ThenAndNot,
        ElseOr,
        ElseOrNot,
        BareAnd,
        BareAndNot,
    }
    // Probe the shape on an immutable borrow first (the fold takes the
    // whole If apart; matching and rebuilding inside one borrow is not
    // expressible).
    let shape = match &*st {
        // One-sided diamonds: the shared branch is the implicit empty
        // fall-through (`if (c1) { if (c2) {T} }` → `if (c1 && c2) {T}`).
        Stmt::If {
            then_stmt,
            else_stmt: None,
            ..
        } => match &**then_stmt {
            Stmt::If {
                then_stmt: t2,
                else_stmt: None,
                ..
            } if !matches!(&**t2, Stmt::Block(v) if v.is_empty()) => Some(Shape::BareAnd),
            Stmt::If {
                then_stmt: t2,
                else_stmt: Some(_),
                ..
            } if matches!(&**t2, Stmt::Block(v) if v.is_empty()) => Some(Shape::BareAndNot),
            _ => None,
        },
        Stmt::If {
            then_stmt,
            else_stmt: Some(els),
            ..
        } => match (&**then_stmt, &**els) {
            (Stmt::If { else_stmt: Some(f2), .. }, _) if f2.as_ref() == els.as_ref() => {
                Some(Shape::ThenAnd)
            }
            (Stmt::If { then_stmt: t2, else_stmt: Some(_), .. }, _)
                if t2.as_ref() == els.as_ref() =>
            {
                Some(Shape::ThenAndNot)
            }
            (_, Stmt::If { then_stmt: t2, else_stmt: Some(_), .. })
                if t2.as_ref() == then_stmt.as_ref() =>
            {
                Some(Shape::ElseOr)
            }
            (_, Stmt::If { else_stmt: Some(f2), .. })
                if f2.as_ref() == then_stmt.as_ref() =>
            {
                Some(Shape::ElseOrNot)
            }
            _ => None,
        },
        _ => None,
    };
    let Some(shape) = shape else {
        return false;
    };

    let taken = std::mem::replace(st, Stmt::Block(vec![]));
    // Bare (no-else) diamonds fold without an else at all.
    if matches!(shape, Shape::BareAnd | Shape::BareAndNot) {
        let Stmt::If {
            cond: c1,
            then_stmt,
            else_stmt: None,
        } = taken
        else {
            unreachable!()
        };
        let Stmt::If {
            cond: c2,
            then_stmt: t2,
            else_stmt: f2,
        } = *then_stmt
        else {
            unreachable!()
        };
        let sc = |op: BinOp, l: Expr, r: Expr| Expr::Bin {
            op,
            l: Box::new(l),
            r: Box::new(r),
            ty: None,
        };
        match shape {
            // if (c1 && c2) {T}
            Shape::BareAnd => {
                *st = Stmt::If {
                    cond: sc(BinOp::LogAnd, c1, c2),
                    then_stmt: t2,
                    else_stmt: None,
                };
            }
            // if (c1) { if (c2) {} else {T} }  →  if (c1 && !c2) {T}
            _ => {
                let body = f2.unwrap();
                *st = Stmt::If {
                    cond: sc(
                        BinOp::LogAnd,
                        c1,
                        Expr::Un {
                            op: UnOp::Not,
                            e: Box::new(c2),
                        },
                    ),
                    then_stmt: body,
                    else_stmt: None,
                };
            }
        }
        return true;
    }
    let Stmt::If {
        cond: c1,
        then_stmt,
        else_stmt: Some(els),
    } = taken
    else {
        unreachable!("shape probe guaranteed an If with else")
    };
    let sc = |op: BinOp, l: Expr, r: Expr| Expr::Bin {
        op,
        l: Box::new(l),
        r: Box::new(r),
        ty: None,
    };
    let not = |e: Expr| Expr::Un {
        op: UnOp::Not,
        e: Box::new(e),
    };
    match shape {
        Shape::BareAnd | Shape::BareAndNot => unreachable!("handled above"),
        Shape::ThenAnd | Shape::ThenAndNot => {
            let Stmt::If {
                cond: c2,
                then_stmt: t2,
                else_stmt: Some(f2),
            } = *then_stmt
            else {
                unreachable!()
            };
            let (nc, body, alt) = match shape {
                // if (c1 && c2) {T} else {F}
                Shape::ThenAnd => (sc(BinOp::LogAnd, c1, c2), t2, f2),
                // if (c1 && !c2) {T} else {F}   (inner: {F} else {T})
                _ => (sc(BinOp::LogAnd, c1, not(c2)), f2, t2),
            };
            *st = Stmt::If {
                cond: nc,
                then_stmt: body,
                else_stmt: Some(alt),
            };
        }
        Shape::ElseOr | Shape::ElseOrNot => {
            let Stmt::If {
                cond: c2,
                then_stmt: t2,
                else_stmt: Some(f2),
            } = *els
            else {
                unreachable!()
            };
            // The surviving then-body is the OUTER then (identical to the
            // matching inner branch by the probe).
            let body = then_stmt;
            let (nc, alt) = match shape {
                // if (c1 || c2) {T} else {F}   (inner: {T} else {F})
                Shape::ElseOr => (sc(BinOp::LogOr, c1, c2), f2),
                // if (c1 || !c2) {T} else {F}  (inner: {F} else {T})
                _ => (sc(BinOp::LogOr, c1, not(c2)), t2),
            };
            *st = Stmt::If {
                cond: nc,
                then_stmt: body,
                else_stmt: Some(alt),
            };
        }
    }
    true
}

/// A static initializer cannot contain `return` in source form — the
/// clinit's closing return is a method-level artifact, but the
/// structurer can leave it nested inside loops/branches where the
/// top-level strip cannot see it (x2/a: `static { while { ...; return;
/// } }` — "return outside method", 184 sites on reqable).
pub fn strip_clinit_returns(s: &mut Stmt) {
    walk_mut_deep(s, &mut |st| {
        if let Stmt::Block(v) = st {
            v.retain(|x| !matches!(x, Stmt::Return(None)));
        }
    });
}

pub fn prepend_comment(s: &mut Stmt, text: String) {
    let stmt = Stmt::Comment(text);
    match s {
        Stmt::Block(v) => v.insert(0, stmt),
        other => {
            let inner = std::mem::replace(other, Stmt::Block(vec![]));
            *other = Stmt::Block(vec![stmt, inner]);
        }
    }
}

/// Fused expression rewrites that each used to walk the whole tree:
/// residual cmp sentinels, object-null comparisons, const-first compares.
/// One traversal, three rules — the separate walks were a top profile cost.
/// True when `e` evaluates to an object reference — a legal null-compare
/// side: locals via the reference table, everything else via its own type
/// (fields like `pi.versionName != 0` and call results are references too;
/// constants never are).
fn is_obj_expr(e: &Expr, obj_var: &dyn Fn(u32) -> bool) -> bool {
    match e {
        Expr::Local { var, .. } => obj_var(*var),
        Expr::Const(_) => false,
        _ => e.type_ref().erased().is_reference(),
    }
}

/// `obj == 0` / `0 == obj` → `obj == null`: replace the CONST-0 side.
/// The previous shape replaced the LOCAL side instead — every object
/// null-check in the corpus rendered as `null != 0` (4,598 hits in
/// reqable alone) and the real operand vanished from the condition.
fn null_side_rewrite(x: &mut Expr, obj_var: &dyn Fn(u32) -> bool) {
    if let Expr::Bin { op, l, r, .. } = x {
        if !matches!(op, BinOp::Eq | BinOp::Ne) {
            return;
        }
        let l_zero = matches!(&**l, Expr::Const(ConstVal::Int(0)));
        let r_zero = matches!(&**r, Expr::Const(ConstVal::Int(0)));
        if l_zero && !r_zero && is_obj_expr(r, obj_var) {
            **l = Expr::Const(ConstVal::Null);
        } else if r_zero && !l_zero && is_obj_expr(l, obj_var) {
            **r = Expr::Const(ConstVal::Null);
        }
    }
}

pub fn fused_expr_rewrites(s: &mut Stmt, vt: &VarTable) {
    // var ids are dense: a byte table beats a HashSet lookup.
    let mut obj_vars: Vec<bool> = Vec::with_capacity(vt.vars.len());
    for v in &vt.vars {
        obj_vars.push(v.ty.erased().is_reference());
    }
    rewrite_exprs(s, &mut |e| {
        deep_rewrite(e, &mut |x| {
            // 1. cmp sentinels.
            if let Expr::Invokedynamic { name, args, .. } = x {
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
                    *x = Expr::Method {
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
                    return;
                }
            }
            // 2. null compares (no any_obj gate: the object side may be
            // a field/call whose type is not in the local table).
            null_side_rewrite(x, &|v| obj_vars.get(v as usize).copied().unwrap_or(false));
            // 3. const-first compares.
            if let Expr::Bin { op, l, r, .. } = x {
                if matches!(
                    op,
                    BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Ge | BinOp::Gt | BinOp::Le
                ) {
                    let l_const = matches!(&**l, Expr::Const(_));
                    let r_var = matches!(&**r, Expr::Local { .. });
                    if l_const && r_var {
                        std::mem::swap(l, r);
                    }
                }
            }
        });
    });
    // 4. null into reference-typed assignment targets. The register
    //    machine stores null as const-0; return/throw/field-store/invoke
    //    argument paths were normalized in the lifter, but a null that
    //    MATERIALIZES into a local was rendered as `str = 0;` — javac
    //    "int cannot be converted to String" (the single biggest error
    //    family on real corpora: d8 emits null locals this way after
    //    every `x = null` branch).
    walk_mut_deep(s, &mut |st| {
        if let Stmt::ExprStmt(Expr::Assign { target, value, .. }) = st {
            let tgt_is_obj = match &**target {
                Expr::Local { var, .. } => {
                    obj_vars.get(*var as usize).copied().unwrap_or(false)
                }
                other => other.type_ref().erased().is_reference(),
            };
            if tgt_is_obj {
                if let Expr::Const(ConstVal::Int(0)) = &**value {
                    **value = Expr::Const(ConstVal::Null);
                }
            }
        } else if let Stmt::LocalDef { var, init, .. } = st {
            if obj_vars.get(*var as usize).copied().unwrap_or(false) {
                if let Some(iv) = init {
                    if let Expr::Const(ConstVal::Int(0)) = iv {
                        *iv = Expr::Const(ConstVal::Null);
                    }
                }
            }
        }
    });
}

/// Replace residual cmp sentinels (`\0cmp*` Invokedynamic markers that never
/// reached a condition) with library compare calls.
#[allow(dead_code)]
pub fn desugar_cmp_residuals(s: &mut Stmt) {
    rewrite_exprs(s, &mut |e| {
        deep_rewrite(e, &mut |x| {
            if let Expr::Invokedynamic { name, args, .. } = x {
                if name.starts_with('\0') && args.len() == 2 {
                    let (cls, ty) = match name.as_str() {
                        "\0cmp-long" => ("java/lang/Long", JavaType::Long),
                        "\0cmpl-float" | "\0cmpg-float" => ("java/lang/Float", JavaType::Float),
                        _ => ("java/lang/Double", JavaType::Double),
                    };
                    let desc = jdc_core::types::MethodDescriptor {
                        args: vec![ty.clone(), ty],
                        ret: JavaType::Int,
                    };
                    *x = Expr::Method {
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
        });
    });
}

/// Fold `new StringBuilder(...).append(x)...toString()` chains into
/// `StringConcat`. Single-assignment temporaries carry the chain.
///
/// d8 drops append results (statement-form calls), so a builder whose chain
/// has STATEMENT appends attached is left alone: folding its `toString`
/// with only the ctor arguments would misstate the contents.
pub fn fold_string_builders(s: &mut Stmt, vt: &VarTable) {
    // var → assigned value, for vars assigned exactly once.
    let n_vars = vt.vars.len().max(1);
    let mut counts = vec![0usize; n_vars];
    count_assignments(s, &mut counts);
    let mut values: Vec<Option<Expr>> = vec![None; n_vars];
    record_assignments(s, &counts, &mut values);

    // Builders with statement-form appends (their chains are incomplete).
    let mut appended_stmts: HashSet<u32> = HashSet::default();
    walk_all(s, &mut |st| {
        if let Stmt::ExprStmt(Expr::Method {
            cls,
            name,
            owner,
            args,
            ..
        }) = st
        {
            if name.as_ref() == "append" && is_string_builder(cls) && args.len() == 1 {
                if let Some(Expr::Local { var, .. }) = owner.as_deref() {
                    appended_stmts.insert(*var);
                }
            }
        }
    });

    rewrite_exprs(s, &mut |e| {
        fold_concat_in_expr(e, &values, &appended_stmts);
    });

    // Drop now-unused builder temporaries (their only consumers were
    // folded); the drop COUNT drives iteration — no statement counting
    // walks at all.
    for _ in 0..8 {
        let mut reads: HashSet<u32> = HashSet::default();
        stmt_collect_vars(s, &mut reads, false);
        if drop_unused_assigns(s, &reads) == 0 {
            break;
        }
    }
}

/// Deep statement count (pipeline guard input).
pub fn count_stmts_deep(s: &Stmt, out: &mut usize) {
    *out += 1;
    walk_all(s, &mut |_| {
        *out += 1;
    });
}

/// Fused per-variable analysis gathered in one statement-tree walk.
struct VarAnalysis {
    assigns: Vec<usize>,
    reads: Vec<usize>,
    /// Value of the (single) assignment, when recorded.
    values: Vec<Option<Expr>>,
}

fn analyze_vars(s: &Stmt, a: &mut VarAnalysis) {
    walk_all(s, &mut |st| {
        let exprs: Vec<&Expr> = match st {
            Stmt::ExprStmt(Expr::Assign { target, value, .. }) => {
                if let Expr::Local { var, .. } = &**target {
                    grow_to(&mut a.assigns, *var);
                    let idx = *var as usize;
                    a.assigns[idx] += 1;
                    if a.assigns[idx] == 1 {
                        if idx >= a.values.len() {
                            a.values.resize(idx + 1, None);
                        }
                        a.values[idx] = Some((**value).clone());
                    } else if idx < a.values.len() {
                        a.values[idx] = None;
                    }
                }
                let mut v: Vec<&Expr> = vec![value];
                if !matches!(&**target, Expr::Local { .. }) {
                    v.push(target);
                }
                v
            }
            Stmt::ExprStmt(e @ (Expr::PreIncDec { .. } | Expr::PostIncDec { .. })) => {
                // `v++` is a read-modify-WRITE of v: counting it as a
                // plain read makes v a single-assign/single-read forward
                // candidate, and value-forwarding then fabricates `5++`
                // (or drops v's def and leaves ++ on an undefined name).
                let inner = match e {
                    Expr::PreIncDec { e, .. } | Expr::PostIncDec { e, .. } => e,
                    _ => unreachable!(),
                };
                if let Expr::Local { var, .. } = &**inner {
                    grow_to(&mut a.assigns, *var);
                    let idx = *var as usize;
                    a.assigns[idx] += 1;
                    if idx < a.values.len() {
                        a.values[idx] = None;
                    }
                    return;
                }
                vec![e]
            }
            Stmt::ExprStmt(e) | Stmt::Throw(e) | Stmt::MonitorEnter(e) | Stmt::MonitorExit(e) => {
                vec![e]
            }
            Stmt::Return(Some(e)) => vec![e],
            Stmt::LocalDef {
                var, init: Some(e), ..
            } => {
                grow_to(&mut a.assigns, *var);
                let idx = *var as usize;
                a.assigns[idx] += 1;
                if a.assigns[idx] == 1 {
                    if idx >= a.values.len() {
                        a.values.resize(idx + 1, None);
                    }
                    a.values[idx] = Some(e.clone());
                } else if idx < a.values.len() {
                    a.values[idx] = None;
                }
                vec![e]
            }
            Stmt::If { cond, .. } => vec![cond],
            Stmt::While { cond, .. } => vec![cond],
            Stmt::DoWhile { cond, .. } => vec![cond],
            _ => Vec::new(),
        };
        for e in exprs {
            visit_exprs(e, &mut |x| {
                if let Expr::Local { var, .. } = x {
                    grow_to(&mut a.reads, *var);
                    a.reads[*var as usize] += 1;
                }
            });
        }
    });
    // Keep the three views length-aligned.
    let n = a.assigns.len().max(a.reads.len()).max(a.values.len());
    a.assigns.resize(n, 0);
    a.reads.resize(n, 0);
    a.values.resize(n, None);
}

fn grow_to(v: &mut Vec<usize>, var: u32) {
    if var as usize >= v.len() {
        v.resize(var as usize + 1, 0);
    }
}

fn count_assignments(s: &Stmt, counts: &mut Vec<usize>) {
    walk_all(s, &mut |st| match st {
        Stmt::ExprStmt(Expr::Assign { target, .. }) => {
            if let Expr::Local { var, .. } = &**target {
                grow_to(counts, *var);
                counts[*var as usize] += 1;
            }
        }
        Stmt::LocalDef {
            var, init: Some(_), ..
        } => {
            grow_to(counts, *var);
            counts[*var as usize] += 1;
        }
        _ => {}
    });
}

fn record_assignments(s: &Stmt, counts: &[usize], out: &mut Vec<Option<Expr>>) {
    walk_all(s, &mut |st| match st {
        Stmt::ExprStmt(Expr::Assign { target, value, .. }) => {
            if let Expr::Local { var, .. } = &**target {
                if counts.get(*var as usize).copied().unwrap_or(0) == 1 {
                    if *var as usize >= out.len() {
                        out.resize(*var as usize + 1, None);
                    }
                    out[*var as usize] = Some((**value).clone());
                }
            }
        }
        Stmt::LocalDef {
            var, init: Some(e), ..
        } if counts.get(*var as usize).copied().unwrap_or(0) == 1 => {
            if *var as usize >= out.len() {
                out.resize(*var as usize + 1, None);
            }
            out[*var as usize] = Some(e.clone());
        }
        _ => {}
    });
}

fn fold_concat_in_expr(e: &mut Expr, values: &[Option<Expr>], appended: &HashSet<u32>) {
    deep_rewrite(e, &mut |x| {
        if let Expr::Method {
            cls,
            name,
            args,
            owner,
            ..
        } = x
        {
            if name.as_ref() == "toString" && args.is_empty() && is_string_builder(cls) {
                if let Some(o) = owner {
                    // Statement-form appends attached to any chain var mean
                    // the parts are incomplete — keep the call.
                    let mut has_stmt_appends = false;
                    visit_exprs(o, &mut |y| {
                        if let Expr::Local { var, .. } = y {
                            if appended.contains(var) {
                                has_stmt_appends = true;
                            }
                        }
                    });
                    if has_stmt_appends {
                        return;
                    }
                    let resolved = resolve_local(o, values);
                    if let Some(parts) = collect_sb_parts(&resolved, values, 0) {
                        // NEVER fold an empty chain: d8 splits builder
                        // chains across alias registers (`v2 = v0.append
                        // (x); v2.append(y); v0.toString()`) — the traced
                        // chain sees a bare `new StringBuilder` and would
                        // fold toString() to "" (a wrong VALUE, silently:
                        // lab package Obf.label). With zero parts the
                        // call stays — always correct, usually folded
                        // elsewhere once the parts are visible.
                        if !parts.is_empty() {
                            *x = Expr::StringConcat(parts);
                        }
                    }
                }
            }
        }
    });
}

fn is_string_builder(cls: &str) -> bool {
    cls == "java/lang/StringBuilder" || cls == "java/lang/StringBuffer"
}

fn resolve_local(e: &Expr, values: &[Option<Expr>]) -> Expr {
    match e {
        Expr::Local { var, ty } => values
            .get(*var as usize)
            .and_then(|o| o.clone())
            .unwrap_or_else(|| Expr::Local {
                var: *var,
                ty: ty.clone(),
            }),
        other => other.clone(),
    }
}

/// Collect concat parts from a StringBuilder chain; depth caps cycles.
fn collect_sb_parts(e: &Expr, values: &[Option<Expr>], depth: u32) -> Option<Vec<ConcatPart>> {
    if depth > 16 {
        return None;
    }
    match e {
        Expr::Method {
            cls,
            name,
            args,
            owner,
            ..
        } if name.as_ref() == "append" && is_string_builder(cls) && args.len() == 1 => {
            let mut parts =
                collect_sb_parts(&resolve_local(owner.as_deref()?, values), values, depth + 1)?;
            parts.push(ConcatPart::Str(args[0].clone()));
            Some(parts)
        }
        Expr::New {
            cls,
            args,
            raw: false,
            ..
        } if is_string_builder(cls) => {
            let mut parts = Vec::new();
            for a in args {
                if let Expr::Const(ConstVal::Str(sv)) = a {
                    parts.push(ConcatPart::Const(sv.to_string()));
                } else {
                    parts.push(ConcatPart::Str(a.clone()));
                }
            }
            Some(parts)
        }
        _ => None,
    }
}

fn drop_unused_assigns(s: &mut Stmt, reads: &HashSet<u32>) -> usize {
    let mut dropped = 0usize;
    walk_mut_deep(s, &mut |st| {
        if let Stmt::Block(v) = st {
            let before = v.len();
            v.retain(|x| match x {
                // Drop only when unread AND a builder chain (folded
                // consumer; `new StringBuilder` chains cannot NPE).
                Stmt::ExprStmt(Expr::Assign { target, value, .. }) => match &**target {
                    Expr::Local { var, .. } => reads.contains(var) || !is_sbish(value),
                    _ => true,
                },
                Stmt::LocalDef { var, init, .. } => {
                    reads.contains(var) || !init.as_ref().map(is_sbish).unwrap_or(false)
                }
                _ => true,
            });
            dropped += before - v.len();
        }
    });
    dropped
}

fn is_sbish(e: &Expr) -> bool {
    match e {
        Expr::New {
            cls, raw: false, ..
        } => is_string_builder(cls),
        Expr::Method { name, cls, .. } => name.as_ref() == "append" && is_string_builder(cls),
        _ => false,
    }
}

fn walk_mut_deep<F: FnMut(&mut Stmt)>(s: &mut Stmt, f: &mut F) {
    f(s);
    walk_mut(s, &mut |x| walk_mut_deep(x, f));
}

/// Drop orphaned raw `new-instance` statements. R8 sometimes separates
/// `new-instance vR` from its `<init>` invoke by a register copy
/// (`move-object vS, vR; invoke vS.<init>(..)` — dex-legal: the init
/// initializes the object both registers name). The fold pairs the init
/// with the COPY, and the structurer's branch duplication re-materializes
/// the original raw marker as a bare `new C();` statement on paths whose
/// uses were rewired to the folded side (weixin sns/storage/c0: four
/// `new s1();` beside the real `s1 s1x = new s1(..)` — s1 has no no-arg
/// ctor, "无法将类 C的构造器 C应用到给定类型" ×195). A raw New in
/// statement position binds no variable and initializes nothing — pure
/// verifier-artifact dead code.
/// `try { X } finally { }` with no catches and an empty finally is a
/// no-op wrapper (the close-rethrow handler recast mints them) — unwrap
/// to X. An empty finally changes no semantics: it completes normally on
/// every path.
pub fn drop_empty_finallies(body: &mut Stmt) {
    fn is_empty_block(s: &Stmt) -> bool {
        match s {
            Stmt::Block(v) => v.iter().all(is_empty_block),
            _ => false,
        }
    }
    walk_mut_deep(body, &mut |st| {
        let Stmt::Try {
            body: b,
            catches,
            finally,
        } = st
        else {
            return;
        };
        if !catches.is_empty() {
            return;
        }
        match finally {
            // `try { X }` with neither catches nor a finally (the renderer
            // would paste a compilability `finally { }` back) and the
            // empty-finally wrapper are both no-ops — unwrap to X.
            None => {}
            Some(f) if is_empty_block(f) => {}
            Some(_) => return,
        }
        let inner = std::mem::replace(&mut **b, Stmt::Block(vec![]));
        *st = inner;
    });
}

/// Drop orphan value pushes that render as `/* X; */` comments: expression
/// statements whose expression cannot throw or bind — plain locals,
/// constants, `this`, and field reads on the implicit `this` (owner: None;
/// a deref of an arbitrary receiver could NPE, so owned field reads stay).
pub fn drop_pure_value_stmts(body: &mut Stmt) {
    // Shared honest predicate (jdc-core has_side_effects): a pure read
    // cannot throw or bind, so the statement is dead code everywhere.
    fn is_pure(s: &Stmt) -> bool {
        let Stmt::ExprStmt(e) = s else {
            return false;
        };
        !has_side_effects(e)
    }
    walk_mut_deep(body, &mut |st| {
        // Replace in place: orphans also sit as direct If branches or a
        // Synchronized body where no container retain can reach them.
        if is_pure(st) {
            *st = Stmt::Block(vec![]);
            return;
        }
        match st {
            Stmt::Switch { cases, .. } => {
                for c in cases.iter_mut() {
                    c.body.retain(|x| !is_pure(x));
                }
            }
            Stmt::For { init, .. } => init.retain(|x| !is_pure(x)),
            _ => {}
        }
    });
}

pub fn drop_dead_raw_news(body: &mut Stmt) {
    fn is_orphan(s: &Stmt) -> bool {
        matches!(s, Stmt::ExprStmt(Expr::New { raw: true, .. }))
    }
    walk_mut_deep(body, &mut |st| match st {
        Stmt::Block(v) => v.retain(|x| !is_orphan(x)),
        Stmt::Switch { cases, .. } => {
            for c in cases.iter_mut() {
                c.body.retain(|x| !is_orphan(x));
            }
        }
        Stmt::For { init, .. } => init.retain(|x| !is_orphan(x)),
        Stmt::TryWithResources { resources, .. } => resources.retain(|x| !is_orphan(x)),
        _ => {}
    });
}

/// Recover `synchronized` from the d8 monitor pattern:
/// `monitorenter(e); try { body } catch (Throwable) { monitorexit(e); throw t; }`
/// (optionally followed by `monitorexit(e)`).
pub fn fold_synchronized(s: &mut Stmt) {
    fold_sync_walk(s);
}


fn fold_sync_walk(s: &mut Stmt) {
    walk_mut_deep(s, &mut |st| {
        let Stmt::Block(v) = st else { return };
        let mut i = 0;
        while i < v.len() {
            // A fold landing on a Synchronized re-examines the same slot:
            // a split-region sibling Try (Case E) only becomes visible
            // AFTER the [enter, try] pair folded into the block form.
            #[allow(unused_assignments)]
            let mut advance = true;
            if i + 1 < v.len() {
                if let Some((repl, consumed)) = try_sync_at(v, i) {
                    advance = !matches!(repl.first(), Some(Stmt::Synchronized { .. }));
                    let _ = v.drain(i..i + consumed);
                    let n = repl.len();
                    v.splice(i..i, repl);
                    if advance {
                        i += n;
                    }
                    continue;
                }
            }
            if let Some((repl, consumed)) = try_sync_in_try(v, i) {
                let _ = v.drain(i..i + consumed);
                let n = repl.len();
                v.splice(i..i, repl);
                i += n;
                continue;
            }
            if let Some((repl, consumed)) = try_sync_deep(v, i) {
                let _ = v.drain(i..i + consumed);
                let n = repl.len();
                v.splice(i..i, repl);
                i += n;
                continue;
            }
            if let Some((repl, consumed)) = try_sync_absorb_after(v, i) {
                let _ = v.drain(i..i + consumed);
                let n = repl.len();
                v.splice(i..i, repl);
                i += n;
                continue;
            }
            i += 1;
        }
    });
}

/// Side-effect-free lock-value reads: field/local/const/this only. A call
/// or allocation between the monitor-enter and the try belongs to the
/// protected body, not to a absorbable alias head.
fn is_pure_read_expr(e: &Expr) -> bool {
    let mut pure = true;
    visit_exprs(e, &mut |x| match x {
        Expr::Const(_)
        | Expr::Local { .. }
        | Expr::This
        | Expr::Field { .. } => {}
        _ => pure = false,
    });
    pure
}

/// Resolve a local through its defining expression (bounded) so lock
/// identity survives value-view forwarding: the enter may render as the
/// raw field access while the exit renders as the promoted local (or vice
/// versa).
fn resolve_lock(e: &Expr, defs: &jdc_core::FxHashMap<u32, Expr>, depth: u32) -> Expr {
    if depth >= 4 {
        return e.clone();
    }
    if let Expr::Local { var, .. } = e {
        if let Some(d) = defs.get(var) {
            return resolve_lock(d, defs, depth + 1);
        }
    }
    e.clone()
}

fn lock_same(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (Expr::This, Expr::This) => true,
        (Expr::Local { var: x, .. }, Expr::Local { var: y, .. }) => x == y,
        (
            Expr::Field {
                owner: oa,
                cls: ca,
                name: na,
                ..
            },
            Expr::Field {
                owner: ob,
                cls: cb,
                name: nb,
                ..
            },
        ) => {
            ca == cb
                && na == nb
                && match (oa, ob) {
                    (None, None) => true,
                    (Some(ra), Some(rb)) => lock_same(ra, rb),
                    _ => false,
                }
        }
        (Expr::Const(x), Expr::Const(y)) => x == y,
        _ => false,
    }
}

fn lock_equiv(a: &Expr, b: &Expr, defs: &jdc_core::FxHashMap<u32, Expr>) -> bool {
    lock_same(&resolve_lock(a, defs, 0), &resolve_lock(b, defs, 0))
}

fn flatten_leaves<'a>(s: &'a Stmt, out: &mut Vec<&'a Stmt>) {
    match s {
        Stmt::Block(v) => {
            for x in v {
                flatten_leaves(x, out);
            }
        }
        _ => out.push(s),
    }
}

/// Every `MonitorExit(lock)` inside the folded body is a compiler artifact
/// (the implicit release on each exit path) — drop them so the body reads
/// clean and the absorbed lock alias loses its last reads.
fn strip_lock_exits(s: &mut Stmt, lock: &Expr, defs: &jdc_core::FxHashMap<u32, Expr>) {
    walk_mut_deep(s, &mut |st| {
        let Stmt::Block(v) = st else { return };
        v.retain(|x| !matches!(x, Stmt::MonitorExit(e) if lock_equiv(e, lock, defs)));
    });
}

fn try_sync_at(v: &[Stmt], i: usize) -> Option<(Vec<Stmt>, usize)> {
    // Local defs in scope before the enter, for lock resolution.
    let mut defs: jdc_core::FxHashMap<u32, Expr> = jdc_core::FxHashMap::default();
    for s in &v[..i] {
        if let Stmt::LocalDef { var, init: Some(e), .. } = s {
            defs.insert(*var, e.clone());
        }
    }
    // Case A: the enter is a direct sibling; Case B: it ends a child Block
    // (the converter wraps the pre-try statements of the entry block, so
    // the Try sits beside the wrapper, not beside the enter).
    let (pre, enter_e, mut j, mut absorbed): (Vec<Stmt>, &Expr, usize, Vec<Stmt>) = match &v[i] {
        Stmt::MonitorEnter(e) => (Vec::new(), e, i + 1, Vec::new()),
        Stmt::Block(b) => {
            let split = b
                .iter()
                .position(|x| matches!(x, Stmt::MonitorEnter(_)))?;
            let e = match &b[split] {
                Stmt::MonitorEnter(e) => e,
                _ => unreachable!(),
            };
            let mut absorbed: Vec<Stmt> = Vec::new();
            let mut k = split + 1;
            while let Some(Stmt::LocalDef { var, init: Some(ex), .. }) = b.get(k) {
                if !is_pure_read_expr(ex) {
                    break;
                }
                defs.insert(*var, ex.clone());
                absorbed.push(b[k].clone());
                k += 1;
            }
            // Only a tail split: anything after the absorbable run (other
            // than nothing) means real code follows inside the block.
            if k != b.len() {
                return None;
            }
            let pre: Vec<Stmt> = b[..split].to_vec();
            for s in &pre {
                if let Stmt::LocalDef { var, init: Some(e2), .. } = s {
                    defs.insert(*var, e2.clone());
                }
            }
            (pre, e, i + 1, absorbed)
        }
        _ => return None,
    };
    // Absorb pure alias defs parked between the enter and the try (the
    // value-view promotion hoists the lock register's declaration ahead of
    // the try span). They become the synchronized body's head.
    let mut absorbed_vars: Vec<u32> = Vec::new();
    while let Some(Stmt::LocalDef { var, init: Some(e), .. }) = v.get(j) {
        if !is_pure_read_expr(e) {
            break;
        }
        defs.insert(*var, e.clone());
        absorbed_vars.push(*var);
        absorbed.push(v[j].clone());
        j += 1;
    }
    let Stmt::Try {
        body,
        catches,
        finally,
    } = v.get(j)?
    else {
        return None;
    };
    if finally.is_some() || catches.len() != 1 {
        return None;
    }
    let c = &catches[0];
    if !c.exc.is_empty() {
        return None;
    }
    // The catch-all body: monitorexit(lock) then rethrow (the handler walk
    // may nest it as Block[Block[...]]).
    let mut flat: Vec<&Stmt> = Vec::new();
    flatten_leaves(c.body.as_ref(), &mut flat);
    if flat.len() < 2 {
        return None;
    }
    let Stmt::MonitorExit(exit_e) = flat[0] else {
        return None;
    };
    if !lock_equiv(enter_e, exit_e, &defs) {
        return None;
    }
    if !matches!(flat[flat.len() - 1], Stmt::Throw(_)) {
        return None;
    }
    for mid in &flat[1..flat.len() - 1] {
        if !matches!(mid, Stmt::MonitorEnter(_) | Stmt::MonitorExit(_)) {
            return None;
        }
    }
    // An absorbed alias read anywhere after the try would escape the
    // synchronized scope once folded — refuse the fold instead.
    if !absorbed_vars.is_empty() {
        let outside = count_locals_stmts(&v[j + 1..]);
        if absorbed_vars
            .iter()
            .any(|av| outside.get(av).copied().unwrap_or(0) > 0)
        {
            return None;
        }
    }
    let mut body_stmt = (**body).clone();
    strip_lock_exits(&mut body_stmt, enter_e, &defs);
    absorbed.push(body_stmt);
    let mut consumed = j - i + 1;
    // A matching trailing monitor-exit after the try (the normal-path
    // release parked at the join) is implicit in the block form too.
    if let Some(Stmt::MonitorExit(e)) = v.get(j + 1) {
        if lock_equiv(e, enter_e, &defs) {
            consumed += 1;
        }
    }
    let mut repl = pre;
    repl.push(Stmt::Synchronized {
        lock: enter_e.clone(),
        body: Box::new(Stmt::Block(absorbed)),
    });
    Some((repl, consumed))
}

/// Case C: the monitor-enter sits INSIDE the try body (d8 extends the
/// protected range over the lock evaluation: `try { v=lock; monitorenter v;
/// rest } catch-all { monitorexit v; throw }`). Everything up to and
/// including the enter folds into the synchronized body — the block form
/// re-protects it.
fn try_sync_in_try(v: &[Stmt], i: usize) -> Option<(Vec<Stmt>, usize)> {
    let Stmt::Try {
        body,
        catches,
        finally,
    } = &v[i]
    else {
        return None;
    };
    if finally.is_some() || catches.len() != 1 {
        return None;
    }
    let c = &catches[0];
    if !c.exc.is_empty() {
        return None;
    }
    let mut flat: Vec<&Stmt> = Vec::new();
    flatten_leaves(c.body.as_ref(), &mut flat);
    if flat.len() < 2 {
        return None;
    }
    let Stmt::MonitorExit(_) = flat[0] else {
        return None;
    };
    if !matches!(flat[flat.len() - 1], Stmt::Throw(_)) {
        return None;
    }
    for mid in &flat[1..flat.len() - 1] {
        if !matches!(mid, Stmt::MonitorEnter(_) | Stmt::MonitorExit(_)) {
            return None;
        }
    }
    // Locate the enter inside the body's top level: a direct MonitorEnter
    // or a child Block whose tail is [MonitorEnter, pure-defs*]. The
    // wrapped prelude statements stay ahead of the fold point.
    let Stmt::Block(bv) = body.as_ref() else {
        return None;
    };
    let mut defs: jdc_core::FxHashMap<u32, Expr> = jdc_core::FxHashMap::default();
    for s in &v[..i] {
        if let Stmt::LocalDef { var, init: Some(e), .. } = s {
            defs.insert(*var, e.clone());
        }
    }
    let mut pre: Vec<Stmt> = Vec::new();
    let mut enter_e: Option<Expr> = None;
    let mut absorbed: Vec<Stmt> = Vec::new();
    let mut absorbed_vars: Vec<u32> = Vec::new();
    let mut rest_start: Option<usize> = None;
    for (p, el) in bv.iter().enumerate() {
        match el {
            Stmt::MonitorEnter(e) => {
                enter_e = Some(e.clone());
                rest_start = Some(p + 1);
                break;
            }
            Stmt::Block(inner) => {
                let split = inner
                    .iter()
                    .position(|x| matches!(x, Stmt::MonitorEnter(_)));
                if let Some(sp) = split {
                    let mut k = sp + 1;
                    let mut ok = true;
                    let mut tail: Vec<Stmt> = Vec::new();
                    while let Some(Stmt::LocalDef { var: _, init: Some(ex), .. }) = inner.get(k) {
                        if !is_pure_read_expr(ex) {
                            ok = false;
                            break;
                        }
                        tail.push(inner[k].clone());
                        k += 1;
                    }
                    if ok && k == inner.len() {
                        for s in &inner[..sp] {
                            if let Stmt::LocalDef { var, init: Some(ex), .. } = s {
                                defs.insert(*var, ex.clone());
                            }
                        }
                        pre.extend(inner[..sp].iter().cloned());
                        enter_e = Some(match &inner[sp] {
                            Stmt::MonitorEnter(e) => e.clone(),
                            _ => unreachable!(),
                        });
                        absorbed = tail;
                        rest_start = Some(p + 1);
                        break;
                    }
                }
                // Not a fold point: a plain nested block ahead of the enter.
                for s in inner.iter() {
                    if let Stmt::LocalDef { var, init: Some(ex), .. } = s {
                        defs.insert(*var, ex.clone());
                    }
                }
                pre.push(el.clone());
            }
            other => {
                if let Stmt::LocalDef { var, init: Some(ex), .. } = other {
                    defs.insert(*var, ex.clone());
                }
                pre.push(other.clone());
            }
        }
    }
    let enter_e = enter_e?;
    let rs = rest_start?;
    // Pure defs directly after the enter also join the body head.
    let mut q = rs;
    while let Some(Stmt::LocalDef { var, init: Some(ex), .. }) = bv.get(q) {
        if !is_pure_read_expr(ex) {
            break;
        }
        defs.insert(*var, ex.clone());
        absorbed_vars.push(*var);
        absorbed.push(bv[q].clone());
        q += 1;
    }
    let rest: Vec<Stmt> = bv[q..].to_vec();
    if rest.is_empty() {
        return None;
    }
    // A non-def prelude would move real code past the lock acquisition —
    // the deep fold (try kept, synchronized in place) owns that shape.
    if pre
        .iter()
        .any(|s| !matches!(s, Stmt::LocalDef { init: Some(e), .. } if is_pure_read_expr(e)))
    {
        return None;
    }
    // The catch's release must name the same lock.
    let Stmt::MonitorExit(exit_e) = flat[0] else {
        return None;
    };
    if !lock_equiv(&enter_e, exit_e, &defs) {
        return None;
    }
    // Pre/absorbed defs must not be read outside the try.
    let outside_vars: Vec<u32> = pre
        .iter()
        .chain(absorbed.iter())
        .filter_map(|s| match s {
            Stmt::LocalDef { var, .. } => Some(*var),
            _ => None,
        })
        .collect();
    if !outside_vars.is_empty() {
        let outside = count_locals_stmts(&v[i + 1..]);
        if outside_vars
            .iter()
            .any(|av| outside.get(av).copied().unwrap_or(0) > 0)
        {
            return None;
        }
    }
    let mut body_stmts = pre;
    body_stmts.extend(absorbed);
    body_stmts.extend(rest);
    let mut sync_body = Stmt::Block(body_stmts);
    strip_lock_exits(&mut sync_body, &enter_e, &defs);
    let mut consumed = 1;
    if let Some(Stmt::MonitorExit(e)) = v.get(i + 1) {
        if lock_equiv(e, &enter_e, &defs) {
            consumed += 1;
        }
    }
    Some((
        vec![Stmt::Synchronized {
            lock: enter_e,
            body: Box::new(sync_body),
        }],
        consumed,
    ))
}

/// Case E: the protected region was SPLIT across groups — a named try/catch
/// nested inside the synchronized region renders as a SIBLING Try after the
/// folded Synchronized, with the region's normal-path release parked after
/// it (d8 splits javac-style around inner returns; the inner range carries
/// its own named handler plus the shared catch-all, so the same-handler
/// group merge refuses it and the structurer emits two sibling tries).
/// `[Synchronized, Try, MonitorExit(lock)]` -> the Try joins the lock body
/// and the parked release is implicit. Without this the try escapes the
/// lock (racy field writes) and unexpected exceptions leak the monitor.
fn try_sync_absorb_after(v: &[Stmt], i: usize) -> Option<(Vec<Stmt>, usize)> {
    let Stmt::Synchronized { lock, body } = &v[i] else {
        return None;
    };
    let Stmt::Try {
        body: tbody,
        catches,
        finally,
    } = v.get(i + 1)?
    else {
        return None;
    };
    if finally.is_some() {
        return None;
    }
    // The parked release sits at the protected region's end: everything on
    // the normal path before it ran INSIDE the lock (phi commits feeding
    // the in-lock return), everything after it is post-release. Scan past
    // orderable statements (register commits, pure reads, Block wrappers)
    // to the matching release; absorb the run into the lock body.
    let defs = defs_probe(v, i, body);
    let mut absorbed_mid: Vec<Stmt> = Vec::new();
    let mut rest_after: Option<Stmt> = None;
    let mut found = false;
    let mut scan = i + 2;
    'scan: while scan < v.len() && scan < i + 14 {
        match &v[scan] {
            Stmt::MonitorExit(e) => {
                if !lock_equiv(e, lock, &defs) {
                    return None;
                }
                found = true;
                scan += 1;
                break;
            }
            Stmt::Block(bv) => {
                for (k, s) in bv.iter().enumerate() {
                    match s {
                        Stmt::MonitorExit(e) => {
                            if !lock_equiv(e, lock, &defs) {
                                return None;
                            }
                            absorbed_mid.push(Stmt::Block(bv[..k].to_vec()));
                            let rest = &bv[k + 1..];
                            rest_after = (!rest.is_empty())
                                .then(|| Stmt::Block(rest.to_vec()));
                            found = true;
                            scan += 1;
                            break 'scan;
                        }
                        Stmt::ExprStmt(e) if mid_safe(e) || is_local_commit(e) => {}
                        _ => break 'scan,
                    }
                }
                absorbed_mid.push(v[scan].clone());
                scan += 1;
            }
            Stmt::ExprStmt(e) if mid_safe(e) || is_local_commit(e) => {
                absorbed_mid.push(v[scan].clone());
                scan += 1;
            }
            _ => break,
        }
    }
    if !found {
        return None;
    }
    let consumed = scan - i;
    // The adopted try's implicit releases and the handler's own release are
    // owned by the synchronized once inside — strip them.
    let mut t = (**tbody).clone();
    let defs = defs_probe(v, i, body);
    strip_lock_exits(&mut t, lock, &defs);
    let mut cs = Vec::with_capacity(catches.len());
    for c in catches.iter() {
        let mut cb = (*c.body).clone();
        strip_lock_exits(&mut cb, lock, &defs);
        // A catch-all reduced to a bare rethrow of the catch parameter is
        // the synchronized's own handler — drop it; named catches keep
        // (dropping one would widen the checked-exception surface).
        let bare_rethrow = c.exc.is_empty() && {
            let mut leaves: Vec<&Stmt> = Vec::new();
            flatten_leaves(&cb, &mut leaves);
            matches!(leaves.as_slice(), [Stmt::Throw(Expr::Local { var, .. })] if *var == c.var)
        };
        if bare_rethrow {
            continue;
        }
        let mut c2 = c.clone();
        *c2.body = cb;
        cs.push(c2);
    }
    let mut new_body = (**body).clone();
    let adopted = Stmt::Try {
        body: Box::new(t),
        catches: cs,
        finally: None,
    };
    match &mut new_body {
        Stmt::Block(bv) => bv.push(adopted),
        other => {
            let inner = std::mem::replace(other, Stmt::Block(vec![]));
            *other = Stmt::Block(vec![inner, adopted]);
        }
    }
    if let Stmt::Block(bv) = &mut new_body {
        bv.extend(absorbed_mid);
    }
    let sync = Stmt::Synchronized {
        lock: lock.clone(),
        body: Box::new(new_body),
    };
    let mut repl = vec![sync];
    if let Some(r) = rest_after {
        repl.push(r);
    }
    Some((repl, consumed))
}

/// Statements orderable into the lock body: pure reads (cannot throw or
/// bind) and — handled by the caller's pattern — plain local-to-local
/// register commits.
fn mid_safe(e: &Expr) -> bool {
    !has_side_effects(e)
}

/// Plain `v = local;` register commit (phi materialization at the join).
fn is_local_commit(e: &Expr) -> bool {
    match e {
        Expr::Assign { target, op: AssignOp::Plain, .. } => {
            matches!(&**target, Expr::Local { .. })
        }
        _ => false,
    }
}

/// Defs visible to the parked release: statements before the fold plus the
/// absorbed alias head at the synchronized body's top level.
fn defs_probe<'a>(
    v: &'a [Stmt],
    i: usize,
    body: &'a Stmt,
) -> jdc_core::FxHashMap<u32, Expr> {
    let mut defs: jdc_core::FxHashMap<u32, Expr> = jdc_core::FxHashMap::default();
    for s in &v[..i] {
        if let Stmt::LocalDef { var, init: Some(e), .. } = s {
            defs.insert(*var, e.clone());
        }
    }
    if let Stmt::Block(bv) = body {
        for s in bv {
            if let Stmt::LocalDef { var, init: Some(e), .. } = s {
                defs.insert(*var, e.clone());
            }
        }
    }
    defs
}


/// Case D: the enter sits mid-control-flow inside the try body (nested in
/// if/else branches). The spine from the body root to the enter must end
/// at the body end — nothing may follow the fold point at any ancestor
/// level. The Try STAYS; only the enter's tail becomes a synchronized
/// block in place. The catch's release is dropped: with the lock release
/// owned by the synchronized, the handler's monitorexit would double-release
/// (the sync's implicit release fires first, then the outer catch
/// re-releases an unheld monitor).
fn try_sync_deep(v: &[Stmt], i: usize) -> Option<(Vec<Stmt>, usize)> {
    let Stmt::Try {
        body,
        catches,
        finally,
    } = &v[i]
    else {
        return None;
    };
    if finally.is_some() || catches.len() != 1 {
        return None;
    }
    let c = &catches[0];
    if !c.exc.is_empty() {
        return None;
    }
    let mut flat: Vec<&Stmt> = Vec::new();
    flatten_leaves(c.body.as_ref(), &mut flat);
    if flat.len() < 2 {
        return None;
    }
    let catch_exit = match flat[0] {
        Stmt::MonitorExit(e) => e.clone(),
        _ => return None,
    };
    if !matches!(flat[flat.len() - 1], Stmt::Throw(_)) {
        return None;
    }
    for mid in &flat[1..flat.len() - 1] {
        if !matches!(mid, Stmt::MonitorEnter(_) | Stmt::MonitorExit(_)) {
            return None;
        }
    }
    let mut defs0: jdc_core::FxHashMap<u32, Expr> = jdc_core::FxHashMap::default();
    for s in &v[..i] {
        if let Stmt::LocalDef { var, init: Some(e), .. } = s {
            defs0.insert(*var, e.clone());
        }
    }
    let mut body_mut = (**body).clone();
    let folded = match &mut body_mut {
        Stmt::Block(b) => try_fold_spine(b, &catch_exit, &defs0),
        Stmt::If {
            then_stmt,
            else_stmt,
            ..
        } => {
            let mut hit = false;
            if let Stmt::Block(b) = &mut **then_stmt {
                hit |= try_fold_spine(b, &catch_exit, &defs0);
            }
            if !hit {
                if let Some(els) = else_stmt {
                    if let Stmt::Block(b) = &mut **els {
                        hit |= try_fold_spine(b, &catch_exit, &defs0);
                    }
                }
            }
            hit
        }
        _ => false,
    };
    if !folded {
        return None;
    }
    // Reduce the catch-all to a bare rethrow (see fn doc).
    let throw = flat[flat.len() - 1].clone();
    let mut catches_out = catches.clone();
    *catches_out[0].body = Stmt::Block(vec![throw]);
    let mut consumed = 1;
    if let Some(Stmt::MonitorExit(e)) = v.get(i + 1) {
        if lock_equiv(e, &catch_exit, &defs0) {
            consumed += 1;
        }
    }
    Some((
        vec![Stmt::Try {
            body: Box::new(body_mut),
            catches: catches_out,
            finally: None,
        }],
        consumed,
    ))
}

/// Fold `[ …pre…, monitorenter(e), rest… ]` whose spine reaches the
/// sequence end: at the enter's own level everything after it joins the
/// synchronized body; at every ancestor level the spine element must be
/// the LAST element (branches are alternatives, not successors). Mutates
/// in place; returns true when a fold landed.
fn try_fold_spine(
    seq: &mut Vec<Stmt>,
    catch_exit: &Expr,
    defs_in: &jdc_core::FxHashMap<u32, Expr>,
) -> bool {
    for p in (0..seq.len()).rev() {
        // Direct enter at this level: everything after it joins the body.
        if let Stmt::MonitorEnter(e0) = &seq[p] {
            let mut defs = defs_in.clone();
            for s in &seq[..p] {
                if let Stmt::LocalDef { var, init: Some(ex), .. } = s {
                    defs.insert(*var, ex.clone());
                }
            }
            // Lock-relevant defs parked just after the enter (the promoted
            // lock local's declaration) resolve the catch's release.
            for s in &seq[p + 1..] {
                match s {
                    Stmt::LocalDef { var, init: Some(ex), .. } if is_pure_read_expr(ex) => {
                        defs.insert(*var, ex.clone());
                    }
                    _ => break,
                }
            }
            if !lock_equiv(e0, catch_exit, &defs) {
                return false;
            }
            let enter_e = e0.clone();
            let rest: Vec<Stmt> = seq.split_off(p + 1);
            let mut body = Stmt::Block(rest);
            strip_lock_exits(&mut body, &enter_e, &defs);
            seq[p] = Stmt::Synchronized {
                lock: enter_e,
                body: Box::new(body),
            };
            return true;
        }
        // Nested attempt on a clone; commit only when the spine element is
        // this level's last (nothing may follow the fold point).
        let is_last = p + 1 == seq.len();
        let mut trial = seq[p].clone();
        let hit = match &mut trial {
            Stmt::Block(b) => try_fold_spine(b, catch_exit, defs_in),
            Stmt::If {
                then_stmt,
                else_stmt,
                ..
            } => {
                let mut hit = false;
                if let Stmt::Block(b) = &mut **then_stmt {
                    hit |= try_fold_spine(b, catch_exit, defs_in);
                }
                if !hit {
                    if let Some(els) = else_stmt {
                        if let Stmt::Block(b) = &mut **els {
                            hit |= try_fold_spine(b, catch_exit, defs_in);
                        }
                    }
                }
                hit
            }
            _ => false,
        };
        if hit {
            if is_last {
                seq[p] = trial;
                return true;
            }
            return false;
        }
    }
    false
}

/// Copy-forward single-use temporaries: `v = e; ... v ...` inlines `e` at
/// the (single) use and drops the assignment when safe. Pure values inline
/// anywhere; impure values (calls, array reads) only when the use is the
/// statement immediately after the definition.
pub fn forward_single_use(s: &mut Stmt, _vt: &VarTable) {
    // Fused single traversal: assignment counts, read counts and the
    // single-assignment values all come out of ONE walk (three separate
    // full-tree walks dominated the pass cost on large methods).
    let mut analysis = VarAnalysis {
        assigns: Vec::new(),
        reads: Vec::new(),
        values: Vec::new(),
    };
    analyze_vars(s, &mut analysis);
    let mut assigns = analysis.assigns;
    let mut reads = analysis.reads;
    // Var ids beyond the table (synthetic test shapes) are covered by the
    // counters' on-demand growth.
    let n_vars = assigns.len().max(_vt.vars.len()).max(1);
    assigns.resize(n_vars, 0);
    reads.resize(n_vars, 0);
    let mut values = analysis.values;
    values.resize(n_vars, None);

    // Candidates: assigned exactly once, read exactly once. Additionally
    // the inlined VALUE must not reference a multi-assigned var (phi vars):
    // moving the read across the phi's reassignment would be a stale
    // capture (register rotations snapshot through temps for this reason).
    let mut cand = vec![false; n_vars];
    let mut edges: Vec<Vec<u32>> = vec![Vec::new(); n_vars];
    for (v, cand_v) in cand.iter_mut().enumerate() {
        if assigns.get(v).copied().unwrap_or(0) != 1 || reads.get(v).copied().unwrap_or(0) != 1 {
            continue;
        }
        if let Some(val) = values.get(v).and_then(|o| o.as_ref()) {
            let mut refs = HashSet::default();
            collect_vars(val, &mut refs);
            if refs.iter().any(|r| assigns[*r as usize] > 1) {
                continue;
            }
            // Growth-graph edge set, built in the same walk: inlining v
            // inserts a clone of its value, and every Local inside the
            // clone is replaced in turn.
            edges[v] = refs.into_iter().collect();
        }
        *cand_v = true;
    }

    // Cycle rejection on the growth graph. A def-reference cycle
    // (v = f(w), w = g(v) — loop-carried register rotation reaching the
    // pass as two mutually-referencing single-assign/single-read locals)
    // re-introduces the other cycle Local at every replacement level, so
    // deep_rewrite grows the tree one layer per level and never
    // terminates: weixin's com/tencent/mm/plugin/appbrand/widget/input/b4
    // exhausted a 64MB worker stack at ~300k recursion frames. Edges into
    // non-candidates are harmless (they are never replaced, so growth
    // dies there — and they carry no outgoing edges).
    //
    // Three-color DFS, iterative: a legit 60k-long single-use chain must
    // not trade one stack overflow for another. A GRAY child closes a
    // cycle; `bad[v]` (v on a cycle, or v's inlined closure grows into
    // one) then taints the whole current path — everything on it reaches
    // the cycle. Black verdicts memoize: a node whose subtree was proven
    // clean cannot grow a cycle later.
    let mut color = vec![0u8; n_vars]; // 0 white, 1 gray, 2 black
    let mut bad = vec![false; n_vars];
    for root in 0..n_vars {
        if color[root] != 0 || edges[root].is_empty() {
            continue;
        }
        color[root] = 1;
        let mut stack: Vec<(usize, usize)> = vec![(root, 0)];
        while let Some(top) = stack.last_mut() {
            let v = top.0;
            let i = top.1;
            top.1 += 1;
            if i < edges[v].len() {
                let r = edges[v][i] as usize;
                if color[r] == 1 {
                    for &(pn, _) in &stack {
                        bad[pn] = true;
                    }
                } else if color[r] == 0 {
                    color[r] = 1;
                    stack.push((r, 0));
                } else if bad[r] {
                    for &(pn, _) in &stack {
                        bad[pn] = true;
                    }
                }
            } else {
                color[v] = 2;
                stack.pop();
            }
        }
    }
    let single: Vec<bool> = (0..n_vars).map(|v| cand[v] && !bad[v]).collect();
    if !single.iter().any(|&b| b) {
        return;
    }
    let mut pure: Vec<bool> = (0..n_vars)
        .map(|v| {
            single[v]
                && values
                    .get(v)
                    .and_then(|o| o.as_ref())
                    .map(|e| !has_side_effects(e))
                    .unwrap_or(false)
        })
        .collect();
    let mut impure: Vec<bool> = (0..n_vars).map(|v| single[v] && !pure[v]).collect();

    // A Field read is pure (no side effect) but NOT position-independent:
    // re-rendering it at the use site reads whatever the field holds
    // THERE. d8's `v = !this.f; this.f = true;` lifts as iget→xor→iput;
    // the lifter materializes the read at its defining pc (force_mat,
    // lift.rs) precisely so the xor sees the OLD value — and this pass
    // used to re-inline that temp past the write, resurrecting the
    // stale read (weibo Transmitter.exchangeMessageDone rendered
    // `this.f = true; v = !this.f;`). Block the inline-when-anywhere
    // path for a value whose field is written between the def and the
    // single use (statement pre-order positions); such candidates fall
    // through to the adjacent-only impure path, which cannot cross
    // statements.
    {
        type FieldKey = (std::sync::Arc<str>, std::sync::Arc<str>, bool);
        let mut field_writes: jdc_core::FxHashMap<FieldKey, Vec<usize>> =
            jdc_core::FxHashMap::default();
        let mut def_pos: Vec<usize> = vec![usize::MAX; n_vars];
        let mut use_pos: Vec<usize> = vec![usize::MAX; n_vars];
        let mut pos = 0usize;
        walk_all(s, &mut |st| {
            let here = pos;
            pos += 1;
            for e in stmt_read_exprs(st) {
                visit_exprs(e, &mut |x| {
                    if let Expr::Local { var, .. } = x {
                        let v = *var as usize;
                        if v < use_pos.len() && use_pos[v] == usize::MAX {
                            use_pos[v] = here;
                        }
                    }
                });
            }
            match st {
                Stmt::LocalDef { var, .. } => {
                    let v = *var as usize;
                    if v < def_pos.len() {
                        def_pos[v] = here;
                    }
                }
                Stmt::ExprStmt(Expr::Assign { target, .. }) => match &**target {
                    Expr::Local { var, .. } => {
                        let v = *var as usize;
                        if v < def_pos.len() {
                            def_pos[v] = here;
                        }
                    }
                    Expr::Field { cls, name, is_static, .. } => {
                        field_writes
                            .entry((cls.clone(), name.clone(), *is_static))
                            .or_default()
                            .push(here);
                    }
                    _ => {}
                },
                Stmt::ExprStmt(Expr::PreIncDec { e, .. } | Expr::PostIncDec { e, .. }) => {
                    if let Expr::Field { cls, name, is_static, .. } = &**e {
                        field_writes
                            .entry((cls.clone(), name.clone(), *is_static))
                            .or_default()
                            .push(here);
                    }
                }
                _ => {}
            }
        });
        let mut hazard = vec![false; n_vars];
        for v in 0..n_vars {
            if !pure[v] {
                continue;
            }
            let Some(Some(val)) = values.get(v).map(|o| o.as_ref()) else {
                continue;
            };
            let (d, u) = (def_pos[v], use_pos[v]);
            if d == usize::MAX || u == usize::MAX || u <= d {
                continue;
            }
            visit_exprs(val, &mut |x| {
                if let Expr::Field { cls, name, is_static, .. } = x {
                    if let Some(ws) = field_writes.get(&(cls.clone(), name.clone(), *is_static)) {
                        if ws.iter().any(|w| *w > d && *w < u) {
                            hazard[v] = true;
                        }
                    }
                }
            });
        }
        for v in 0..n_vars {
            if hazard[v] {
                pure[v] = false;
                impure[v] = single[v];
            }
        }
    }

    // 1. Pure values: inline anywhere.
    if pure.iter().any(|&b| b) {
        let vals: Vec<Option<Expr>> = (0..n_vars)
            .map(|v| {
                if pure[v] {
                    values.get(v).cloned().flatten()
                } else {
                    None
                }
            })
            .collect();
        rewrite_exprs(s, &mut |e| {
            deep_rewrite_reads(e, &mut |x| {
                if let Expr::Local { var, .. } = x {
                    if let Some(Some(v)) = vals.get(*var as usize) {
                        *x = v.clone();
                    }
                }
            });
        });
        drop_defs(s, &pure);
    }

    // 2. Impure values: inline only into the immediately following
    //    statement.
    if impure.iter().any(|&b| b) {
        forward_adjacent_impure(s, &values, &impure);
    }
}

/// The expression payloads a statement READS — the statement's own node
/// only (walk_all visits nested statements separately). Assignment
/// targets are writes (a non-local target still reads its owner/index
/// subtree); shared by count_reads and the position scan in
/// forward_single_use so the two can never drift apart.
fn stmt_read_exprs(st: &Stmt) -> Vec<&Expr> {
    match st {
        Stmt::ExprStmt(Expr::Assign { target, value, .. }) => {
            let mut v: Vec<&Expr> = vec![value];
            if !matches!(&**target, Expr::Local { .. }) {
                v.push(target);
            }
            v
        }
        // `v++` writes v — not a read (a compound `a[i]++` still reads
        // its owner/index subtree).
        Stmt::ExprStmt(e @ (Expr::PreIncDec { .. } | Expr::PostIncDec { .. })) => {
            let inner = match e {
                Expr::PreIncDec { e, .. } | Expr::PostIncDec { e, .. } => e,
                _ => unreachable!(),
            };
            if matches!(**inner, Expr::Local { .. }) {
                Vec::new()
            } else {
                vec![e]
            }
        }
        Stmt::ExprStmt(e) | Stmt::Throw(e) | Stmt::MonitorEnter(e) | Stmt::MonitorExit(e) => {
            vec![e]
        }
        Stmt::Return(Some(e)) => vec![e],
        Stmt::LocalDef { init: Some(e), .. } => vec![e],
        Stmt::If { cond, .. } => vec![cond],
        Stmt::While { cond, .. } => vec![cond],
        Stmt::DoWhile { cond, .. } => vec![cond],
        // Read sites that are NOT statement children (walk_all recurses
        // into bodies but these payloads are expressions on the node
        // itself). Omitting them under-counts reads, which would let
        // drop_dead_locals over-prune a var whose only use is a switch
        // selector / loop condition / lock / iterable.
        Stmt::Switch { selector, .. } => vec![selector],
        Stmt::ForEach { iterable, .. } => vec![iterable],
        Stmt::Synchronized { lock, .. } => vec![lock],
        Stmt::For { cond, update, .. } => {
            let mut v: Vec<&Expr> = Vec::with_capacity(update.len() + 1);
            if let Some(c) = cond {
                v.push(c);
            }
            v.extend(update.iter());
            v
        }
        Stmt::Assert { cond, msg } => {
            let mut v: Vec<&Expr> = vec![cond];
            if let Some(m) = msg {
                v.push(m);
            }
            v
        }
        _ => Vec::new(),
    }
}

/// Count READS of variables (assignment targets excluded).
fn count_reads(s: &Stmt, out: &mut Vec<usize>) {
    walk_all(s, &mut |st| {
        for e in stmt_read_exprs(st) {
            visit_exprs(e, &mut |x| {
                if let Expr::Local { var, .. } = x {
                    grow_to(out, *var);
                    out[*var as usize] += 1;
                }
            });
        }
    });
}


fn drop_defs(s: &mut Stmt, vars: &[bool]) {
    walk_mut_deep(s, &mut |st| {
        if let Stmt::Block(v) = st {
            v.retain(|x| match x {
                Stmt::ExprStmt(Expr::Assign { target, .. }) => match &**target {
                    Expr::Local { var, .. } => !vars.get(*var as usize).copied().unwrap_or(false),
                    _ => true,
                },
                Stmt::LocalDef { var, .. } => !vars.get(*var as usize).copied().unwrap_or(false),
                _ => true,
            });
        }
    });
}

/// For each statement list: `v = IMPURE; NEXT(v)` folds NEXT's reference.
fn forward_adjacent_impure(s: &mut Stmt, values: &[Option<Expr>], impure: &[bool]) {
    if let Stmt::Block(v) = s {
        let mut i = 0;
        while i + 1 < v.len() {
            let def_var = match &v[i] {
                Stmt::ExprStmt(Expr::Assign { target, .. }) => match &**target {
                    Expr::Local { var, .. } => Some(*var),
                    _ => None,
                },
                Stmt::LocalDef { var, .. } => Some(*var),
                _ => None,
            };
            if let Some(var) = def_var {
                if impure.get(var as usize).copied().unwrap_or(false) {
                    // Does the NEXT statement reference var?
                    let mut reads_here = vec![0usize; values.len().max(1)];
                    count_reads(&v[i + 1], &mut reads_here);
                    if reads_here.get(var as usize).copied().unwrap_or(0) == 1 {
                        // The def statement's CURRENT value (earlier inlines
                        // in this walk may have rewritten it — the pre-built
                        // values map would be stale).
                        let val = match &v[i] {
                            Stmt::ExprStmt(Expr::Assign { value, .. }) => (**value).clone(),
                            Stmt::LocalDef { init: Some(e), .. } => (*e).clone(),
                            _ => {
                                i += 1;
                                continue;
                            }
                        };
                        rewrite_exprs(&mut v[i + 1], &mut |e| {
                            deep_rewrite_reads(e, &mut |x| {
                                if let Expr::Local { var: v2, .. } = x {
                                    if *v2 == var {
                                        *x = val.clone();
                                    }
                                }
                            });
                        });
                        v.remove(i);
                        // The PREVIOUS definition may now be adjacent to its
                        // (shifted) use — re-examine it.
                        i = i.saturating_sub(1);
                        continue;
                    }
                }
            }
            i += 1;
        }
    }
    walk_mut(s, &mut |x| forward_adjacent_impure(x, values, impure));
}

// ---------------------------------------------------------------------------
// Type inference, booleanization, null comparisons, declarations
// ---------------------------------------------------------------------------

/// Evidence-based type inference for synthetic (non-parameter) vars.
pub fn infer_types(vt: &mut VarTable, body: &mut Stmt, ret: &JavaType, env: &MethodEnv) {
    let n = vt.vars.len();
    // (type, strong) — DIRECTIONAL evidence. Strong facts (an assigned
    // value's type, the declared return/throw/monitor type) DEFINE the
    // variable; weak facts (call argument expectations, receiver
    // contexts, numeric literals) only constrain a use. jadx's bound
    // system carries the same direction; a flat pool let a weak
    // expectation (`v26 = p1` with boolean p1 beside `v26 = 0` sugar)
    // outvote the definition.
    let mut evidence: Vec<Vec<(JavaType, bool)>> = vec![Vec::new(); n];
    let ev = |evidence: &mut Vec<Vec<(JavaType, bool)>>, var: u32, t: JavaType, strong: bool| {
        if (var as usize) < evidence.len() {
            evidence[var as usize].push((t, strong));
        }
    };

    walk_all(body, &mut |st| match st {
        Stmt::LocalDef { var, init, .. } => {
            if let Some(e) = init {
                expr_evidence(e, &mut |v, t| ev(&mut evidence, v, t, false));
                // The initializer DEFINES the declared type: `long v = l(...)`
                // must not stay `int v` (silent 64-bit truncation in the
                // eyes of a reader; javac rejects it as lossy). Null
                // carries no type (it would DOWNGRADE String to Object).
                // Literals are weak (0/1 is boolean sugar half the time);
                // typed producers and parameter sources are strong;
                // local-to-local copies are weak (register reuse).
                let strong = match e {
                    Expr::Const(_) => false,
                    Expr::Local { var: src, .. } => {
                        let src_ty = vt.vars.get(*src as usize).map(|v| v.ty.erased());
                        let tgt_ty = vt.vars.get(*var as usize).map(|v| v.ty.erased());
                        match (src_ty, tgt_ty) {
                            (Some(s), Some(g)) => {
                                (s.is_numeric() && g.is_numeric())
                                    || (s.is_reference() && g.is_reference())
                            }
                            _ => false,
                        }
                    }
                    _ => true,
                };
                ev(&mut evidence, *var, e.type_ref().erased(), strong);
            }
        }
        Stmt::ExprStmt(e) => {
            // Assignment targets: a typed PRODUCER (method/new/field) or a
            // PARAMETER source is a STRONG definition (`v26 = p1` with
            // boolean p1). A local-to-local copy is weak — register reuse
            // and merge materialization emit exactly that shape with
            // mismatched types (`sb7 = compareTo10` in a Kotlin when).
            // Literals stay weak.
            if let Expr::Assign { target, value, .. } = e {
                // A field WRITE through a local (`v.f = x`, dex iput)
                // proves v is a reference of the field's declaring
                // class AT THAT POINT — an iput on a boolean register
                // cannot exist in valid dex. Strong: it must outrank a
                // strong boolean copy chain into the same reused slot
                // (weixin coroutine state machines).
                if let Expr::Field { owner: Some(o), cls, is_static: false, .. } = &**target {
                    if cls.is_empty() {
                        // array-length pseudo-field: no declaring class
                    } else if let Expr::Local { var, .. } = &**o {
                        ev(
                            &mut evidence,
                            *var,
                            JavaType::Object(cls.clone()),
                            true,
                        );
                    }
                }
                if let Expr::Local { var, .. } = &**target {
                    let strong = match &**value {
                        Expr::Const(_) => false,
                        Expr::Local { var: src, .. } => {
                            // A local copy is strong when the source and
                            // the target's register type are the same
                            // FAMILY (numeric↔numeric, reference↔
                            // reference) — a cross-family copy (int into
                            // a StringBuilder local) is merge-material
                            // residue and stays weak.
                            let src_ty = vt.vars.get(*src as usize).map(|v| v.ty.erased());
                            let tgt_ty = vt.vars.get(*var as usize).map(|v| v.ty.erased());
                            match (src_ty, tgt_ty) {
                                (Some(s), Some(g)) => {
                                    (s.is_numeric() && g.is_numeric())
                                        || (s.is_reference() && g.is_reference())
                                }
                                _ => false,
                            }
                        }
                        _ => true,
                    };
                    ev(&mut evidence, *var, value.type_ref().erased(), strong);
                }
            }
            expr_evidence(e, &mut |v, t| ev(&mut evidence, v, t, false));
        }
        Stmt::Throw(e) => {
            expr_evidence(e, &mut |v, t| ev(&mut evidence, v, t, false));
            if let Expr::Local { var, .. } = e {
                ev(
                    &mut evidence,
                    *var,
                    JavaType::Object("java/lang/Throwable".into()),
                    true,
                );
            }
        }
        Stmt::Return(Some(e)) => {
            expr_evidence(e, &mut |v, t| ev(&mut evidence, v, t, false));
            if let Expr::Local { var, .. } = e {
                ev(&mut evidence, *var, ret.clone(), true);
            }
        }
        Stmt::MonitorEnter(e) | Stmt::MonitorExit(e) => {
            if let Expr::Local { var, .. } = e {
                ev(
                    &mut evidence,
                    *var,
                    JavaType::Object("java/lang/Object".into()),
                    true,
                );
            }
        }
        Stmt::ForEach {
            var,
            iterable,
            is_array,
            ..
        } => {
            if let Expr::Local { var: v0, .. } = iterable {
                if *is_array {
                    ev(&mut evidence, *v0, JavaType::Array(Box::new(JavaType::Int)), true);
                } else {
                    ev(
                        &mut evidence,
                        *v0,
                        JavaType::Object("java/lang/Object".into()),
                        true,
                    );
                }
            }
            let _ = var;
        }
        _ => {}
    });
    let _ = env;

    // Return-position locals take the declared return type (also inside
    // nested returns — handled by the walk above).

        // Vars dereferenced as field/method receivers or arrays: their
    // resolved type must be a dereferenceable class — a boxed Boolean
    // in the strong set (Kotlin Result registers hold `Boolean | impl`
    // across coroutine case arms) must lose to the real class even
    // when the Boolean evidence comes first (weixin ry0/h2's
    // `Boolean bool19` receiving `bool19.d = x`).
    let mut derefed: Vec<bool> = vec![false; vt.vars.len()];
    mark_derefs(body, &mut derefed);
    // Array-indexed vars: their resolved type must be an ARRAY when
    // the evidence has one (`num4[v68]` with Integer-typed num4 —
    // 需要数组但找到Integer family); member derefs keep the class
    // preference below.
    let mut arr_derefed: Vec<bool> = vec![false; vt.vars.len()];
    visit_all_exprs(body, &mut |x| {
        if let Expr::ArrayIndex { array, .. } = x {
            if let Expr::Local { var, .. } = &**array {
                if (*var as usize) < arr_derefed.len() {
                    arr_derefed[*var as usize] = true;
                }
            }
        }
        // `v != null` is STRONG reference evidence: the lifter only
        // renders the null form when the dex value view held a
        // reference register at that pc — an int-typed generation
        // compared against null is phi-confluence residue (yq0/g1's
        // `v95_g12 != null`, 546-line 二元运算符 family). Strong Object
        // makes the resolver retype and split_generations separate the
        // int writes into their own generation.
        if let Expr::Bin { op, l, r, .. } = x {
            if matches!(op, BinOp::Eq | BinOp::Ne)
                && (matches!(&**r, Expr::Const(ConstVal::Null))
                    || matches!(&**l, Expr::Const(ConstVal::Null)))
            {
                for side in [l, r] {
                    if let Expr::Local { var, .. } = &**side {
                        ev(
                            &mut evidence,
                            *var,
                            JavaType::Object("java/lang/Object".into()),
                            true,
                        );
                    }
                }
            }
        }
    });
    const FINAL_JDK: &[&str] = &[
        "java/lang/Boolean",
        "java/lang/String",
        "java/lang/Integer",
        "java/lang/Long",
        "java/lang/Short",
        "java/lang/Byte",
        "java/lang/Character",
        "java/lang/Float",
        "java/lang/Double",
    ];
    for (i, evs) in evidence.iter().enumerate() {
        let info = &vt.vars[i];
        if info.is_param {
            continue;
        }
        let strong: Vec<JavaType> = evs
            .iter()
            .filter(|(_, s)| *s)
            .map(|(t, _)| t.clone())
            .collect();
        let all: Vec<JavaType> = evs.iter().map(|(t, _)| t.clone()).collect();
        if info.ty.erased().is_reference() {
            // Materialization residue: no strong definition, and the weak
            // pool names ≥2 DIFFERENT classes (a Kotlin `when` lowered
            // every branch's value into one register slot) — neither
            // class can win, every assignment needs boxing headroom:
            // widen to Object (uses get receiver casts).
            if strong.is_empty() {
                let refs: std::collections::HashSet<&str> = all
                    .iter()
                    .filter_map(|t| match t {
                        JavaType::Object(n) if n.as_ref() != "java/lang/Object" => {
                            Some(n.as_ref())
                        }
                        _ => None,
                    })
                    .collect();
                let mixed = all.iter().any(|t| t.is_numeric());
                if refs.len() >= 2 || (mixed && !refs.is_empty()) || (mixed && all.iter().any(|t| matches!(t, JavaType::Array(_)))) {
                    vt.vars[i].ty =
                        TypeRef::J(JavaType::Object("java/lang/Object".into()));
                    continue;
                }
            }
                        // Array-dereferenced vars prefer array evidence (an
            // Integer producer cannot be indexed).
            if arr_derefed[i] {
                let arr = strong
                    .iter()
                    .chain(all.iter())
                    .find(|t| matches!(t, JavaType::Array(_)))
                    .cloned();
                if let Some(t) = arr {
                    vt.vars[i].ty = TypeRef::J(t);
                    continue;
                }
            }
            // Strong definitions outrank weak expectations. A
            // dereferenced var prefers a real class over final JDK
            // boxes (Boolean can never carry `.d = x`).
            let deref_pick = || {
                if !derefed[i] {
                    return None;
                }
                strong
                    .iter()
                    .find(|t| {
                        matches!(t, JavaType::Object(n)
                            if !n.is_empty()
                                && n.as_ref() != "java/lang/Object"
                                && !FINAL_JDK.contains(&n.as_ref()))
                    })
                    .cloned()
            };
            if let Some(t) = deref_pick()
                .or_else(|| pick_object(&strong))
                .or_else(|| pick_object(&all))
            {
                vt.vars[i].ty = TypeRef::J(t);
            }
        } else if let Some(t) = pick_object(&strong) {
            // A strong reference definition (typed producer or field
            // write) outranks a strong boolean copy: Boolean assigns
            // INTO an Object slot legally, never the reverse.
            vt.vars[i].ty = TypeRef::J(t);
        } else if strong.contains(&JavaType::Boolean) {
            // A boolean-typed SOURCE assigned into the variable is a
            // definition (`v26 = p1` with boolean p1); the 0/1 literals
            // that share the pool are boolean sugar as often as not.
            vt.vars[i].ty = TypeRef::J(JavaType::Boolean);
        } else if !all.is_empty() && all.iter().all(|t| t.is_numeric()) {
            if let Some(t) = pick_numeric(&all) {
                vt.vars[i].ty = TypeRef::J(t);
            }
        } else {
            // Mixed-family materialization on a numeric-declared slot
            // (int slot receiving StringBuilders — the register was the
            // `when` accumulator): Object, same as the reference side.
            let refs = all.iter().filter(|t| t.is_reference()).count();
            let nums = all.iter().filter(|t| t.is_numeric()).count();
            if refs > 0 && nums > 0 {
                vt.vars[i].ty =
                    TypeRef::J(JavaType::Object("java/lang/Object".into()));
            } else if let Some(t) = pick_object(&strong).or_else(|| pick_object(&all)) {
                vt.vars[i].ty = TypeRef::J(t);
            }
        }
    }

    // Rewrite embedded Local types. Borrowed view + write-only-on-change:
    // the table clone was n_vars JavaType clones per method, and the
    // rewrite cloned a type into EVERY Local node — most already carry
    // the right one (JavaType::Object clones allocate).
    let types: Vec<&TypeRef> = vt.vars.iter().map(|v| &v.ty).collect();
    rewrite_exprs(body, &mut |e| {
        deep_rewrite(e, &mut |x| {
            if let Expr::Local { var, ty } = x {
                if let Some(want) = types.get(*var as usize) {
                    if ty != *want {
                        *ty = (*want).clone();
                    }
                }
            }
        });
    });

    // Numeric literals take their variable's declared width (`double v = 0L`
    // prints as `0.0`; `int x = 5L` as `5`).
    coerce_num_consts(body, &types);
}

fn coerce_num_consts(body: &mut Stmt, types: &[&TypeRef]) {
    walk_mut_deep(body, &mut |st| {
        let (var, val): (u32, &mut Expr) = match st {
            Stmt::ExprStmt(Expr::Assign { target, value, .. }) => match &mut **target {
                Expr::Local { var, .. } => (*var, value),
                _ => return,
            },
            Stmt::LocalDef {
                var, init: Some(e), ..
            } => (*var, e),
            _ => return,
        };
        let want = match types.get(var as usize) {
            Some(TypeRef::J(t)) => t.clone(),
            _ => return,
        };
        let rewritten = match &*val {
            Expr::Const(ConstVal::Long(l)) => match &want {
                JavaType::Double => Some(Expr::Const(ConstVal::Double(*l as f64))),
                JavaType::Float => Some(Expr::Const(ConstVal::Float(*l as f32))),
                _ => None,
            },
            Expr::Const(ConstVal::Int(i)) => match &want {
                JavaType::Double => Some(Expr::Const(ConstVal::Double(*i as f64))),
                JavaType::Float => Some(Expr::Const(ConstVal::Float(*i as f32))),
                _ => None,
            },
            _ => None,
        };
        if let Some(e) = rewritten {
            *val = e;
        }
    });
}

fn pick_object(evs: &[JavaType]) -> Option<JavaType> {
    evs.iter()
        .find(|t| {
            t.is_reference()
                && !matches!(t, JavaType::Object(n) if n.is_empty() || n.as_ref() == "java/lang/Object")
        })
        .cloned()
}

fn pick_numeric(evs: &[JavaType]) -> Option<JavaType> {
    if evs.is_empty() {
        return None;
    }
    let mut t = evs[0].clone();
    for x in &evs[1..] {
        t = join_numeric(&t, x);
    }
    Some(t)
}

fn join_numeric(a: &JavaType, b: &JavaType) -> JavaType {
    if a == b {
        return a.clone();
    }
    match (a, b) {
        (JavaType::Double, _) | (_, JavaType::Double) => JavaType::Double,
        (JavaType::Float, _) | (_, JavaType::Float) => JavaType::Float,
        (JavaType::Long, _) | (_, JavaType::Long) => JavaType::Long,
        (JavaType::Int, x) if x.is_integral() => JavaType::Int,
        (x, JavaType::Int) if x.is_integral() => JavaType::Int,
        (x, y) if x.is_integral() && y.is_integral() => JavaType::Int,
        _ => JavaType::Int,
    }
}

/// Gather per-var type evidence from one expression.
fn expr_evidence<F: FnMut(u32, JavaType)>(e: &Expr, f: &mut F) {
    visit_exprs(e, &mut |x| match x {
        Expr::Method {
            owner,
            args,
            desc,
            is_static,
            cls,
            ..
        } => {
            if !*is_static {
                if let Some(o) = owner {
                    if let Expr::Local { var, .. } = &**o {
                        f(*var, JavaType::Object(cls.clone()));
                    }
                }
            }
            for (i, a) in args.iter().enumerate() {
                if let Expr::Local { var, .. } = a {
                    if let Some(t) = desc.args.get(i) {
                        f(*var, t.clone());
                    }
                }
            }
        }
        Expr::Field {
            owner,
            cls,
            ty,
            is_static,
            ..
        } => {
            if !*is_static && !cls.is_empty() {
                if let Some(o) = owner {
                    if let Expr::Local { var, .. } = &**o {
                        // The field ref's declaring class is dex-exact
                        // evidence — the old hardcoded Object let a
                        // boolean copy chain outvote it (weixin ry0/h2's
                        // `Boolean bool19` receiving `bool19.d = x` —
                        // the 339-line 找不到符号 变量 d family).
                        // EMPTY cls = the array-length pseudo-field
                        // (lift models `a.length` as Field{cls:""}) —
                        // Object("") evidence poisoned pick_object into
                        // empty-typed declarations and `() x` casts.
                        f(*var, JavaType::Object(cls.clone()));
                    }
                }
            }
            let _ = ty;
        }
        // `v != null` / `v == null`: the register holds a reference at
        // that point — weak Object evidence keeps int-typed generations
        // from winning the pool (yq0/g1's `v95_g12 != null` with int
        // v95_g12 — 二元运算符 '!=' 546-line family).
        Expr::Bin { op, l, r, .. }
            if matches!(op, BinOp::Eq | BinOp::Ne)
                && (matches!(&**r, Expr::Const(ConstVal::Null))
                    || matches!(&**l, Expr::Const(ConstVal::Null))) =>
        {
            for side in [l, r] {
                if let Expr::Local { var, .. } = &**side {
                    f(*var, JavaType::Object("java/lang/Object".into()));
                }
            }
        }
        Expr::ArrayIndex { array, .. } => {
            if let Expr::Local { var, ty, .. } = &**array {
                // Reinforce the local's KNOWN array type: the hardcoded
                // int[] fallback contradicted a typed array local (a
                // register reused for byte[], int[] then byte[] across
                // one clinit labeled the byte[] defs `int[]`).
                let t = ty.erased();
                if matches!(t, JavaType::Array(_)) {
                    f(*var, t);
                } else {
                    f(*var, JavaType::Array(Box::new(JavaType::Int)));
                }
            }
        }
        Expr::Assign { .. } => {
            // Covered by the typed strong-evidence walk in infer_types;
            // keeping a weak duplicate here let expectations outvote it.
        }
        Expr::Cast { e: inner, .. } => {
            // A cast is an EXPLICIT narrowing at the use site — it must
            // NOT feed the cast target as evidence for the variable
            // (`Object get2 = list.get(2); j((String) get2);` had get2
            // retyped to String, making the assignment incompatible).
            // jadx's bounds carry direction: a USE bound narrows nothing.
            let _ = inner;
        }
        Expr::InstanceOf { e: inner, .. } => {
            let _ = inner;
        }
        _ => {}
    });
}

/// Mark locals that OWN a non-static field access and whose inferred type
/// is a concrete `$<digits>` class (a d8/R8-desugared lambda or a Kotlin
/// suspend-lambda). Emit demotes such a type to its SAM interface in
/// DECLARED positions (`Function2 v = new Outer$..$1(..)`) for phi-friendly
/// readability — but interfaces carry no instance fields, so the Kotlin
/// coroutine `create()` capture writes (`v.L$0 = obj`, also I$/J$) render
/// "找不到符号 变量 L$0" (weibo MutableScatterMap$..$iterator$1, ~1k).
/// Forcing the concrete declared type keeps the field access legal; the phi
/// case is excluded because a disagreed merge stays Object (wide_stack_vars),
/// never a specific concrete class. Gate mirrors emit's demotion exactly
/// (last `$` segment all digits) so nothing else changes.
pub fn mark_field_owner_concrete(vt: &mut VarTable, body: &mut Stmt) {
    let mut owners: HashSet<u32> = HashSet::default();
    rewrite_exprs(body, &mut |e| {
        visit_exprs(e, &mut |x| {
            if let Expr::Field {
                owner: Some(o),
                is_static: false,
                ..
            } = x
            {
                if let Expr::Local { var, .. } = &**o {
                    owners.insert(*var);
                }
            }
        });
    });
    for var in owners {
        let i = var as usize;
        if i >= vt.vars.len() {
            continue;
        }
        if let JavaType::Object(cls) = vt.vars[i].ty.erased() {
            if let Some(last) = cls.rsplit('$').next() {
                if !last.is_empty() && last.chars().all(|c| c.is_ascii_digit()) {
                    vt.force_concrete_vars.insert(var);
                }
            }
        }
    }
    // EVERY local whose resolved type is a concrete `$<digits>` class
    // keeps its binary name, not just field owners: when one side of an
    // assignment falls back to the SAM/super (`ContinuationImpl v7`) and
    // its sibling stays concrete (`$reportWhenComplete$1 v9`), the pair
    // is an uncastable "ContinuationImpl无法转换为…$1" (weibo coroutine
    // prologues ×282). The anon fallback still serves INTERFACE-typed
    // phi slots (branches minting different lambdas) — their vt type is
    // the join, never the $N class.
    let concrete: Vec<u32> = vt
        .vars
        .iter()
        .filter(|v| {
            if let JavaType::Object(cls) = v.ty.erased() {
                cls.rsplit('$')
                    .next()
                    .is_some_and(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_digit()))
            } else {
                false
            }
        })
        .map(|v| v.id)
        .collect();
    for var in concrete {
        vt.force_concrete_vars.insert(var);
    }
}

/// Type inference can leave a specific-reference-typed target assigned
/// from a `java/lang/Object`-typed value: a phi that merged String and
/// Object then settled on String (`String str4; ... str4 = obj;` where
/// `obj` is an Object field — y5/n.java), or an Object local flowing to a
/// typed field. Java requires a narrowing cast there; insert `(T) value`
/// at the USE site (round-40 philosophy: narrow at use, never retype the
/// declaration). Guarded tightly: only when the value's resolved static
/// type is EXACTLY `java/lang/Object` (the untyped top — a known subtype
/// is never re-cast), the value is not a null const (assignable to any
/// ref) nor already a cast, and the target is a specific reference type
/// (Object→int/boolean is unboxing, a different problem left alone).
pub fn insert_object_narrowing_casts(
    vt: &VarTable,
    body: &mut Stmt,
    pool: &DexPool,
    ret: &JavaType,
) {
    // Resolve a value's static type through the VarTable for locals (the
    // embedded Local ty can lag infer_types), else the expr's own type.
    // The erased materialization happens ONCE per statement and threads
    // through the checks — the old helpers re-erased 3-4x per statement
    // (each a JavaType clone with Array chains) and this pass runs over
    // every Assign/LocalDef/Return/call-arg corpus-wide.
    let value_ty = |e: &Expr| -> JavaType {
        match e {
            Expr::Local { var, .. } => vt.var(*var).ty.erased(),
            other => other.type_ref().erased(),
        }
    };
    let is_top_object_ty = |v: &JavaType| -> bool {
        matches!(v, JavaType::Object(c) if c.as_ref() == "java/lang/Object")
    };
    // A specific reference target type (a class other than
    // java/lang/Object, or an array — `String[] v = obj` needs the
    // cast exactly like `String v = obj`). The J arm pattern-matches
    // directly (no erased clone); only the rare G form erases.
    let specific_ref = |ty: &TypeRef| -> Option<TypeRef> {
        match ty {
            TypeRef::J(JavaType::Object(c)) if c.as_ref() != "java/lang/Object" => {
                Some(ty.clone())
            }
            TypeRef::J(JavaType::Array(_)) => Some(ty.clone()),
            _ => match ty.erased() {
                JavaType::Object(c) if c.as_ref() != "java/lang/Object" => Some(ty.clone()),
                JavaType::Array(_) => Some(ty.clone()),
                _ => None,
            },
        }
    };
    let non_cast_shape = |e: &Expr| -> bool {
        !matches!(e, Expr::Const(_) | Expr::Cast { .. } | Expr::InstanceOf { .. })
    };
    // Downcast witness: a SPECIFIC-ref value whose static type does not
    // fit the specific-ref target (`v9($1) = v7` with v7:
    // ContinuationImpl — weibo coroutine prologue ×282). The dex move
    // was verifier-proven against the target type, so the cast states
    // the bytecode truth; an actually-unconvertible static pair (both
    // classes, unrelated — the value view conflated) rides the
    // emitter's (Object) relay and still compiles. Framework types the
    // pool cannot order take the cast too: a downcast renders directly,
    // an unrelated pair relays.
    let needs_downcast = |vty: &JavaType, tt: &JavaType| -> bool {
        let (JavaType::Object(vn), JavaType::Object(tn)) = (vty, tt) else {
            return false;
        };
        if vn.as_ref() == "java/lang/Object" || tn.as_ref() == "java/lang/Object" {
            return false;
        }
        vn != tn && !pool.is_subtype(vn, tn)
    };
    // Combined decision on a materialized value type.
    let wants_cast = |e: &Expr, vty: &JavaType, tt: &JavaType| -> bool {
        non_cast_shape(e) && (is_top_object_ty(vty) || needs_downcast(vty, tt))
    };
    walk_mut_deep(body, &mut |st| match st {
        Stmt::ExprStmt(Expr::Assign {
            target,
            value,
            op: AssignOp::Plain,
            ..
        }) => {
            // Decide on materialized erased types (one per side); the
            // old code cloned the whole target TypeRef per statement
            // before knowing whether a cast fires at all.
            let tgt_erased: JavaType;
            let tgt_ref: Option<&TypeRef>;
            match &**target {
                Expr::Local { var, .. } => {
                    let ty = &vt.var(*var).ty;
                    tgt_erased = ty.erased();
                    tgt_ref = Some(ty);
                }
                Expr::Field { ty, .. } => {
                    tgt_erased = ty.erased();
                    tgt_ref = Some(ty);
                }
                // Array-element store `arr[i] = value`: the target type is
                // the ELEMENT type (Object→element assignments failed
                // "Object无法转换为String", weixin ×~150 — see history).
                Expr::ArrayIndex { array, .. } => {
                    let aty = match &**array {
                        Expr::Local { var, .. } => vt.var(*var).ty.erased(),
                        other => other.type_ref().erased(),
                    };
                    match aty {
                        JavaType::Array(inner) => {
                            tgt_erased = *inner;
                            tgt_ref = None;
                        }
                        _ => return,
                    }
                }
                _ => return,
            }
            // Specific check: the J-arm patterns fall through without
            // erasing; the array-element arm (no TypeRef to borrow)
            // re-materializes the erased form.
            let specific = match tgt_ref {
                Some(tr) => specific_ref(tr),
                None => specific_ref(&TypeRef::J(tgt_erased.clone())),
            };
            let Some(t) = specific else {
                return;
            };
            let vty = value_ty(value);
            if wants_cast(value, &vty, &tgt_erased) {
                let v = std::mem::replace(value, Box::new(Expr::This));
                **value = Expr::Cast { ty: t, e: v };
            }
        }
        Stmt::LocalDef { var, init: Some(value), .. } => {
            if let Some(t) = specific_ref(&vt.var(*var).ty) {
                let tt = vt.var(*var).ty.erased();
                let vty = value_ty(value);
                if wants_cast(value, &vty, &tt) {
                    let v = std::mem::replace(value, Expr::This);
                    *value = Expr::Cast {
                        ty: t,
                        e: Box::new(v),
                    };
                }
            }
        }
        Stmt::Return(Some(value)) => {
            if let Some(t) = specific_ref(&TypeRef::J(ret.clone())) {
                let vty = value_ty(value);
                if wants_cast(value, &vty, ret) {
                    let v = std::mem::replace(value, Expr::This);
                    *value = Expr::Cast { ty: t, e: Box::new(v) };
                }
            }
        }
        _ => {}
    });
    // Descriptor-exact formal casts at CALL args: the dex already
    // resolved the target method, so casting a top-Object actual to
    // its declared formal cannot shift overload resolution (the r57
    // args lesson applies to GUESSED casts, not descriptor-exact
    // ones). `"source_type:".concat(obj2)`, `new my0.j(obj2, ..)` —
    // Object无法转换为String family.
    walk_stmt_exprs(body, &mut |e| {
        deep_rewrite(e, &mut |x| {
            let (args, arg_tys): (&mut Vec<Expr>, &[JavaType]) = match x {
                Expr::Method { desc, args, is_dynamic: false, .. } => {
                    (args, desc.args.as_slice())
                }
                // Ctor calls are their own variant — the dex resolved the
                // exact <init>, so formal casts are just as safe here
                // (`new r(obj)` — Object无法转换为r family).
                Expr::New { arg_tys, args, .. } => (args, arg_tys.as_slice()),
                _ => return,
            };
            for (i, a) in args.iter_mut().enumerate() {
                let Some(formal) = arg_tys.get(i) else {
                    continue;
                };
                let Some(t) = specific_ref(&TypeRef::J(formal.clone())) else {
                    continue;
                };
                let aty = value_ty(a);
                if !wants_cast(a, &aty, formal) {
                    continue;
                }
                let v = std::mem::replace(a, Expr::Const(ConstVal::Null));
                *a = Expr::Cast { ty: t, e: Box::new(v) };
            }
        });
    });
}

/// Numeric narrowing/widening mismatches at Assign / LocalDef / Return
/// positions get an explicit cast: dex int-to-byte / long-to-int flows
/// are implicit in the register machine, Java rejects them ("从long
/// 转换到int可能会有损失" — okio Buffer ×321 weibo lines). Casts at
/// these three positions are descriptor-faithful and cannot shift
/// overload resolution (the r57 args-position lesson). Boolean/numeric
/// mixes are NOT castable in Java and stay with the generation-split
/// machinery.
pub fn fix_primitive_assign_casts(vt: &VarTable, body: &mut Stmt, ret_ty: &JavaType) {
    fn numlike(t: &JavaType) -> bool {
        matches!(
            t,
            JavaType::Byte
                | JavaType::Short
                | JavaType::Int
                | JavaType::Char
                | JavaType::Long
                | JavaType::Float
                | JavaType::Double
        )
    }
    fn val_ty(e: &Expr, vt: &VarTable) -> JavaType {
        match e {
            Expr::Local { var, .. } if (*var as usize) < vt.vars.len() => {
                vt.var(*var).ty.erased()
            }
            other => other.type_ref().erased(),
        }
    }
    fn wrap(ty: &JavaType, value: &mut Expr) {
        if matches!(value, Expr::Cast { .. }) {
            return;
        }
        if val_ty_static(value) == *ty {
            return;
        }
        let taken = std::mem::replace(value, Expr::Const(ConstVal::Null));
        *value = Expr::Cast {
            ty: TypeRef::J(ty.clone()),
            e: Box::new(taken),
        };
    }
    fn val_ty_static(e: &Expr) -> JavaType {
        e.type_ref().erased()
    }
    /// Target-typed coercion for one assignment value. Numeric mixes
    /// cast; boolean mixes CONVERT (Java has no cast between boolean
    /// and any numeric): `d = b` → `(double) (b ? 1 : 0)`, `b = i` →
    /// `i != 0` — mirrors fix_primitive_arg_bridges' formal-side arms.
    /// The generation split cannot own these: at an if-join the
    /// one-sided split is undone (rename_gen_back) and the incompatible
    /// assign re-exposes ("boolean无法转换为double", weixin s9/t01-f).
    fn coerce(tt: &JavaType, value: &mut Expr, vt: &VarTable) {
        // Raw const bits against a floating target are the FLOAT value,
        // not an int to widen (`float f = <0x41000000 bits>` is 8.0f;
        // the (float) cast path would silently store 1.09e9f). Mirror of
        // fix_primitive_arg_bridges' descriptor-formal reinterpretation.
        match (&*value, tt) {
            (Expr::Const(ConstVal::Int(b)), JavaType::Float) => {
                *value = Expr::Const(ConstVal::Float(f32::from_bits(*b as u32)));
                return;
            }
            (Expr::Const(ConstVal::Long(b)), JavaType::Double) => {
                *value = Expr::Const(ConstVal::Double(f64::from_bits(*b as u64)));
                return;
            }
            (Expr::Const(ConstVal::Float(f)), JavaType::Int) => {
                *value = Expr::Const(ConstVal::Int(f.to_bits() as i32));
                return;
            }
            (Expr::Const(ConstVal::Double(d)), JavaType::Long) => {
                *value = Expr::Const(ConstVal::Long(d.to_bits() as i64));
                return;
            }
            _ => {}
        }
        let vt_val = val_ty(value, vt);
        let tt_bool = matches!(tt, JavaType::Boolean);
        let val_bool = matches!(vt_val, JavaType::Boolean);
        if tt_bool && !val_bool && numlike(&vt_val) {
            if matches!(
                value,
                Expr::Const(_)
                    | Expr::Bin { op: BinOp::Eq | BinOp::Ne, .. }
                    | Expr::Un { op: UnOp::Not, .. }
            ) {
                // Bare 0/1 constants render as false/true through the
                // emitter's bool coercion — wrapping them in `!= 0`
                // produced `0 != 0` noise the value-diamond fold then
                // had to re-interpret.
                return;
            }
            let taken = std::mem::replace(value, Expr::Const(ConstVal::Null));
            *value = Expr::Bin {
                op: BinOp::Ne,
                l: Box::new(taken),
                r: Box::new(Expr::Const(ConstVal::Int(0))),
                ty: Some(TypeRef::J(JavaType::Boolean)),
            };
        } else if !tt_bool && val_bool && numlike(tt) {
            if matches!(value, Expr::Cast { .. }) {
                return;
            }
            let taken = std::mem::replace(value, Expr::Const(ConstVal::Null));
            *value = Expr::Cast {
                ty: TypeRef::J(tt.clone()),
                e: Box::new(Expr::Cond {
                    c: Box::new(taken),
                    t: Box::new(Expr::Const(ConstVal::Int(1))),
                    f: Box::new(Expr::Const(ConstVal::Int(0))),
                }),
            };
        } else if numlike(tt) && numlike(&vt_val) {
            wrap(tt, value);
        }
    }
    walk_mut_deep(body, &mut |st| match st {
        Stmt::ExprStmt(Expr::Assign {
            target,
            op: AssignOp::Plain,
            value,
        }) => {
            let tt = match &**target {
                Expr::Local { var, .. } if (*var as usize) < vt.vars.len() => {
                    vt.var(*var).ty.erased()
                }
                Expr::Field { ty, .. } => ty.erased(),
                // Array store: the element type is the target type
                // (`sFormatStr[i] = v7` into char[] — "从int转换到char
                // 可能会有损失", weibo TimeUtils).
                Expr::ArrayIndex { array, .. } => match array.type_ref().erased() {
                    JavaType::Array(inner) => *inner,
                    _ => return,
                },
                _ => return,
            };
            coerce(&tt, value, vt);
        }
        Stmt::LocalDef {
            var,
            init: Some(value),
            ..
        } => {
            if (*var as usize) >= vt.vars.len() {
                return;
            }
            let tt = vt.var(*var).ty.erased();
            coerce(&tt, value, vt);
        }
        Stmt::Return(Some(e)) => {
            if !numlike(ret_ty) || !numlike(&val_ty(e, vt)) {
                return;
            }
            wrap(ret_ty, e);
        }
        _ => {}
    });
}

/// Shadowed-ancestor field witness (the upcast sibling of
/// fix_field_owner_downcasts).
///
/// Java resolves a field access on the RECEIVER'S STATIC TYPE downward:
/// when the dex writes an ancestor's field (`iput … Fragment.A02:I`)
/// but a class between the receiver's static type and the declaring
/// ancestor declares a field with the SAME rendered name (`ImageView
/// A02`), the plain `this.A02` silently binds to the subclass field —
/// `int无法转换为ImageView` when the types differ, and a wrong-field
/// read/write when they don't (WhatsApp ActivitySheetFragment writes
/// the layout resid into the ancestor int field; its own A02 is an
/// ImageView — 1,216 assignment-shape sites in the WA tree). jadx
/// renders `super.A02`; the IR has no super-field form, so witness
/// with the always-legal upcast `((Fragment) this).A02` — field lookup
/// on a cast expression is static, landing on the ancestor at any
/// chain depth. The receiver may still be the typed this-Local at this
/// pipeline point (the This rewrite comes later), so the compile-time
/// type comes from the owner expr / var table, mirroring
/// fix_field_owner_downcasts' owner_internal. Fires ONLY on a real
/// same-display shadow; unshadowed ancestor refs inherit through
/// `this.X` and must not churn. Same-raw-name lookups only: a shadow
/// whose raw name differs but display collides is beyond the pool
/// lookup keyed by name.
pub fn fix_shadowed_super_fields(
    body: &mut Stmt,
    vt: &VarTable,
    pool: &DexPool,
    class_name: &str,
) {
    /// Rendered name of `(cls, raw field name)`: the registry display
    /// when the class declares the field, else the raw name. None when
    /// the class is not pool-materialized or has no such field.
    fn display(pool: &DexPool, cls: &str, name: &str) -> Option<String> {
        let pc = pool.get(cls)?;
        let f = pc
            .static_fields
            .iter()
            .chain(pc.instance_fields.iter())
            .find(|f| f.name.as_ref() == name)?;
        Some(
            jdc_core::rename::field_display(cls, name, &f.desc)
                .map(|d| d.to_string())
                .unwrap_or_else(|| name.to_string()),
        )
    }
    fn owner_ct(
        o: &Option<Box<Expr>>,
        vt: &VarTable,
        class_name: &str,
    ) -> Option<std::sync::Arc<str>> {
        match o.as_deref() {
            None | Some(Expr::This) => Some(std::sync::Arc::from(class_name)),
            Some(Expr::Local { var, .. }) => match vt.var(*var).ty.erased() {
                JavaType::Object(n) => Some(n),
                _ => None,
            },
            Some(other) => match other.type_ref().erased() {
                JavaType::Object(n) => Some(n),
                _ => None,
            },
        }
    }
    walk_stmt_exprs(body, &mut |e| {
        deep_rewrite(e, &mut |x| {
            let Expr::Field {
                owner,
                cls,
                name,
                is_static: false,
                ..
            } = x
            else {
                return;
            };
            let Some(ot) = owner_ct(owner, vt, class_name) else {
                return;
            };
            if cls.as_ref() == ot.as_ref() {
                return;
            }
            let Some(ref_display) = display(pool, cls, name) else {
                return;
            };
            // Walk from the receiver's compile-time type up to (not
            // including) the declaring ancestor: any same-display
            // declaration on the way shadows the ref.
            let mut cur = ot.to_string();
            let mut shadowed = false;
            let mut found = false;
            for _ in 0..64 {
                if display(pool, &cur, name).as_deref() == Some(ref_display.as_str()) {
                    shadowed = true;
                }
                let Some(pc) = pool.get(&cur) else {
                    return;
                };
                let Some(sup) = pc.super_name.clone() else {
                    return;
                };
                if sup.as_str() == cls.as_ref() {
                    found = true;
                    break;
                }
                cur = sup;
            }
            if !found || !shadowed {
                return;
            }
            let ty = TypeRef::J(JavaType::Object(cls.clone()));
            match owner {
                Some(o) => {
                    if let Expr::Cast { ty: ct, .. } = &mut **o {
                        *ct = ty;
                    } else {
                        let taken = std::mem::replace(&mut **o, Expr::This);
                        **o = Expr::Cast {
                            ty,
                            e: Box::new(taken),
                        };
                    }
                }
                None => {
                    *owner = Some(Box::new(Expr::Cast {
                        ty,
                        e: Box::new(Expr::This),
                    }))
                }
            }
        });
    });
}

/// Subclass-field downcast witness + assign-cast narrowing.
///
/// R8 pushes fields DOWN into leaf classes (weibo: coroutine `label`
/// lives on each `Foo$bar$1` continuation, NOT on the rendered
/// BaseContinuationImpl/ContinuationImpl supers) while a check-cast in
/// the same flow types the receiver as the SUPERCLASS. The dex field
/// ref names the leaf (`$1->label` — verifier-legal through the
/// instance-of branch type), but Java resolves `((ContinuationImpl) c)
/// .label` against ContinuationImpl — "找不到符号 变量 label" (weibo
/// ×420). When the field's dex owner is a strict subtype of the
/// receiver's static type and the receiver's hierarchy does NOT
/// declare the name, re-target the receiver cast to the owner (the
/// runtime object IS the owner type — the verifier proved it).
///
/// Second rule: a `(T) e` value assigned into an S-typed target where
/// S <: T strictly ("ContinuationImpl无法转换为$reportWhenComplete$1")
/// narrows the cast to S — same verifier guarantee, and an assignment
/// position has no overload resolution to perturb.
pub fn fix_field_owner_downcasts(body: &mut Stmt, vt: &VarTable, pool: &DexPool) {
    fn owner_internal(o: &Expr, vt: &VarTable) -> Option<std::sync::Arc<str>> {
        let t = match o {
            Expr::Local { var, .. } => vt.var(*var).ty.erased(),
            other => other.type_ref().erased(),
        };
        match t {
            JavaType::Object(n) => Some(n),
            _ => None,
        }
    }
    fn declares_up(pool: &DexPool, class: &str, name: &str) -> bool {
        let mut cur = Some(class.to_string());
        let mut hops = 0;
        while let Some(c) = cur {
            hops += 1;
            if hops > 64 {
                return false;
            }
            let Some(pc) = pool.get(&c) else { return false };
            if pc
                .instance_fields
                .iter()
                .chain(pc.static_fields.iter())
                .any(|f| f.name.as_ref() == name)
            {
                return true;
            }
            cur = pc.super_name.clone();
        }
        false
    }
    walk_stmt_exprs(body, &mut |e| {
        deep_rewrite(e, &mut |x| {
            if let Expr::Field {
                owner: Some(o),
                cls,
                name,
                is_static: false,
                ..
            } = x
            {
                let Some(ot) = owner_internal(o, vt) else {
                    return;
                };
                if ot.as_ref() == cls.as_ref() || !pool.is_subtype(cls, &ot) {
                    return;
                }
                if declares_up(pool, &ot, name) || !declares_up(pool, cls, name) {
                    return;
                }
                let ty = TypeRef::J(JavaType::Object(cls.clone()));
                if let Expr::Cast { ty: ct, .. } = &mut **o {
                    *ct = ty;
                } else {
                    let taken = std::mem::replace(&mut **o, Expr::This);
                    **o = Expr::Cast { ty, e: Box::new(taken) };
                }
            }
            // Rule 2: `(T) e` into a strict-subtype target narrows to S.
            let (tgt_ty, value) = match x {
                Expr::Assign { target, op: AssignOp::Plain, value } => {
                    let t = match &**target {
                        Expr::Local { var, .. } => vt.var(*var).ty.erased(),
                        Expr::Field { ty, .. } => ty.erased(),
                        _ => return,
                    };
                    (t, value)
                }
                _ => return,
            };
            let JavaType::Object(sn) = tgt_ty else { return };
            if let Expr::Cast { ty, .. } = &mut **value {
                if let TypeRef::J(JavaType::Object(tn)) = ty {
                    if tn.as_ref() != sn.as_ref() && pool.is_subtype(&sn, tn) {
                        *ty = TypeRef::J(JavaType::Object(sn.clone()));
                    }
                }
            }
        });
    });
    // LocalDef inits: same narrowing against the declared var type.
    walk_mut_deep(body, &mut |st| {
        if let Stmt::LocalDef { var, init: Some(value), .. } = st {
            let tt = if (*var as usize) < vt.vars.len() {
                vt.vars[*var as usize].ty.erased()
            } else {
                return;
            };
            let JavaType::Object(sn) = tt else { return };
            if let Expr::Cast { ty, .. } = value {
                if let TypeRef::J(JavaType::Object(tn)) = ty {
                    if tn.as_ref() != sn.as_ref() && pool.is_subtype(&sn, tn) {
                        *ty = TypeRef::J(JavaType::Object(sn.clone()));
                    }
                }
            }
        }
    });
}

/// `==`/`!=` between UNRELATED class types is a compile error in Java
/// ("不可比较的类型: f0和a") while dex if-eqObj happily compares any two
/// references (weixin fn1/w1 ×298 files). One side takes an `(Object)`
/// witness cast — legal against anything. Interfaces and subtype-related
/// pairs are already comparable and stay untouched; java/lang/Object and
/// arrays-of it likewise.
pub fn fix_incomparable_equality(body: &mut Stmt, vt: &VarTable, pool: &crate::DexPool) {
    fn side_ty(e: &Expr, vt: &VarTable) -> JavaType {
        match e {
            Expr::Local { var, .. } if (*var as usize) < vt.vars.len() => {
                vt.var(*var).ty.erased()
            }
            other => other.type_ref().erased(),
        }
    }
    let incomparable = |a: &JavaType, b: &JavaType| -> bool {
        use JavaType::{Array, Object};
        match (a, b) {
            (Object(x), Object(y)) => {
                if x == y || x.as_ref() == "java/lang/Object" || y.as_ref() == "java/lang/Object"
                {
                    return false;
                }
                let xi = pool.get(x).map(|c| c.is_interface()).unwrap_or(true);
                let yi = pool.get(y).map(|c| c.is_interface()).unwrap_or(true);
                // Unknown classes (framework, not in pool): a cast is
                // harmless but usually unnecessary — treat as comparable
                // to stay conservative.
                if xi || yi {
                    return false;
                }
                !pool.is_subtype(x, y) && !pool.is_subtype(y, x)
            }
            (Array(_), Object(y)) | (Object(y), Array(_)) => {
                y.as_ref() != "java/lang/Object"
                    && y.as_ref() != "java/lang/Cloneable"
                    && y.as_ref() != "java/io/Serializable"
                    && !pool.get(y).map(|c| c.is_interface()).unwrap_or(false)
            }
            _ => false,
        }
    };
    let is_ref = |t: &JavaType| matches!(t, JavaType::Object(_) | JavaType::Array(_));
    walk_stmt_exprs(body, &mut |e| {
        deep_rewrite(e, &mut |x| {
            if let Expr::Bin {
                op: BinOp::Eq | BinOp::Ne,
                l,
                r,
                ..
            } = x
            {
                let lt = side_ty(l, vt);
                let rt = side_ty(r, vt);
                // `obj != 0` → `obj != null` with FINAL types: the
                // lift-time null_side_rewrite ran before infer_types
                // (its obj table saw the pre-inference int), leaving
                // `sQLiteClosable != 0` — "二元运算符 '!=' 的操作数
                // 类型错误" ×700 weixin.
                if is_ref(&lt) && matches!(&**r, Expr::Const(ConstVal::Int(0))) {
                    **r = Expr::Const(ConstVal::Null);
                    return;
                }
                if is_ref(&rt) && matches!(&**l, Expr::Const(ConstVal::Int(0))) {
                    **l = Expr::Const(ConstVal::Null);
                    return;
                }
                if incomparable(&lt, &rt) {
                    let taken = std::mem::replace(l, Box::new(Expr::Const(ConstVal::Null)));
                    **l = Expr::Cast {
                        ty: TypeRef::J(JavaType::Object("java/lang/Object".into())),
                        e: taken,
                    };
                }
            }
        });
    });
}

/// Primitive bridges at CALL/CTOR argument positions. Dex slots are
/// untyped category-1 integers: the verifier happily passes a Z-slot to
/// a `(B)` formal (weibo's Meituan-Robust hotpatch boilerplate boxes
/// boolean params via `new Byte(zreg)` — 4.2k+ `对于Byte(boolean),
/// 找不到合适的构造器`), and obfuscated/Kotlin code passes 0/1 slots to
/// `(Z)` formals (`int无法转换为boolean` at call sites). Java source
/// cannot express either — bridge at the IR level:
///   Boolean actual → numeric formal:  `(byte) (b ? 1 : 0)`
///   numeric actual → Boolean formal:  `x != 0`
/// The target method is DESCRIPTOR-bound (bytecode method_ref), so a
/// cast/compare to the exact formal type cannot flip overload
/// resolution the way r57's inferred object casts did — the rendered
/// call selects the same signature the dex named.
pub fn fix_primitive_arg_bridges(body: &mut Stmt, vt: &VarTable, pool: &crate::DexPool) {
    fn val_ty(e: &Expr, vt: &VarTable) -> JavaType {
        match e {
            Expr::Local { var, .. } => vt.var(*var).ty.erased(),
            other => other.type_ref().erased(),
        }
    }
    #[allow(clippy::type_complexity)]
    let is_bool = |t: &JavaType| matches!(t, JavaType::Boolean);
    let is_num = |t: &JavaType| {
        matches!(
            t,
            JavaType::Byte | JavaType::Short | JavaType::Int | JavaType::Char
                | JavaType::Long | JavaType::Float | JavaType::Double
        )
    };
    // BORROWED-arg form: returns the bridged replacement without
    // consuming the original (a `mem::replace` + `Option` dance dropped
    // the argument on the None path — every Boolean→Boolean arg became
    // a literal `null`: 61k 引用不明确 in one battery run).
    fn bridge(actual: &JavaType, formal: &JavaType, arg: &Expr) -> Option<Expr> {
        // A dex const carries the CALLEE's BITS: `const v, #0x41000000`
        // feeding a float formal IS 8.0f. Rendering the raw int literal
        // (a) mis-selects overloads — a competing `a(Context,int):void`
        // beats the int→float-widened `a(Context,float):int` in javac's
        // phase order ("不兼容的类型: void无法转换为int", weibo ju.a
        // ×1,259) — and (b) corrupts the VALUE (widening turns the bits
        // into 1.09e9f). The descriptor formal is dex ground truth:
        // reinterpret. Symmetric for the float-const-into-int-formal
        // direction (caller's bits, callee's reading).
        match (arg, formal) {
            (Expr::Const(ConstVal::Int(b)), JavaType::Float) => {
                return Some(Expr::Const(ConstVal::Float(f32::from_bits(*b as u32))));
            }
            (Expr::Const(ConstVal::Long(b)), JavaType::Double) => {
                return Some(Expr::Const(ConstVal::Double(f64::from_bits(*b as u64))));
            }
            (Expr::Const(ConstVal::Float(f)), JavaType::Int) => {
                return Some(Expr::Const(ConstVal::Int(f.to_bits() as i32)));
            }
            (Expr::Const(ConstVal::Double(d)), JavaType::Long) => {
                return Some(Expr::Const(ConstVal::Long(d.to_bits() as i64)));
            }
            _ => {}
        }
        if matches!(actual, JavaType::Boolean)
            && matches!(
                formal,
                JavaType::Byte | JavaType::Short | JavaType::Int | JavaType::Char
                    | JavaType::Long | JavaType::Float | JavaType::Double
            )
        {
            Some(Expr::Cast {
                ty: TypeRef::J(formal.clone()),
                e: Box::new(Expr::Cond {
                    c: Box::new(arg.clone()),
                    t: Box::new(Expr::Const(ConstVal::Int(1))),
                    f: Box::new(Expr::Const(ConstVal::Int(0))),
                }),
            })
        } else if matches!(formal, JavaType::Boolean)
            && matches!(
                actual,
                JavaType::Byte | JavaType::Short | JavaType::Int | JavaType::Char
                    | JavaType::Long | JavaType::Float | JavaType::Double
            )
        {
            Some(Expr::Bin {
                op: BinOp::Ne,
                l: Box::new(arg.clone()),
                r: Box::new(Expr::Const(ConstVal::Int(0))),
                ty: Some(TypeRef::J(JavaType::Boolean)),
            })
        } else if matches!(
            actual,
            JavaType::Byte | JavaType::Short | JavaType::Int | JavaType::Char
                | JavaType::Long | JavaType::Float | JavaType::Double
        ) && matches!(
            formal,
            JavaType::Byte | JavaType::Short | JavaType::Int | JavaType::Char
                | JavaType::Long | JavaType::Float | JavaType::Double
        ) && !implicit_widening(actual, formal)
        {
            // Numeric NARROWING into a formal (int arg → char formal):
            // javac rejects the lossy conversion, or — worse when the
            // class has no wider overload — silently resolves a
            // DIFFERENT overload than the dex descriptor names
            // (SpannableStringBuilder.append(int) doesn't exist;
            // `append(char)` is "从int转换到char可能会有损失", weibo ×76).
            // The descriptor formal is the dex ground truth; casting to
            // it pins the overload the bytecode actually invoked.
            Some(Expr::Cast {
                ty: TypeRef::J(formal.clone()),
                e: Box::new(arg.clone()),
            })
        } else {
            None
        }
    }
    /// A bare `null` argument against a SPECIFIC reference formal takes
    /// the descriptor cast: `this(context, null)` matching both
    /// Builder(Context,Notification) and Builder(Context,String) is
    /// "对Builder的引用不明确" (weibo NotificationCompat; protobuf
    /// `new FieldSet.Builder(null)` ×54). The descriptor names the
    /// exact invoked ctor, so the cast is dex ground truth — the
    /// descriptor-exact doctrine, not a guessed overload shift. Bare
    /// null stays for Object formals (casting those would add noise
    /// without resolving anything javac could not already pick).
    /// Framework classes javac resolves through android.jar even when
    /// the dex pool lacks them — the embedded API database answers
    /// precisely (the hand-written prefix list it replaced missed
    /// everything outside the guessed roots and over-trusted within
    /// them; removed-API families like org.apache.http are in the DB
    /// too, matching what a legacy-jar classpath resolves).
    fn fw_resolvable(n: &str) -> bool {
        crate::fwdb::exists(n)
    }
    fn null_arg_cast(a: &mut Expr, f: &JavaType, pool: &DexPool) -> bool {
        if !matches!(&*a, Expr::Const(ConstVal::Null)) {
            return false;
        }
        if let JavaType::Object(n) = f {
            // Cast only to RESOLVABLE types: a phantom formal (R8 kept
            // the ref, dropped the class — androidx.window.sidecar)
            // would turn a compiling bare `null` into a fresh
            // cannot-find. Pool classes render into the output;
            // java/javax/android/dalvik resolve through android.jar.
            let resolvable = pool.get(n).is_some() || fw_resolvable(n);
            if n.as_ref() != "java/lang/Object" && resolvable {
                *a = Expr::Cast {
                    ty: TypeRef::J(f.clone()),
                    e: Box::new(Expr::Const(ConstVal::Null)),
                };
                return true;
            }
        } else if let JavaType::Array(_) = f {
            // Descriptor-exact ARRAY cast: the dex named this exact
            // overload; a bare null beside sibling overloads is javac-
            // ambiguous (`a(String,b2)` + `a(String,String[])` both match
            // — weibo org/a/c/b, 对a的引用不明确 family). Same phantom
            // gate on the element class: casting to a phantom-element
            // array would trade the ambiguity for a cannot-find.
            let mut e: &JavaType = f;
            while let JavaType::Array(inner) = e {
                e = &**inner;
            }
            let resolvable = match e {
                JavaType::Object(n) => pool.get(n).is_some() || fw_resolvable(n),
                _ => true, // primitive-element array
            };
            if resolvable {
                *a = Expr::Cast {
                    ty: TypeRef::J(f.clone()),
                    e: Box::new(Expr::Const(ConstVal::Null)),
                };
                return true;
            }
        }
        // Bare null deliberately stays (Object formal / phantom type):
        // the caller's ambiguity pin may still act on it.
        false
    }
    /// JLS 5.1.2 widening primitive conversion (char joins the chain
    /// only at int; byte/short do NOT widen to char).
    fn implicit_widening(a: &JavaType, f: &JavaType) -> bool {
        use JavaType::*;
        match (a, f) {
            (Byte, Short) | (Byte, Int) | (Byte, Long) | (Byte, Float) | (Byte, Double) => true,
            (Short, Int) | (Short, Long) | (Short, Float) | (Short, Double) => true,
            (Char, Int) | (Char, Long) | (Char, Float) | (Char, Double) => true,
            (Int, Long) | (Int, Float) | (Int, Double) => true,
            (Long, Float) | (Long, Double) => true,
            (Float, Double) => true,
            (x, y) => x == y,
        }
    }
    // Does the owner class declare ANOTHER method with the same name
    // and arity but a different signature? Then javac's most-specific
    // overload resolution can steal the call from the dex descriptor's
    // target (weibo 对a的引用不明确 ×391: `c.a(null)` against
    // a(ExecuteResult)/a(Exception)/a(Object) — the descriptor names
    // a(Object), but bare null leaves the two specific overloads
    // ambiguously most-specific). Framework owners are not in the
    // pool — conservatively report no competition.
    let overload_competes = |cls: &str, name: &str, args: &[JavaType]| -> bool {
        if let Some(c) = pool.get(cls) {
            return c.all_methods().any(|m| {
                &*m.name == name
                    // Bridge/synthetic siblings are compiler artifacts
                    // resolving to the SAME target — counting them as
                    // competitors cast enum compareTo args to the erased
                    // formal the rendered specialization no longer
                    // accepts (Enum无法转换为LogLevel ×2,079 reqable).
                    && m.access & (crate::access::ACC_BRIDGE | crate::access::ACC_SYNTHETIC) == 0
                    && m.parsed_desc()
                        .is_some_and(|d| d.args.len() == args.len() && d.args != args)
            });
        }
        // Framework owner: NOT enumerable for pin purposes — the
        // full-DB enumeration experiment (step-5) measured +532 battery
        // with ZERO ambiguous gain: a subtype arg against a concrete
        // erased formal fires the cast even when the call was never
        // ambiguous, and bounded-generic formals (List<T> -> List,
        // Property<T,Float> erasures) make the cast break inference or
        // miss the rendered specialization. Framework subtype pins stay
        // on the measured allowlist below.
        let _ = args;
        false
    };
    // Ctors: formal types via the pool (unanimous across same-arity
    // overloads, else skip — ambiguity must not guess).
    let ctor_formals = |cls: &str, n: usize| -> Option<Vec<JavaType>> {
        let c = pool.get(cls)?;
        let mut out: Option<Vec<JavaType>> = None;
        for m in c.all_methods() {
            if &*m.name != "<init>" {
                continue;
            }
            let Some(d) = m.parsed_desc() else { continue };
            if d.args.len() != n {
                continue;
            }
            match &out {
                None => out = Some(d.args.clone()),
                Some(prev) if *prev != d.args => return None,
                Some(_) => {}
            }
        }
        out
    };
    walk_stmt_exprs(body, &mut |e| {
        deep_rewrite(e, &mut |x| match x {
            Expr::Method {
                cls,
                name,
                desc,
                args,
                is_static,
                ..
            } => {
                if args.len() != desc.args.len() {
                    return;
                }
                let _ = is_static;
                let mut competes: Option<bool> = None;
                for (a, f) in args.iter_mut().zip(desc.args.iter()) {
                    if null_arg_cast(a, f, pool) {
                        continue;
                    }
                    if matches!(a, Expr::Cast { .. }) {
                        continue;
                    }
                    // Descriptor-exact ambiguity pin: a bare null
                    // against an Object formal (null_arg_cast
                    // deliberately leaves those bare — safe only
                    // WITHOUT competing specific overloads) or a
                    // strict-subtype arg can match several declared
                    // overloads; casting to the descriptor formal
                    // leaves exactly the dex-invoked method
                    // applicable (JLS 15.12.2 — the descriptor is
                    // ground truth, so the cast pins resolution
                    // instead of shifting it).
                    let subtype_arg = match (&val_ty(a, vt), f) {
                        (JavaType::Object(an), JavaType::Object(fn_)) => {
                            // java/lang/Object is the root: ANY reference
                            // arg is a strict subtype of it — pool.is_subtype
                            // cannot see framework chains (String -> Object),
                            // so the Object formal gets the direct rule.
                            (an != fn_ && pool.is_subtype(an, fn_))
                                || (fn_.as_ref() == "java/lang/Object"
                                    && an.as_ref() != "java/lang/Object")
                        }
                        (JavaType::Array(_), JavaType::Object(fn_))
                            if fn_.as_ref() == "java/lang/Object" =>
                        {
                            true
                        }
                        _ => false,
                    };
                    // Bare null against java/lang/Object itself: the
                    // descriptor method exists (dex resolved it), so
                    // `(Object) null` is always legal and excludes every
                    // more-specific sibling overload from applicability.
                    // Pool owners take the competes gate (emitted
                    // declarations are raw, so the cast cannot disturb
                    // inference). FRAMEWORK owners only via allowlist of
                    // the classic non-generic null-ambiguous JDK methods
                    // (StringBuilder.append ×66 lark): a blanket cast
                    // breaks GENERIC framework methods whose descriptor
                    // formal is an ERASED type variable —
                    // `ofFloat((Object) null, View.ALPHA, v)` killed the
                    // T=View inference bare null provided (weibo +4).
                    let null_obj =
                        matches!(a, Expr::Const(ConstVal::Null)) && matches!(f, JavaType::Object(_));
                    let framework_owner = pool.get(cls.as_ref()).is_none();
                    let fw_null_ambiguous = framework_owner
                        && matches!(
                            (cls.as_ref(), name.as_ref()),
                            ("java/lang/StringBuilder", "append")
                                | ("java/lang/StringBuffer", "append")
                                | ("java/io/PrintStream", "println")
                                | ("java/io/PrintStream", "print")
                                | ("java/io/PrintWriter", "println")
                                | ("java/io/PrintWriter", "print")
                                | ("java/lang/String", "valueOf")
                        );
                    // Framework owners with famously overload-heavy
                    // methods take the subtype pin too: the arg's
                    // rendered type implements SEVERAL formal types
                    // (weibo putExtra ×41: floatMsgData is both
                    // Parcelable and Serializable; schedule/submit:
                    // Runnable-and-Callable workers) and the competes
                    // gate cannot enumerate framework overloads.
                    // Formal == java/lang/Object stays excluded — that
                    // is an erased type variable and the cast would
                    // kill generic inference (ofFloat lesson).
                    let fw_subtype_pinnable = framework_owner
                        && subtype_arg
                        && !matches!(f, JavaType::Object(fn_) if fn_.as_ref() == "java/lang/Object")
                        && matches!(
                            (cls.as_ref(), name.as_ref()),
                            ("android/content/Intent", "putExtra")
                                | ("java/util/concurrent/ScheduledExecutorService", "schedule")
                                | ("java/util/concurrent/ExecutorService", "submit")
                                | ("java/util/concurrent/ExecutorService", "invokeAll")
                        );
                    if fw_subtype_pinnable
                        || (subtype_arg
                            && *competes.get_or_insert_with(|| {
                                overload_competes(cls, name, &desc.args)
                            }))
                        || (null_obj
                            && (fw_null_ambiguous
                                || *competes.get_or_insert_with(|| {
                                    overload_competes(cls, name, &desc.args)
                                })))
                    {
                        let old =
                            std::mem::replace(a, Expr::Const(ConstVal::Null));
                        *a = Expr::Cast {
                            ty: TypeRef::J(f.clone()),
                            e: Box::new(old),
                        };
                        continue;
                    }
                    if !is_bool(&val_ty(a, vt)) && !is_num(&val_ty(a, vt)) {
                        continue;
                    }
                    if !is_bool(f) && !is_num(f) {
                        continue;
                    }
                    if let Some(b) = bridge(&val_ty(a, vt), f, a) {
                        *a = b;
                    }
                }
            }
            Expr::New { cls, args, arg_tys, .. } => {
                let formals_owned = if arg_tys.len() == args.len() && !arg_tys.is_empty() {
                    Some(arg_tys.clone())
                } else {
                    ctor_formals(cls, args.len())
                };
                if let Some(formals) = formals_owned {
                    for (a, f) in args.iter_mut().zip(formals.iter()) {
                        if null_arg_cast(a, f, pool) {
                            continue;
                        }
                        if matches!(a, Expr::Cast { .. }) {
                            continue;
                        }
                        if !is_bool(f) && !is_num(f) {
                            continue;
                        }
                        if let Some(b) = bridge(&val_ty(a, vt), f, a) {
                            *a = b;
                        }
                    }
                }
            }
            _ => {}
        });
    });
}

/// `b ^ 1` / `1 ^ b` with a BOOLEAN operand is the bytecode's boolean
/// negation (`!b`) — weibo kotlin-stdlib `return v2 ^ 1;` ("二元运算符
/// '^' 的操作数类型错误"), and `x ^ true` shapes. Java only types `^`
/// for same-category operands.
/// Rename locals whose name equals a ROOT PACKAGE first segment: a
/// package-qualified render (`v2.n.a` — the FQN fallback a twin-shadowed
/// same-package ref must take) binds its first identifier lexically, and
/// a local `v2` in scope captures it (weixin v2/i: 找不到符号 变量 n on a
/// legit `v2.n.a(this, 0)`). Method-scope renames carry no registry
/// surface — every use renders through this same vt — unlike class/field
/// renames, whose reference coverage blew up five times before.
pub fn deshadow_locals(vt: &mut VarTable, pool: &crate::DexPool, class_name: &str, body: &Stmt) {
    let segs = pool.root_pkg_segs();
    // Import-layer simples: the file renders `import p.h;` + bare `h.e`
    // for obscured refs — a local `h` captures that simple name (locals
    // beat imports in expression position; 无法取消引用int family).
    let obscured = crate::classdec::obscured_simples_snapshot();
    // Same-package static-ref simples: a static member ref renders its
    // class qualifier as the SIMPLE name (same package — no import/FQN),
    // and a local with that name captures it (`a2.changeQuickRedirect`
    // bound to a String local `a2` — "找不到符号 变量 changeQuickRedirect
    // 位置: 类型为String的变量 a2", weibo robust-field refs ×317). Only
    // STATIC refs qualify (an instance ref's qualifier IS the local).
    let self_pkg = class_name.rfind('/').map(|i| &class_name[..i]).unwrap_or("");
    let mut same_pkg_simples: jdc_core::FxHashSet<String> = jdc_core::FxHashSet::default();
    visit_all_exprs(body, &mut |x| {
        let cls = match x {
            Expr::Field { cls, is_static: true, .. } => Some(cls.as_ref()),
            Expr::Method { cls, is_static: true, .. } => Some(cls.as_ref()),
            _ => None,
        };
        if let Some(c) = cls {
            let pkg = c.rfind('/').map(|i| &c[..i]).unwrap_or("");
            if pkg == self_pkg {
                let simple = &c[c.rfind('/').map(|i| i + 1).unwrap_or(0)..];
                let head = simple.split('$').next().unwrap_or(simple);
                if !head.is_empty() {
                    same_pkg_simples.insert(head.to_string());
                }
            }
        }
    });
    let hit = |name: &str| {
        segs.contains(name) || obscured.contains(name) || same_pkg_simples.contains(name)
    };
    if (segs.is_empty() && obscured.is_empty() && same_pkg_simples.is_empty())
        || !vt.vars.iter().any(|v| hit(&v.name))
    {
        return;
    }
    let mut taken: jdc_core::FxHashSet<String> =
        vt.vars.iter().map(|v| v.name.clone()).collect();
    for v in vt.vars.iter_mut() {
        if !hit(&v.name) {
            continue;
        }
        let mut k = 0u32;
        let cand = loop {
            k += 1;
            let c = if k == 1 {
                format!("{}x", v.name)
            } else {
                format!("{}x{}", v.name, k)
            };
            if !taken.contains(&c) {
                break c;
            }
        };
        taken.insert(cand.clone());
        v.name = cand;
    }
}

/// Receiver rescue: a Field/Method receiver whose final vt type is
/// PRIMITIVE can never hold the member (structurer tail-copy regions
/// can map the receiver register onto a boolean/int generation —
/// wcdb HandleOperation's `v10_g2.finalizeStatement()` where the
/// statement var is right there; weixin deref-int/bool families,
/// ~2.5k lines). The member descriptor's owner class is authoritative:
/// when EXACTLY ONE var in the method has that type (or a subtype),
/// swap the receiver to it. The pre-rescue output is a guaranteed
/// compile error AND runtime NPE, so the unambiguous swap cannot make
/// it worse; ambiguous sites stay untouched.
/// Anonymous/inner-class outer reads: the dex reads `this$0` off the
/// instance register, which copy materialization can route through a
/// local (`v41 = this; ... v41.this$0` — weibo HorseRaceDetector$1,
/// 2,326-line 找不到符号 变量 this$0 family). When the field's
/// declaring class IS the class being decompiled, the only instance in
/// scope is `this` — strip the owner so it renders the bare declared
/// field.
/// Fold single-assigned `this`-copy locals in ctors back to `this`.
/// R8 materializes the receiver with `move-object/from16 v1, p0` and
/// runs every iput through the copy; the copy renders as a local and a
/// final-field store through it is "无法为 final 变量 g 分配值" (Java
/// allows blank-final writes only via `this`/simple name — hf/n2 ×29,
/// weixin final-var residual). Transitively: `b = a; c = b` chains
/// collapse. Single-assignment gate: a var ever written from another
/// value is not an alias.
pub fn fix_ctor_this_aliases(body: &mut Stmt, vt: &VarTable) {
    let Some(this_var) = vt
        .vars
        .iter()
        .find(|v| v.is_param && v.name == "this")
        .map(|v| v.id)
    else {
        return;
    };
    let mut analysis = VarAnalysis {
        assigns: Vec::new(),
        reads: Vec::new(),
        values: Vec::new(),
    };
    analyze_vars(body, &mut analysis);
    // All-sources rule: a local is a this-alias only when EVERY value
    // assignment traces to `this_var` through pure local copies. The
    // single-assign constraint this replaces missed R8's multi-branch
    // `this` spills (uuyc androidx.camera x_2: `v160 = v150` on two
    // paths, both this, chain this→v131→v140→v169→v174→v183→v150→v160),
    // so `v160.d = executor` never became `this.d = executor` and javac
    // rejected the final-field init (无法为 final 变量 d/i/j 分配值 ×103).
    // A single non-this source (field read, `new`, param, a literal)
    // disqualifies the local, so a marked alias is provably always this
    // and rewriting every read of it to `this` is sound.
    enum Src {
        This,
        Loc(u32),
        Other,
    }
    let mut sources: jdc_core::FxHashMap<u32, Vec<Src>> = jdc_core::FxHashMap::default();
    walk_all(body, &mut |st| {
        let (tgt, val): (u32, &Expr) = match st {
            Stmt::ExprStmt(Expr::Assign { target, value, .. }) => match &**target {
                Expr::Local { var, .. } => (*var, &**value),
                _ => return,
            },
            Stmt::LocalDef { var, init: Some(value), .. } => (*var, value),
            _ => return,
        };
        let s = match val {
            Expr::This => Src::This,
            Expr::Local { var: src, .. } if *src == this_var => Src::This,
            Expr::Local { var: src, .. } => Src::Loc(*src),
            _ => Src::Other,
        };
        sources.entry(tgt).or_default().push(s);
    });
    let n = vt
        .vars
        .len()
        .max(analysis.assigns.len())
        .max(sources.keys().map(|k| *k as usize + 1).max().unwrap_or(0));
    let mut alias = vec![false; n];
    loop {
        let mut changed = false;
        for (v, srcs) in &sources {
            let vi = *v as usize;
            if vi >= n || alias[vi] || srcs.is_empty() {
                continue;
            }
            let ok = srcs.iter().all(|s| match s {
                Src::This => true,
                Src::Loc(w) => (*w as usize) < n && alias[*w as usize],
                Src::Other => false,
            });
            if ok {
                alias[vi] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    if !alias.iter().any(|&b| b) {
        return;
    }
    walk_stmt_exprs(body, &mut |e| {
        deep_rewrite_reads(e, &mut |x| {
            if let Expr::Local { var, .. } = x {
                if (*var as usize) < n && alias[*var as usize] {
                    *x = Expr::This;
                }
            }
        });
    });
}

pub fn fix_this0_owners(body: &mut Stmt, class_name: &str) {
    walk_stmt_exprs(body, &mut |e| {
        deep_rewrite(e, &mut |x| {
            if let Expr::Field { owner: Some(_), cls, name, .. } = x {
                if name.as_ref() == "this$0" && cls.as_ref() == class_name {
                    if let Expr::Field { owner, .. } = x {
                        *owner = None;
                    }
                }
            }
        });
    });
}

pub fn rescue_primitive_receivers(body: &mut Stmt, vt: &VarTable, pool: &crate::DexPool) {
    fn prim(t: &JavaType) -> bool {
        match t {
            JavaType::Int
            | JavaType::Long
            | JavaType::Short
            | JavaType::Byte
            | JavaType::Char
            | JavaType::Float
            | JavaType::Double
            | JavaType::Boolean => true,
            // Boxed JDK finals carry no app members either (`bool19.d`
            // where bool19 came out java/lang/Boolean — the coroutine
            // Result-register conflation).
            JavaType::Object(n) => matches!(
                n.as_ref(),
                "java/lang/Boolean"
                    | "java/lang/String"
                    | "java/lang/Integer"
                    | "java/lang/Long"
                    | "java/lang/Short"
                    | "java/lang/Byte"
                    | "java/lang/Character"
                    | "java/lang/Float"
                    | "java/lang/Double"
            ),
            _ => false,
        }
    }
    walk_stmt_exprs(body, &mut |e| {
        deep_rewrite(e, &mut |x| {
            // Phase 1 (read-only): primitive-typed Local receiver?
            let (recv_var, recv_ty) = {
                let o: &Expr = match &*x {
                    Expr::Method { owner: Some(o), .. } => o,
                    Expr::Field { owner: Some(o), is_static: false, .. } => o,
                    _ => return,
                };
                let Expr::Local { var, ty } = o else {
                    return;
                };
                let vt_ty = if (*var as usize) < vt.vars.len() {
                    vt.var(*var).ty.erased()
                } else {
                    ty.erased()
                };
                if !prim(&vt_ty) {
                    return;
                }
                (*var, vt_ty)
            };
            let cls: std::sync::Arc<str> = match &*x {
                Expr::Method { cls, .. } | Expr::Field { cls, .. } => cls.clone(),
                _ => return,
            };
            // Type-consistent call: the receiver's own type IS the
            // member owner (or a subtype) — `str.hashCode()` on a
            // String param needs no rescue. Without this the boxed-JDK
            // prim() arm fired on every String member call and the
            // max-id fallback below hijacked the reads to the highest
            // String-typed var in the method — often a case-local
            // (lark LynxUIBaseInput str11: 37 reads rewritten to a
            // switch-case-scoped def, 找不到符号 ×37; x0/c$a18 str4).
            if let JavaType::Object(n) = &recv_ty {
                if n.as_ref() == cls.as_ref() || pool.is_subtype(n.as_ref(), cls.as_ref()) {
                    return;
                }
            }
            // Phase 2: vars of the owner type (exact or subtype).
            let mut cands: Vec<u32> = Vec::new();
            for v in &vt.vars {
                if let JavaType::Object(n) = v.ty.erased() {
                    if n.as_ref() == cls.as_ref()
                        || pool.is_subtype(n.as_ref(), cls.as_ref())
                    {
                        cands.push(v.id);
                    }
                }
            }
            let id = if cands.len() == 1 {
                cands[0]
            } else if cands.len() > 1 {
                // Multiple owner-typed vars: prefer the same-SLOT one
                // (register lineage), latest generation wins — the
                // receiver position is scope-insensitive (any method-
                // scope var renders, the decl hoist covers it).
                let slot = if (recv_var as usize) < vt.vars.len() {
                    Some(vt.var(recv_var).slot)
                } else {
                    None
                };
                let mut hits: Vec<u32> = match slot {
                    Some(sl) => cands
                        .iter()
                        .copied()
                        .filter(|&c| {
                            c != recv_var
                                && (c as usize) < vt.vars.len()
                                && vt.var(c).slot == sl
                        })
                        .collect(),
                    None => Vec::new(),
                };
                if hits.is_empty() {
                    // No same-slot twin: fall back to the latest
                    // owner-typed generation (max id).
                    hits = cands
                        .iter()
                        .copied()
                        .filter(|&c| c != recv_var)
                        .collect();
                }
                hits.sort_unstable();
                hits.dedup();
                if hits.is_empty() {
                    return;
                }
                *hits.last().unwrap()
            } else {
                // Lineage disambiguation by SLOT: register reuse makes
                // the Boolean/primitive twin and the object version of
                // one dex register share VarInfo.slot (ry0/h2's
                // `bool19.d` — the v1-typed twin rides the same
                // register). Multiple slot matches stay untouched
                // (deterministic conservatism).
                if cands.is_empty() {
                    return;
                }
                let slot = if (recv_var as usize) < vt.vars.len() {
                    vt.var(recv_var).slot
                } else {
                    return;
                };
                let mut hits: Vec<u32> = cands
                    .into_iter()
                    .filter(|&c| {
                        c != recv_var
                            && (c as usize) < vt.vars.len()
                            && vt.var(c).slot == slot
                    })
                    .collect();
                hits.sort_unstable();
                hits.dedup();
                if hits.len() != 1 {
                    return;
                }
                hits[0]
            };
            // Phase 3: swap.
            let o: &mut Box<Expr> = match x {
                Expr::Method { owner: Some(o), .. } => o,
                Expr::Field { owner: Some(o), is_static: false, .. } => o,
                _ => return,
            };
            if let Expr::Local { var, ty } = &mut **o {
                if *var == recv_var {
                    *var = id;
                    *ty = vt.vars[id as usize].ty.clone();
                }
            }
        });
    });

}

/// Scope-safe arg/return swaps for mistyped generations — runs AFTER
/// ensure_declared hoists every declaration to the method top, so the
/// top_ids visibility precondition holds by construction (the r134
/// falsification: swapping in a block-scoped var broke lexically).
/// Same-slot + type-match + uniqueness gates as the receiver rescue.
pub fn rescue_arg_return_swaps(
    body: &mut Stmt,
    vt: &VarTable,
    pool: &crate::DexPool,
    ret_ty: &JavaType,
) {
    // SCOPE-SAFE arg/return swap: a primitive-typed actual against a
    // specific-reference formal (or a reference-returning `return`) is
    // a mistyped generation — the r134 falsification showed the swap
    // target MUST be lexically visible at the use site, so candidates
    // are restricted to params + TOP-LEVEL declared locals (a hoisted
    // decl is visible to javac name resolution everywhere in the
    // method) AND same-slot (same dex register lineage) AND unique.
    let mut top_ids: jdc_core::FxHashSet<u32> = jdc_core::FxHashSet::default();
    for v in &vt.vars {
        if v.is_param {
            top_ids.insert(v.id);
        }
    }
    if let Stmt::Block(stmts) = body {
        for st in stmts.iter() {
            if let Stmt::LocalDef { var, .. } = st {
                top_ids.insert(*var);
            }
        }
    }
    let ret_wants_ref = match ret_ty {
        JavaType::Object(n) => n.as_ref() != "java/lang/Object",
        JavaType::Array(_) => true,
        _ => false,
    };
    let try_swap = |a: &mut Expr, want: &JavaType| {
        let Expr::Local { var, ty } = &mut *a else {
            return;
        };
        if (*var as usize) >= vt.vars.len() {
            return;
        }
        let avt = vt.var(*var).ty.erased();
        let avt_ref = matches!(avt, JavaType::Object(_) | JavaType::Array(_));
        let want_arr = matches!(want, JavaType::Array(_));
        // Only swap a primitive-typed actual (an Object actual gets the
        // descriptor-exact cast instead — never both).
        if avt_ref && !(avt == JavaType::Object("java/lang/Object".into())) {
            return;
        }
        if avt_ref && !matches!(want, JavaType::Object(_)) {
            return;
        }
        let slot = vt.var(*var).slot;
        let mut hits: Vec<u32> = Vec::new();
        for v in &vt.vars {
            if v.slot != slot || !top_ids.contains(&v.id) || v.id == *var {
                continue;
            }
            let t = v.ty.erased();
            let ok = match (&t, want) {
                (JavaType::Object(n), JavaType::Object(w)) => {
                    n.as_ref() == w.as_ref()
                        || pool.is_subtype(n.as_ref(), w.as_ref())
                }
                (JavaType::Array(_), JavaType::Array(_)) => want_arr,
                _ => false,
            };
            if ok {
                hits.push(v.id);
            }
        }
        hits.sort_unstable();
        hits.dedup();
        if hits.is_empty() {
            return;
        }
        // Multiple same-slot candidates: the LATEST generation (max id —
        // split mints in walk order ≈ program order) is the closest
        // approximation of the register's value at the use site. A
        // wrong pick still compiles (type-matched by construction);
        // leaving the primitive actual never does.
        let id = *hits.last().unwrap();
        *var = id;
        *ty = vt.vars[id as usize].ty.clone();
    };
    walk_stmt_exprs(body, &mut |e| {
        deep_rewrite(e, &mut |x| {
            match x {
                Expr::Method { desc, args, is_dynamic: false, .. } => {
                    for (i, a) in args.iter_mut().enumerate() {
                        if let Some(f) = desc.args.get(i) {
                            if matches!(f, JavaType::Object(n) if n.as_ref() != "java/lang/Object")
                                || matches!(f, JavaType::Array(_))
                            {
                                try_swap(a, f);
                            }
                        }
                    }
                }
                Expr::New { arg_tys, args, .. } => {
                    for (i, a) in args.iter_mut().enumerate() {
                        if let Some(f) = arg_tys.get(i) {
                            if matches!(f, JavaType::Object(n) if n.as_ref() != "java/lang/Object")
                                || matches!(f, JavaType::Array(_))
                            {
                                try_swap(a, f);
                            }
                        }
                    }
                }
                _ => {}
            }
        });
    });
    if ret_wants_ref {
        walk_mut_deep(body, &mut |st| {
            if let Stmt::Return(Some(e)) = st {
                try_swap(e, ret_ty);
            }
        });
    }
}

/// Reference-array initializers take null, not 0: dex fill-array-data
/// over a String[]/Object[] slot carries the zero word, and the lifted
/// `new String[] {0}` is int无法转换为String (qs0/b family). A Const
/// Int(0) element of a reference-typed array init is the null ref.
/// `refVar = const 0` (dex `const/4 vN, 0` into a reference register) is
/// the NULL sentinel, not an int. By the time this runs infer_types has
/// typed the var a reference (its `.close()`/`.length`/method reads
/// proved it), so retype the const to Null BEFORE split_generations —
/// otherwise the ref/primitive mismatch splits off an int generation and
/// orphans the reference reads (u7.l try-with-resources finally
/// `v16_g3 = 0; if (v16_g3 != 0) v16_g3.close();` → "无法取消引用int").
/// `fix_incomparable_equality` already retypes the `refVar != 0`
/// COMPARISON side (but runs after the split); this covers the ASSIGN
/// side, which is what triggers the bad split.
pub fn fix_ref_null_assigns(vt: &VarTable, body: &mut Stmt) {
    let is_ref_var = |var: u32| -> bool {
        (var as usize) < vt.vars.len()
            && matches!(
                vt.vars[var as usize].ty.erased(),
                JavaType::Object(_) | JavaType::Array(_)
            )
    };
    walk_mut_deep(body, &mut |st| match st {
        Stmt::ExprStmt(Expr::Assign {
            target,
            value,
            op: AssignOp::Plain,
            ..
        }) => {
            if let Expr::Local { var, .. } = &**target {
                if is_ref_var(*var) && matches!(&**value, Expr::Const(ConstVal::Int(0))) {
                    **value = Expr::Const(ConstVal::Null);
                }
            }
        }
        Stmt::LocalDef {
            var,
            init: Some(e),
            ..
        } => {
            if is_ref_var(*var) && matches!(e, Expr::Const(ConstVal::Int(0))) {
                *e = Expr::Const(ConstVal::Null);
            }
        }
        _ => {}
    });
}

pub fn fix_ref_array_null_consts(body: &mut Stmt) {
    walk_stmt_exprs(body, &mut |e| {
        deep_rewrite(e, &mut |x| {
            if let Expr::NewArray { elem, init: Some(list), .. } = x {
                if matches!(elem.erased(), JavaType::Object(_) | JavaType::Array(_)) {
                    for slot in list.iter_mut() {
                        if matches!(&*slot, Expr::Const(ConstVal::Int(0))) {
                            *slot = Expr::Const(ConstVal::Null);
                        }
                    }
                }
            }
        });
    });
}

// ---------------------------------------------------------------------------
// Null-sentinel repair: const-0 registers in reference-typed slots
// ---------------------------------------------------------------------------

/// A dex `const/4 vN, 0` register is ambiguous between int 0 and the
/// null reference; the value view types the bare constant as Int.
/// Kotlin coroutine state machines lean on this heavily — captured
/// object slots are zero-initialized ahead of the label switch, then
/// stored into `Object` fields and passed as `Continuation` ctor args
/// (weixin y01/s9 q8: `new e6(s94x, v287)` "int无法转换为Continuation";
/// uf5/l0 `throw 0;`; wcdb RepairKit `setOnCancelListener(0)`). The
/// dex verifier guarantees a const-0 register reaching a reference-
/// typed slot holds null, so in those slots the rendering must be the
/// null literal.
///
/// Two rewrites, both gated on the slot's expected type being a
/// reference (descriptor formal types, field type, array component,
/// cast target, throw, method return):
///   1. a literal `0` in a ref slot → `null`. SKIPPED for call-arg
///      slots whose formal is exactly `java/lang/Object`: `f(0)` there
///      compiles TODAY (boxing, or an int-overload pick like
///      `String.valueOf(0)`), and `f(null)` could newly fail on
///      overload ambiguity (`valueOf(Object)` vs `valueOf(char[])`) —
///      a guaranteed-error slot becomes a compile, a compiling slot
///      must not become an error.
///   2. a proven null-sentinel LOCAL: an Int-typed var whose every
///      write is `= 0` or `= <another sentinel>` and whose every read
///      sits in a ref slot (or feeds another sentinel's write — a dead
///      chain) → `null` at each read. The same Object-formal skip
///      applies at arg slots. A read in ANY int/unknown context
///      (binop, cond test, receiver deref, selector, anon-ctor arg,
///      lambda capture…) disqualifies the var — register reuse across
///      generations keeps its int rendering.
///
/// Runs after fix_ref_null_assigns (which converts writes of REF-typed
/// vars) and before split_generations (sentinel writes must not mint
/// int generations). Writes of confirmed sentinels stay as harmless
/// dead int stores for drop_dead_locals.
pub fn fix_null_sentinels(body: &mut Stmt, vt: &VarTable, ret: &JavaType) {
    #[derive(Clone, Copy, PartialEq)]
    enum WK {
        Zero,
        Copy(u32),
        Other,
    }
    fn is_ref_jt(t: &JavaType) -> bool {
        matches!(t, JavaType::Object(_) | JavaType::Array(_))
    }
    fn cls_write(e: &Expr) -> WK {
        match e {
            Expr::Const(ConstVal::Int(0)) => WK::Zero,
            Expr::Local { var, .. } => WK::Copy(*var),
            _ => WK::Other,
        }
    }
    fn expr_writes(e: &Expr, writes: &mut jdc_core::FxHashMap<u32, Vec<WK>>) {
        visit_exprs(e, &mut |x| match x {
            Expr::Assign { target, op, value } => {
                if let Expr::Local { var, .. } = &**target {
                    let k = if matches!(op, AssignOp::Plain) {
                        cls_write(value)
                    } else {
                        WK::Other
                    };
                    writes.entry(*var).or_default().push(k);
                }
            }
            Expr::PreIncDec { e: inner, .. } | Expr::PostIncDec { e: inner, .. } => {
                if let Expr::Local { var, .. } = &**inner {
                    writes.entry(*var).or_default().push(WK::Other);
                }
            }
            _ => {}
        });
    }
    fn collect_writes(
        s: &Stmt,
        writes: &mut jdc_core::FxHashMap<u32, Vec<WK>>,
        has_zero: &mut bool,
    ) {
        fn note(e: &Expr, has_zero: &mut bool, writes: &mut jdc_core::FxHashMap<u32, Vec<WK>>) {
            visit_exprs(e, &mut |x| {
                if let Expr::Const(ConstVal::Int(0)) = x {
                    *has_zero = true;
                }
            });
            expr_writes(e, writes);
        }
        match s {
            Stmt::Block(v) => {
                for x in v {
                    collect_writes(x, writes, has_zero);
                }
            }
            Stmt::LocalDef { var, init, .. } => {
                if let Some(e) = init {
                    if matches!(e, Expr::Const(ConstVal::Int(0))) {
                        *has_zero = true;
                    }
                    writes.entry(*var).or_default().push(cls_write(e));
                }
            }
            Stmt::ExprStmt(e) => note(e, has_zero, writes),
            Stmt::Return(e) => {
                if let Some(x) = e {
                    note(x, has_zero, writes);
                }
            }
            Stmt::Throw(e) | Stmt::MonitorEnter(e) | Stmt::MonitorExit(e) => {
                note(e, has_zero, writes);
            }
            Stmt::TernaryValue { e } => note(e, has_zero, writes),
            Stmt::If { cond, then_stmt, else_stmt } => {
                note(cond, has_zero, writes);
                collect_writes(then_stmt, writes, has_zero);
                if let Some(b) = else_stmt {
                    collect_writes(b, writes, has_zero);
                }
            }
            Stmt::While { cond, body } | Stmt::DoWhile { body, cond } => {
                note(cond, has_zero, writes);
                collect_writes(body, writes, has_zero);
            }
            Stmt::For { init, cond, update, body } => {
                for x in init {
                    collect_writes(x, writes, has_zero);
                }
                if let Some(c) = cond {
                    note(c, has_zero, writes);
                }
                for u in update {
                    note(u, has_zero, writes);
                }
                collect_writes(body, writes, has_zero);
            }
            Stmt::ForEach { var, iterable, body, .. } => {
                writes.entry(*var).or_default().push(WK::Other);
                note(iterable, has_zero, writes);
                collect_writes(body, writes, has_zero);
            }
            Stmt::Switch { selector, cases, default, .. } => {
                note(selector, has_zero, writes);
                for c in cases {
                    if let Some(g) = &c.guard {
                        note(g, has_zero, writes);
                    }
                    for x in &c.body {
                        collect_writes(x, writes, has_zero);
                    }
                }
                if let Some(d) = default {
                    collect_writes(d, writes, has_zero);
                }
            }
            Stmt::Try { body, catches, finally } => {
                collect_writes(body, writes, has_zero);
                for c in catches {
                    writes.entry(c.var).or_default().push(WK::Other);
                    collect_writes(&c.body, writes, has_zero);
                }
                if let Some(f) = finally {
                    collect_writes(f, writes, has_zero);
                }
            }
            Stmt::TryWithResources { resources, body, catches, finally, .. } => {
                for r in resources {
                    collect_writes(r, writes, has_zero);
                }
                collect_writes(body, writes, has_zero);
                for c in catches {
                    writes.entry(c.var).or_default().push(WK::Other);
                    collect_writes(&c.body, writes, has_zero);
                }
                if let Some(f) = finally {
                    collect_writes(f, writes, has_zero);
                }
            }
            Stmt::Assert { cond, msg } => {
                note(cond, has_zero, writes);
                if let Some(m) = msg {
                    note(m, has_zero, writes);
                }
            }
            Stmt::Synchronized { lock, body } => {
                note(lock, has_zero, writes);
                collect_writes(body, writes, has_zero);
            }
            Stmt::Labeled { body, .. } => collect_writes(body, writes, has_zero),
            _ => {}
        }
    }

    let mut writes: jdc_core::FxHashMap<u32, Vec<WK>> =
        jdc_core::FxHashMap::default();
    let mut has_zero = false;
    collect_writes(body, &mut writes, &mut has_zero);

    // Write-proven eligibility fixpoint (copy chains: `v = u`).
    let mut eligible: HashSet<u32> = HashSet::default();
    let int_typed = |v: u32| {
        (v as usize) < vt.vars.len() && vt.vars[v as usize].ty.erased() == JavaType::Int
    };
    loop {
        let mut changed = false;
        for (&v, ws) in writes.iter() {
            if eligible.contains(&v) || ws.is_empty() || !int_typed(v) {
                continue;
            }
            if ws.iter().all(|k| match k {
                WK::Zero => true,
                WK::Copy(u) => *u != v && eligible.contains(u),
                WK::Other => false,
            }) {
                eligible.insert(v);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    if eligible.is_empty() && !has_zero {
        return; // nothing this pass can do
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Scan,
        Rewrite,
    }
    struct St<'a> {
        mode: Mode,
        vt: &'a VarTable,
        eligible: &'a HashSet<u32>,
        ref_read: HashSet<u32>,
        dead: HashSet<u32>,
        sentinels: HashSet<u32>,
        n_lit: usize,
        n_var: usize,
    }
    // A call-arg slot whose formal is exactly java/lang/Object keeps
    // its current rendering (see the fn doc — boxing compiles today,
    // null could newly ambiguize).
    fn arg_ctx(t: &JavaType) -> Option<JavaType> {
        match t {
            JavaType::Object(n) if n.as_ref() == "java/lang/Object" => None,
            t if is_ref_jt(t) => Some(t.clone()),
            _ => None,
        }
    }
    fn walk_expr(e: &mut Expr, ctx: Option<JavaType>, st: &mut St) {
        walk_expr_c(e, ctx, false, st);
    }
    /// `cast_arg`: the slot is a call/ctor ARGUMENT — a rewritten null
    /// gets an explicit formal-type cast. Bare `f(null)` re-triggers
    /// overload ambiguity where `f(0)` used to pick an int overload
    /// (`in(null)` "引用不明确", `ContentValues.put(String,null)` vs its
    /// 9 overloads); `(String) null` is unambiguous and jadx-shaped.
    fn walk_expr_c(e: &mut Expr, ctx: Option<JavaType>, cast_arg: bool, st: &mut St) {
        let slot_ref = ctx.as_ref().is_some_and(is_ref_jt);
        fn mk_null(ctx: &Option<JavaType>, cast_arg: bool) -> Expr {
            match (cast_arg, ctx) {
                (true, Some(t)) => Expr::Cast {
                    ty: TypeRef::J(t.clone()),
                    e: Box::new(Expr::Const(ConstVal::Null)),
                },
                _ => Expr::Const(ConstVal::Null),
            }
        }
        match e {
            Expr::Const(ConstVal::Int(0)) if slot_ref => {
                if st.mode == Mode::Rewrite {
                    *e = mk_null(&ctx, cast_arg);
                    st.n_lit += 1;
                }
                return;
            }
            Expr::Local { var, .. } => {
                if st.eligible.contains(var) {
                    if slot_ref {
                        st.ref_read.insert(*var);
                    } else {
                        st.dead.insert(*var);
                    }
                }
                if st.mode == Mode::Rewrite && slot_ref && st.sentinels.contains(var) {
                    *e = mk_null(&ctx, cast_arg);
                    st.n_var += 1;
                }
                return;
            }
            _ => {}
        }
        match e {
            Expr::Method { owner, desc, args, .. } => {
                if let Some(o) = owner {
                    walk_expr(o, None, st);
                }
                for (i, a) in args.iter_mut().enumerate() {
                    let c = desc.args.get(i).and_then(arg_ctx);
                    walk_expr_c(a, c, true, st);
                }
            }
            Expr::Invokedynamic { desc, args, .. } => {
                for (i, a) in args.iter_mut().enumerate() {
                    let c = desc.args.get(i).and_then(arg_ctx);
                    walk_expr_c(a, c, true, st);
                }
            }
            Expr::New { args, arg_tys, .. } => {
                for (i, a) in args.iter_mut().enumerate() {
                    let c = arg_tys.get(i).and_then(arg_ctx);
                    walk_expr_c(a, c, true, st);
                }
            }
            Expr::AnonNew { args, .. } => {
                for a in args.iter_mut() {
                    walk_expr(a, None, st);
                }
            }
            Expr::Assign { target, value, .. } => {
                // Sentinel-to-sentinel copy: the whole store is dead;
                // its RHS is by the write gate a const-0 or a bare
                // local, so no nested reads can hide in it.
                if let Expr::Local { var: u, .. } = &**target {
                    if st.eligible.contains(u) {
                        return;
                    }
                }
                let vctx = match &**target {
                    Expr::Field { ty, .. } => {
                        let t = ty.erased();
                        is_ref_jt(&t).then_some(t)
                    }
                    Expr::ArrayIndex { array, .. } => match array.type_ref().erased() {
                        JavaType::Array(inner) if is_ref_jt(&inner) => Some(*inner),
                        _ => None,
                    },
                    _ => None,
                };
                walk_expr(target, None, st);
                walk_expr(value, vctx, st);
            }
            Expr::Cast { ty, e: inner } => {
                let t = ty.erased();
                let c = is_ref_jt(&t).then_some(t);
                walk_expr(inner, c, st);
            }
            Expr::InstanceOf { e: inner, .. } => {
                walk_expr(inner, Some(JavaType::Object("java/lang/Object".into())), st);
            }
            Expr::Cond { c, t, f } => {
                walk_expr(c, None, st);
                let tc = ctx.clone();
                walk_expr(t, tc, st);
                walk_expr(f, ctx, st);
            }
            Expr::NewArray { elem, dims, init, .. } => {
                for d in dims.iter_mut() {
                    walk_expr(d, None, st);
                }
                if let Some(list) = init {
                    let et = elem.erased();
                    let c = is_ref_jt(&et).then_some(et);
                    for slot in list.iter_mut() {
                        walk_expr(slot, c.clone(), st);
                    }
                }
            }
            Expr::NewMultiArray { dims, .. } => {
                for d in dims.iter_mut() {
                    walk_expr(d, None, st);
                }
            }
            Expr::ArrayIndex { array, index } => {
                walk_expr(array, None, st);
                walk_expr(index, None, st);
            }
            Expr::Field { owner, .. } => {
                if let Some(o) = owner {
                    walk_expr(o, None, st);
                }
            }
            Expr::Bin { l, r, .. } => {
                walk_expr(l, None, st);
                walk_expr(r, None, st);
            }
            Expr::Un { e: inner, .. } => walk_expr(inner, None, st),
            Expr::PreIncDec { e: inner, .. } | Expr::PostIncDec { e: inner, .. } => {
                walk_expr(inner, None, st);
            }
            Expr::StringConcat(parts) => {
                for p in parts.iter_mut() {
                    if let ConcatPart::Str(x) = p {
                        walk_expr(x, None, st);
                    }
                }
            }
            Expr::Lambda(l) => {
                for c in l.captures.iter_mut() {
                    walk_expr(c, None, st);
                }
            }
            _ => {}
        }
    }
    fn walk_stmt(s: &mut Stmt, st: &mut St, ret: &JavaType) {
        match s {
            Stmt::Block(v) => {
                for x in v.iter_mut() {
                    walk_stmt(x, st, ret);
                }
            }
            Stmt::ExprStmt(e) => walk_expr(e, None, st),
            Stmt::LocalDef { var, init, .. } => {
                if let Some(e) = init {
                    let ctx = if (*var as usize) < st.vt.vars.len() {
                        let t = st.vt.vars[*var as usize].ty.erased();
                        is_ref_jt(&t).then_some(t)
                    } else {
                        None
                    };
                    walk_expr(e, ctx, st);
                }
            }
            Stmt::Return(e) => {
                if let Some(x) = e {
                    let c = is_ref_jt(ret).then(|| ret.clone());
                    walk_expr(x, c, st);
                }
            }
            Stmt::Throw(e) => {
                walk_expr(e, Some(JavaType::Object("java/lang/Throwable".into())), st);
            }
            Stmt::If { cond, then_stmt, else_stmt } => {
                walk_expr(cond, None, st);
                walk_stmt(then_stmt, st, ret);
                if let Some(b) = else_stmt {
                    walk_stmt(b, st, ret);
                }
            }
            Stmt::While { cond, body } => {
                walk_expr(cond, None, st);
                walk_stmt(body, st, ret);
            }
            Stmt::DoWhile { body, cond } => {
                walk_stmt(body, st, ret);
                walk_expr(cond, None, st);
            }
            Stmt::For { init, cond, update, body } => {
                for x in init.iter_mut() {
                    walk_stmt(x, st, ret);
                }
                if let Some(c) = cond {
                    walk_expr(c, None, st);
                }
                for u in update.iter_mut() {
                    walk_expr(u, None, st);
                }
                walk_stmt(body, st, ret);
            }
            Stmt::ForEach { iterable, body, .. } => {
                walk_expr(iterable, None, st);
                walk_stmt(body, st, ret);
            }
            Stmt::Switch { selector, cases, default, .. } => {
                walk_expr(selector, None, st);
                for c in cases.iter_mut() {
                    if let Some(g) = &mut c.guard {
                        walk_expr(g, None, st);
                    }
                    for x in c.body.iter_mut() {
                        walk_stmt(x, st, ret);
                    }
                }
                if let Some(d) = default {
                    walk_stmt(d, st, ret);
                }
            }
            Stmt::Try { body, catches, finally } => {
                walk_stmt(body, st, ret);
                for c in catches.iter_mut() {
                    walk_stmt(&mut c.body, st, ret);
                }
                if let Some(f) = finally {
                    walk_stmt(f, st, ret);
                }
            }
            Stmt::TryWithResources { resources, body, catches, finally, .. } => {
                for r in resources.iter_mut() {
                    walk_stmt(r, st, ret);
                }
                walk_stmt(body, st, ret);
                for c in catches.iter_mut() {
                    walk_stmt(&mut c.body, st, ret);
                }
                if let Some(f) = finally {
                    walk_stmt(f, st, ret);
                }
            }
            Stmt::Assert { cond, msg } => {
                walk_expr(cond, None, st);
                if let Some(m) = msg {
                    walk_expr(m, None, st);
                }
            }
            Stmt::Synchronized { lock, body } => {
                walk_expr(lock, Some(JavaType::Object("java/lang/Object".into())), st);
                walk_stmt(body, st, ret);
            }
            Stmt::TernaryValue { e } => walk_expr(e, None, st),
            Stmt::MonitorEnter(e) | Stmt::MonitorExit(e) => {
                walk_expr(e, Some(JavaType::Object("java/lang/Object".into())), st);
            }
            Stmt::Labeled { body, .. } => walk_stmt(body, st, ret),
            _ => {}
        }
    }

    let mut st = St {
        mode: Mode::Scan,
        vt,
        eligible: &eligible,
        ref_read: HashSet::default(),
        dead: HashSet::default(),
        sentinels: HashSet::default(),
        n_lit: 0,
        n_var: 0,
    };
    if !eligible.is_empty() {
        walk_stmt(body, &mut st, ret);
        st.sentinels = eligible
            .iter()
            .copied()
            .filter(|v| st.ref_read.contains(v) && !st.dead.contains(v))
            .collect();
    }
    if st.sentinels.is_empty() && !has_zero {
        return;
    }
    st.mode = Mode::Rewrite;
    walk_stmt(body, &mut st, ret);
    if std::env::var_os("DDC_SENT").is_some() {
        eprintln!(
            "[sent] elig={} confirmed={} lit={} var={}",
            eligible.len(),
            st.sentinels.len(),
            st.n_lit,
            st.n_var
        );
    }
}

/// Int-context operand bridge: a BOOLEAN-typed side of an int-kind Bin
/// (bitwise, arithmetic, comparison against a numeric) becomes
/// `(b ? 1 : 0)` — the dex-level truth (booleans ARE 0/1 ints there;
/// register reuse puts a bool-typed var into an int expression:
/// `v5x | 4`, `b == 0` — "boolean无法转换为int" / "二元运算符操作数
/// 类型错误" families). Runs AFTER fix_bool_xor so boolean-SINK chains
/// were already converted to all-boolean form and are skipped here.
pub fn fix_int_operand_bridges(body: &mut Stmt, vt: &VarTable) {
    fn side_bool(e: &Expr, vt: &VarTable) -> bool {
        match e {
            Expr::Local { var, .. } => {
                matches!(vt.var(*var).ty.erased(), JavaType::Boolean)
            }
            // An all-boolean bitwise bin IS boolean even when its
            // embedded ty is still the frozen Int of the or-int lift
            // (booleanize retypes the vt; mixed_rewrite sets bin ty
            // only on the nodes it rewrites). The flat lookup called
            // `(ci13|ci14)` an int side, so the sibling `v408 != 0`
            // got `? 1 : 0`-wrapped while the bool bin stayed bare —
            // rendering int | boolean inside the wrap (news compose
            // CoreTextFieldKt `((v408!=0 ?1:0) | (ci13|ci14) ?1:0)|v412`
            // ×349 residual). Mirrors fix_bool_xor's bty recursion.
            Expr::Bin {
                op: BinOp::And | BinOp::Or | BinOp::Xor,
                l,
                r,
                ..
            } => side_bool(l, vt) && side_bool(r, vt),
            other => matches!(other.type_ref().erased(), JavaType::Boolean),
        }
    }
    fn side_int(e: &Expr, vt: &VarTable) -> bool {
        // A boolean-composed bin keeps its frozen Int embedded ty —
        // without this guard it double-answers as the "int side" of
        // its own bridge arm and steers the wrap onto the wrong operand.
        if side_bool(e, vt) {
            return false;
        }
        matches!(
            match e {
                Expr::Local { var, .. } => vt.var(*var).ty.erased(),
                other => other.type_ref().erased(),
            },
            JavaType::Int | JavaType::Short | JavaType::Byte | JavaType::Char | JavaType::Long
        )
    }
    fn wrap(e: Box<Expr>) -> Box<Expr> {
        Box::new(Expr::Cond {
            c: e,
            t: Box::new(Expr::Const(ConstVal::Int(1))),
            f: Box::new(Expr::Const(ConstVal::Int(0))),
        })
    }
    walk_stmt_exprs(body, &mut |e| {
        deep_rewrite(e, &mut |x| {
            // Field / array-store positions: the target's type is
            // descriptor-backed (authoritative); a boolean value into
            // an int slot bridges `(b ? 1 : 0)` (with a narrowing cast
            // for byte/short/char slots), an int value into a boolean
            // slot bridges `v != 0` (weixin `this.L[v3x] = max3`,
            // `g2x.d = p1x` — 785 field + ~700 array lines).
            if let Expr::Assign { target, value, op: AssignOp::Plain } = x {
                let tgt_ty: Option<JavaType> = match &**target {
                    Expr::Field { ty, .. } => Some(ty.erased()),
                    Expr::ArrayIndex { array, .. } => {
                        match array.type_ref().erased() {
                            JavaType::Array(el) => Some(el.as_ref().clone()),
                            _ => None,
                        }
                    }
                    // Local targets: the split/booleanize machinery owns
                    // the mixed-kind register cases; the residue that
                    // reached render (boolean无法转换为byte/long, ~360
                    // lines) gets the same value-side bridge.
                    Expr::Local { var, .. } => {
                        if (*var as usize) < vt.vars.len() {
                            Some(vt.var(*var).ty.erased())
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                if let Some(t) = tgt_ty {
                    let tgt_int = matches!(
                        t,
                        JavaType::Int
                            | JavaType::Short
                            | JavaType::Byte
                            | JavaType::Char
                            | JavaType::Long
                    );
                    if tgt_int && side_bool(value, vt) && !side_int(value, vt) {
                        let taken = std::mem::replace(
                            value,
                            Box::new(Expr::Const(ConstVal::Null)),
                        );
                        let mut wrapped = wrap(taken);
                        if !matches!(t, JavaType::Int | JavaType::Long) {
                            wrapped = Box::new(Expr::Cast {
                                ty: TypeRef::J(t.clone()),
                                e: wrapped,
                            });
                        }
                        *value = wrapped;
                    } else if matches!(t, JavaType::Boolean)
                        && side_int(value, vt)
                        && !side_bool(value, vt)
                    {
                        let taken = std::mem::replace(
                            value,
                            Box::new(Expr::Const(ConstVal::Null)),
                        );
                        **value = Expr::Bin {
                            op: BinOp::Ne,
                            l: taken,
                            r: Box::new(Expr::Const(ConstVal::Int(0))),
                            ty: Some(TypeRef::J(JavaType::Boolean)),
                        };
                    }
                }
            }
            // Explicit casts across the bool/num line are invalid Java
            // in BOTH directions: `(byte) this.r` (dex int-to-byte over
            // a boolean-rendered field — weixin AccInfo writeByte ×207)
            // becomes `(byte)(r ? 1 : 0)`, and `(boolean) x` over an
            // int becomes `x != 0`.
            if let Expr::Cast { ty, e } = x {
                let t = ty.erased();
                let t_num = matches!(
                    t,
                    JavaType::Int
                        | JavaType::Short
                        | JavaType::Byte
                        | JavaType::Char
                        | JavaType::Long
                        | JavaType::Float
                        | JavaType::Double
                );
                if t_num && side_bool(e, vt) && !side_int(e, vt) {
                    let taken = std::mem::replace(e, Box::new(Expr::Const(ConstVal::Null)));
                    **e = Expr::Cond {
                        c: taken,
                        t: Box::new(Expr::Const(ConstVal::Int(1))),
                        f: Box::new(Expr::Const(ConstVal::Int(0))),
                    };
                } else if matches!(t, JavaType::Boolean)
                    && side_int(e, vt)
                    && !side_bool(e, vt)
                {
                    let taken = std::mem::replace(e, Box::new(Expr::Const(ConstVal::Null)));
                    *x = Expr::Bin {
                        op: BinOp::Ne,
                        l: taken,
                        r: Box::new(Expr::Const(ConstVal::Int(0))),
                        ty: Some(TypeRef::J(JavaType::Boolean)),
                    };
                }
            }
            // Null-vs-int comparison: dex if-eqz/if-nez is type-agnostic
            // and null IS zero — when the local came out int-typed (phi
            // confluence residue), rewrite the Null constant to 0
            // (`v95_g12 != null` → `v95_g12 != 0`, faithful and legal;
            // yq0/g1 546-line 二元运算符 family). BOOLEAN side: the same
            // zero-register compare reads as the truth test — `b != null`
            // IS `b`, `b == null` IS `!b` (alipay v87_g7_g2 != null,
            // 二元运算符 '!=' boolean vs <空值> ×43 / news ×30).
            if let Expr::Bin { op, l, r, .. } = x {
                if matches!(op, BinOp::Eq | BinOp::Ne) {
                    if matches!(&**r, Expr::Const(ConstVal::Null))
                        && side_int(l, vt)
                        && !side_bool(l, vt)
                    {
                        **r = Expr::Const(ConstVal::Int(0));
                    } else if matches!(&**l, Expr::Const(ConstVal::Null))
                        && side_int(r, vt)
                        && !side_bool(r, vt)
                    {
                        **l = Expr::Const(ConstVal::Int(0));
                    } else if matches!(&**r, Expr::Const(ConstVal::Null))
                        && side_bool(l, vt)
                    {
                        let taken =
                            std::mem::replace(l, Box::new(Expr::Const(ConstVal::Null)));
                        *x = if matches!(op, BinOp::Eq) {
                            Expr::Un { op: UnOp::Not, e: taken }
                        } else {
                            *taken
                        };
                    } else if matches!(&**l, Expr::Const(ConstVal::Null))
                        && side_bool(r, vt)
                    {
                        let taken =
                            std::mem::replace(r, Box::new(Expr::Const(ConstVal::Null)));
                        *x = if matches!(op, BinOp::Eq) {
                            Expr::Un { op: UnOp::Not, e: taken }
                        } else {
                            *taken
                        };
                    }
                }
            }
            // A boolean-typed array INDEX is a reused register holding
            // an int (`this.L[v3x]` — boolean无法转换为int at the index
            // position): bridge it in reads and writes alike.
            if let Expr::ArrayIndex { index, .. } = x {
                if side_bool(index, vt) && !side_int(index, vt) {
                    let taken =
                        std::mem::replace(index, Box::new(Expr::Const(ConstVal::Null)));
                    *index = wrap(taken);
                }
            }
            // A boolean-typed array DIMENSION is the same reused-register
            // case at the allocation site (`new Object[v200]` where v200
            // is boolean-typed — weixin AppBrandRuntime, bool→int family):
            // dex new-array reads the size register as int and booleans
            // ARE 0/1 ints there, so bridge `(b ? 1 : 0)`.
            if let Expr::NewArray { dims, .. } = x {
                for d in dims.iter_mut() {
                    if side_bool(d, vt) && !side_int(d, vt) {
                        let taken =
                            std::mem::replace(d, Expr::Const(ConstVal::Null));
                        *d = *wrap(Box::new(taken));
                    }
                }
            }
            if let Expr::Bin { op, l, r, .. } = x {
                let int_kind = matches!(
                    op,
                    BinOp::Or
                        | BinOp::And
                        | BinOp::Xor
                        | BinOp::Add
                        | BinOp::Sub
                        | BinOp::Mul
                        | BinOp::Div
                        | BinOp::Rem
                        | BinOp::Shl
                        | BinOp::Shr
                        | BinOp::Ushr
                        | BinOp::Eq
                        | BinOp::Ne
                        | BinOp::Lt
                        | BinOp::Ge
                        | BinOp::Gt
                        | BinOp::Le
                );
                if !int_kind {
                    return;
                }
                // A Null CONSTANT operand of an int-kind bitwise/arith
                // bin is the dex zero register (or-int reads 0 out of a
                // register whose value view was null — `b | null`
                // renders boolean | <空值>, alipay ×43 / news ×30).
                // Rewriting to Int(0) is register-faithful and lets the
                // bool-side wraps below take over (`(b ? 1 : 0) | 0`).
                // Only when the SIBLING is a bool/int side: a
                // reference-typed sibling keeps the null shape (that is
                // SSA-merge residue, not a zero-register read), and a
                // String sibling keeps `+ null` concatenation ("null" —
                // valid and faithful; side_str excludes it via the
                // side_bool/side_int guard).
                if matches!(
                    op,
                    BinOp::Or
                        | BinOp::And
                        | BinOp::Xor
                        | BinOp::Add
                        | BinOp::Sub
                        | BinOp::Mul
                        | BinOp::Div
                        | BinOp::Rem
                        | BinOp::Shl
                        | BinOp::Shr
                        | BinOp::Ushr
                ) {
                    if matches!(&**r, Expr::Const(ConstVal::Null))
                        && (side_bool(l, vt) || side_int(l, vt))
                    {
                        **r = Expr::Const(ConstVal::Int(0));
                    } else if matches!(&**l, Expr::Const(ConstVal::Null))
                        && (side_bool(r, vt) || side_int(r, vt))
                    {
                        **l = Expr::Const(ConstVal::Int(0));
                    }
                }
                // PURE arithmetic/shift and ORDERING operands never take
                // a boolean side in ANY combination (`boolean + boolean`
                // is as invalid as `boolean + int` — reused 0/1 registers
                // in `p4x + v20 + (v21?1:0)`, uuyc u0/u02; `boolean <
                // boolean` likewise) — bridge each bool side on its own.
                // String concatenation is exempt: `"s" + b` is valid Java
                // and wrapping would print 1/0 instead of true/false.
                // Equality stays on the mixed-side rule below (a
                // bool == bool compare IS valid).
                let arith = matches!(
                    op,
                    BinOp::Add
                        | BinOp::Sub
                        | BinOp::Mul
                        | BinOp::Div
                        | BinOp::Rem
                        | BinOp::Shl
                        | BinOp::Shr
                        | BinOp::Ushr
                );
                let ordered = matches!(op, BinOp::Lt | BinOp::Ge | BinOp::Gt | BinOp::Le);
                if arith || ordered {
                    let side_str = |e: &Expr| {
                        matches!(
                            match e {
                                Expr::Local { var, .. } => vt.var(*var).ty.erased(),
                                other => other.type_ref().erased(),
                            },
                            JavaType::Object(ref s) if s.as_ref() == "java/lang/String"
                        )
                    };
                    let concat = arith && (side_str(l) || side_str(r));
                    if !concat {
                        if side_bool(l, vt) {
                            let taken =
                                std::mem::replace(l, Box::new(Expr::Const(ConstVal::Null)));
                            *l = wrap(taken);
                        }
                        if side_bool(r, vt) {
                            let taken =
                                std::mem::replace(r, Box::new(Expr::Const(ConstVal::Null)));
                            *r = wrap(taken);
                        }
                    }
                    return;
                }
                // Comparisons only bridge against a NUMERIC constant or
                // int-typed side (a bool == bool compare is valid Java).
                if side_bool(l, vt) && side_int(r, vt) && !side_bool(r, vt) {
                    let taken = std::mem::replace(l, Box::new(Expr::Const(ConstVal::Null)));
                    *l = wrap(taken);
                } else if side_bool(r, vt) && side_int(l, vt) && !side_bool(l, vt) {
                    let taken = std::mem::replace(r, Box::new(Expr::Const(ConstVal::Null)));
                    *r = wrap(taken);
                }
            }
        });
    });
}

/// Statement-level idiom restoration (readability parity with
/// jadx/DAD, flagged by the androguard comparison): `v = v + 1` →
/// `v++`, `v = v op x` → `v op= x`. Local targets only — a compound
/// field/array target would change how often the receiver expression
/// evaluates. Var must be the LEFT operand (non-commutative ops;
/// string-concat order). The ±1 fold requires a numeric vt type
/// (`s++` on a String is invalid while `s += 1` is not).
/// Boolean VALUE-diamond fold (DAD short_circuit value forms):
/// ```text
///   v = false; if (c) { v = true; }   →  v = c
///   v = true;  if (c) { v = false; }  →  v = !c
///   v = false; if (c) { v = E; }      →  v = c && E     (E bool-valued)
///   v = true;  if (c) { v = E; }      →  v = !c || E
///   v = D; if (c) { v = E1; } else { v = E2; }  →  v = c ? E1 : E2
/// ```
/// `v` must be Boolean-typed; the arm value bool-valued; the guard may
/// not touch `v`; base and arm constants must not coincide (a no-op
/// diamond could drop guard side effects). Kotlin default-arg bridge
/// ctors compute defaulted booleans in exactly this shape INSIDE the
/// branch ahead of the delegation (lark MmCreateAudioRequest's v21x
/// chain) — split_branch rejects the control flow as Dirty and the
/// this() call stays in the branch ("对this的调用必须是构造器中的第一
/// 个语句", lark ×456). Folded to a linear def (interleaved with
/// forward_single_use for the `[u = call; v = u]` arm shape) the
/// merge_at ternary fold applies. Source-faithful: the original was
/// `x?.m() ?: false` — one expression; the inlined call stays under
/// the guard arm, preserving evaluation order and short-circuiting.
/// General value-diamond fold (fold_bool_value_diamonds for ANY type):
/// `if (c) { v = a; } else { v = b; }` → `v = c ? a : b;`, plus the
/// split-pair shape `v = b; if (c) { v = a; }` → the same ternary. The
/// Kotlin default-arg ctor's mask-guarded per-arg defaults arrive as
/// diamonds (DocFreshInfo v10x/v14x); folding gives each carrier a
/// SINGLE-assignment def so the chain-aware delegation inline can hoist
/// the this() first ("对this的调用必须是构造器中的第一个语句"). Fold
/// only when both sides are exactly one plain assignment to the SAME
/// local and the value types are ternary-compatible (equal erased or
/// both numeric — mixed-category ternaries box/surprise).
pub fn fold_value_diamonds(body: &mut Stmt, vt: &VarTable) {
    let Stmt::Block(stmts) = body else { return };
    fold_diamonds_in(stmts, vt);
}

fn diamond_types_ok(a: &Expr, b: &Expr) -> bool {
    fn num(t: &JavaType) -> bool {
        matches!(
            t,
            JavaType::Byte
                | JavaType::Short
                | JavaType::Int
                | JavaType::Char
                | JavaType::Long
                | JavaType::Float
                | JavaType::Double
        )
    }
    // A null branch is compatible with any REFERENCE other branch: the
    // ternary types as lub(T, null) = T. Kotlin default-args ctors
    // render null defaults (`if ((mask&2)==0) v=x; else v=null;`) —
    // rejecting the pair left the diamond unfolded, the defs
    // un-inlineable, and the this(...) delegation unhoistable (weibo
    // ctor-not-first robust-guard/second-delegation ×818). A primitive
    // sibling stays rejected (`c ? 5 : null` is not Java).
    if let Some(other) = if matches!(a, Expr::Const(ConstVal::Null)) {
        Some(b)
    } else if matches!(b, Expr::Const(ConstVal::Null)) {
        Some(a)
    } else {
        None
    } {
        return !num(&other.type_ref().erased());
    }
    let ta = a.type_ref().erased();
    let tb = b.type_ref().erased();
    ta == tb || (num(&ta) && num(&tb))
}

/// `(var, target clone, value)` when the stmt is exactly ONE plain
/// assign to a local (bare or block-wrapped). The unwrap must be
/// RECURSIVE: the Kotlin default-args ctor diamonds arrive as
/// `Block([Block([Assign])])` (structurer nesting), and the one-level
/// peel missed them — the unfolded `if ((mask&2)==0) v=p; else v=0;`
/// prelude then blocked fix_ctor_super_first from hoisting the
/// `this(...)` delegation (weibo/lark ctor-not-first ×841 family).
fn single_local_assign(st: &Stmt) -> Option<(u32, Expr, Expr)> {
    let mut inner = st;
    while let Stmt::Block(v) = inner {
        if v.len() != 1 {
            return None;
        }
        inner = &v[0];
    }
    match inner {
        Stmt::ExprStmt(Expr::Assign { target, value, op: AssignOp::Plain }) => {
            match &**target {
                Expr::Local { var, .. } => Some((*var, *target.clone(), *value.clone())),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Like `single_local_assign`, but ALSO accepts an arm whose value is
/// produced by an arm-local def: `Block([LocalDef vX = init, v = vX])`
/// (possibly Block-wrapped) inlines `init` into the returned value when
/// `vX` is read exactly once there (single evaluation preserved). The
/// Kotlin mask-ctor long defaults render this shape (`else { long u =
/// Color.getUnspecified(); v37 = u; }` — compose TextStyle/SpanStyle);
/// without the inline the diamond stays unfolded and blocks the whole
/// delegation-inlining chain downstream.
fn arm_assign_inlined(st: &Stmt) -> Option<(u32, Expr, Expr)> {
    if let Some(r) = single_local_assign(st) {
        return Some(r);
    }
    let mut inner = st;
    while let Stmt::Block(v) = inner {
        if v.len() == 1 {
            inner = &v[0];
        } else {
            break;
        }
    }
    let Stmt::Block(v) = inner else { return None };
    if v.len() != 2 {
        return None;
    }
    let Stmt::LocalDef {
        var: x,
        init: Some(init),
        ..
    } = &v[0]
    else {
        return None;
    };
    let (var, tgt, val) = single_local_assign(&v[1])?;
    let mut count = 0usize;
    let mut c = val.clone();
    deep_rewrite(&mut c, &mut |e| {
        if let Expr::Local { var: lv, .. } = e {
            if lv == x {
                count += 1;
            }
        }
    });
    if count != 1 {
        return None;
    }
    let mut out = val.clone();
    deep_rewrite(&mut out, &mut |e| {
        if let Expr::Local { var: lv, .. } = e {
            if lv == x {
                *e = init.clone();
            }
        }
    });
    Some((var, tgt, out))
}

fn mk_ternary_assign(tgt: Expr, c: Expr, a: Expr, b: Expr) -> Stmt {
    Stmt::ExprStmt(Expr::Assign {
        target: Box::new(tgt),
        op: AssignOp::Plain,
        value: Box::new(Expr::Cond {
            c: Box::new(c),
            t: Box::new(a),
            f: Box::new(b),
        }),
    })
}

/// Two var ids that render as the SAME Java local: identical base name
/// (generation suffixes only appear on collisions, and same-base
/// generations of one dex register share the rendered name) and
/// identical erased type (a bool/num split pair must not merge).
fn same_render_var(v1: u32, v2: u32, vt: &VarTable) -> bool {
    let (a, b) = (vt.vars.get(v1 as usize), vt.vars.get(v2 as usize));
    match (a, b) {
        (Some(x), Some(y)) => x.name == y.name && x.ty.erased() == y.ty.erased(),
        _ => false,
    }
}

/// Structural equality of two statement lists MODULO SSA generations:
/// each arm of a diamond writes its OWN generation of the rendered
/// locals, so strict `==` on the tails fails even when the rendered
/// code is identical. Canonicalize every local id to the first id seen
/// for its render key (name + erased type) under a SHARED map, then
/// compare structurally.
fn canon_stmts_eq(a: &[Stmt], b: &[Stmt], vt: &VarTable) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut map: std::collections::HashMap<String, u32> = std::collections::HashMap::default();
    let canon = |s: &Stmt, map: &mut std::collections::HashMap<String, u32>| -> Stmt {
        let mut c = s.clone();
        walk_stmt_exprs(&mut c, &mut |e| {
            deep_rewrite(e, &mut |x| {
                if let Expr::Local { var, .. } = x {
                    let key = match vt.vars.get(*var as usize) {
                        Some(vi) => format!("{}#{:?}", vi.name, vi.ty.erased()),
                        None => format!("v{}", var),
                    };
                    let id = *map.entry(key).or_insert(*var);
                    *x = Expr::Local {
                        var: id,
                        ty: vt.var(id).ty.clone(),
                    };
                }
            });
        });
        c
    };
    a.iter()
        .zip(b.iter())
        .all(|(x, y)| canon(x, &mut map) == canon(y, &mut map))
}

fn fold_diamonds_in(stmts: &mut Vec<Stmt>, vt: &VarTable) {
    let mut i = 0usize;
    while i < stmts.len() {
        match &mut stmts[i] {
            Stmt::Block(v) => fold_diamonds_in(v, vt),
            Stmt::If { then_stmt, else_stmt, .. } => {
                if let Stmt::Block(v) = &mut **then_stmt {
                    fold_diamonds_in(v, vt);
                }
                if let Some(e) = else_stmt {
                    if let Stmt::Block(v) = &mut **e {
                        fold_diamonds_in(v, vt);
                    }
                }
            }
            _ => {}
        }
        // Shape 1: standalone if/else diamond.
        let shape1 = if let Stmt::If { cond, then_stmt, else_stmt: Some(el), .. } = &stmts[i] {
            match (arm_assign_inlined(then_stmt), arm_assign_inlined(el)) {
                (Some((v1, tgt, a)), Some((v2, _, b)))
                    if v1 == v2 && diamond_types_ok(&a, &b) =>
                {
                    Some(mk_ternary_assign(tgt, cond.clone(), a, b))
                }
                _ => None,
            }
        } else {
            None
        };
        if let Some(f) = shape1 {
            stmts[i] = f;
            i += 1;
            continue;
        }
        // Shape 3: identical-delegation diamond — BOTH arms assign the
        // same rendered local, then run the same tail headed by a ctor
        // delegation (Kotlin mask-ctor last slot: `if ((m&M)==0) { v =
        // x; this(.., v, ..); return; } else { v = null; this(.., v,
        // ..); return; }` — alipay compose TextStyle/SpanStyle family).
        // Fold to `v = c ? x : null;` + the shared tail: the delegation
        // becomes unconditional (the hotfix-guard strip downstream needs
        // that) and fold_default_arg_bridge can then inline the prelude
        // carriers into the args. Tails compare modulo SSA generation
        // (each arm reads its own generation of the one rendered local).
        // Gates: tail headed by a bare delegation, tails canonically
        // identical, the tail writes NEITHER generation, types ok.
        let shape3 = if let Stmt::If {
            cond,
            then_stmt,
            else_stmt: Some(el),
            ..
        } = &stmts[i]
        {
            let arm = |st: &Stmt| -> Option<((u32, Expr, Expr), Vec<Stmt>)> {
                let Stmt::Block(v) = st else { return None };
                if v.len() < 2 {
                    return None;
                }
                let first = single_local_assign(&v[0])?;
                Some((first, v[1..].to_vec()))
            };
            match (arm(then_stmt), arm(el)) {
                (Some(((v1, tgt1, a), mut rest_t)), Some(((v2, _t2, b), rest_e)))
                    if !rest_t.is_empty()
                        && is_bare_ctor_call(first_leaf_stmt(&rest_t[0]))
                        && stmts_count_writes(&rest_t, v1) == 0
                        && stmts_count_writes(&rest_t, v2) == 0
                        && diamond_types_ok(&a, &b) =>
                {
                    if v1 == v2 || same_render_var(v1, v2, vt) {
                        // same rendered local: fold the assign, keep the tail
                        if canon_stmts_eq(&rest_t, &rest_e, vt) {
                            Some((mk_ternary_assign(tgt1, cond.clone(), a, b), rest_t))
                        } else {
                            None
                        }
                    } else if stmts_read_var(&rest_t, v1) == 1
                        && stmts_read_var(&rest_e, v2) == 1
                        && stmts_read_var(&rest_t, v2) == 0
                        && stmts_read_var(&rest_e, v1) == 0
                    {
                        // SPLIT arm-local carriers (compose Span: then
                        // reads v23_g3, else reads v23 — different render
                        // names, one read each): unify by substituting
                        // v2 -> v1 in the else tail and requiring EXACT
                        // equality, then inline `c ? A1 : A2` at the
                        // single v1 read of the kept tail. No ternary
                        // assign needed — both carriers die in the fold.
                        let mut unified = rest_e.clone();
                        for st in unified.iter_mut() {
                            let mut c2 = st.clone();
                            walk_stmt_exprs(&mut c2, &mut |e| {
                                deep_rewrite(e, &mut |x| {
                                    if let Expr::Local { var, .. } = x {
                                        if *var == v2 {
                                            *x = Expr::Local {
                                                var: v1,
                                                ty: vt.var(v1).ty.clone(),
                                            };
                                        }
                                    }
                                });
                            });
                            *st = c2;
                        }
                        if unified != rest_t {
                            None
                        } else {
                            let tern = Expr::Cond {
                                c: Box::new(cond.clone()),
                                t: Box::new(a.clone()),
                                f: Box::new(b.clone()),
                            };
                            for st in rest_t.iter_mut() {
                                let mut c2 = st.clone();
                                walk_stmt_exprs(&mut c2, &mut |e| {
                                    deep_rewrite(e, &mut |x| {
                                        if let Expr::Local { var, .. } = x {
                                            if *var == v1 {
                                                *x = tern.clone();
                                            }
                                        }
                                    });
                                });
                                *st = c2;
                            }
                            // the If is REPLACED by the (rewritten) tail:
                            // no ternary assign survives — both carriers
                            // die in the fold.
                            let first = rest_t.remove(0);
                            Some((first, rest_t))
                        }
                    } else {
                        None
                    }
                }
                _ => None,
            }
        } else {
            None
        };
        if let Some((folded, rest)) = shape3 {
            stmts[i] = folded;
            stmts.splice(i + 1..i + 1, rest);
            i += 1;
            continue;
        }
        // Shape 2: adjacent `v = b;` + `if (c) { v = a; }` (no else).
        // The two assigns may target DIFFERENT SSA generations of the
        // same rendered local (sequential conditional overwrite — the
        // Kotlin mask-ctor `v11x = p3x; if ((mask&4)!=0) v11x = -1L;`
        // prelude, lark/weixin branched-defs ctor family): same base
        // name + same erased type renders as one Java local, so the
        // fold's else arm reads generation 1 and the assign targets
        // generation 2 — textually `v = c ? a : v`, exactly the
        // observable behavior of the unfolded pair.
        if i + 1 < stmts.len() {
            // An EMPTY else block counts as no-else: the structurer
            // emits Some(Block([])) where the source shape had none,
            // and cleanup (which would drop it) runs AFTER the fold —
            // the strict None match missed every real-world overwrite
            // (zero shape-2 folds on lark mask ctors).
            // Shape 2: adjacent `v = b;` + conditional overwrite with
            // NO other effect:
            //   form A: `if (c) { v = a; }` (else absent/empty)  -> v = c ? a : b
            //   form B: `if (c) { } else { v = a; }` (then empty) -> v = c ? b : a
            // Form B is the FOLD-time shape of every rendered no-else
            // overwrite: invert_empty_thens flips then/else AFTER this
            // pass, so the strict `else_stmt: None, overwrite-in-then`
            // match never saw a real-world instance (zero shape-2 folds
            // on lark mask ctors). The two assigns may target DIFFERENT
            // SSA generations of one rendered local (same base name +
            // same erased type): the ternary's keep-arm then reads
            // generation 1 — textually `v = c ? a : v`, exactly the
            // observable behavior of the unfolded pair (Kotlin mask-ctor
            // preludes: `v11x = p3x; if ((mask&4)!=0) v11x = -1L;`).
            let shape2 = {
                let s_i = single_local_assign(&stmts[i]);
                if let Stmt::If {
                    cond,
                    then_stmt,
                    else_stmt,
                    ..
                } = &stmts[i + 1]
                {
                    let then_a = single_local_assign(then_stmt);
                    let else_a = else_stmt.as_deref().and_then(single_local_assign);
                    let then_empty =
                        matches!(&**then_stmt, Stmt::Block(v) if v.is_empty());
                    let else_empty = else_stmt
                        .as_deref()
                        .map(|e| matches!(e, Stmt::Block(v) if v.is_empty()))
                        .unwrap_or(true);
                    let try_fold = |over: Option<(u32, Expr, Expr)>,
                                    keep_over: bool|
                     -> Option<Stmt> {
                        let (v2, tgt2, a) = over?;
                        let (v1, _t1, b) = s_i.as_ref()?;
                        if !(*v1 == v2 || same_render_var(*v1, v2, vt)) {
                            return None;
                        }
                        let keep = if *v1 == v2 {
                            b.clone()
                        } else {
                            Expr::Local {
                                var: *v1,
                                ty: vt.var(*v1).ty.clone(),
                            }
                        };
                        if !diamond_types_ok(&a, &keep) {
                            return None;
                        }
                        if keep_over {
                            Some(mk_ternary_assign(tgt2, cond.clone(), keep, a))
                        } else {
                            Some(mk_ternary_assign(tgt2, cond.clone(), a, keep))
                        }
                    };
                    if then_a.is_some() && else_empty {
                        try_fold(then_a, false)
                    } else if else_a.is_some() && then_empty {
                        try_fold(else_a, true)
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            // Leading-assign variant: the branch STARTS with the
            // overwrite and carries a tail (Kotlin mask-ctor last slot:
            // `str12 = null; if ((m&32)==0) { str12 = str6; this(..,
            // str12); return; }` — news_article CommonData/DTO family).
            // Fold the assign into the ternary and leave the tail in the
            // branch: the tail's reads see the identical value on both
            // paths (the ternary assigns before the branch runs). Gate:
            // the tail must not WRITE the var.
            let mut lead_fold: Option<(Stmt, Stmt)> = None;
            if shape2.is_none() && i + 1 < stmts.len() {
                let s_i2 = single_local_assign(&stmts[i]);
                let leading = |st: &Stmt| -> Option<((u32, Expr, Expr), Vec<Stmt>)> {
                    let Stmt::Block(v) = st else { return None };
                    if v.len() < 2 {
                        return None;
                    }
                    let first = single_local_assign(&v[0])?;
                    Some((first, v[1..].to_vec()))
                };
                if let (Some((v1, _t1, b)), Stmt::If { cond, then_stmt, else_stmt, .. }) =
                    (&s_i2, &stmts[i + 1])
                {
                    let else_absent = else_stmt.is_none()
                        || else_stmt
                            .as_deref()
                            .is_some_and(|e| matches!(e, Stmt::Block(v) if v.is_empty()));
                    let then_absent = matches!(&**then_stmt, Stmt::Block(v) if v.is_empty());
                    let try_lead = |over: Option<((u32, Expr, Expr), Vec<Stmt>)>,
                                    keep_over: bool|
                     -> Option<(Stmt, Stmt)> {
                        let ((v2, tgt2, a), rest) = over?;
                        if !(*v1 == v2 || same_render_var(*v1, v2, vt)) {
                            return None;
                        }
                        if stmts_count_writes(&rest, *v1) != 0
                            || stmts_count_writes(&rest, v2) != 0
                        {
                            return None;
                        }
                        let keep = if *v1 == v2 {
                            b.clone()
                        } else {
                            Expr::Local {
                                var: *v1,
                                ty: vt.var(*v1).ty.clone(),
                            }
                        };
                        if !diamond_types_ok(&a, &keep) {
                            return None;
                        }
                        let t = if keep_over {
                            mk_ternary_assign(tgt2, cond.clone(), keep, a)
                        } else {
                            mk_ternary_assign(tgt2, cond.clone(), a, keep)
                        };
                        let mut new_if = stmts[i + 1].clone();
                        if let Stmt::If { then_stmt, else_stmt, .. } = &mut new_if {
                            if keep_over {
                                *else_stmt = Some(Box::new(Stmt::Block(rest)));
                            } else {
                                **then_stmt = Stmt::Block(rest);
                            }
                        }
                        Some((t, new_if))
                    };
                    if else_absent {
                        lead_fold = try_lead(leading(then_stmt), false);
                    } else if then_absent {
                        if let Some(e) = else_stmt {
                            lead_fold = try_lead(leading(e), true);
                        }
                    }
                }
            }
            if let Some((folded, new_if)) = lead_fold {
                stmts[i] = folded;
                stmts[i + 1] = new_if;
                continue;
            }
            if let Some(f) = shape2 {
                stmts[i] = f;
                stmts.remove(i + 1);
                continue; // re-scan the folded position
            }
        }
        i += 1;
    }
}

pub fn fold_bool_value_diamonds(body: &mut Stmt, vt: &VarTable) {
    fn is_bool_var(v: u32, vt: &VarTable) -> bool {
        (v as usize) < vt.vars.len()
            && matches!(vt.vars[v as usize].ty.erased(), JavaType::Boolean)
    }
    fn bool_const(e: &Expr) -> Option<bool> {
        fn cint(e: &Expr) -> Option<i32> {
            match e {
                Expr::Const(ConstVal::Int(n)) => Some(*n),
                _ => None,
            }
        }
        match e {
            Expr::Const(ConstVal::Int(0)) => Some(false),
            Expr::Const(ConstVal::Int(1)) => Some(true),
            // int↔bool bridge wrappings of constants: `0 != 0` is the
            // fallback pipeline's rendered `false` by the time the
            // ctor-block fold runs.
            Expr::Bin { op: op @ (BinOp::Eq | BinOp::Ne), l, r, .. } => {
                let (a, b) = (cint(l)?, cint(r)?);
                let eq = a == b;
                Some(if matches!(op, BinOp::Eq) { eq } else { !eq })
            }
            _ => None,
        }
    }
    fn reads_var(e: &Expr, v: u32) -> bool {
        let mut hit = false;
        visit_exprs(e, &mut |x| {
            if let Expr::Local { var, .. } = x {
                if *var == v {
                    hit = true;
                }
            }
        });
        hit
    }
    /// Bool-valued expression for the composite arms (&&, ||, ?:).
    /// Bare 0/1 constants are handled by the caller's special cases —
    /// inside a composite they would render as invalid `c && 1`.
    fn bool_valued(e: &Expr, vt: &VarTable) -> bool {
        fn side(e: &Expr, vt: &VarTable) -> bool {
            bool_const(e).is_some() || bool_valued(e, vt)
        }
        match e {
            Expr::Local { var, .. } => is_bool_var(*var, vt),
            Expr::Method { desc, .. } => desc.ret == JavaType::Boolean,
            Expr::InstanceOf { .. } => true,
            Expr::Un { op: UnOp::Not, e: x, .. } => bool_valued(x, vt) || bool_const(x).is_some(),
            Expr::Bin { op, l, r, .. } => match op {
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Ge | BinOp::Gt | BinOp::Le => true,
                // A logical operand may itself be a 0/1 constant
                // (booleanize leaves mixed forms).
                BinOp::LogAnd | BinOp::LogOr => {
                    side(l, vt) && side(r, vt)
                }
                _ => false,
            },
            Expr::Cond { t, f, .. } => side(t, vt) && side(f, vt),
            _ => false,
        }
    }
    /// A branch that is exactly one plain `v = E` with E bool-valued
    /// (or a 0/1 constant) and not reading v — OR the forwarding PAIR
    /// `[u = E; v = u]` where u is single-assigned/single-read in the
    /// whole method: the fold consumes the pair into `v = E` (the impure
    /// `u = src.m()` arm shape forward_single_use cannot inline — its
    /// phi-ref gate rejects values reading multi-assigned prelude vars,
    /// and inlining here is scope-safe because E moves exactly one
    /// block level out with the arm's lone def consumed).
    fn lone_assign(
        b: &Stmt,
        v: u32,
        vt: &VarTable,
        an: &VarAnalysis,
        consumed: &mut HashSet<u32>,
    ) -> Option<Expr> {
        let l = flat_list(b);
        if l.len() == 2 {
            let (u, e): (u32, &Expr) = match l[0] {
                Stmt::LocalDef { var, init: Some(e), .. } => (*var, e),
                Stmt::ExprStmt(Expr::Assign { target, op: AssignOp::Plain, value }) => {
                    match &**target {
                        Expr::Local { var, .. } => (*var, &**value),
                        _ => return None,
                    }
                }
                _ => return None,
            };
            if u == v || consumed.contains(&u) {
                return None;
            }
            // Whole-method single-assign/single-read: the fold drops
            // u's def with the arm, so ANY other read would dangle.
            // (Analysis counts are per-round; folds only ever REMOVE
            // reads elsewhere or clone E's — whose variable reads were
            // already counted at their def sites — so stale counts
            // over-approximate and the ==1 gates stay sound.)
            if an.assigns.get(u as usize).copied().unwrap_or(0) != 1
                || an.reads.get(u as usize).copied().unwrap_or(0) != 1
            {
                return None;
            }
            let (tv, rv) = match l[1] {
                Stmt::ExprStmt(Expr::Assign { target, op: AssignOp::Plain, value }) => {
                    match (&**target, &**value) {
                        (Expr::Local { var: t, .. }, Expr::Local { var: r, .. }) => (*t, *r),
                        _ => return None,
                    }
                }
                Stmt::LocalDef { var, init: Some(Expr::Local { var: r, .. }), .. } => (*var, *r),
                _ => return None,
            };
            if tv != v || rv != u {
                return None;
            }
            if reads_var(e, v) || !(bool_valued(e, vt) || bool_const(e).is_some()) {
                return None;
            }
            consumed.insert(u);
            return Some(e.clone());
        }
        if l.len() != 1 {
            return None;
        }
        let (target, value) = match l[0] {
            Stmt::ExprStmt(Expr::Assign { target, op: AssignOp::Plain, value }) => (target, value),
            Stmt::LocalDef { var, init: Some(value), .. } if *var == v => {
                if reads_var(value, v) || !(bool_valued(value, vt) || bool_const(value).is_some()) {
                    return None;
                }
                return Some(value.clone());
            }
            _ => return None,
        };
        let Expr::Local { var: tv, .. } = &**target else { return None };
        if *tv != v || reads_var(value, v) {
            return None;
        }
        if !(bool_valued(value, vt) || bool_const(value).is_some()) {
            return None;
        }
        Some((**value).clone())
    }
    fn not(e: Expr) -> Expr {
        Expr::Un { op: UnOp::Not, e: Box::new(e) }
    }
    /// Peephole the int↔bool bridge artifacts the fallback pipeline
    /// leaves in guards: `!(x != y)` → `x == y`; `(b ? 1 : 0) == 1` →
    /// `b`; `!(b ? 1 : 0 == 1)` chains likewise. Purely a readability
    /// pass — the unsimplified forms compile.
    fn simplify_bool(e: Expr) -> Expr {
        match e {
            Expr::Un { op: UnOp::Not, e: inner, .. } => match *inner {
                // Flip first, THEN re-simplify: `!((u?1:0) != 1)` must
                // reach the cond01 arm as `(u?1:0) == 1` → `u` (the
                // flip result never re-entered simplification and the
                // pair arm saw a Bin where it needed the bare local).
                Expr::Bin { op: BinOp::Ne, l, r, ty } => {
                    simplify_bool(Expr::Bin { op: BinOp::Eq, l, r, ty })
                }
                Expr::Bin { op: BinOp::Eq, l, r, ty } => {
                    simplify_bool(Expr::Bin { op: BinOp::Ne, l, r, ty })
                }
                other => Expr::Un { op: UnOp::Not, e: Box::new(simplify_bool(other)) },
            },
            Expr::Bin { op: op @ (BinOp::Eq | BinOp::Ne), l, r, ty } => {
                // (c ? 1 : 0) <op> 1  →  c / !c ; <op> 0 → !c / c
                fn cond01(e: &Expr) -> Option<Expr> {
                    if let Expr::Cond { c, t, f } = e {
                        if matches!(&**t, Expr::Const(ConstVal::Int(1)))
                            && matches!(&**f, Expr::Const(ConstVal::Int(0)))
                        {
                            return Some((**c).clone());
                        }
                    }
                    None
                }
                let (lc, rc) = (cond01(&l), cond01(&r));
                match (lc, rc) {
                    (Some(c), None) => match (&*r, op) {
                        (Expr::Const(ConstVal::Int(1)), BinOp::Eq) => c,
                        (Expr::Const(ConstVal::Int(1)), BinOp::Ne) => not(c),
                        (Expr::Const(ConstVal::Int(0)), BinOp::Eq) => not(c),
                        (Expr::Const(ConstVal::Int(0)), BinOp::Ne) => c,
                        _ => Expr::Bin { op, l: Box::new(c), r, ty },
                    },
                    (None, Some(c)) => match (&*l, op) {
                        (Expr::Const(ConstVal::Int(1)), BinOp::Eq) => c,
                        (Expr::Const(ConstVal::Int(1)), BinOp::Ne) => not(c),
                        (Expr::Const(ConstVal::Int(0)), BinOp::Eq) => not(c),
                        (Expr::Const(ConstVal::Int(0)), BinOp::Ne) => c,
                        _ => Expr::Bin { op, l, r: Box::new(c), ty },
                    },
                    _ => Expr::Bin { op, l, r, ty },
                }
            }
            other => other,
        }
    }
    fn bin(op: BinOp, l: Expr, r: Expr) -> Expr {
        Expr::Bin { op, l: Box::new(l), r: Box::new(r), ty: None }
    }

    for _round in 0..4 {
        let mut changed = false;
        let mut analysis = VarAnalysis {
            assigns: Vec::new(),
            reads: Vec::new(),
            values: Vec::new(),
        };
        analyze_vars(body, &mut analysis);
        let mut consumed: HashSet<u32> = HashSet::default();
        walk_mut_deep(body, &mut |st| {
            let Stmt::Block(list) = st else { return };
            let mut i = 0usize;
            while i + 1 < list.len() {
                let folded = try_at(list, i, vt, &is_bool_var, &bool_const, &reads_var, &lone_assign, &analysis, &mut consumed);
                match folded {
                    Some(expr) => {
                        let is_def = matches!(&list[i], Stmt::LocalDef { .. });
                        let var = match &list[i] {
                            Stmt::LocalDef { var, .. } => *var,
                            Stmt::ExprStmt(Expr::Assign { target, .. }) => {
                                match &**target { Expr::Local { var, .. } => *var, _ => unreachable!() }
                            }
                            _ => unreachable!(),
                        };
                        let (is_final, force_type) = match &list[i] {
                            Stmt::LocalDef { is_final, force_type, .. } => (*is_final, *force_type),
                            _ => (false, false),
                        };
                        list[i] = if is_def {
                            Stmt::LocalDef { var, init: Some(expr), is_final, force_type }
                        } else {
                            Stmt::ExprStmt(Expr::Assign {
                                target: Box::new(Expr::Local {
                                    var,
                                    ty: TypeRef::J(JavaType::Boolean),
                                }),
                                op: AssignOp::Plain,
                                value: Box::new(expr),
                            })
                        };
                        list.remove(i + 1);
                        changed = true;
                        // stay at i: a preceding base may now pair with
                        // nothing new, but a FOLLOWING diamond may chain.
                    }
                    None => i += 1,
                }
            }
        });
        if !changed {
            break;
        }
    }

    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    fn try_at(
        list: &[Stmt],
        i: usize,
        vt: &VarTable,
        is_bool_var: &dyn Fn(u32, &VarTable) -> bool,
        bool_const: &dyn Fn(&Expr) -> Option<bool>,
        reads_var: &dyn Fn(&Expr, u32) -> bool,
        lone_assign: &dyn Fn(&Stmt, u32, &VarTable, &VarAnalysis, &mut HashSet<u32>) -> Option<Expr>,
        an: &VarAnalysis,
        consumed: &mut HashSet<u32>,
    ) -> Option<Expr> {
        // s1: `v = <0|1>` (Assign or LocalDef), v Boolean-typed.
        let (v, base) = match &list[i] {
            Stmt::ExprStmt(Expr::Assign { target, op: AssignOp::Plain, value }) => {
                match &**target {
                    Expr::Local { var, .. } => {
                        // The value may be a wrapped constant (`0 != 0`
                        // — the fallback pipeline's rendered false).
                        let b = bool_const(value)?;
                        if !is_bool_var(*var, vt) { return None; }
                        (*var, b)
                    }
                    _ => return None,
                }
            }
            Stmt::LocalDef { var, init: Some(e), .. } => {
                let b = bool_const(e)?;
                if !is_bool_var(*var, vt) { return None; }
                (*var, b)
            }
            _ => return None,
        };
        let Stmt::If { cond, then_stmt, else_stmt } = &list[i + 1] else {
            return None;
        };
        if reads_var(cond, v) {
            return None;
        }
        let mut touches_v = false;
        visit_exprs(cond, &mut |x| {
            if let Expr::Assign { target, .. } = x {
                if let Expr::Local { var, .. } = &**target {
                    if *var == v { touches_v = true; }
                }
            }
        });
        if touches_v {
            return None;
        }
        // The structurer emits inverted diamonds (`if (c) {} else { v = E }`,
        // empty-then) — normalize by negating the guard.
        let (cond_n, then_arm, else_arm): (Expr, &Stmt, Option<&Stmt>) =
            if flat_list(then_stmt).is_empty() && else_stmt.is_some() {
                (not(cond.clone()), else_stmt.as_ref().unwrap(), None)
            } else {
                (cond.clone(), then_stmt.as_ref(), else_stmt.as_deref())
            };
        let e1 = lone_assign(then_arm, v, vt, an, consumed)?;
        let e2 = match else_arm {
            None => None,
            Some(b) => Some(lone_assign(b, v, vt, an, consumed)?),
        };
        let e1c = bool_const(&e1);
        let c = simplify_bool(cond_n);
        Some(match e2 {
            None => {
                if !base {
                    match e1c {
                        Some(true) => c,
                        Some(false) => return None, // no-op diamond: guard effects would drop
                        None => bin(BinOp::LogAnd, c, e1),
                    }
                } else {
                    match e1c {
                        Some(false) => not(c),
                        Some(true) => return None,
                        None => bin(BinOp::LogOr, not(c), e1),
                    }
                }
            }
            Some(e2) => {
                // Full diamond: both arms assign — the base is dead.
                if e1 == e2 {
                    return None; // both arms equal: folding drops guard effects
                }
                Expr::Cond { c: Box::new(c), t: Box::new(e1), f: Box::new(e2) }
            }
        })
    }
}

pub fn idiom_compounds(body: &mut Stmt, vt: &VarTable) {
    walk_mut_deep(body, &mut |st| {
        let Stmt::ExprStmt(e) = st else { return };
        let Expr::Assign { target, op: AssignOp::Plain, value } = &mut *e else {
            return;
        };
        // Target: a local, or a bare field (static / this — no complex
        // receiver whose re-evaluation a compound would elide).
        enum Tgt {
            Loc(u32),
            Fld,
        }
        let tgt = match &**target {
            Expr::Local { var, .. } => Tgt::Loc(*var),
            Expr::Field { owner, .. }
                if owner.is_none()
                    || matches!(&**owner.as_ref().unwrap(), Expr::This) =>
            {
                Tgt::Fld
            }
            _ => return,
        };
        let Expr::Bin { op, l, r, .. } = &mut **value else {
            return;
        };
        // The left operand must be the SAME storage as the target.
        match (&tgt, &**l) {
            (Tgt::Loc(v), Expr::Local { var, .. }) if *var == *v => {}
            (Tgt::Fld, Expr::Field { cls: c2, name: n2, .. }) => {
                let Expr::Field { cls: c1, name: n1, .. } = &**target else {
                    return;
                };
                if c1 != c2 || n1 != n2 {
                    return;
                }
            }
            _ => return,
        }
        // ±1 constant on a numeric var → post-inc/dec.
        let one = match &**r {
            Expr::Const(ConstVal::Int(1)) => Some(1i64),
            Expr::Const(ConstVal::Int(-1)) => Some(-1),
            Expr::Const(ConstVal::Long(1)) => Some(1),
            Expr::Const(ConstVal::Long(-1)) => Some(-1),
            _ => None,
        };
        if *op == BinOp::Add || *op == BinOp::Sub {
            if let Some(mut d) = one {
                if *op == BinOp::Sub {
                    d = -d;
                }
                let tgt_ty = match &tgt {
                    Tgt::Loc(v) => vt.var(*v).ty.erased(),
                    Tgt::Fld => {
                        let Expr::Field { ty, .. } = &**target else {
                            return;
                        };
                        ty.erased()
                    }
                };
                let numeric = matches!(
                    tgt_ty,
                    JavaType::Int
                        | JavaType::Long
                        | JavaType::Short
                        | JavaType::Byte
                        | JavaType::Char
                        | JavaType::Float
                        | JavaType::Double
                );
                if numeric {
                    let wide = matches!(tgt_ty, JavaType::Long);
                    let inner = match &tgt {
                        Tgt::Loc(v) => Expr::Local {
                            var: *v,
                            ty: vt.var(*v).ty.clone(),
                        },
                        Tgt::Fld => (**target).clone(),
                    };
                    *e = Expr::PostIncDec {
                        e: Box::new(inner),
                        delta: d,
                        wide,
                    };
                    return;
                }
            }
        }
        let cop = match op {
            BinOp::Add => AssignOp::Add,
            BinOp::Sub => AssignOp::Sub,
            BinOp::Mul => AssignOp::Mul,
            BinOp::Div => AssignOp::Div,
            BinOp::Rem => AssignOp::Rem,
            BinOp::Shl => AssignOp::Shl,
            BinOp::Shr => AssignOp::Shr,
            BinOp::Ushr => AssignOp::Ushr,
            BinOp::And => AssignOp::And,
            BinOp::Or => AssignOp::Or,
            BinOp::Xor => AssignOp::Xor,
            _ => return,
        };
        let rhs = std::mem::replace(r, Box::new(Expr::Const(ConstVal::Null)));
        let new_target = match &tgt {
            Tgt::Loc(v) => Box::new(Expr::Local {
                var: *v,
                ty: vt.var(*v).ty.clone(),
            }),
            Tgt::Fld => target.clone(),
        };
        *e = Expr::Assign {
            target: new_target,
            op: cop,
            value: rhs,
        };
    });
}

pub fn fix_bool_xor(body: &mut Stmt, vt: &VarTable, ret_bool: bool) {
    fn is_one(e: &Expr) -> bool {
        matches!(e, Expr::Const(ConstVal::Int(1)))
    }
    fn bty(e: &Expr, vt: &VarTable) -> bool {
        match e {
            Expr::Local { var, .. } => {
                matches!(vt.var(*var).ty.erased(), JavaType::Boolean)
            }
            // A bitwise bin over all-boolean sides IS boolean: the
            // embedded bin ty stays frozen Int from the or-int lift
            // (booleanize retypes the vt, not bin nodes), so the flat
            // lookup saw `(b1|b2) | i3` as int-sided and skipped the
            // mixed rewrite — rendered `boolean | int` (uuyc i5/a_3
            // `(v37|v38|v39)==0` ×21; news ×995 二元运算符 '|').
            // Mirrors emit.rs bool_ish_vt's recursion so the rewrite
            // here and the logical-form render there agree.
            Expr::Bin {
                op: BinOp::And | BinOp::Or | BinOp::Xor,
                l,
                r,
                ..
            } => bty(l, vt) && bty(r, vt),
            other => matches!(other.type_ref().erased(), JavaType::Boolean),
        }
    }
    // Int-typed `x ^ 1` in a BOOLEAN sink is the compiler's `!x` over a
    // 0/1 slot: rewrite to `x == 0` (the booleanize return-wrap misses
    // Xor shapes — weibo ArraysKt `return v2 ^ 1;` against a boolean
    // return, "int无法转换为boolean").
    fn inv_of_xor(x: &Expr, vt: &VarTable) -> Option<Expr> {
        if let Expr::Bin { op: BinOp::Xor, l, r, .. } = x {
            let (int_side, one_side) = if is_one(r) {
                (l, r)
            } else if is_one(l) {
                (r, l)
            } else {
                return None;
            };
            let _ = one_side;
            if !bty(int_side, vt) {
                return Some(Expr::Bin {
                    op: BinOp::Eq,
                    l: int_side.clone(),
                    r: Box::new(Expr::Const(ConstVal::Int(0))),
                    ty: Some(TypeRef::J(JavaType::Boolean)),
                });
            }
        }
        None
    }
    let tgt_bool = |target: &Expr, vt: &VarTable| -> bool {
        match target {
            Expr::Local { var, .. } => matches!(vt.var(*var).ty.erased(), JavaType::Boolean),
            Expr::Field { ty, .. } => matches!(ty.erased(), JavaType::Boolean),
            _ => false,
        }
    };
    walk_mut_deep(body, &mut |st| match st {
        Stmt::Return(Some(e)) if ret_bool => {
            if let Some(r) = inv_of_xor(e, vt) {
                *e = r;
            }
        }
        Stmt::ExprStmt(Expr::Assign { target, value, op: AssignOp::Plain, .. }) => {
            if tgt_bool(target, vt) {
                if let Some(r) = inv_of_xor(value, vt) {
                    **value = r;
                }
            }
        }
        Stmt::LocalDef { var, init: Some(value), .. }
            if matches!(vt.var(*var).ty.erased(), JavaType::Boolean) =>
        {
            if let Some(r) = inv_of_xor(value, vt) {
                *value = r;
            }
        }
        Stmt::If { cond, .. } | Stmt::While { cond, .. } | Stmt::DoWhile { cond, .. } => {
            if let Some(r) = inv_of_xor(cond, vt) {
                *cond = r;
            }
        }
        _ => {}
    });
    fn ity(e: &Expr, vt: &VarTable) -> bool {
        matches!(
            match e {
                Expr::Local { var, .. } => vt.var(*var).ty.erased(),
                other => other.type_ref().erased(),
            },
            JavaType::Int | JavaType::Short | JavaType::Byte | JavaType::Char
        )
    }
    /// Mixed-kind bitwise rewrite: Kotlin's non-short-circuit
    /// `or`/`and` over 0/1 ints mixes generations once booleanize
    /// converts one side (`delete | delete2` — "boolean无法转换为int" at
    /// the OPERAND, weixin SQLiteDatabase ×3.3k), and `b ^ 1` is `!b`.
    /// The int side is a 0/1 encoding: compare it, keeping the chain
    /// boolean end to end. ONLY legal into a BOOLEAN sink — ungated, it
    /// rewrote int-sink `v6 = v5x | 4` into `v5x | 4 != 0` assigned to
    /// an int var (boolean无法转换为int, weixin yq5/c ×2.1k family).
    fn mixed_rewrite(e: &mut Expr, vt: &VarTable) {
        deep_rewrite(e, &mut |x| {
            if let Expr::Bin { op, l, r, ty } = x {
                match op {
                    BinOp::Xor => {
                        if is_one(r) && bty(l, vt) {
                            let taken =
                                std::mem::replace(l, Box::new(Expr::Const(ConstVal::Null)));
                            *x = Expr::Un { op: UnOp::Not, e: taken };
                        } else if is_one(l) && bty(r, vt) {
                            let taken =
                                std::mem::replace(r, Box::new(Expr::Const(ConstVal::Null)));
                            *x = Expr::Un { op: UnOp::Not, e: taken };
                        }
                    }
                    BinOp::Or | BinOp::And => {
                        if bty(l, vt) && ity(r, vt) {
                            let taken =
                                std::mem::replace(r, Box::new(Expr::Const(ConstVal::Null)));
                            **r = Expr::Bin {
                                op: BinOp::Ne,
                                l: taken,
                                r: Box::new(Expr::Const(ConstVal::Int(0))),
                                ty: Some(TypeRef::J(JavaType::Boolean)),
                            };
                            *ty = Some(TypeRef::J(JavaType::Boolean));
                        } else if ity(l, vt) && bty(r, vt) {
                            let taken =
                                std::mem::replace(l, Box::new(Expr::Const(ConstVal::Null)));
                            **l = Expr::Bin {
                                op: BinOp::Ne,
                                l: taken,
                                r: Box::new(Expr::Const(ConstVal::Int(0))),
                                ty: Some(TypeRef::J(JavaType::Boolean)),
                            };
                            *ty = Some(TypeRef::J(JavaType::Boolean));
                        }
                    }
                    _ => {}
                }
            }
        });
    }
    // Boolean sinks: bool-typed assign/def targets, boolean-method
    // returns, and loop/branch conditions.
    walk_mut_deep(body, &mut |st| match st {
        Stmt::Return(Some(e)) if ret_bool => mixed_rewrite(e, vt),
        Stmt::ExprStmt(Expr::Assign { target, value, op: AssignOp::Plain, .. }) => {
            if tgt_bool(target, vt) {
                mixed_rewrite(value, vt);
            }
        }
        Stmt::LocalDef { var, init: Some(e), .. }
            if matches!(vt.var(*var).ty.erased(), JavaType::Boolean) =>
        {
            mixed_rewrite(e, vt);
        }
        Stmt::If { cond, .. } | Stmt::While { cond, .. } | Stmt::DoWhile { cond, .. } => {
            mixed_rewrite(cond, vt);
        }
        _ => {}
    });
    // A conditional's test is a boolean sink wherever it sits (including
    // inside int-sink statements the walk above skips).
    walk_stmt_exprs(body, &mut |e| {
        deep_rewrite(e, &mut |x| {
            if let Expr::Cond { c, .. } = x {
                mixed_rewrite(c, vt);
            }
        });
    });
}

/// Boolean inference: vars only ever assigned 0/1/comparisons/booleans and
/// read in conditions become `boolean`, with `v != 0` → `v` in conditions.
/// Returns the number of vars converted (0 = nothing changed — the
/// caller skips the post-booleanize re-split, which only has work when
/// a conversion exposed a mixed-kind register).
pub fn booleanize(vt: &mut VarTable, body: &mut Stmt, ret_bool: bool) -> usize {
    // Booleans propagate through local chains (`v17 = v24` where v24
    // itself became boolean in the first round) — iterate to a fixpoint;
    // the common corpus converts nothing and exits after one round.
    // The deref disqualifier set is structural (positions, not types) —
    // compute ONCE per call, not per round (12 full walks on big
    // methods cost weibo +60% wall).
    let n0 = vt.vars.len();
    let mut derefed = vec![false; n0];
    mark_derefs(body, &mut derefed);
    let mut total = 0usize;
    for _ in 0..4 {
        let n = booleanize_round(vt, body, ret_bool, &derefed);
        if n == 0 {
            break;
        }
        total += n;
    }
    total
}

/// Mark vars used in DEREF positions (field/method receiver, array
/// base) — allocation-free recursion (visit_all_exprs allocates a
/// child Vec per statement; two callers run this per method).
fn mark_deref_expr(e: &Expr, derefed: &mut [bool]) {
    match e {
        Expr::Field { owner: Some(o), is_static: false, .. }
        | Expr::Method { owner: Some(o), .. } => {
            if let Expr::Local { var, .. } = &**o {
                if (*var as usize) < derefed.len() {
                    derefed[*var as usize] = true;
                }
            }
        }
        Expr::ArrayIndex { array, .. } => {
            if let Expr::Local { var, .. } = &**array {
                if (*var as usize) < derefed.len() {
                    derefed[*var as usize] = true;
                }
            }
        }
        _ => {}
    }
    for_each_child(e, &mut |c| mark_deref_expr(c, derefed));
}

fn mark_derefs(body: &Stmt, derefed: &mut [bool]) {
    walk_all(body, &mut |st| {
        match st {
            Stmt::ExprStmt(e) | Stmt::Throw(e) | Stmt::MonitorEnter(e) | Stmt::MonitorExit(e) => {
                mark_deref_expr(e, derefed);
            }
            Stmt::Return(Some(e)) => mark_deref_expr(e, derefed),
            Stmt::LocalDef { init: Some(e), .. } => mark_deref_expr(e, derefed),
            Stmt::If { cond, .. } | Stmt::While { cond, .. } | Stmt::DoWhile { cond, .. } => {
                mark_deref_expr(cond, derefed);
            }
            Stmt::For { init, .. } => {
                for x in init {
                    mark_derefs(x, derefed);
                }
            }
            Stmt::Switch { selector, .. } => mark_deref_expr(selector, derefed),
            Stmt::ForEach { iterable, .. } => mark_deref_expr(iterable, derefed),
            _ => {}
        }
    });
}

fn booleanize_round(
    vt: &mut VarTable,
    body: &mut Stmt,
    ret_bool: bool,
    derefed: &[bool],
) -> usize {
    let n = vt.vars.len();
    let mut in_cond = vec![false; n];

    // Condition uses: `v == 0` / `v != 0` if-conditions and comparison
    // operands inside assigned values.
    walk_all(body, &mut |st| {
        let cond: Option<&Expr> = match st {
            Stmt::If { cond, .. } | Stmt::While { cond, .. } | Stmt::DoWhile { cond, .. } => {
                Some(cond)
            }
            _ => None,
        };
        if let Some(c) = cond {
            if let Expr::Bin {
                op: BinOp::Eq | BinOp::Ne,
                l,
                r,
                ..
            } = c
            {
                for (x, other) in [(l, r), (r, l)] {
                    if let (Expr::Local { var, .. }, Expr::Const(ConstVal::Int(0))) =
                        (&**x, &**other)
                    {
                        if (*var as usize) < n {
                            in_cond[*var as usize] = true;
                        }
                    }
                }
            }
        }
        // A local RETURNED from a boolean method is boolean-typed even
        // though it never appears in a condition (`int v9 = 0/1 … return
        // v9;` — returning an int from `boolean check()` is a compile
        // error; lab package Obf.check).
        if ret_bool {
            if let Stmt::Return(Some(Expr::Local { var, .. })) = st {
                if (*var as usize) < n {
                    in_cond[*var as usize] = true;
                }
            }
        }
        // A local STORED into a boolean field is boolean-typed even though
        // it never appears in a condition (`b = v1` where `b` is a boolean
        // field, `v1` an int 0/1 local — q.java static init). The emit
        // layer already coerces a boolean-target assign through expr_bool,
        // but that renders a bare int LOCAL unchanged, so the local itself
        // must be booleanized (its 0/1 assigns then print false/true).
        if let Stmt::ExprStmt(Expr::Assign { target, value, .. }) = st {
            if let Expr::Field { ty, .. } = &**target {
                if ty.erased() == JavaType::Boolean {
                    if let Expr::Local { var, .. } = &**value {
                        if (*var as usize) < n {
                            in_cond[*var as usize] = true;
                        }
                    }
                }
            }
        }
        let val: Option<&Expr> = match st {
            Stmt::ExprStmt(Expr::Assign { value, .. }) => Some(value),
            // Bare expression statements (e.g. `q(v117);`) so a local
            // passed to a boolean parameter is caught below.
            Stmt::ExprStmt(e) => Some(e),
            Stmt::LocalDef { init: Some(e), .. } => Some(e),
            Stmt::Return(Some(e)) => Some(e),
            _ => None,
        };
        if let Some(v) = val {
            visit_exprs(v, &mut |x| {
                // A local passed as a BOOLEAN parameter is boolean-typed
                // (`q(v117)` where q's param is boolean, v117 an int 0/1 —
                // f8/b.java). The DEX passes booleans as 0/1 ints, so a
                // boolean param slot is authoritative for the argument's
                // type.
                if let Expr::Method { desc, args, .. } = x {
                    for (i, a) in args.iter().enumerate() {
                        if desc.args.get(i) == Some(&JavaType::Boolean) {
                            if let Expr::Local { var, .. } = a {
                                if (*var as usize) < n {
                                    in_cond[*var as usize] = true;
                                }
                            }
                        }
                    }
                }
                if let Expr::Bin { op, l, r, .. } = x {
                    if matches!(
                        op,
                        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Ge | BinOp::Gt | BinOp::Le
                    ) {
                        for side in [l, r] {
                            if let Expr::Local { var, .. } = &**side {
                                if (*var as usize) < n {
                                    in_cond[*var as usize] = true;
                                }
                            }
                        }
                    }
                    // Kotlin/d8 merge booleans through INT bitwise ops
                    // (`v3 | obj instanceof g`) == 0 — the other side's
                    // static type being boolean makes this a boolean
                    // context for the local (a genuine int bitwise
                    // expression never has a boolean operand).
                    if matches!(op, BinOp::Or | BinOp::And) {
                        let l_bool = l.type_ref().erased() == JavaType::Boolean;
                        let r_bool = r.type_ref().erased() == JavaType::Boolean;
                        if l_bool != r_bool {
                            let side = if l_bool { r } else { l };
                            if let Expr::Local { var, .. } = &**side {
                                if (*var as usize) < n {
                                    in_cond[*var as usize] = true;
                                }
                            }
                        }
                    }
                }
            });
        }
    });

    // A var whose every assignment value is boolean-shaped AND which is
    // referenced in conditions becomes boolean.
    // (Perf: this used to re-walk the whole statement tree ONCE PER VAR —
    // O(vars × stmts); on R8-merged monsters (2000+ vars, 15k statements)
    // that was tens of millions of visits. One pass collects the same
    // (any, all_bool) facts for every var.)
    let mut assigned_any = vec![false; n];
    let mut all_bool = vec![true; n];
    let mut edges: Vec<(usize, usize)> = Vec::new();
    walk_all(body, &mut |st| {
        let (var, value) = match st {
            Stmt::ExprStmt(Expr::Assign { target, value, .. }) => match &**target {
                Expr::Local { var, .. } => (var, &**value),
                _ => return,
            },
            Stmt::LocalDef {
                var, init: Some(e), ..
            } => (var, e),
            _ => return,
        };
        let i = *var as usize;
        if i < n {
            assigned_any[i] = true;
            if !is_boolean_valued(value, vt) {
                all_bool[i] = false;
            }
            // Chain edges for the context closure below: `v17 = v24`
            // links the two locals; when the TARGET is in a boolean
            // context, the source inherits it (its only reader is the
            // boolean-shaped chain). Collected as edges because the
            // target's context can be discovered anywhere in the tree.
            if let Expr::Local { var: src, .. } = value {
                if (*src as usize) < n {
                    edges.push((i, *src as usize));
                }
            }
        }
    });
    // Context closure along local chains: a var in a boolean context
    // pushes that context through every local it is assigned from
    // (`return v17` — v17 = v24 — v24 = 0/1) — the statement order
    // cannot be relied on (the return may follow the assignment).
    loop {
        let mut changed = false;
        for &(tgt, src) in &edges {
            if in_cond[tgt] && !in_cond[src] && all_bool[src] {
                in_cond[src] = true;
                changed = true;
            }
            // FORWARD through pure copies: `v21 = v17` with v17 boolean
            // makes v21 boolean when ALL of v21's assignments are
            // bool-shaped (the loop-state save pattern — weixin u2/f's
            // v62 → v17 → v21 chains left the copy targets int while
            // the accumulator booleanized).
            if in_cond[src] && !in_cond[tgt] && all_bool[tgt] {
                in_cond[tgt] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let reads = count_locals_stmts(std::slice::from_ref(body));
    let mut boolean_vars: HashSet<u32> = HashSet::default();
    for i in 0..n {
        if vt.vars[i].is_param {
            continue;
        }
        if derefed[i] {
            continue;
        }
        if !in_cond[i] && !(reads.get(&(i as u32)).copied().unwrap_or(0) == 0) {
            continue;
        }
        if assigned_any[i] && all_bool[i] {
            boolean_vars.insert(i as u32);
        }
    }
    if boolean_vars.is_empty() {
        return 0;
    }
    for v in &boolean_vars {
        vt.vars[*v as usize].ty = TypeRef::J(JavaType::Boolean);
    }
    let types: Vec<&TypeRef> = vt.vars.iter().map(|v| &v.ty).collect();
    rewrite_exprs(body, &mut |e| {
        deep_rewrite(e, &mut |x| {
            if let Expr::Local { var, ty } = x {
                if let Some(want) = types.get(*var as usize) {
                    if ty != *want {
                        *ty = (*want).clone();
                    }
                }
            }
        });
    });

    // Condition folding: `b != 0` → `b`, `b == 0` → `!b` (boolean vars).
    fold_bool_conditions(body, &boolean_vars);
    boolean_vars.len()
}

fn is_boolean_valued(e: &Expr, vt: &VarTable) -> bool {
    match e {
        Expr::Const(ConstVal::Int(0)) | Expr::Const(ConstVal::Int(1)) => true,
        // Kotlin's non-short-circuit boolean `or`/`and` compiles to `|`/`&`
        // over boolean locals (`v15 = delete | delete2 | ..`). EITHER side
        // boolean suffices: `int | boolean` is illegal in source — it is
        // always a not-yet-booleanized side of a boolean chain (the
        // loop-carried accumulator `v15 = v20; v20 = v15 | delete3` can
        // never seed its all_bool fixpoint otherwise).
        Expr::Bin { op: BinOp::Or | BinOp::And, l, r, .. } => {
            is_boolean_valued(l, vt) || is_boolean_valued(r, vt)
        }
        // Kotlin's `!x` lowers to `x ^ 1`; at booleanize time the value
        // is still the Xor (fix_bool_xor runs later), so an int-typed
        // generation receiving it never retyped (weixin v2/i's
        // `int v264_g160_g7 = !v200`). BOTH sides must be bool-shaped:
        // `bool ^ 1` qualifies (Const 0/1 is boolean-valued above),
        // a genuine int `flags ^ 1` does not (flags stays Int in vt).
        // MUST precede the generic Bin arm (which would swallow Xor).
        Expr::Bin {
            op: BinOp::Xor,
            l,
            r,
            ..
        } => is_boolean_valued(l, vt) && is_boolean_valued(r, vt),
        Expr::Bin { op, .. } => matches!(
            op,
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Ge | BinOp::Gt | BinOp::Le
        ),
        Expr::InstanceOf { .. } => true,
        // The VarTable is the authority: the embedded Local ty is a
        // lift-time snapshot and goes STALE across booleanize rounds —
        // a converted copy source kept its partners int (`v150 = v131`
        // boolean→int, weixin ConstraintLayout ×1.4k files) because the
        // all_bool fixpoint consulted the stale Int.
        Expr::Local { var, ty } => {
            let t = if (*var as usize) < vt.vars.len() {
                vt.var(*var).ty.erased()
            } else {
                ty.erased()
            };
            t == JavaType::Boolean
        }
        Expr::Method { desc, .. } => desc.ret == JavaType::Boolean,
        Expr::Cond { t, f, .. } => is_boolean_valued(t, vt) && is_boolean_valued(f, vt),
        Expr::Un { op: UnOp::Not, .. } => true,
        // Field ty comes from the dex descriptor — authoritative, not a
        // lift snapshot (`!w1x.i` arrives here still as `w1x.i ^ 1`; the
        // Xor arm needs this side bool-shaped).
        Expr::Field { ty, .. } => ty.erased() == JavaType::Boolean,
        _ => false,
    }
}

fn fold_bool_conditions(s: &mut Stmt, bv: &HashSet<u32>) {
    let fold = |e: &mut Expr| {
        deep_rewrite(e, &mut |x| {
            if let Expr::Bin { op, l, r, .. } = x {
                let (var_side, const_side) = match (&**l, &**r) {
                    (Expr::Local { .. }, Expr::Const(ConstVal::Int(_))) => (l, 0),
                    (Expr::Const(ConstVal::Int(_)), Expr::Local { .. }) => (r, 1),
                    _ => return,
                };
                let var = match &**var_side {
                    Expr::Local { var, .. } => *var,
                    _ => return,
                };
                if !bv.contains(&var) {
                    return;
                }
                let _ = const_side;
                match op {
                    BinOp::Ne => {
                        let inner = var_side.clone();
                        *x = *inner;
                    }
                    BinOp::Eq => {
                        let inner = var_side.clone();
                        *x = Expr::Un {
                            op: UnOp::Not,
                            e: Box::new(*inner),
                        };
                    }
                    _ => {}
                }
            }
        });
    };
    match s {
        Stmt::If { cond, .. } | Stmt::While { cond, .. } | Stmt::DoWhile { cond, .. } => fold(cond),
        _ => {}
    }
    walk_mut_deep(s, &mut |st| {
        let cond = match st {
            Stmt::If { cond, .. } => Some(cond),
            Stmt::While { cond, .. } => Some(cond),
            Stmt::DoWhile { cond, .. } => Some(cond),
            _ => None,
        };
        if let Some(c) = cond {
            fold(c);
        }
    });
}

/// Normalize `1 == v` comparisons to `v == 1` (d8 emits const-first if-eq).
pub fn flip_const_compares(s: &mut Stmt) {
    rewrite_exprs(s, &mut |e| {
        deep_rewrite(e, &mut |x| {
            if let Expr::Bin { op, l, r, .. } = x {
                if matches!(
                    op,
                    BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Ge | BinOp::Gt | BinOp::Le
                ) {
                    let l_const = matches!(&**l, Expr::Const(_));
                    let r_var = matches!(&**r, Expr::Local { .. });
                    if l_const && r_var {
                        std::mem::swap(l, r);
                    }
                }
            }
        });
    });
}

/// Object-typed locals compared against 0 compare against `null`.
pub fn null_compares(vt: &VarTable, body: &mut Stmt) {
    let obj_vars: HashSet<u32> = vt
        .vars
        .iter()
        .filter(|v| v.ty.erased().is_reference())
        .map(|v| v.id)
        .collect();
    rewrite_exprs(body, &mut |e| {
        deep_rewrite(e, &mut |x| {
            null_side_rewrite(x, &|v| obj_vars.contains(&v));
        });
    });
}

/// Drop assignments/declarations of locals that are NEVER read anywhere
/// in the method. Register-machine merges commit values no Java-level
/// code consumes (phi residue like `printStream = check;` — not just
/// noise: the merge variable's inferred type need not match the value,
/// so these lines are often type errors as well). Side-effect-free
/// values drop entirely; an impure value survives as a bare expression
/// statement. Iterated to a fixpoint: dropping `a = b` can make `b`
/// unread.
pub fn drop_dead_locals(body: &mut Stmt) {
    loop {
        let mut reads: Vec<usize> = Vec::new();
        count_reads(body, &mut reads);
        let mut dropped = 0usize;
        walk_mut_deep(body, &mut |st| match st {
            Stmt::Block(items) => prune_dead_items(items, &reads, &mut dropped),
            // Switch case bodies and For inits are bare `Vec<Stmt>`, NOT
            // wrapped in a `Stmt::Block`, so the Block arm never sees them.
            // Dead phi commits land directly in case bodies (`sb4 =
            // compareTo;` per case): pruning only Blocks dropped the
            // commit's reader (in a real post-switch Block) but left the
            // now-dead commits, stalling the fixpoint cascade and leaking
            // mistyped phi residue. TryWithResources.resources is
            // deliberately excluded — its LocalDefs carry auto-close
            // semantics that a bare-expression rewrite would break.
            Stmt::Switch { cases, .. } => {
                for c in cases {
                    prune_dead_items(&mut c.body, &reads, &mut dropped);
                }
            }
            Stmt::For { init, .. } => prune_dead_items(init, &reads, &mut dropped),
            _ => {}
        });
        if dropped == 0 {
            break;
        }
    }
    // Post-fixpoint in-place neutralization: dead assignments in BARE
    // positions (single-statement If arms, bare case bodies) that no
    // container retain ever reached. Runs AFTER the loop so the clean
    // container prune stays primary — the replacement (empty Block) is
    // residue the prune cannot match, so doing it inside the loop would
    // cannibalize clean removals (batch2 experiment: +5% lines, +534
    // errors). Absorbed from the deleted final_dead_assigns: without it
    // the decl gets pruned while the bare write survives → 找不到符号.
    {
        let mut reads: Vec<usize> = Vec::new();
        count_reads(body, &mut reads);
        let dead_inplace = |st: &Stmt| -> bool {
            let Stmt::ExprStmt(Expr::Assign {
                target,
                value,
                op: jdc_core::ir::expr::AssignOp::Plain,
                ..
            }) = st
            else {
                return false;
            };
            let Expr::Local { var, .. } = &**target else {
                return false;
            };
            reads.get(*var as usize).copied().unwrap_or(0) == 0 && !has_side_effects(value)
        };
        walk_mut_deep(body, &mut |st| {
            if dead_inplace(st) {
                *st = Stmt::Block(vec![]);
            }
        });
    }
}

/// Remove dead assignments/declarations from one bare statement vector.
/// Shared by `drop_dead_locals` across every prunable `Vec<Stmt>`
/// container. Side-effect-free values drop entirely; an impure value
/// survives as a bare expression statement.
fn prune_dead_items(items: &mut Vec<Stmt>, reads: &[usize], dropped: &mut usize) {
    let dead = |v: u32| reads.get(v as usize).copied().unwrap_or(0) == 0;
    items.retain_mut(|x| match x {
        Stmt::ExprStmt(Expr::Assign { target, value, op, .. }) => {
            let dead_target = matches!(&**target, Expr::Local { var, .. } if dead(*var));
            if dead_target && matches!(*op, AssignOp::Plain) {
                *dropped += 1;
                if has_side_effects(value) {
                    let e = std::mem::replace(value, Box::new(Expr::This));
                    *x = Stmt::ExprStmt(*e);
                    true
                } else {
                    false
                }
            } else {
                true
            }
        }
        Stmt::LocalDef { var, init, .. } if dead(*var) => {
            *dropped += 1;
            if let Some(e) = init.take() {
                if has_side_effects(&e) {
                    *x = Stmt::ExprStmt(e);
                    return true;
                }
            }
            false
        }
        _ => true,
    });
}


/// Vars occurring in this statement's OWN expression payloads (nested
/// sub-statements are descended separately so every occurrence carries
/// its innermost block id). ForEach/Catch bound vars are declarations,
/// not occurrences.
fn stmt_shallow_vars<F: FnMut(u32)>(st: &Stmt, out: &mut F) {
    fn ex<F: FnMut(u32)>(e: &Expr, out: &mut F) {
        visit_exprs(e, &mut |x| {
            if let Expr::Local { var, .. } = x {
                out(*var);
            }
        });
    }
    match st {
        Stmt::ExprStmt(e) | Stmt::Throw(e) | Stmt::MonitorEnter(e) | Stmt::MonitorExit(e) => {
            ex(e, out)
        }
        Stmt::TernaryValue { e } => ex(e, out),
        Stmt::Return(Some(e)) => ex(e, out),
        Stmt::LocalDef { var, init, .. } => {
            out(*var);
            if let Some(e) = init {
                ex(e, out);
            }
        }
        Stmt::If { cond, .. } | Stmt::While { cond, .. } | Stmt::DoWhile { cond, .. } => {
            ex(cond, out)
        }
        Stmt::For { cond, update, .. } => {
            if let Some(c) = cond {
                ex(c, out);
            }
            for u in update {
                ex(u, out);
            }
        }
        Stmt::ForEach { iterable, .. } => ex(iterable, out),
        Stmt::Switch { selector, cases, .. } => {
            ex(selector, out);
            for cg in cases {
                if let Some(g) = &cg.guard {
                    ex(g, out);
                }
            }
        }
        Stmt::Assert { cond, msg } => {
            ex(cond, out);
            if let Some(m) = msg {
                ex(m, out);
            }
        }
        Stmt::Synchronized { lock, .. } => ex(lock, out),
        _ => {}
    }
}

/// Pass A: number every Block in pre-order (root = 0) and record each
/// var's defining block. `sizes[id]` = the id-range span of the subtree.
/// Pass A: number every EMITTER-BRACED position in pre-order (root = 0)
/// and record each var's defining block. `sizes[id]` = the id-range span
/// of the subtree. A braced position is a Java scope whether or not the
/// IR node is a Stmt::Block — switch case bodies are bare Vecs and
/// single-statement if arms are bare statements, and sharing the parent's
/// id for them hid the scope violation (Telegram jk.java: a case-local
/// `notificationCenterDelegate`/`d0x` read after the switch never
/// hoisted — 找不到符号 ×76 in one file; the `d9x.L` reads even resolved
/// as a PACKAGE — 程序包不存在 ×92 tree-wide).
fn scope_pass_a(st: &Stmt, cur: u32, next: &mut u32, def_block: &mut [u32], sizes: &mut Vec<u32>) {
    if let Stmt::LocalDef { var, .. } = st {
        let i = *var as usize;
        if i < def_block.len() && def_block[i] == u32::MAX {
            def_block[i] = cur;
        }
    }
    fn scoped(c: &Stmt, next: &mut u32, def_block: &mut [u32], sizes: &mut Vec<u32>) {
        let id = *next;
        *next += 1;
        sizes.push(0);
        scope_pass_a(c, id, next, def_block, sizes);
        sizes[id as usize] = *next - id;
    }
    match st {
        Stmt::Block(v) => {
            for x in v {
                scope_pass_a(x, cur, next, def_block, sizes);
            }
        }
        Stmt::If {
            then_stmt,
            else_stmt,
            ..
        } => {
            scoped(then_stmt, next, def_block, sizes);
            if let Some(e) = else_stmt {
                scoped(e, next, def_block, sizes);
            }
        }
        Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => {
            scoped(body, next, def_block, sizes)
        }
        Stmt::For { init, body, .. } => {
            // A for-init declaration scopes over the WHOLE for statement
            // (header + body): one shared id, the body a nested one.
            let id = *next;
            *next += 1;
            sizes.push(0);
            for x in init {
                scope_pass_a(x, id, next, def_block, sizes);
            }
            scoped(body, next, def_block, sizes);
            sizes[id as usize] = *next - id;
        }
        Stmt::ForEach { body, .. } => scoped(body, next, def_block, sizes),
        Stmt::Switch { cases, default, .. } => {
            for c in cases {
                // One scope per case group (the emitter braces each);
                // non-Block statements share the case id, nested Blocks
                // get sub-ids — siblings in one brace group must share,
                // or a def's later same-case reads fall "outside".
                let id = *next;
                *next += 1;
                sizes.push(0);
                for x in &c.body {
                    if matches!(x, Stmt::Block(_)) {
                        scoped(x, next, def_block, sizes);
                    } else {
                        scope_pass_a(x, id, next, def_block, sizes);
                    }
                }
                sizes[id as usize] = *next - id;
            }
            if let Some(d) = default {
                scoped(d, next, def_block, sizes);
            }
        }
        Stmt::Try {
            body,
            catches,
            finally,
        } => {
            scoped(body, next, def_block, sizes);
            for c in catches {
                scoped(&c.body, next, def_block, sizes);
            }
            if let Some(f) = finally {
                scoped(f, next, def_block, sizes);
            }
        }
        Stmt::TryWithResources {
            resources,
            body,
            catches,
            finally,
        } => {
            // Resource declarations scope over the WHOLE try statement.
            let id = *next;
            *next += 1;
            sizes.push(0);
            for r in resources {
                scope_pass_a(r, id, next, def_block, sizes);
            }
            scoped(body, next, def_block, sizes);
            for c in catches {
                scoped(&c.body, next, def_block, sizes);
            }
            if let Some(f) = finally {
                scoped(f, next, def_block, sizes);
            }
            sizes[id as usize] = *next - id;
        }
        Stmt::Synchronized { body, .. } => scoped(body, next, def_block, sizes),
        Stmt::Labeled { body, .. } => scope_pass_a(body, cur, next, def_block, sizes),
        _ => {}
    }
}

/// Pass B (same numbering walk): count each var's occurrences overall
/// and within its defining block's subtree. For/TWR take dedicated arms
/// so their header expressions count INSIDE the statement scope (a
/// normal `for (int i…)` must not look like an outside read of i).
fn scope_pass_b(
    st: &Stmt,
    cur: u32,
    next: &mut u32,
    def_block: &[u32],
    sizes: &[u32],
    within: &mut [u32],
    total: &mut [u32],
) {
    fn cnt(
        v: u32,
        at: u32,
        def_block: &[u32],
        sizes: &[u32],
        within: &mut [u32],
        total: &mut [u32],
    ) {
        let i = v as usize;
        if i < total.len() {
            total[i] += 1;
            let d = def_block[i];
            if d != u32::MAX && d <= at && at < d + sizes[d as usize] {
                within[i] += 1;
            }
        }
    }
    fn scoped(
        c: &Stmt,
        next: &mut u32,
        def_block: &[u32],
        sizes: &[u32],
        within: &mut [u32],
        total: &mut [u32],
    ) {
        let id = *next;
        *next += 1;
        scope_pass_b(c, id, next, def_block, sizes, within, total);
    }
    fn ex_at(
        e: &Expr,
        at: u32,
        def_block: &[u32],
        sizes: &[u32],
        within: &mut [u32],
        total: &mut [u32],
    ) {
        visit_exprs(e, &mut |x| {
            if let Expr::Local { var, .. } = x {
                cnt(*var, at, def_block, sizes, within, total);
            }
        });
    }
    match st {
        Stmt::For {
            init,
            cond,
            update,
            body,
        } => {
            let id = *next;
            *next += 1;
            for x in init {
                scope_pass_b(x, id, next, def_block, sizes, within, total);
            }
            if let Some(c) = cond {
                ex_at(c, id, def_block, sizes, within, total);
            }
            for u in update {
                ex_at(u, id, def_block, sizes, within, total);
            }
            scoped(body, next, def_block, sizes, within, total);
        }
        Stmt::TryWithResources {
            resources,
            body,
            catches,
            finally,
        } => {
            let id = *next;
            *next += 1;
            for r in resources {
                scope_pass_b(r, id, next, def_block, sizes, within, total);
            }
            scoped(body, next, def_block, sizes, within, total);
            for c in catches {
                scoped(&c.body, next, def_block, sizes, within, total);
            }
            if let Some(f) = finally {
                scoped(f, next, def_block, sizes, within, total);
            }
        }
        _ => {
            stmt_shallow_vars(st, &mut |v| cnt(v, cur, def_block, sizes, within, total));
            match st {
                Stmt::Block(v) => {
                    for x in v {
                        scope_pass_b(x, cur, next, def_block, sizes, within, total);
                    }
                }
                Stmt::If {
                    then_stmt,
                    else_stmt,
                    ..
                } => {
                    scoped(then_stmt, next, def_block, sizes, within, total);
                    if let Some(e) = else_stmt {
                        scoped(e, next, def_block, sizes, within, total);
                    }
                }
                Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => {
                    scoped(body, next, def_block, sizes, within, total)
                }
                Stmt::ForEach { body, .. } => {
                    scoped(body, next, def_block, sizes, within, total)
                }
                Stmt::Switch { cases, default, .. } => {
                    for c in cases {
                        let id = *next;
                        *next += 1;
                        for x in &c.body {
                            if matches!(x, Stmt::Block(_)) {
                                scoped(x, next, def_block, sizes, within, total);
                            } else {
                                scope_pass_b(x, id, next, def_block, sizes, within, total);
                            }
                        }
                    }
                    if let Some(d) = default {
                        scoped(d, next, def_block, sizes, within, total);
                    }
                }
                Stmt::Try {
                    body,
                    catches,
                    finally,
                } => {
                    scoped(body, next, def_block, sizes, within, total);
                    for c in catches {
                        scoped(&c.body, next, def_block, sizes, within, total);
                    }
                    if let Some(f) = finally {
                        scoped(f, next, def_block, sizes, within, total);
                    }
                }
                Stmt::Synchronized { body, .. } => {
                    scoped(body, next, def_block, sizes, within, total)
                }
                Stmt::Labeled { body, .. } => {
                    scope_pass_b(body, cur, next, def_block, sizes, within, total)
                }
                _ => {}
            }
        }
    }
}

/// Declaration hygiene: every used var that has no LocalDef gets a bare
/// declaration at the top; duplicate LocalDefs demote to assignments.
/// In an enum's `<clinit>`, every constant's assignment
/// (`Self.FIELD = new Self("NAME", ordinal, ...)`) is compiler-mandated
/// boilerplate — when the class renders as a true `enum` declaration the
/// constants live in the header and these assignments must go. Only
/// ACC_ENUM-flagged static fields of the class itself are touched; the
/// `$VALUES` array assignment stays (a static block in an enum is legal).
pub fn strip_enum_const_inits(s: &mut Stmt, class: &crate::PoolClass) {
    let const_fields: jdc_core::FxHashSet<&str> = class
        .static_fields
        .iter()
        .filter(|f| f.access & crate::access::ACC_ENUM != 0)
        .map(|f| f.name.as_ref())
        .collect();
    if const_fields.is_empty() {
        return;
    }
    if let Stmt::Block(stmts) = s {
        stmts.retain(|st| {
            !matches!(
                st,
                Stmt::ExprStmt(Expr::Assign { target, value, .. })
                    if matches!(&**target,
                        Expr::Field { cls, name, is_static: true, .. }
                            if cls.as_ref() == class.name
                                && const_fields.contains(&**name))
                        && matches!(&**value, Expr::New { cls: ncls, .. } if ncls.as_ref() == class.name)
            )
        });
    }
}

/// Java requires the `super(...)`/`this(...)` delegation to be the FIRST
/// statement of a constructor. d8/R8 order the outer-reference capture
/// (`this.b = p1;`) or other field writes before the invokesuper in the
/// bytecode, which lifts as-is into an uncompilable statement order —
/// hoist the bare delegation call to position 0 (jadx does the same).
/// Does the expression touch `this` (explicit, implicit-instance member
/// access, or super)? Such a def cannot be inlined into a delegation
/// that must run BEFORE the supertype constructor.
fn contains_this_access(e: &Expr) -> bool {
    let mut hit = false;
    visit_exprs(e, &mut |x| {
        if hit {
            return;
        }
        match x {
            Expr::This => hit = true,
            Expr::Field { owner, is_static: false, .. } => {
                if owner.is_none() || matches!(&**owner.as_ref().unwrap(), Expr::This) {
                    hit = true;
                }
            }
            Expr::Method { owner, is_static: false, .. } => {
                if owner.is_none() || matches!(&**owner.as_ref().unwrap(), Expr::This) {
                    hit = true;
                }
            }
            _ => {}
        }
    });
    hit
}

/// Inline the transitive straight-line defs of a delegation call's
/// arg locals into the call, so hoisting it to position 0 leaves no
/// dangling name (lark ProtoAdapter's `super(str)` with `str` defined
/// BELOW the hoist point — 找不到符号 变量 str). Returns false when any
/// arg var has no inlineable def (param-less, missing, this-touching,
/// or cyclic) — the caller then leaves the call in place: a
/// not-first-statement error is cheaper than an unresolved name that
/// poisons every downstream type.
/// Any occurrence of `var` (read OR assign-target) in `stmts` — the
/// suffix-use test for dropping a consumed def: count_locals_stmts only
/// sees reads, and a suffix WRITE to a dropped declaration makes
/// ensure_declared re-add `Type v;` at the method top — BEFORE the
/// hoisted this()/super() ("对this的调用必须是构造器中的第一个语句",
/// lark +587 the read-only test caused).
fn var_occurs_in(stmts: &[Stmt], var: u32) -> bool {
    stmts.iter().any(|st| {
        let mut hit = false;
        visit_stmt_exprs_ro(st, &mut |e| {
            if !hit {
                visit_exprs(e, &mut |x| {
                    if let Expr::Local { var: v, .. } = x {
                        if *v == var {
                            hit = true;
                        }
                    }
                });
            }
        });
        hit
    })
}

// ---------------------------------------------------------------------------
// Branched-delegation ctors → static resolver helper.
// ---------------------------------------------------------------------------

/// A synthetic static method extracted from a constructor whose
/// delegation is preceded by computation (the Kotlin default-arg /
/// conditional-bridge shape). Rendered by classdec after the real
/// methods; the ctor becomes a single first-statement delegation whose
/// carrier arg is the helper call.
pub struct CtorHelper {
    pub name: String,
    pub ret: JavaType,
    pub params: Vec<(JavaType, String)>,
    pub body: Stmt,
    pub vt: VarTable,
}

fn ctor_helpers() -> &'static std::sync::Mutex<std::collections::HashMap<String, Vec<CtorHelper>>> {
    static H: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Vec<CtorHelper>>>> =
        std::sync::OnceLock::new();
    H.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::default()))
}

/// Synthetic relay ctor for the FULL mode of
/// extract_branched_delegation_helper: `private C(Object[] h$relay) {
/// this/super((T0) h$relay[0], ..); }` — the public ctor becomes
/// `this(resolve$X(params))`, both delegations first-statement legal.
pub struct CtorRelay {
    /// The delegation target's formal types (the casts of the unpack).
    pub formals: Vec<JavaType>,
    pub is_super: bool,
}

type RelayMap = std::collections::HashMap<String, Vec<(String, CtorRelay)>>;

fn ctor_relays() -> &'static std::sync::Mutex<RelayMap> {
    static H: std::sync::OnceLock<std::sync::Mutex<RelayMap>> = std::sync::OnceLock::new();
    H.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::default()))
}

/// Take (and clear) the relay ctors registered for `class` — sorted by
/// the originating ctor descriptor for deterministic emission.
pub fn take_ctor_relays(class: &str) -> Vec<(String, CtorRelay)> {
    let mut m = ctor_relays().lock().unwrap_or_else(|e| e.into_inner());
    let mut v = m.remove(class).unwrap_or_default();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

fn array_init(args: Vec<Expr>) -> Expr {
    Expr::NewArray {
        elem: TypeRef::J(JavaType::Object(std::sync::Arc::from("java/lang/Object"))),
        dims: Vec::new(),
        trailing_dims: 0,
        init: Some(args),
    }
}

/// Deterministic helper name: FNV-1a over (class, ctor descriptor) —
/// worker-thread registration order must not leak into the output.
fn helper_name(class: &str, desc: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in class.as_bytes().iter().chain(b"::").chain(desc.as_bytes()) {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("resolve${:x}", (h >> 16) & 0xffff_ffff)
}

/// Drain the helpers registered for a class (render-time). Sorted by
/// name: registration order is worker-scheduling dependent.
pub fn take_ctor_helpers(class: &str) -> Vec<CtorHelper> {
    let mut m = ctor_helpers().lock().unwrap_or_else(|e| e.into_inner());
    let mut v = m.remove(class).unwrap_or_default();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

fn strip_delegations_deep(stmts: &mut Vec<Stmt>) {
    stmts.retain(|s| !is_bare_ctor_call(s));
    for s in stmts.iter_mut() {
        strip_delegation_one(s);
    }
}

fn strip_delegation_one(st: &mut Stmt) {
    if is_bare_ctor_call(st) {
        *st = Stmt::Block(Vec::new());
        return;
    }
    match st {
        Stmt::Block(v) => strip_delegations_deep(v),
        Stmt::If { then_stmt, else_stmt, .. } => {
            strip_delegation_one(then_stmt);
            if let Some(e) = else_stmt {
                strip_delegation_one(e);
            }
        }
        Stmt::While { body, .. }
        | Stmt::DoWhile { body, .. }
        | Stmt::ForEach { body, .. }
        | Stmt::Synchronized { body, .. }
        | Stmt::Labeled { body, .. } => strip_delegation_one(body),
        Stmt::For { init, body, .. } => {
            strip_delegations_deep(init);
            strip_delegation_one(body);
        }
        Stmt::Switch { cases, default, .. } => {
            for c in cases.iter_mut() {
                strip_delegations_deep(&mut c.body);
            }
            if let Some(d) = default {
                strip_delegation_one(d);
            }
        }
        Stmt::Try { body, catches, finally } | Stmt::TryWithResources { body, catches, finally, .. } => {
            strip_delegation_one(body);
            for c in catches.iter_mut() {
                strip_delegation_one(&mut c.body);
            }
            if let Some(f) = finally {
                strip_delegation_one(f);
            }
        }
        _ => {}
    }
}

#[allow(clippy::ptr_arg)]
fn rewrite_bare_returns(stmts: &mut Vec<Stmt>, val: &Expr) {
    for s in stmts.iter_mut() {
        rewrite_bare_returns_one(s, val);
    }
}

fn rewrite_bare_returns_one(st: &mut Stmt, val: &Expr) {
    match st {
        Stmt::Return(None) => *st = Stmt::Return(Some(val.clone())),
        Stmt::Block(v) => rewrite_bare_returns(v, val),
        Stmt::If { then_stmt, else_stmt, .. } => {
            rewrite_bare_returns_one(then_stmt, val);
            if let Some(e) = else_stmt {
                rewrite_bare_returns_one(e, val);
            }
        }
        Stmt::While { body, .. }
        | Stmt::DoWhile { body, .. }
        | Stmt::ForEach { body, .. }
        | Stmt::Synchronized { body, .. }
        | Stmt::Labeled { body, .. } => rewrite_bare_returns_one(body, val),
        Stmt::For { init, body, .. } => {
            rewrite_bare_returns(init, val);
            rewrite_bare_returns_one(body, val);
        }
        Stmt::Switch { cases, default, .. } => {
            for c in cases.iter_mut() {
                rewrite_bare_returns(&mut c.body, val);
            }
            if let Some(d) = default {
                rewrite_bare_returns_one(d, val);
            }
        }
        Stmt::Try { body, catches, finally } | Stmt::TryWithResources { body, catches, finally, .. } => {
            rewrite_bare_returns_one(body, val);
            for c in catches.iter_mut() {
                rewrite_bare_returns_one(&mut c.body, val);
            }
            if let Some(f) = finally {
                rewrite_bare_returns_one(f, val);
            }
        }
        _ => {}
    }
}

fn collect_def_vars(stmts: &[Stmt], out: &mut jdc_core::FxHashSet<u32>) {
    for st in stmts {
        match st {
            Stmt::LocalDef { var, .. } => {
                out.insert(*var);
            }
            Stmt::ExprStmt(Expr::Assign { target, .. }) => {
                if let Expr::Local { var, .. } = &**target {
                    out.insert(*var);
                }
            }
            Stmt::Block(v) => collect_def_vars(v, out),
            Stmt::If { then_stmt, else_stmt, .. } => {
                collect_def_vars(std::slice::from_ref(then_stmt.as_ref()), out);
                if let Some(e) = else_stmt {
                    collect_def_vars(std::slice::from_ref(e.as_ref()), out);
                }
            }
            Stmt::While { body, .. }
            | Stmt::DoWhile { body, .. }
            | Stmt::Synchronized { body, .. }
            | Stmt::Labeled { body, .. } => {
                collect_def_vars(std::slice::from_ref(body.as_ref()), out);
            }
            Stmt::ForEach { var, body, .. } => {
                out.insert(*var);
                collect_def_vars(std::slice::from_ref(body.as_ref()), out);
            }
            Stmt::For { init, body, .. } => {
                collect_def_vars(init, out);
                collect_def_vars(std::slice::from_ref(body.as_ref()), out);
            }
            Stmt::Switch { cases, default, .. } => {
                for c in cases {
                    collect_def_vars(&c.body, out);
                }
                if let Some(d) = default {
                    collect_def_vars(std::slice::from_ref(d.as_ref()), out);
                }
            }
            Stmt::Try { body, catches, finally }
            | Stmt::TryWithResources { body, catches, finally, .. } => {
                collect_def_vars(std::slice::from_ref(body.as_ref()), out);
                for c in catches {
                    out.insert(c.var);
                    collect_def_vars(std::slice::from_ref(c.body.as_ref()), out);
                }
                if let Some(f) = finally {
                    collect_def_vars(std::slice::from_ref(f.as_ref()), out);
                }
            }
            _ => {}
        }
    }
}

/// v1 extraction is limited to Block/If/straight-line computation (the
/// Kotlin default-arg shape); loops/switch/try in the extraction bail.
fn extraction_shape_ok(stmts: &[Stmt]) -> bool {
    stmts.iter().all(|st| match st {
        Stmt::Block(v) => extraction_shape_ok(v),
        Stmt::LocalDef { .. }
        | Stmt::ExprStmt(_)
        | Stmt::Return(_)
        | Stmt::Throw(_)
        | Stmt::Assert { .. }
        | Stmt::Break(_)
        | Stmt::Continue(_) => true,
        Stmt::If { then_stmt, else_stmt, .. } => {
            ok_one(then_stmt)
                && else_stmt.as_deref().map(ok_one).unwrap_or(true)
        }
        Stmt::While { body, .. }
        | Stmt::DoWhile { body, .. }
        | Stmt::ForEach { body, .. }
        | Stmt::Synchronized { body, .. } => ok_one(body),
        Stmt::Labeled { body, .. } => ok_one(body),
        Stmt::For { init, body, .. } => extraction_shape_ok(init) && ok_one(body),
        Stmt::Switch { cases, default, .. } => {
            cases.iter().all(|c| extraction_shape_ok(&c.body))
                && default.as_deref().map(ok_one).unwrap_or(true)
        }
        Stmt::Try { body, catches, finally }
        | Stmt::TryWithResources { body, catches, finally, .. } => {
            ok_one(body)
                && catches.iter().all(|c| ok_one(&c.body))
                && finally.as_deref().map(ok_one).unwrap_or(true)
        }
        // Label/Goto (unstructured), MonitorEnter/Exit (raw), ClassDecl
        // (local class), TernaryValue, Raw: stay out of v2 extraction.
        _ => false,
    })
}

fn ok_one(st: &Stmt) -> bool {
    extraction_shape_ok(std::slice::from_ref(st))
}

fn static_safe(stmts: &[Stmt], this_id: Option<u32>) -> bool {
    let mut ok = true;
    for st in stmts {
        visit_stmt_exprs_ro(st, &mut |e| {
            visit_exprs(e, &mut |x| {
                match x {
                    Expr::This => ok = false,
                    // ddc's IR usually carries `this` as the var-0
                    // PARAM local, not the Expr::This variant — a
                    // helper body reading it dangles (the this param
                    // is excluded from the helper's param list) AND
                    // renders 无法从静态上下文中引用非静态 (weixin y9/i).
                    Expr::Local { var, .. } => {
                        if Some(*var) == this_id {
                            ok = false;
                        }
                    }
                    // implicit-this member access / call
                    Expr::Field { owner: None, is_static: false, .. } => ok = false,
                    Expr::Method { owner: None, is_static: false, .. } => ok = false,
                    _ => {}
                }
            });
        });
    }
    ok
}

/// The bytedance PatchProxy hotfix guard in <clinit>:
/// `if (vsChange != null) { PatchProxyResult p = PatchProxy.proxy(..);
/// if (p.isSupported) { return; } }` — a bare `return;` inside a STATIC
/// INITIALIZER is illegal Java (javac: 返回外部方法; videocut ×28,029 +
/// news_article ×19,058 — sampled 400/400 this shape). The patch path
/// is dead in a fresh compile (the IVsChange field is null), so the
/// whole leading guard statement strips, leaving the real static inits —
/// the <clinit> counterpart of the Titan/Robust ctor-guard strips.
/// Gate: FIRST top-level stmt is an If with no else whose body holds
/// both a bare return and a *PatchProxy* call.
pub fn strip_clinit_hotfix_guard(body: &mut Stmt) {
    // The structurer can wrap the sequence in a nested singleton block
    // (the enum-clinit path flattens for the same reason); the normal
    // emit_method path does not.
    if let Stmt::Block(vs) = body {
        flatten_top_blocks(vs);
    }
    let Stmt::Block(stmts) = body else { return };
    // The guard may sit behind hoisted bare declarations (`int[] v8x;`
    // from ensure_declared) — find the first non-decl statement.
    let mut gi = 0usize;
    while gi < stmts.len() && matches!(&stmts[gi], Stmt::LocalDef { init: None, .. }) {
        gi += 1;
    }
    let Some(first) = stmts.get(gi) else { return };
    let Stmt::If { then_stmt, else_stmt, .. } = first else {
        return;
    };
    if else_stmt.is_some() {
        return;
    }
    let mut has_ret = false;
    let mut has_proxy = false;
    let c = then_stmt.as_ref();
    walk_all(c, &mut |st| {
        if matches!(st, Stmt::Return(None)) {
            has_ret = true;
        }
    });
    let mut cc = c.clone();
    walk_stmt_exprs(&mut cc, &mut |e| {
        deep_rewrite(e, &mut |x| {
            if let Expr::Method { cls, .. } = x {
                if cls.contains("PatchProxy") {
                    has_proxy = true;
                }
            }
        });
    });
    if has_ret && has_proxy {
        stmts.remove(gi);
    }
}

/// Vendor hotfix guards in CTORS that wrap a delegation:
/// `if (__ != null) { Object[] v = ..; ConstructorCode proxy =
/// ConstructorCode.proxy(..); if (proxy != null) { this(unpack(v));
/// proxy.afterSuper(this); return; } }` ahead of the real top-level
/// delegation (alipay InstantRun — ctor-not-first ×1,436; the same
/// shape as the already-stripped Baidu Titan / bytedance clinit /
/// Meituan Robust guards). The patch path is dead in a fresh compile
/// (the redirect field is null), and Java cannot place the wrapped
/// delegation legally — strip the WHOLE guard statement.
///
/// VENDOR-GATED on purpose: a conditional delegation with a top-level
/// fallback (`if (c) { this(a); return; } this(b);`) is legitimate
/// bytecode whose guard must NOT be dropped. The gate: the guard
/// subtree mentions a known hotfix framework type (hotfix / instantrun
/// / robust / titan / PatchProxy / ChangeQuickRedirect / ConstructorCode
/// in any method-owner, field owner+type, cast, or new).
/// First LEAF statement after unwrapping nested `Stmt::Block` wrappers —
/// the lifted ctor body reaches the ctor chain with the delegation and
/// the hotfix guard each wrapped in plain Blocks (the flattening that
/// produces the rendered shape runs later).
pub(crate) fn first_leaf_stmt(s: &Stmt) -> &Stmt {
    match s {
        Stmt::Block(v) if !v.is_empty() => first_leaf_stmt(&v[0]),
        other => other,
    }
}

/// Vendor hotfix guards in CTORS that wrap a delegation:
/// `if (__ != null) { Object[] v = ..; ConstructorCode proxy =
/// ConstructorCode.proxy(..); if (proxy != null) { this(unpack(v));
/// proxy.afterSuper(this); return; } }` ahead of the real top-level
/// delegation (alipay InstantRun — ctor-not-first ×1,436, nearly all in
/// FALLBACK ENUM ctors; the same shape as the already-stripped Baidu
/// Titan / bytedance clinit / Meituan Robust guards). The patch path is
/// dead in a fresh compile (the redirect field is null), and Java cannot
/// place the wrapped delegation legally — strip the WHOLE guard
/// statement (at this pipeline stage it arrives Block-wrapped, and its
/// branches are still un-inverted: `If(then=∅, else=guard-body)`).
///
/// VENDOR-GATED on purpose: a conditional delegation with a top-level
/// fallback (`if (c) { this(a); return; } this(b);`) is legitimate
/// bytecode whose guard must NOT be dropped. The gate: the guard
/// subtree mentions a known hotfix framework type (hotfix / instantrun
/// / robust / titan / PatchProxy / ChangeQuickRedirect / ConstructorCode
/// in any method-owner, field owner+type, cast, or new).
pub fn strip_ctor_hotfix_guards(body: &mut Stmt) {
    fn marker_hit(e: &Expr) -> bool {
        let has = |n: &str| {
            let l = n.to_ascii_lowercase();
            l.contains("hotfix")
                || l.contains("instantrun")
                || l.contains("robust")
                || l.contains("titan")
                || l.contains("patchproxy")
                || l.contains("changequickredirect")
                || l.contains("constructorcode")
        };
        let mut hit = false;
        visit_exprs(e, &mut |x| {
            match x {
                Expr::Method { cls, .. } | Expr::New { cls, .. } | Expr::Field { cls, .. } => {
                    if has(cls) {
                        hit = true;
                    }
                }
                _ => {}
            }
            if let Expr::Field { ty, .. } = x {
                if let TypeRef::J(JavaType::Object(n)) = ty {
                    if has(n) {
                        hit = true;
                    }
                }
            }
        });
        hit
    }
    // A top-level statement that IS a hotfix guard: unwrap single-child
    // Block chains down to an If whose subtree holds BOTH a delegation
    // and a vendor marker.
    fn is_guard(st: &Stmt) -> bool {
        let mut cur = st;
        loop {
            match cur {
                Stmt::Block(v) if v.len() == 1 => cur = &v[0],
                Stmt::If { .. } => break,
                _ => return false,
            }
        }
        let mut has_del = false;
        walk_all(cur, &mut |s2| {
            if is_bare_ctor_call(s2) {
                has_del = true;
            }
        });
        if !has_del {
            return false;
        }
        let mut has_marker = false;
        visit_stmt_exprs_ro(cur, &mut |e| {
            if marker_hit(e) {
                has_marker = true;
            }
        });
        has_marker
    }
    // Strip at EVERY Block level: the lifted body arrives either as
    // [Block(guard), Block(del, ..)] (AtomicDataCollector) or as ONE big
    // Block [decls, guard, prelude-computes, del] (compose TextStyle).
    // No outside-delegation requirement: the vendor gate already proves
    // the statement is dead hotfix machinery (the redirect field is null
    // in a fresh compile — the same assumption the Titan/PatchProxy
    // strips run on), and the dex's fresh path IS whatever remains after
    // the guard. rpc/a.java: the real super() sits inside a switch arm —
    // requiring an unconditional outside delegation kept the guard (and
    // its illegal pre-super field writes) alive.
    fn strip_in(stmts: &mut Vec<Stmt>) {
        stmts.retain(|st| !is_guard(st));
        for st in stmts.iter_mut() {
            if let Stmt::Block(v) = st {
                strip_in(v);
            }
        }
    }
    let Stmt::Block(stmts) = body else { return };
    strip_in(stmts);
}

/// Repeated identical branch-site delegations with param-only args
/// (weixin appbrand/zc: bare `super()` at four branch ends, each
/// followed by field-write tails) carry no computed carrier, so the
/// helper extraction has nothing to return — but they need no helper:
/// every delegating path runs the SAME call, so hoist one copy to
/// position 0 and strip the sites. The per-branch tails stay in their
/// branches and now legally follow the first-statement delegation.
/// Same reordering approximation fix_ctor_super_first already ships for
/// the straight-line case (paths that threw BEFORE delegating now run
/// the delegation first — Object-super, the dominant shape, is inert).
/// Gate: every del deep-equal and reading params only (a computed local
/// would dangle at position 0 — that is the helper extractor's job).
pub fn hoist_branch_delegations(body: &mut Stmt, vt: &VarTable) {
    let Stmt::Block(stmts) = body else { return };
    if stmts.first().is_some_and(is_bare_ctor_call) {
        return; // already first
    }
    let mut dels: Vec<Expr> = Vec::new();
    for st in stmts.iter() {
        visit_stmt_exprs_ro(st, &mut |e| {
            if is_delegation_expr(e) {
                dels.push(e.clone());
            }
        });
    }
    if dels.is_empty() {
        return;
    }
    if !dels.iter().all(|d| *d == dels[0]) {
        return;
    }
    let mut bad = false;
    visit_exprs(&dels[0], &mut |x| {
        if let Expr::Local { var, .. } = x {
            if (*var as usize) >= vt.vars.len() || !vt.vars[*var as usize].is_param {
                bad = true;
            }
        }
    });
    if bad {
        return;
    }
    let del = dels[0].clone();
    strip_delegations_deep(stmts);
    stmts.insert(0, Stmt::ExprStmt(del));
}

/// The branched-delegation ctor family ("对this的调用必须是构造器中的
/// 第一个语句", ~2k across corpora): R8 renders a Kotlin default-arg /
/// conditional-bridge ctor as computation statements (straight-line plus
/// mask-guarded overrides) with every control path ending in a
/// `this(a, b, carrier)` delegation. Java cannot express statements
/// before the delegation, and unlike the linear-chain case the carrier's
/// value is computed by multi-statement branched code that cannot fold
/// into a ternary. Extract the computation into a synthetic
/// `private static T resolve$X(<ctor params>)` and make the ctor a
/// single first-statement `this(a, b, resolve$X(..))` — compilable AND
/// faithful (jadx renders the un-compilable statements-before-this form
/// for this family).
///
/// Two carrier modes:
/// - Single (v1): every delegation textually identical and exactly one
///   non-param local (the carrier) read exactly once across the args.
/// - Position (v2): delegations differ at exactly ONE arg position k,
///   every site passing a bare non-param local there (per-branch
///   carriers, lark BaseProtocol$ReliablePushList's v22_g1/v22x). Each
///   stripped delegation's own carrier fills the return that follows it;
///   merge-point returns fall back to the primary site's carrier.
///
/// The top-level primary delegation is optional in BOTH modes — when
/// every path delegates inside branches (weixin ln1/b1's super() in
/// if/try/catch) the whole body is extracted and any del serves as the
/// template (all non-k parts identical).
///
/// Shared gates (all conservative, bail keeps today's behavior): not an
/// enum (trace-param strip shifts render params); nested only when the
/// class RENDERS static-nested (member-inner ctors lose the outer param
/// at render); computation is Block/If/straight-line only; no
/// `this`-dependence; no local defined in the extraction used after the
/// primary.
pub fn extract_branched_delegation_helper(
    body: &mut Stmt,
    vt: &VarTable,
    pool: &DexPool,
    class: &crate::PoolClass,
    desc_str: &str,
) {
    if class.is_enum() {
        return;
    }
    // Member-inner ctors lose the synthetic outer param at render
    // (qualified-new absorbs it), so the helper's param list would no
    // longer match the rendered ctor. STATIC nested classes render all
    // params like top-level ones (lark BaseProtocol$PushList family).
    if class.nesting.enclosing_class.is_some() && !crate::ctx::nested_is_static(pool, class) {
        return;
    }
    let Stmt::Block(stmts) = body else { return };
    let mut dels: Vec<Expr> = Vec::new();
    for st in stmts.iter() {
        visit_stmt_exprs_ro(st, &mut |e| {
            if is_delegation_expr(e) {
                dels.push(e.clone());
            }
        });
    }
    if dels.is_empty() {
        return;
    }
    if dels.len() == 1 && stmts.first().is_some_and(is_bare_ctor_call) {
        return; // already first
    }
    let template = dels[0].clone();
    let Expr::Method { args: t_args, .. } = &template else {
        return;
    };
    // Carrier discovery: Single (all dels identical, one non-param local
    // read once) or Position (dels differ at exactly one arg slot, every
    // site a bare non-param local of one erased type).
    enum Mode {
        Single(u32),
        Position(usize),
        /// Every path delegates with the SAME target signature but its
        /// own arg expressions: the whole computation moves to the
        /// helper (each delegation becomes `return new Object[]{its
        /// args}`), the public ctor becomes `this(resolve$X(params))`
        /// and a synthetic `private C(Object[] h)` relay unpacks the
        /// carriers into the one legal first-statement delegation.
        Full,
    }
    // Full-mode gate: uniform delegation target (this/super, class,
    // descriptor), non-empty args, no pre-existing (Object[]) ctor,
    // and — the relay signature is per-CLASS unique — only the
    // lexicographically least <init> descriptor of the class may take
    // the relay (deterministic across parallel workers; a second
    // branched ctor in one class keeps its honest error).
    let full_gate = (|| -> Option<()> {
        let Expr::Method {
            args: a0,
            desc: d0,
            is_super: s0,
            cls: c0,
            ..
        } = &dels[0]
        else {
            return None;
        };
        if a0.is_empty() {
            return None;
        }
        for d in &dels[1..] {
            match d {
                Expr::Method {
                    args,
                    desc,
                    is_super,
                    cls,
                    ..
                } if args.len() == a0.len()
                    && **desc == **d0
                    && is_super == s0
                    && cls == c0 => {}
                _ => return None,
            }
        }
        if class
            .all_methods()
            .any(|m| &*m.name == "<init>" && &*m.desc == "([Ljava/lang/Object;)V")
        {
            return None;
        }
        Some(())
    })()
    .is_some();
    let mode = if dels.iter().all(|d| *d == dels[0]) {
        let mut carrier: Option<u32> = None;
        let mut reads = 0usize;
        let mut multi = false;
        for a in t_args.iter() {
            let c = a.clone();
            visit_exprs(&c, &mut |x| {
                if let Expr::Local { var, .. } = x {
                    if (*var as usize) < vt.vars.len() && !vt.vars[*var as usize].is_param {
                        reads += 1;
                        match carrier {
                            Some(v) if v == *var => {}
                            Some(_) => multi = true,
                            None => carrier = Some(*var),
                        }
                    }
                }
            });
        }
        match carrier {
            Some(cv) if !multi && reads == 1 => Mode::Single(cv),
            _ if full_gate => Mode::Full,
            _ => return,
        }
    } else {
        let positioned: Option<Mode> = (|| {
        let arity = t_args.len();
        // Uniform arity FIRST: a bare `super()` mixed among arg-carrying
        // dels made the single-diff slot k index past a short del's args
        // (deepseek v52 / kimi c60.o panics, index-out-of-bounds).
        if dels
            .iter()
            .any(|d| !matches!(d, Expr::Method { args, .. } if args.len() == arity))
        {
            return None;
        }
        let mut diff: Vec<usize> = Vec::new();
        for j in 0..arity {
            if dels.iter().any(|d| match d {
                Expr::Method { args, .. } => args.get(j) != t_args.get(j),
                _ => true,
            }) {
                diff.push(j);
            }
        }
        if diff.len() != 1 {
            return None;
        }
        let k = diff[0];
        // Non-arg parts AND all other arg positions identical: neutralize
        // slot k on every del and compare whole exprs (catches mixed
        // this()/super() or different targets).
        let mut base0 = dels[0].clone();
        if let Expr::Method { args, .. } = &mut base0 {
            args[k] = Expr::Const(ConstVal::Null);
        }
        for d in dels.iter() {
            let mut c = d.clone();
            if let Expr::Method { args, .. } = &mut c {
                args[k] = Expr::Const(ConstVal::Null);
            }
            if c != base0 {
                return None;
            }
        }
        // Slot k: bare non-param local at every site, one erased type,
        // and the carrier never read at another position.
        let mut carrier_ty: Option<JavaType> = None;
        for d in dels.iter() {
            let Expr::Method { args, .. } = d else {
                return None;
            };
            let Expr::Local { var, .. } = &args[k] else {
                return None;
            };
            if (*var as usize) >= vt.vars.len() || vt.vars[*var as usize].is_param {
                return None;
            }
            let ty = vt.vars[*var as usize].ty.erased();
            match &carrier_ty {
                Some(t) if *t == ty => {}
                Some(_) => return None,
                None => carrier_ty = Some(ty),
            }
            // Non-k positions must be param/const/static-only — a
            // computed local there would have its def extracted into
            // the helper and the ctor arg list would dangle (lark
            // audiosave/i 找不到符号 ×5-per-site). Also covers the
            // carrier leaking into another position.
            for (j, a) in args.iter().enumerate() {
                if j == k {
                    continue;
                }
                let mut bad = false;
                visit_exprs(a, &mut |x| {
                    if let Expr::Local { var: v2, .. } = x {
                        if (*v2 as usize) >= vt.vars.len() || !vt.vars[*v2 as usize].is_param {
                            bad = true;
                        }
                    }
                });
                if bad {
                    return None;
                }
            }
        }
        Some(Mode::Position(k))
        })();
        match positioned {
            Some(m) => m,
            None if full_gate => Mode::Full,
            None => return,
        }
    };
    let l = stmts.iter().rposition(is_bare_ctor_call);
    if l == Some(0) {
        return;
    }
    let (mut extracted, mut tail): (Vec<Stmt>, Vec<Stmt>) = match l {
        Some(l) => (stmts[..l].to_vec(), stmts[l + 1..].to_vec()),
        None => (stmts.clone(), Vec::new()),
    };
    if !extraction_shape_ok(&extracted) {
        return;
    }
    // Post-delegation tails (field writes / trace stubs between each
    // branch delegation and its return) are CTOR work, not helper
    // computation — they commonly touch `this`. When every site's tail
    // is identical (and equals the top-level continuation when one
    // exists) hoist one copy after the merged delegation; otherwise
    // they stay in the helper and the static_safe gate below decides.
    let mut site_tails: Vec<Vec<Stmt>> = Vec::new();
    {
        let mut probe = extracted.clone();
        site_tails_walk(&mut probe, &mut site_tails, false);
    }
    let hoist = !site_tails.is_empty() && {
        let n0 = normalize_tail(&site_tails[0]);
        site_tails.iter().all(|t| normalize_tail(t) == n0)
            && (l.is_none() || normalize_tail(&tail) == n0)
    };
    if hoist {
        site_tails_walk(&mut extracted, &mut Vec::new(), true);
        if l.is_none() {
            tail = normalize_tail(&site_tails[0]);
        }
    }
    // The delegation the ctor keeps: the top-level primary when present,
    // else any del (all non-k parts are identical).
    let primary_del: Expr = match l {
        Some(l) => match &stmts[l] {
            Stmt::ExprStmt(e) => e.clone(),
            _ => return,
        },
        None => dels[0].clone(),
    };
    let ret = match mode {
        Mode::Single(cv) => vt.var(cv).ty.erased(),
        Mode::Position(k) => match &primary_del {
            Expr::Method { args, .. } => match args.get(k) {
                Some(Expr::Local { var, .. }) => vt.var(*var).ty.erased(),
                _ => return,
            },
            _ => return,
        },
        Mode::Full => JavaType::Array(Box::new(JavaType::Object(std::sync::Arc::from(
            "java/lang/Object",
        )))),
    };
    match mode {
        Mode::Single(cv) => {
            strip_delegations_deep(&mut extracted);
            let cv_local = Expr::Local {
                var: cv,
                ty: vt.var(cv).ty.clone(),
            };
            rewrite_bare_returns(&mut extracted, &cv_local);
            if !always_returns(&extracted) {
                extracted.push(Stmt::Return(Some(cv_local)));
            }
        }
        Mode::Position(k) => {
            // Each stripped delegation's own slot-k carrier fills the
            // next bare return in its list; merge-point returns fall
            // back to the primary carrier below.
            fill_returns_by_position(&mut extracted, &|args: &[Expr]| args.get(k).cloned());
            let fallback = match &primary_del {
                Expr::Method { args, .. } => args[k].clone(),
                _ => return,
            };
            rewrite_bare_returns(&mut extracted, &fallback);
            if !always_returns(&extracted) {
                extracted.push(Stmt::Return(Some(fallback)));
            }
        }
        Mode::Full => {
            // Every delegation becomes `return new Object[]{its own
            // args}`; merge-point bare returns and the fall-through
            // completion take the PRIMARY delegation's args (the
            // top-level last del when present — its args are
            // top-level-scoped by construction). With no primary,
            // every path must already delegate explicitly.
            fill_returns_by_position(&mut extracted, &|args: &[Expr]| {
                Some(array_init(args.to_vec()))
            });
            if l.is_none() {
                // No top-level primary: every path must delegate. A
                // dead tail can still dangle after the shortest
                // always-returning prefix (DataHolder: an assign after
                // a try whose body returns and whose catch throws) —
                // truncate it (unreachable by Java's flow model, so
                // sound), else bail.
                if !always_returns(&extracted) {
                    let cut = (1..=extracted.len())
                        .find(|&i| always_returns(&extracted[..i]));
                    match cut {
                        Some(i) => extracted.truncate(i),
                        None => return,
                    }
                }
            }
            let fallback_args = match &primary_del {
                Expr::Method { args, .. } => args.clone(),
                _ => return,
            };
            let fallback = array_init(fallback_args);
            rewrite_bare_returns(&mut extracted, &fallback);
            if !always_returns(&extracted) {
                extracted.push(Stmt::Return(Some(fallback)));
            }
        }
    }
    let this_id = vt
        .vars
        .iter()
        .find(|v| v.is_param && v.name == "this")
        .map(|v| v.id);
    if !static_safe(&extracted, this_id) {
        return;
    }
    let mut defs: jdc_core::FxHashSet<u32> = jdc_core::FxHashSet::default();
    collect_def_vars(&extracted, &mut defs);
    if defs.iter().any(|v| var_occurs_in(&tail, *v)) {
        return;
    }
    let params: Vec<(u32, JavaType, String)> = vt
        .vars
        .iter()
        .filter(|v| v.is_param && v.name != "this")
        .map(|v| (v.id, v.ty.erased(), v.name.clone()))
        .collect();
    let name = helper_name(&class.name, desc_str);
    let call = Expr::Method {
        owner: None,
        cls: std::sync::Arc::from(class.name.as_str()),
        name: std::sync::Arc::from(name.as_str()),
        desc: std::sync::Arc::new(jdc_core::types::MethodDescriptor {
            ret: ret.clone(),
            args: params.iter().map(|p| p.1.clone()).collect(),
        }),
        args: params
            .iter()
            .map(|(id, ty, _)| Expr::Local {
                var: *id,
                ty: TypeRef::J(ty.clone()),
            })
            .collect(),
        is_static: true,
        is_interface: false,
        is_special: false,
        is_super: false,
        is_dynamic: false,
        type_args: Vec::new(),
    };
    let mut new_del = primary_del.clone();
    match mode {
        Mode::Single(cv) => {
            deep_rewrite(&mut new_del, &mut |x| {
                if let Expr::Local { var, .. } = x {
                    if *var == cv {
                        *x = call.clone();
                    }
                }
            });
        }
        Mode::Position(k) => {
            let Expr::Method { args, .. } = &mut new_del else {
                return;
            };
            if k >= args.len() {
                return;
            }
            args[k] = call.clone();
        }
        Mode::Full => {
            // The public ctor's one legal statement: `this(resolve$X(p..))`
            // — the synthetic relay ctor below unpacks the carriers into
            // the uniform target delegation.
            new_del = Expr::Method {
                owner: None,
                cls: std::sync::Arc::from(class.name.as_str()),
                name: "<init>".into(),
                desc: std::sync::Arc::new(jdc_core::types::MethodDescriptor {
                    args: vec![JavaType::Array(Box::new(JavaType::Object(
                        std::sync::Arc::from("java/lang/Object"),
                    )))],
                    ret: JavaType::Void,
                }),
                args: vec![call.clone()],
                is_static: false,
                is_interface: false,
                is_special: true,
                is_super: false,
                is_dynamic: false,
                type_args: Vec::new(),
            };
        }
    }
    {
        let mut m = ctor_helpers().lock().unwrap_or_else(|e| e.into_inner());
        let v = m.entry(class.name.clone()).or_default();
        if !v.iter().any(|h| h.name == name) {
            // The extracted body skips the late pipeline polish (it left
            // the ctor before cleanup ran) — invert empty thens so the
            // helper doesn't render `if (..) {} else {..}`.
            let mut hbody = Stmt::Block(extracted);
            invert_empty_thens(&mut hbody);
            v.push(CtorHelper {
                name,
                ret: ret.clone(),
                params: params.into_iter().map(|(_, ty, nm)| (ty, nm)).collect(),
                body: hbody,
                vt: vt.clone(),
            });
            if matches!(mode, Mode::Full) {
                let Expr::Method {
                    desc: d0,
                    is_super: s0,
                    ..
                } = &primary_del
                else {
                    return;
                };
                let mut rm = ctor_relays().lock().unwrap_or_else(|e| e.into_inner());
                let rv = rm.entry(class.name.clone()).or_default();
                if !rv.iter().any(|(d, _)| d == desc_str) {
                    rv.push((
                        desc_str.to_string(),
                        CtorRelay {
                            formals: d0.args.clone(),
                            is_super: *s0,
                        },
                    ));
                }
            }
        } else {
            return; // duplicate decompile of the same ctor: keep the first
        }
    }
    *stmts = std::iter::once(Stmt::ExprStmt(new_del)).chain(tail).collect();
}

/// Position-mode return fill: walk each statement list left-to-right; a
/// delegation at slot k hands its carrier to the next bare `return;` in
/// the SAME list (the bytecode shape is always `this(..); trace..;
/// return;`). With no following return in the list the delegation
/// becomes the return itself and the rest of the list (trace stubs only
/// — a second delegation there is impossible) is dropped as unreachable.
fn fill_returns_by_position(
    stmts: &mut Vec<Stmt>,
    pick: &dyn Fn(&[Expr]) -> Option<Expr>,
) {
    let mut i = 0;
    while i < stmts.len() {
        if is_bare_ctor_call(&stmts[i]) {
            let karg = match &stmts[i] {
                Stmt::ExprStmt(Expr::Method { args, .. }) => pick(args),
                _ => None,
            };
            if let Some(v) = karg {
                let mut j = i + 1;
                let mut found = None;
                while j < stmts.len() {
                    if matches!(stmts[j], Stmt::Return(None)) {
                        found = Some(j);
                        break;
                    }
                    if is_bare_ctor_call(&stmts[j]) {
                        break; // next delegation owns the next return
                    }
                    j += 1;
                }
                match found {
                    Some(j) => {
                        stmts[j] = Stmt::Return(Some(v));
                        stmts.remove(i);
                        continue;
                    }
                    None => {
                        stmts[i] = Stmt::Return(Some(v));
                        stmts.truncate(i + 1);
                        break;
                    }
                }
            }
        }
        fill_returns_one(&mut stmts[i], pick);
        i += 1;
    }
}

fn fill_returns_one(st: &mut Stmt, pick: &dyn Fn(&[Expr]) -> Option<Expr>) {
    match st {
        Stmt::Block(v) => fill_returns_by_position(v, pick),
        Stmt::If { then_stmt, else_stmt, .. } => {
            fill_returns_stmt(then_stmt, pick);
            if let Some(e) = else_stmt {
                fill_returns_stmt(e, pick);
            }
        }
        Stmt::While { body, .. }
        | Stmt::DoWhile { body, .. }
        | Stmt::ForEach { body, .. }
        | Stmt::Synchronized { body, .. }
        | Stmt::Labeled { body, .. } => fill_returns_stmt(body, pick),
        Stmt::For { init, body, .. } => {
            fill_returns_by_position(init, pick);
            fill_returns_stmt(body, pick);
        }
        Stmt::Switch { cases, default, .. } => {
            for c in cases.iter_mut() {
                fill_returns_by_position(&mut c.body, pick);
            }
            if let Some(d) = default {
                fill_returns_stmt(d, pick);
            }
        }
        Stmt::Try { body, catches, finally } | Stmt::TryWithResources { body, catches, finally, .. } => {
            fill_returns_stmt(body, pick);
            for c in catches.iter_mut() {
                fill_returns_stmt(&mut c.body, pick);
            }
            if let Some(f) = finally {
                fill_returns_stmt(f, pick);
            }
        }
        _ => {}
    }
}

fn fill_returns_stmt(st: &mut Stmt, pick: &dyn Fn(&[Expr]) -> Option<Expr>) {
    if is_bare_ctor_call(st) {
        // Single-statement branch body (`if (c) this(..);`) — becomes
        // the return directly.
        if let Stmt::ExprStmt(Expr::Method { args, .. }) = st {
            if let Some(v) = pick(args) {
                *st = Stmt::Return(Some(v));
                return;
            }
        }
    }
    fill_returns_one(st, pick);
}

fn normalize_tail(t: &[Stmt]) -> Vec<Stmt> {
    let mut v = t.to_vec();
    while matches!(v.last(), Some(Stmt::Return(None))) {
        v.pop();
    }
    v
}

/// Record (and with `carve`, remove) the statements between each
/// delegation and the next bare return in its list — a delegation with
/// no following return takes the rest of the list. Single-statement
/// branch bodies record an empty tail.
fn site_tails_walk(stmts: &mut Vec<Stmt>, out: &mut Vec<Vec<Stmt>>, carve: bool) {
    let mut i = 0;
    while i < stmts.len() {
        if is_bare_ctor_call(&stmts[i]) {
            let mut j = i + 1;
            let mut end = stmts.len();
            while j < stmts.len() {
                if matches!(stmts[j], Stmt::Return(None)) {
                    end = j;
                    break;
                }
                if is_bare_ctor_call(&stmts[j]) {
                    end = j;
                    break;
                }
                j += 1;
            }
            out.push(stmts[i + 1..end].to_vec());
            if carve {
                stmts.splice(i + 1..end, []);
            }
            i += 1;
            continue;
        }
        site_tails_one(&mut stmts[i], out, carve);
        i += 1;
    }
}

fn site_tails_one(st: &mut Stmt, out: &mut Vec<Vec<Stmt>>, carve: bool) {
    if is_bare_ctor_call(st) {
        out.push(Vec::new());
        return;
    }
    match st {
        Stmt::Block(v) => site_tails_walk(v, out, carve),
        Stmt::If { then_stmt, else_stmt, .. } => {
            site_tails_one(then_stmt, out, carve);
            if let Some(e) = else_stmt {
                site_tails_one(e, out, carve);
            }
        }
        Stmt::While { body, .. }
        | Stmt::DoWhile { body, .. }
        | Stmt::ForEach { body, .. }
        | Stmt::Synchronized { body, .. }
        | Stmt::Labeled { body, .. } => site_tails_one(body, out, carve),
        Stmt::For { init, body, .. } => {
            site_tails_walk(init, out, carve);
            site_tails_one(body, out, carve);
        }
        Stmt::Switch { cases, default, .. } => {
            for c in cases.iter_mut() {
                site_tails_walk(&mut c.body, out, carve);
            }
            if let Some(d) = default {
                site_tails_one(d, out, carve);
            }
        }
        Stmt::Try { body, catches, finally } | Stmt::TryWithResources { body, catches, finally, .. } => {
            site_tails_one(body, out, carve);
            for c in catches.iter_mut() {
                site_tails_one(&mut c.body, out, carve);
            }
            if let Some(f) = finally {
                site_tails_one(f, out, carve);
            }
        }
        _ => {}
    }
}

/// Can this statement list NOT complete normally? Conservative for
/// loops (false); If needs BOTH branches, Switch needs every case plus
/// a default. Used to decide whether the extracted helper needs the
/// trailing `return carrier;` (appending one after exhaustive branches
/// is javac's "unreachable statement" error).
fn always_returns(stmts: &[Stmt]) -> bool {
    let Some(last) = stmts
        .iter()
        .rev()
        .find(|s| !matches!(s, Stmt::Block(v) if v.is_empty()))
    else {
        return false;
    };
    match last {
        Stmt::Return(Some(_)) | Stmt::Throw(_) => true,
        Stmt::Block(v) => always_returns(v),
        Stmt::If { then_stmt, else_stmt, .. } => match else_stmt {
            Some(e) => always_returns_one(then_stmt) && always_returns_one(e),
            None => false,
        },
        Stmt::Labeled { body, .. } | Stmt::Synchronized { body, .. } => {
            always_returns_one(body)
        }
        Stmt::Switch { cases, default, .. } => {
            default.as_ref().is_some_and(|d| always_returns_one(d))
                && cases.iter().all(|c| always_returns(&c.body))
        }
        Stmt::Try { body, catches, finally }
        | Stmt::TryWithResources { body, catches, finally, .. } => {
            finally.as_ref().is_some_and(|f| always_returns_one(f))
                || (always_returns_one(body)
                    && catches.iter().all(|c| always_returns_one(&c.body)))
        }
        _ => false,
    }
}

fn always_returns_one(st: &Stmt) -> bool {
    always_returns(std::slice::from_ref(st))
}

fn inline_ctor_arg_defs(
    call: &mut Stmt,
    prefix: &[&Stmt],
    vt: &VarTable,
    consumed: &mut Vec<(u32, usize)>,
) -> bool {
    let Stmt::ExprStmt(cexpr) = call else {
        return false;
    };
    // Replace every read of `v` in `e` with its value-so-far; a read
    // BEFORE any def (val None) is unresolvable → false.
    fn subst(e: &mut Expr, v: u32, val: &Option<Expr>) -> bool {
        let mut ok = true;
        deep_rewrite(e, &mut |x| {
            if let Expr::Local { var, .. } = x {
                if *var == v {
                    match val {
                        Some(e2) => *x = e2.clone(),
                        None => ok = false,
                    }
                }
            }
        });
        ok
    }
    let mut seen: jdc_core::FxHashSet<u32> = jdc_core::FxHashSet::default();
    for _ in 0..32 {
        let mut need: Option<u32> = None;
        visit_exprs(cexpr, &mut |x| {
            if need.is_none() {
                if let Expr::Local { var, .. } = x {
                    if (*var as usize) < vt.vars.len() && !vt.vars[*var as usize].is_param {
                        need = Some(*var);
                    }
                }
            }
        });
        let Some(v) = need else { break };
        if !seen.insert(v) {
            return false; // cyclic reuse (`str = .. + str`) — bail
        }
        // CHAIN walk: fold EVERY prefix def of v into the value the
        // delegation actually receives. The old strict single-occurrence
        // rule rejected the convenience-ctor reassignment chain
        // (`f = getInstance(ctx); f = transform(f, ..); this(ctx, f)` —
        // androidx CameraController, weibo this-not-first ×831): the
        // SOURCE inlined the computation into the delegation args and
        // R8 decomposed it into register writes. Reads of v are allowed
        // only INSIDE its own def chain; any other prefix statement
        // touching v means the value escapes or the object mutates
        // (`stringBuilder.append(..)` — inlining a fresh `new
        // StringBuilder()` would lose the message), so bail.
        let mut val: Option<Expr> = None;
        let mut ok = true;
        for (si, st) in prefix.iter().enumerate() {
            match st {
                // A bare declaration (init None — the structurer hoists
                // `T v;` to the top) carries no value: skip it. Failing
                // here rejected every chain under a hoisted decl (ci1/a).
                Stmt::LocalDef { var, init: None, .. } if *var == v => {}
                Stmt::LocalDef { var, init, .. } if *var == v => {
                    let Some(e) = init else { ok = false; break };
                    let mut e2 = (*e).clone();
                    if !subst(&mut e2, v, &val) {
                        ok = false;
                        break;
                    }
                    val = Some(e2);
                    consumed.push((v, si));
                }
                Stmt::ExprStmt(Expr::Assign { target, op, .. })
                    if matches!(&**target, Expr::Local { var: tv, .. } if *tv == v) =>
                {
                    if !matches!(op, AssignOp::Plain) {
                        ok = false; // compound write: not a plain def
                        break;
                    }
                    let Stmt::ExprStmt(Expr::Assign { value, .. }) = st else {
                        unreachable!()
                    };
                    let mut e2 = value.as_ref().clone();
                    if !subst(&mut e2, v, &val) {
                        ok = false;
                        break;
                    }
                    val = Some(e2);
                    consumed.push((v, si));
                }
                _ => {
                    let mut touched = false;
                    visit_all_exprs(st, &mut |x| {
                        if let Expr::Local { var, .. } = x {
                            if *var == v {
                                touched = true;
                            }
                        }
                    });
                    if touched {
                        ok = false;
                        break;
                    }
                }
            }
        }
        if !ok {
            return false;
        }
        let Some(de) = val else { return false };
        if contains_this_access(&de) {
            return false;
        }
        // Huge defs multiply per read site per round — bound them.
        let mut de_nodes = 0usize;
        visit_exprs(&de, &mut |_| de_nodes += 1);
        if de_nodes > 256 {
            return false;
        }
        deep_rewrite(cexpr, &mut |x| {
            if let Expr::Local { var, .. } = x {
                if *var == v {
                    *x = de.clone();
                }
            }
        });
        // Exponential blowup guard: a var read twice doubles its def
        // per round, compounding across the transitive chain — cap the
        // expression size (a 2^k node tree overflowed the worker stack
        // process-wide).
        let mut nodes = 0usize;
        visit_exprs(cexpr, &mut |_| nodes += 1);
        if nodes > 2_048 {
            return false;
        }
    }
    true
}

/// Fold a prelude StringBuilder mutation chain into the delegation
/// argument as a fluent chain: `StringBuilder sb = new SB(x);
/// sb.append(a); sb.append(b); super(sb.toString())` →
/// `super(new SB(x).append(a).append(b).toString())`. The generic def
/// inliner rejects mutation calls — their effect is not expressible as
/// a value — but `append` RETURNS its receiver, so the chain
/// reconstructs exactly the same mutations in one expression (androidx
/// emoji2/flatbuffer UnpairedSurrogateException family, the shapes the
/// strict-occurrence rule was written to refuse). Aborts on any `this`
/// touch inside the chain (illegal before super) or any other read of
/// the SB local (the value must not escape the fold). Returns the
/// delegation's new index (statements before it were removed).
fn fold_sb_chain(stmts: &mut Vec<Stmt>, pos: usize) -> usize {
    // 1. the SB local: first `v = new StringBuilder(..)` in the prefix.
    let mut sbv: Option<u32> = None;
    let mut def_i: Option<usize> = None;
    let mut def_init: Option<Expr> = None;
    for (i, st) in stmts[..pos].iter().enumerate() {
        let cand: Option<(u32, &Expr)> = match st {
            Stmt::LocalDef {
                var,
                init: Some(e),
                ..
            } => Some((*var, e)),
            Stmt::ExprStmt(Expr::Assign {
                target,
                value,
                op: AssignOp::Plain,
                ..
            }) => match &**target {
                Expr::Local { var, .. } => Some((*var, value.as_ref())),
                _ => None,
            },
            _ => None,
        };
        if let Some((v, init)) = cand {
            if let Expr::New { cls, .. } = init {
                if cls.as_ref() == "java/lang/StringBuilder" {
                    sbv = Some(v);
                    def_i = Some(i);
                    def_init = Some(init.clone());
                    break;
                }
            }
        }
    }
    let (Some(sbv), Some(def_i), Some(init)) = (sbv, def_i, def_init) else {
        return pos;
    };
    if contains_this_access(&init) {
        return pos;
    }
    // 2. append statements on sbv, in order.
    let mut appends: Vec<usize> = Vec::new();
    for (i, st) in stmts[..pos].iter().enumerate() {
        if let Stmt::ExprStmt(Expr::Method {
            owner: Some(o),
            cls,
            name,
            ..
        }) = st
        {
            if matches!(&**o, Expr::Local { var, .. } if *var == sbv)
                && cls.as_ref() == "java/lang/StringBuilder"
                && &**name == "append"
            {
                appends.push(i);
            }
        }
    }
    // 3. no read of sbv after the delegation.
    let mut post_read = false;
    for st in stmts.iter().skip(pos + 1) {
        visit_all_exprs(st, &mut |e| {
            if let Expr::Local { var, .. } = e {
                if *var == sbv {
                    post_read = true;
                }
            }
        });
    }
    if post_read {
        return pos;
    }
    // 4. prefix reads of sbv only in the def and the append receivers
    //    (arguments are checked for `this`, receivers are the chain).
    for (i, st) in stmts[..pos].iter().enumerate() {
        if i == def_i || appends.contains(&i) {
            // append ARGUMENTS must not touch this; receiver is fine.
            if let Stmt::ExprStmt(Expr::Method { args, .. }) = st {
                if args.iter().any(contains_this_access) {
                    return pos;
                }
            }
            continue;
        }
        let mut leak = false;
        visit_all_exprs(st, &mut |e| {
            if let Expr::Local { var, .. } = e {
                if *var == sbv {
                    leak = true;
                }
            }
        });
        if leak {
            return pos;
        }
    }
    // 5. the call reads sbv exactly once, as `sbv.toString()`.
    let mut local_reads = 0usize;
    let mut tostring_reads = 0usize;
    visit_all_exprs(&stmts[pos], &mut |e| match e {
        Expr::Local { var, .. } if *var == sbv => local_reads += 1,
        Expr::Method {
            owner: Some(o),
            name,
            args,
            ..
        } if args.is_empty()
            && &**name == "toString"
            && matches!(&**o, Expr::Local { var, .. } if *var == sbv) =>
        {
            tostring_reads += 1;
        }
        _ => {}
    });
    if local_reads != 1 || tostring_reads != 1 {
        return pos;
    }
    // 6. build the fluent chain and swap it into the toString receiver.
    let mut chain = init;
    for ai in &appends {
        if let Stmt::ExprStmt(Expr::Method {
            owner: _,
            cls,
            name,
            desc,
            args,
            is_static,
            is_interface,
            is_special,
            is_super,
            is_dynamic,
            type_args,
            ..
        }) = &stmts[*ai]
        {
            chain = Expr::Method {
                owner: Some(Box::new(chain)),
                cls: cls.clone(),
                name: name.clone(),
                desc: desc.clone(),
                args: args.clone(),
                is_static: *is_static,
                is_interface: *is_interface,
                is_special: *is_special,
                is_super: *is_super,
                is_dynamic: *is_dynamic,
                type_args: type_args.clone(),
            };
        }
    }
    let mut replaced = false;
    walk_stmt_exprs(&mut stmts[pos], &mut |e| {
        deep_rewrite(e, &mut |x| {
            if let Expr::Method {
                owner: Some(o),
                name,
                args,
                ..
            } = x
            {
                if args.is_empty()
                    && &**name == "toString"
                    && matches!(&**o, Expr::Local { var, .. } if *var == sbv)
                {
                    **o = chain.clone();
                    replaced = true;
                }
            }
        });
    });
    if !replaced {
        return pos; // never remove without the swap — atomicity
    }
    // 7. drop the consumed statements (descending), adjust pos.
    let mut removed: Vec<usize> = appends.clone();
    removed.push(def_i);
    removed.sort_unstable();
    removed.dedup();
    for i in removed.iter().rev() {
        stmts.remove(*i);
    }
    pos - removed.len()
}

pub fn fix_ctor_super_first(body: &mut Stmt, vt: &VarTable) {
    let Stmt::Block(stmts) = body else { return };
    if stmts.is_empty() || is_bare_ctor_call(stmts.first().unwrap()) {
        return;
    }
    // Top-level delegation call.
    if let Some(pos0) = stmts.iter().position(is_bare_ctor_call) {
        let pos = fold_sb_chain(stmts, pos0);
        let mut call = stmts[pos].clone();
        let prefix: Vec<&Stmt> = stmts[..pos].iter().collect();
        let mut consumed: Vec<(u32, usize)> = Vec::new();
        if !inline_ctor_arg_defs(&mut call, &prefix, vt, &mut consumed) {
            return;
        }
        stmts.remove(pos);
        // Drop the chain defs the inline consumed (all indices < pos) —
        // leaving them would DOUBLE-evaluate their (possibly impure)
        // inits. But ONLY for vars with no post-delegation use: dropping
        // such a var's DECLARATION makes ensure_declared re-add it at
        // the method top, i.e. BEFORE this() — a fresh "对this的调用必须
        // 是构造器中的第一个语句" (lark +587 the blind drop caused).
        let mut drop_idx: Vec<usize> = consumed
            .iter()
            .filter(|(v, _)| !var_occurs_in(&stmts[pos..], *v))
            .map(|(_, k)| *k)
            .collect();
        drop_idx.sort_unstable();
        drop_idx.dedup();
        for k in drop_idx.iter().rev() {
            stmts.remove(*k);
        }
        stmts.insert(0, call);
        return;
    }
    // The structurer can wrap the delegation in a bare nested block —
    // with the parameter null-checks AHEAD of it (weixin's Kotlin
    // intrinsics shape: `o.h(parcel, "source"); super();` at inner
    // positions 0/1, flattened to straight-line by cleanup AFTER this
    // pass, which is why the miss surfaced as 1150 weixin "对super的
    // 调用必须是构造器中的第一个语句"). Hoist the call from any
    // position whose predecessors are all straight-line statements; a
    // delegation behind control flow is the conditional-super family
    // (needs restructuring, not hoisting) and stays put.
    let mut found: Option<(usize, usize)> = None;
    for (i, st) in stmts.iter().enumerate() {
        if let Stmt::Block(inner) = st {
            if let Some(p) = inner.iter().position(is_bare_ctor_call) {
                if inner[..p]
                    .iter()
                    .all(|s| matches!(s, Stmt::LocalDef { .. } | Stmt::ExprStmt(_)))
                {
                    found = Some((i, p));
                    break;
                }
            }
        }
    }
    if let Some((i, p)) = found {
        let mut call = match &stmts[i] {
            Stmt::Block(inner) => inner[p].clone(),
            _ => unreachable!(),
        };
        let mut prefix: Vec<&Stmt> = stmts[..i].iter().collect();
        if let Stmt::Block(inner) = &stmts[i] {
            prefix.extend(inner[..p].iter());
        }
        let mut consumed: Vec<(u32, usize)> = Vec::new();
        if !inline_ctor_arg_defs(&mut call, &prefix, vt, &mut consumed) {
            return;
        }
        // Prefix index k maps to stmts[k] for k < i, inner[k - i] for
        // k >= i (all such k are < p). Suffix = the rest of the inner
        // block + the following top-level statements; a consumed def may
        // only drop when its var has no suffix use (declaration loss →
        // ensure_declared re-adds it BEFORE this()).
        // Compute the droppable set BEFORE mutating (borrow split).
        let mut ks: Vec<usize> = Vec::new();
        {
            let inner_tail: &[Stmt] = match &stmts[i] {
                Stmt::Block(inner) if p < inner.len() => &inner[p + 1..],
                _ => &[],
            };
            for (v, k) in &consumed {
                if !var_occurs_in(inner_tail, *v) && !var_occurs_in(&stmts[i + 1..], *v) {
                    ks.push(*k);
                }
            }
        }
        ks.sort_unstable();
        ks.dedup();
        if let Stmt::Block(inner) = &mut stmts[i] {
            inner.remove(p);
            for k in ks.iter().rev().filter(|k| **k >= i) {
                inner.remove(k - i);
            }
        }
        for k in ks.iter().rev().filter(|k| **k < i) {
            stmts.remove(*k);
        }
        stmts.insert(0, call);
    }
}

/// A true constructor DELEGATION statement: `super(..)` (owner None) or
/// `this(..)` (owner This). An un-folded `new X; <init>` (lost
/// allocation — the ctor-fold-bug family) arrives as a `<init>` Method
/// with a NON-this owner and prints as `new X(..)`: it is not a
/// delegation and must neither satisfy nor anchor the super-first fixes
/// (v7.a$a2: a dead `new f(0,..)` init at position 0 made both hoists
/// believe the delegation was already first, leaving the real `super`
/// last — "对super的调用必须是构造器中的第一个语句").
fn is_delegation_expr(e: &Expr) -> bool {
    matches!(
        e,
        Expr::Method { name, is_special: true, owner, .. }
            if &**name == "<init>"
                && (owner.is_none() || matches!(owner.as_deref(), Some(Expr::This)))
    )
}

pub(crate) fn is_bare_ctor_call(s: &Stmt) -> bool {
    matches!(s, Stmt::ExprStmt(e) if is_delegation_expr(e))
}

/// Is the local ever ASSIGNED (write position: `x = ..`, `++x`)?
/// Distinct from a read count: a ctor's synthetic outer param is only
/// ever read; a param that gets written must stay declared.
pub(crate) fn local_is_written(body: &Stmt, var: u32) -> bool {
    let mut hit = false;
    visit_stmt_exprs_ro(body, &mut |e| {
        if hit {
            return;
        }
        visit_exprs(e, &mut |x| {
            if hit {
                return;
            }
            match x {
                Expr::Assign { target, .. }
                    if matches!(&**target, Expr::Local { var: v, .. } if *v == var) =>
                {
                    hit = true;
                }
                Expr::PreIncDec { e, .. } | Expr::PostIncDec { e, .. }
                    if matches!(&**e, Expr::Local { var: v, .. } if *v == var) =>
                {
                    hit = true;
                }
                _ => {}
            }
        });
    });
    hit
}

/// Source-form normalization for a non-static member-inner constructor.
/// The dex descriptor carries the synthetic outer instance as args[0]
/// (typed as the direct enclosing class); the emitter's qualified
/// `outer.new Inner(..)` / `this.new Inner(..)` sites pass it
/// implicitly and `super(..)` delegations drop it, so the signature
/// loses the param — and the body must stop referencing it: plain uses
/// become `Outer.this` (a `this.this$0 = p` assignment becomes
/// `this.this$0 = Outer.this`, exactly the runtime value), and
/// this()-delegations to `eligible` classes drop the leading param arg.
pub(crate) fn rewrite_inner_ctor_outer_param(
    body: &mut Stmt,
    param0: u32,
    outer_display: &str,
    outer_ty: &TypeRef,
    eligible: &[String],
) {
    // this()/super() delegations (and lost-alloc construction calls):
    // the leading arg IS the synthetic outer exactly when it is the
    // param itself.
    walk_stmt_exprs(body, &mut |e| {
        if let Expr::Method { name, cls, args, .. } = e {
            if &**name == "<init>"
                && eligible.iter().any(|c| c.as_str() == cls.as_ref())
                && args.first().is_some_and(|a| matches!(a, Expr::Local { var: v, .. } if *v == param0))
            {
                args.remove(0);
            }
        }
    });
    // Every remaining read of the param becomes `Outer.this`.
    let text = format!("{outer_display}.this");
    let ty = outer_ty.clone();
    walk_stmt_exprs(body, &mut |e| {
        deep_rewrite_reads(e, &mut |x| {
            if let Expr::Local { var: v, .. } = x {
                if *v == param0 {
                    *x = Expr::RawT(text.clone(), ty.clone());
                }
            }
        });
    });
}

/// Ctors whose delegation is buried in control flow or behind arg
/// computations — the "对super的调用必须是构造器中的第一个语句" family
/// that plain hoisting cannot reach (weixin 264 / weibo 116 / reqable 71
/// / lark 62 after round 60): R8/Kotlin compute super-args conditionally
/// and dex legally invokes super mid-branch.
///
/// Shape B (branch merge): `prelude; if (c) {d1; super(A1); t1} else
/// {d2; super(A2); t2}; post` → one leading `super(..)` where each
/// differing arg becomes `c ? a1i : a2i`; branch remainders stay an
/// if/else AFTER the call; prelude statements move after it (nothing
/// may precede super in Java; bytecode pre-super work is local-only by
/// construction — dex forbids instance-field reads before the delegate).
///
/// Shape A (linear): `defs; super(A); rest` where `A` references the
/// def locals — plain hoisting (fix_ctor_super_first case 1) would move
/// the call above its argument definitions (forward refs). Inline the
/// referenced defs into the args first, drop the consumed defs, hoist.
/// Runs BEFORE fix_ctor_super_first and only fires when args reference
/// preceding locals, leaving the battle-tested plain hoist untouched
/// otherwise.
///
/// Inlining duplicates the def per use site, so a side-effecting def is
/// inlined only when its var's use count is provably preserved: a
/// single use in a merged arg (1 copy), or a single use in the
/// condition with ≤1 differing arg (the cond then appears in the `if`
/// plus one ternary — 2 copies of e.g. one Kotlin getter call, which
/// the original bytecode typically made twice anyway: `p.u() == null ?
/// .. : p.u().i()`). Pure defs inline freely. Every other shape aborts:
/// broken-but-faithful stays the status quo, wrong evaluation counts
/// would be worse.
/// Linear `def; delegation(args read def)` shapes. A plain hoist of the
/// delegation past its prefix would place the call ABOVE a definition
/// it reads — a forward reference (`super(context2, ..)` with `context2
/// = ..` below it). javac's attribution for the whole class then
/// collapses: the undefined identifier cascades into "non-static
/// super" and even bare `Object` resolution failures, poisoning every
/// later diagnostic in the file (weibo AppCompatTextView — the root of
/// a 4.1k-file Object cascade). The original source had the def's
/// expression INLINE in the delegation args; d8 computed it into a
/// register first. Inline the def into the args (the def is read
/// exactly once there — evaluation count preserved even for calls),
/// and when the register is REASSIGNED later (a fresh generation
/// sharing the slot), convert that first write into the declaration so
/// the var stays defined for its later readers. Any shape that cannot
/// be proven aborts untouched.
/// Count WRITE positions of `var` (assign / inc-dec targets) — the
/// shape-C validator needs exact counts, not just presence.
fn stmts_count_writes(stmts: &[Stmt], var: u32) -> usize {
    let mut n = 0usize;
    for s in stmts {
        visit_stmt_exprs_ro(s, &mut |e| {
            visit_exprs(e, &mut |x| match x {
                Expr::Assign { target, .. }
                    if matches!(&**target, Expr::Local { var: v, .. } if *v == var) =>
                {
                    n += 1;
                }
                Expr::PreIncDec { e, .. } | Expr::PostIncDec { e, .. }
                    if matches!(&**e, Expr::Local { var: v, .. } if *v == var) =>
                {
                    n += 1;
                }
                _ => {}
            });
        });
    }
    n
}

/// Collect every local WRITTEN in `stmts` — the complement inside a
/// ctor prefix is the param set (the only locals a pre-delegation arg
/// expression may read).
fn prefix_written_vars(stmts: &[Stmt]) -> jdc_core::FxHashSet<u32> {
    let mut set = jdc_core::FxHashSet::default();
    for s in stmts {
        let mut c = s.clone();
        walk_stmt_exprs(&mut c, &mut |e| {
            deep_rewrite(e, &mut |x| match x {
                Expr::Assign { target, .. } => {
                    if let Expr::Local { var, .. } = &**target {
                        set.insert(*var);
                    }
                }
                Expr::PreIncDec { e, .. } | Expr::PostIncDec { e, .. } => {
                    if let Expr::Local { var, .. } = &**e {
                        set.insert(*var);
                    }
                }
                _ => {}
            });
        });
        if let Stmt::LocalDef { var, .. } = s {
            set.insert(*var);
        }
    }
    set
}

/// Is `e` safe to evaluate INSIDE a ctor-delegation argument (before
/// this/super exists)? Params/consts/pure ops only — no instance-field
/// reads, no `this`, no captures.
fn pre_this_safe(e: &Expr, prefix_vars: &jdc_core::FxHashSet<u32>) -> bool {
    match e {
        Expr::Local { var, .. } => !prefix_vars.contains(var),
        Expr::Const(_) => true,
        Expr::Cast { e, .. } => pre_this_safe(e, prefix_vars),
        Expr::Bin { l, r, .. } => {
            pre_this_safe(l, prefix_vars) && pre_this_safe(r, prefix_vars)
        }
        Expr::Cond { c, t, f } => {
            pre_this_safe(c, prefix_vars)
                && pre_this_safe(t, prefix_vars)
                && pre_this_safe(f, prefix_vars)
        }
        Expr::Field { owner, is_static, .. } => {
            *is_static
                && owner
                    .as_ref()
                    .is_none_or(|o| pre_this_safe(o, prefix_vars))
        }
        Expr::Method {
            owner,
            args,
            is_static,
            ..
        } => {
            args.iter().all(|a| pre_this_safe(a, prefix_vars))
                && match owner {
                    Some(o) => pre_this_safe(o, prefix_vars),
                    None => *is_static,
                }
        }
        // Allocation in a delegation arg is legal Java before this()
        // (the ctor call touches no instance state of THIS class) — the
        // missing arm sent every `new Date()`-style Kotlin default
        // through `_ => false`, rejecting the whole fold (lark
        // Request$AppToApp this-not-first family, ~650 lark lines).
        // An inner-class construction needing an outer `this` carries
        // Expr::This in its args, which still fails below.
        Expr::New { args, .. } => args.iter().all(|a| pre_this_safe(a, prefix_vars)),
        Expr::NewArray { dims, init, .. } => {
            dims.iter().all(|d| pre_this_safe(d, prefix_vars))
                && init
                    .as_ref()
                    .map(|l| l.iter().all(|x| pre_this_safe(x, prefix_vars)))
                    .unwrap_or(true)
        }
        // Pure unary/instanceof/array reads.
        Expr::Un { e, .. } | Expr::InstanceOf { e, .. } => pre_this_safe(e, prefix_vars),
        Expr::ArrayIndex { array, index } => {
            pre_this_safe(array, prefix_vars) && pre_this_safe(index, prefix_vars)
        }
        _ => false,
    }
}

/// `s` is exactly one plain assignment to local `v` (optionally wrapped
/// in a singleton Block): the branch shape of a Kotlin default-arg
/// override.
fn single_assign_to_v(s: &Stmt, v: u32) -> Option<Expr> {
    match s {
        Stmt::ExprStmt(Expr::Assign {
            target,
            op: AssignOp::Plain,
            value,
        }) if matches!(&**target, Expr::Local { var: vv, .. } if *vv == v) => {
            Some((**value).clone())
        }
        Stmt::Block(xs) if xs.len() == 1 => single_assign_to_v(&xs[0], v),
        _ => None,
    }
}

/// Kotlin default-argument BRIDGE ctors (shape C of the delegation
/// family): `<init>(params.., int mask, DefaultConstructorMarker)`
/// computes each defaulted arg into a local — `v = p; if ((mask&bit)
/// != 0) { v = DEFAULT; }` — then delegates `this(.., v, ..)`. Java
/// allows NOTHING before the delegation, so the computations must fold
/// INTO the args (`this(.., (mask&bit) != 0 ? DEFAULT : p, ..)`).
/// Unfolded, the hoist passes lift the delegation above the raw local
/// reads: "找不到符号 变量 str2/v4/creationExtras2" — 2.5k root files
/// across weixin/lark/weibo (weixin MvvmObserverOwner$..Observer, lark
/// SimpleArrayMap/LongSparseArray). Per-var provability: exactly one
/// top-level base write, an optional single conditional override whose
/// branches are lone assignments, no other prefix touch of the var,
/// nothing after the delegation touches it, and the folded expression
/// is pre-this-safe. Unprovable vars stay untouched (partial folds are
/// strictly better — the rest was broken before too).
/// Flatten nested plain Blocks among the top-level statements. The
/// structurer hands ctor bodies as region-nested blocks (`Block([def,
/// Block([assign, if]), Block([delegate, return])])` — the Kotlin
/// bridge shape), and every delegation-repair pass scans the TOP level
/// only. Straight-line nesting flattens without semantics; control
/// flow lives inside If/While/etc. arms, never as a direct member.
pub(crate) fn flatten_top_blocks(stmts: &mut Vec<Stmt>) {
    fn rec(s: Stmt, out: &mut Vec<Stmt>) {
        match s {
            Stmt::Block(inner) => {
                for x in inner {
                    rec(x, out);
                }
            }
            other => out.push(other),
        }
    }
    if !stmts.iter().any(|s| matches!(s, Stmt::Block(_))) {
        return;
    }
    let mut out: Vec<Stmt> = Vec::with_capacity(stmts.len());
    for s in std::mem::take(stmts) {
        rec(s, &mut out);
    }
    *stmts = out;
}

fn fold_default_arg_bridge(stmts: &mut Vec<Stmt>) -> bool {
    flatten_top_blocks(stmts);
    let Some(pos) = stmts.iter().position(is_bare_ctor_call) else {
        return false;
    };
    if pos == 0 {
        return false;
    }
    let Stmt::ExprStmt(Expr::Method { args, .. }) = &stmts[pos] else {
        return false;
    };
    let mut arg_vars: Vec<u32> = Vec::new();
    let mut reads = 0usize;
    for a in args {
        let mut c = a.clone();
        deep_rewrite_reads(&mut c, &mut |x| {
            if let Expr::Local { var, .. } = x {
                reads += 1;
                if !arg_vars.contains(var) {
                    arg_vars.push(*var);
                }
            }
        });
    }
    if arg_vars.is_empty() || reads != arg_vars.len() {
        return false; // a var read twice would duplicate evaluation
    }
    // The bridge ENDS at the delegation: nothing after may touch the
    // arg vars (reads OR writes).
    for &v in &arg_vars {
        if stmts_read_var(&stmts[pos + 1..], v) != 0
            || stmts_count_writes(&stmts[pos + 1..], v) != 0
        {
            return false;
        }
    }
    let prefix_vars = prefix_written_vars(&stmts[..pos]);
    let mut repl: Vec<(u32, Expr)> = Vec::new();
    let mut drop: Vec<usize> = Vec::new();
    for &v in &arg_vars {
        // Exact prefix shape for v.
        let mut base: Option<(usize, Expr)> = None;
        let mut decl: Option<usize> = None;
        let mut condf: Option<(usize, Expr, Option<Expr>, Option<Expr>)> = None;
        let mut ok = true;
        for (i, s) in stmts[..pos].iter().enumerate() {
            match s {
                Stmt::LocalDef { var, init, .. } if *var == v => match init {
                    None => {
                        if decl.is_some() {
                            ok = false;
                            break;
                        }
                        decl = Some(i);
                    }
                    Some(e) => {
                        if base.is_some() {
                            ok = false;
                            break;
                        }
                        base = Some((i, e.clone()));
                    }
                },
                Stmt::ExprStmt(Expr::Assign {
                    target,
                    op: AssignOp::Plain,
                    value,
                }) if matches!(&**target, Expr::Local { var: vv, .. } if *vv == v) => {
                    if base.is_some() {
                        ok = false;
                        break;
                    }
                    base = Some((i, (**value).clone()));
                }
                Stmt::If {
                    cond,
                    then_stmt,
                    else_stmt,
                } => {
                    let t = single_assign_to_v(then_stmt, v);
                    let f = else_stmt.as_ref().and_then(|b| single_assign_to_v(b, v));
                    let then_touches = t.is_some()
                        || stmts_read_var(std::slice::from_ref(&**then_stmt), v) != 0
                        || stmts_count_writes(std::slice::from_ref(&**then_stmt), v) != 0;
                    let else_touches = else_stmt.as_ref().is_some_and(|b| {
                        f.is_some()
                            || stmts_read_var(std::slice::from_ref(b), v) != 0
                            || stmts_count_writes(std::slice::from_ref(b), v) != 0
                    });
                    if !then_touches && !else_touches {
                        continue; // unrelated if
                    }
                    // Both branches must be lone assignments (a branch
                    // with its own delegation/return is the conditional-
                    // super machinery's shape, not this one), the cond
                    // must not read v, and only one override per var.
                    if condf.is_some()
                        || base.is_none()
                        || (then_touches && t.is_none())
                        || (else_touches && f.is_none())
                    {
                        ok = false;
                        break;
                    }
                    let cond_stmt = Stmt::ExprStmt(cond.clone());
                    if stmts_read_var(std::slice::from_ref(&cond_stmt), v) != 0 {
                        ok = false;
                        break;
                    }
                    condf = Some((i, cond.clone(), t, f));
                }
                _ => {}
            }
        }
        if !ok {
            continue;
        }
        let Some((bi, be)) = base else { continue };
        // Exact write/read accounting: base (+then/else) writes, zero
        // prefix reads.
        let want_writes = 1
            + condf
                .as_ref()
                .map(|(_, _, t, f)| t.is_some() as usize + f.is_some() as usize)
                .unwrap_or(0);
        if stmts_count_writes(&stmts[..pos], v) != want_writes
            || stmts_read_var(&stmts[..pos], v) != 0
        {
            continue;
        }
        let rep = match condf {
            Some((ci, c, t, f)) => {
                drop.push(ci);
                match (t, f) {
                    (Some(t), Some(f)) => Expr::Cond {
                        c: Box::new(c),
                        t: Box::new(t),
                        f: Box::new(f),
                    },
                    (Some(t), None) => Expr::Cond {
                        c: Box::new(c),
                        t: Box::new(t),
                        f: Box::new(be.clone()),
                    },
                    (None, Some(f)) => Expr::Cond {
                        c: Box::new(c),
                        t: Box::new(be.clone()),
                        f: Box::new(f),
                    },
                    (None, None) => unreachable!("condf requires a branch assign"),
                }
            }
            None => be.clone(),
        };
        if !pre_this_safe(&rep, &prefix_vars) {
            continue;
        }
        drop.push(bi);
        if let Some(d) = decl {
            drop.push(d);
        }
        repl.push((v, rep));
    }
    if repl.is_empty() {
        return false;
    }
    if let Stmt::ExprStmt(Expr::Method { args, .. }) = &mut stmts[pos] {
        for a in args.iter_mut() {
            deep_rewrite_reads(a, &mut |x| {
                if let Expr::Local { var, .. } = x {
                    if let Some((_, e)) = repl.iter().find(|(vv, _)| vv == var) {
                        *x = e.clone();
                    }
                }
            });
        }
    }
    drop.sort_unstable();
    drop.dedup();
    for i in drop.into_iter().rev() {
        stmts.remove(i);
    }
    true
}

/// Rewrite static member references on Kotlin multi-file facade PARTS
/// (`StringsKt__StringsKt.trim(..)`) to the public facade the parts are
/// compiled into (`StringsKt.trim(..)`): the parts are package-private,
/// so every cross-package call site is "在kotlin.text中不是公共的; 无法
/// 从外部程序包中对其进行访问" (lark 1.4k root files; weibo's 8.4k-line
/// Kt cluster). Static members resolve through the facade by
/// inheritance (the map is gated on the extends chain).
pub fn rewrite_kotlin_facades(body: &mut Stmt, pool: &crate::DexPool) {
    let map = pool.kotlin_facade_map();
    if map.is_empty() {
        return;
    }
    walk_stmt_exprs(body, &mut |e| {
        match e {
            Expr::Method {
                cls,
                name,
                desc,
                is_static,
                ..
            } if *is_static => {
                if let Some((base, cover)) = map.get(cls.as_ref()) {
                    let hit = match cover {
                        None => true,
                        Some(c) => c.0.contains(&(name.to_string(), desc.to_string())),
                    };
                    if hit {
                        *cls = std::sync::Arc::from(base.as_str());
                    }
                }
            }
            Expr::Field {
                cls,
                name,
                is_static,
                ..
            } if *is_static => {
                if let Some((base, cover)) = map.get(cls.as_ref()) {
                    let hit = match cover {
                        None => true,
                        Some(c) => c.1.contains(name.as_ref()),
                    };
                    if hit {
                        *cls = std::sync::Arc::from(base.as_str());
                    }
                }
            }
            _ => {}
        }
    });
}

/// Kotlin default-arg bridge fold for ENUM ctors: the synthetic
/// `(String, int, .., int, DefaultConstructorMarker)` ctor computes
/// defaulted args then chains `this(..)` mid-body — Java requires the
/// delegation first (对this的调用必须是构造器中的第一个语句, lark's
/// UserCustomStatusExtraParams$* family, 752 lines). Same fold the
/// class-ctor path runs; the enum branch of the pipeline used to skip
/// it (only strip_enum_ctor_super ran there).
pub fn fold_enum_default_arg_bridge(body: &mut Stmt) {
    let Stmt::Block(stmts) = body else { return };
    fold_default_arg_bridge(stmts);
}

pub fn fix_ctor_delegation_arg_defs(body: &mut Stmt) {
    let Stmt::Block(stmts) = body else { return };
    // Shape C first: the Kotlin default-arg bridge (computations +
    // conditional overrides folded into the delegation args).
    fold_default_arg_bridge(stmts);
    // Only the top-level linear shape; the first statement needs no
    // repair and control flow ahead of the delegation belongs to the
    // conditional-super machinery.
    let Some(pos) = stmts.iter().position(is_bare_ctor_call) else {
        return;
    };
    if pos == 0 {
        return;
    }
    // Vars read by the delegation args, and the total read count (a var
    // read twice would duplicate a call evaluation on inline).
    let mut arg_vars: Vec<u32> = Vec::new();
    let mut total_reads = 0usize;
    {
        let Stmt::ExprStmt(Expr::Method { args, .. }) = &stmts[pos] else {
            return;
        };
        for a in args {
            let mut c = a.clone();
            deep_rewrite_reads(&mut c, &mut |x| {
                if let Expr::Local { var: v, .. } = x {
                    total_reads += 1;
                    if !arg_vars.contains(v) {
                        arg_vars.push(*v);
                    }
                }
            });
        }
    }
    if arg_vars.is_empty() || total_reads != arg_vars.len() {
        return;
    }
    // Vars DEFINED anywhere in the prefix (for the init-referenced
    // check below — an inlined init may not itself read prefix defs).
    let prefix_defs: Vec<u32> = stmts[..pos]
        .iter()
        .filter_map(|s| match s {
            Stmt::LocalDef { var, init: Some(_), .. } => Some(*var),
            _ => None,
        })
        .collect();
    // Per referenced var: exactly one prefix def, no other prefix
    // reader between def and delegation, the def's init reads no
    // prefix-def var, and the first post-delegation use is a top-level
    // write (becomes the declaration) or nothing at all.
    let mut plan: Vec<(u32, usize, Option<usize>)> = Vec::new();
    for v in arg_vars {
        let defs: Vec<usize> = stmts[..pos]
            .iter()
            .enumerate()
            .filter(|(i, _)| {
                matches!(&stmts[*i], Stmt::LocalDef { var, init: Some(_), .. } if *var == v)
            })
            .map(|(i, _)| i)
            .collect();
        if defs.len() > 1 {
            return; // ambiguous: multiple prefix defs
        }
        let Some(&q) = defs.first() else {
            continue; // a param: nothing to inline for this var
        };
        let init = match &stmts[q] {
            Stmt::LocalDef { init: Some(e), .. } => e.clone(),
            _ => return,
        };
        if stmts_read_var(&stmts[q + 1..pos], v) != 0 {
            return; // another reader between the def and the delegation
        }
        // The inlined init may not read a prefix-def var either — the
        // delegation lands ABOVE those defs after the hoist.
        let mut init_vars: Vec<u32> = Vec::new();
        deep_rewrite_reads(&mut { init.clone() }, &mut |x| {
            if let Expr::Local { var: vv, .. } = x {
                if !init_vars.contains(vv) {
                    init_vars.push(*vv);
                }
            }
        });
        if init_vars.iter().any(|iv| prefix_defs.contains(iv)) {
            return;
        }
        match stmts[pos + 1..]
            .iter()
            .position(|s| {
                matches!(s, Stmt::ExprStmt(Expr::Assign { target, .. })
                    if matches!(&**target, Expr::Local { var: vv, .. } if *vv == v))
            }) {
            Some(w) => {
                // Reads before the re-declaration would be dangling.
                if stmts_read_var(&stmts[pos + 1..pos + 1 + w], v) != 0 {
                    return;
                }
                plan.push((v, q, Some(pos + 1 + w)));
            }
            None => {
                // No later write: the def must be dead after inline.
                if stmts_read_var(&stmts[pos + 1..], v) != 0 {
                    return;
                }
                plan.push((v, q, None));
            }
        }
    }
    if plan.is_empty() {
        return;
    }
    // Inline the def inits into the delegation args.
    let values: Vec<(u32, Expr)> = plan
        .iter()
        .map(|(_v, q, _)| match &stmts[*q] {
            Stmt::LocalDef { var, init: Some(e), .. } => (*var, e.clone()),
            _ => unreachable!("plan entries carry a LocalDef with init"),
        })
        .collect();
    if let Stmt::ExprStmt(Expr::Method { args, .. }) = &mut stmts[pos] {
        for a in args {
            deep_rewrite_reads(a, &mut |x| {
                if let Expr::Local { var: v, .. } = x {
                    if let Some((_, e)) = values.iter().find(|(vv, _)| vv == v) {
                        *x = e.clone();
                    }
                }
            });
        }
    }
    // Convert the first writes into declarations BEFORE removing defs
    // (removals shift indices); then remove the defs in descending
    // order.
    for (v, _, w) in &plan {
        if let Some(w) = w {
            if let Stmt::ExprStmt(Expr::Assign { target, value, .. }) = &mut stmts[*w] {
                if matches!(&**target, Expr::Local { var: vv, .. } if vv == v) {
                    let taken = std::mem::replace(&mut **value, Expr::Const(ConstVal::Null));
                    stmts[*w] = Stmt::LocalDef {
                        var: *v,
                        init: Some(taken),
                        is_final: false,
                        force_type: false,
                    };
                }
            }
        }
    }
    let mut order: Vec<usize> = plan.iter().map(|(_, q, _)| *q).collect();
    order.sort_unstable();
    order.reverse();
    for q in order {
        stmts.remove(q);
    }
}

/// An INT-typed method returning a boolean value: the register held the
/// boolean test result in one generation and the `if (v) 1 else 0` int
/// in another; booleanize picks the boolean generation and the plain
/// `return v;` breaks ("boolean无法转换为int", weixin Handle
/// onPausableTransaction — the source was `if (v) 1 else 0`). Wrap the
/// return in the recovering conditional.
/// Mirror of fix_int_returns: a BOOLEAN method returning a numeric-
/// typed value (register reuse left an int generation) wraps it
/// `x != 0` — the dex branch encoding, and the only legal Java form
/// (kh5/a `return v116_g53;` int无法转换为boolean family).
pub fn fix_bool_returns(vt: &VarTable, body: &mut Stmt) {
    walk_mut_deep(body, &mut |st| {
        if let Stmt::Return(Some(e)) = st {
            let numeric = match &*e {
                Expr::Local { var, ty } => {
                    let t = if (*var as usize) < vt.vars.len() {
                        vt.var(*var).ty.erased()
                    } else {
                        ty.erased()
                    };
                    matches!(
                        t,
                        JavaType::Int
                            | JavaType::Long
                            | JavaType::Short
                            | JavaType::Byte
                            | JavaType::Char
                    )
                }
                other => matches!(
                    other.type_ref().erased(),
                    JavaType::Int | JavaType::Long | JavaType::Short | JavaType::Byte
                        | JavaType::Char
                ),
            };
            if numeric {
                let taken = e.clone();
                *e = Expr::Bin {
                    op: BinOp::Ne,
                    l: Box::new(taken),
                    r: Box::new(Expr::Const(ConstVal::Int(0))),
                    ty: Some(TypeRef::J(JavaType::Boolean)),
                };
            }
        }
    });
}

pub fn fix_int_returns(vt: &VarTable, body: &mut Stmt) {
    walk_mut_deep(body, &mut |st| {
        if let Stmt::Return(Some(e)) = st {
            let is_bool = match e {
                Expr::Local { var, .. } => {
                    (*var as usize) < vt.vars.len()
                        && vt.vars[*var as usize].ty.erased() == JavaType::Boolean
                }
                Expr::Method { desc, .. } => desc.ret == JavaType::Boolean,
                _ => false,
            };
            if is_bool {
                *e = Expr::Cond {
                    c: Box::new(e.clone()),
                    t: Box::new(Expr::Const(ConstVal::Int(1))),
                    f: Box::new(Expr::Const(ConstVal::Int(0))),
                };
            }
        }
    });
}

/// Generation splitting: when an assignment's value type is
/// INCOMPATIBLE with the var's declared type, mint a FRESH var for the
/// new generation instead of widening (widening was measured twice as
/// net-negative — every use-site cast gap re-triggers javac's
/// error-type contagion). Reads after the split point follow the
/// active generation; control-flow joins intersect; loops run a
/// 3-round fixpoint so the back-edge carries the tail generation to
/// the head reads (the loop-state-tuple shape, qf5/v0's
/// `u0 = (qf5.u0) get;` … `u0 = looper2;`).
///
/// Split condition (reference incompatibility or a primitive/value
/// mix — the instanceof-corruption shape): both specific references of
/// different classes, or prim→ref. NOT numeric-vs-numeric (promotion
/// handles those) and NOT Object targets (assignable — the narrowing
/// cast pass covers the value side).
static WIDEN_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn split_generations(vt: &mut VarTable, body: &mut Stmt, pool: &DexPool) {
    if std::env::var("DDC_DBG_PHI").is_ok() {
        eprintln!("[widen] cumulative={}", WIDEN_COUNT.load(std::sync::atomic::Ordering::Relaxed));
    }
    let mut counter: u32 = 0;
    let mut gen: std::collections::HashMap<u32, u32> = std::collections::HashMap::default();
    split_walk_stmt(body, vt, &mut gen, &mut counter, pool);
}

/// Should the assignment `target = value` split the target?
fn split_needed(vt: &VarTable, target: u32, value: &Expr, pool: &DexPool) -> bool {
    let n = vt.vars.len();
    if target as usize >= n {
        return false;
    }
    let declared = vt.vars[target as usize].ty.erased();
    let vt_is_ref = |t: &JavaType| matches!(t, JavaType::Object(_) | JavaType::Array(_));
    // Locals consult the VarTable: the embedded ty is a lift-time
    // snapshot, stale after booleanize converts a copy SOURCE (the
    // post-booleanize re-split below relies on seeing `v131` as the
    // boolean it became). Bitwise Or/And/Xor over boolean operands
    // carry the same staleness at the EXPRESSION level (dex lowered
    // Kotlin's non-short-circuit `or` to int `|` — the embedded Bin ty
    // stays Int after booleanize converts the operands): `int v18 =
    // delete | delete2` must split the target generation.
    fn side_bool(e: &Expr, vt: &VarTable) -> bool {
        match e {
            Expr::Local { var, .. } if (*var as usize) < vt.vars.len() => {
                matches!(vt.var(*var).ty.erased(), JavaType::Boolean)
            }
            Expr::Const(ConstVal::Int(0 | 1)) => true,
            Expr::Bin { op: BinOp::Or | BinOp::And | BinOp::Xor, l, r, .. } => {
                side_bool(l, vt) && side_bool(r, vt)
            }
            other => matches!(other.type_ref().erased(), JavaType::Boolean),
        }
    }
    let value_ty = match value {
        Expr::Local { var, ty } if (*var as usize) < vt.vars.len() => vt.var(*var).ty.erased(),
        Expr::Bin {
            op: BinOp::Or | BinOp::And | BinOp::Xor,
            l,
            r,
            ..
        } if side_bool(l, vt) && side_bool(r, vt) => JavaType::Boolean,
        other => other.type_ref().erased(),
    };
    if vt_is_ref(&declared) && vt_is_ref(&value_ty) {
        // Both references: split only when incompatible — different
        // classes where neither side is Object AND the value is not a
        // SUBTYPE of the declared type. An assignable store needs no
        // fresh generation; splitting one minted an orphan at an
        // if-join whose sibling arm kept the base (rename_gen_back's
        // primitive-only gate refused the undo, drop_dead_locals then
        // stripped the orphan to a bare allocation): pq0/g's bridge
        // `map2 = new LinkedHashMap()` against the HashMap-typed phi
        // rendered `new LinkedHashMap();` and the arm lost its store.
        let obj = JavaType::Object("java/lang/Object".into());
        if declared == value_ty || declared == obj || value_ty == obj {
            return false;
        }
        // A FRESH ALLOCATION whose class relation to the declared type
        // is UNKNOWABLE (a framework↔framework pair — LinkedHashMap
        // into a HashMap-typed phi): the dex stored the instance into
        // this register flow, so the store was verifier-legal; the
        // split cannot know better and orphaned the generation (the
        // if-join undo is primitive-only; the dropper stripped the
        // orphan to a bare `new LinkedHashMap();` and pq0/g's bridge
        // lost the else-arm store, killing the whole branched-super
        // merge). Pool-known pairs KEEP the split: the generation's
        // precise type is what downstream subtype sinks need (weibo
        // `v7 = new $reportWhenComplete$1(..)` into a ContinuationImpl
        // phi — exempting it made `v9($1) = v7` inconvertible, +50).
        if let (JavaType::Object(v), JavaType::Object(d)) = (&value_ty, &declared) {
            if matches!(value, Expr::New { .. })
                && (pool.get(v).is_none() || pool.get(d).is_none())
            {
                return false;
            }
        }
        true
    } else {
        // Primitive/reference mix: the value generation has a kind the
        // target can never accept — split (the declared type stays with
        // the old generation's readers).
        if vt_is_ref(&declared) != vt_is_ref(&value_ty) {
            return true;
        }
        // Boolean/numeric mix: `boolean` and `int` have NO cast between
        // them in Java, so a register reused across the kinds can only
        // be expressed as two generations (weixin ConstraintLayout
        // `boolean v60; int v56; v60 = v56;` — 9.5k bool↔int lines once
        // the obscuring suppression lifted). 0/1 constants are the
        // boolean encoding, not a numeric generation — never split on
        // those (booleanize owns them).
        let d_bool = matches!(declared, JavaType::Boolean);
        let v_bool = matches!(value_ty, JavaType::Boolean);
        let zero_one = matches!(value, Expr::Const(ConstVal::Int(0 | 1)));
        d_bool != v_bool && !zero_one
    }
}

fn rewrite_gen_reads(e: &mut Expr, gen: &std::collections::HashMap<u32, u32>, vt: &VarTable) {
    deep_rewrite(e, &mut |x| {
        if let Expr::Local { var, ty } = x {
            if let Some(&to) = gen.get(var) {
                if to != *var {
                    *var = to;
                    *ty = vt.var(to).ty.clone();
                }
            }
        }
    });
}

fn do_split(
    vt: &mut VarTable,
    target: &mut Expr,
    old: u32,
    value_ty: &JavaType,
    gen: &mut std::collections::HashMap<u32, u32>,
    counter: &mut u32,
) {
    *counter += 1;
    let info = &vt.vars[old as usize];
    let id = vt.vars.len() as u32;
    let name = format!("{}_g{}", info.name, counter);
    let slot = info.slot;
    // A split generation is a fresh LOCAL, not a parameter — even when
    // the parent register is one (the param identity stays on the
    // original var id; the generation only carries a later write). The
    // inherited flag made booleanize (and every other param-skip pass)
    // permanently ignore param-register generations: weixin v2/i's
    // `int v264_g160_g7 = !v200` with `!= 0` reads (bool→int assign
    // family, 2.8k lines).
    let is_param = false;
    vt.vars.push(jdc_core::var::VarInfo {
        id,
        slot,
        name,
        ty: TypeRef::J(value_ty.clone()),
        is_param,
        range_start: 0,
        range_end: u16::MAX,
        synthetic_name: true,
    });
    while vt.by_slot.len() <= slot as usize {
        vt.by_slot.push(Vec::new());
    }
    vt.by_slot[slot as usize].push((0, u16::MAX, id));
    gen.insert(old, id);
    if let Expr::Local { var, ty, .. } = target {
        *var = id;
        *ty = TypeRef::J(value_ty.clone());
    }
}

/// Undo a DIVERGED generation split: rewrite every occurrence of the
/// freshly-minted generation `from` back to the pre-if var `to` within
/// `s` (both read positions and assignment targets, plus LocalDef var
/// fields). At a control-flow join only splits BOTH branches produced
/// survive (the phi merges on the shared var); a gen one branch minted
/// alone does not. But `do_split` already rewrote that branch's write
/// target IN PLACE, so without this undo the branch stores into a
/// write-only orphan gen that no confluence reader ever reads — the
/// value is silently lost AND the orphan renders undeclared (kt5.b
/// Kotlin default-arg bridge: `v18_g1 = p6x` in then vs `v18 = false`
/// in else, `this(.., v18, ..)` reads only else's value).
/// Batched rename_gen_back: one LocalDef pass + one expression pass with
/// a var→(target, ty) table, instead of two full subtree walks per undone
/// candidate (475k widenings on weixin paid 15µs each in walks).
fn rename_gen_back_batch(s: &mut Stmt, pairs: &[(u32, u32)], vt: &VarTable) {
    use jdc_core::FxHashMap;
    if pairs.is_empty() {
        return;
    }
    let map: FxHashMap<u32, (u32, JavaType)> = pairs
        .iter()
        .map(|&(from, to)| (from, (to, vt.vars[to as usize].ty.erased())))
        .collect();
    walk_mut_deep(s, &mut |st| {
        if let Stmt::LocalDef { var, .. } = st {
            if let Some(&(to, _)) = map.get(var) {
                *var = to;
            }
        }
    });
    rewrite_exprs(s, &mut |e| {
        deep_rewrite(e, &mut |x| {
            if let Expr::Local { var, ty } = x {
                if let Some(&(to, ref to_ty)) = map.get(var) {
                    *var = to;
                    *ty = TypeRef::J(to_ty.clone());
                }
            }
        });
    });
}

/// Remove an undone generation's `by_slot` segment entry so slot-based
/// resolution can never hand back the now-unreferenced gen id (its
/// VarInfo stays — ids index `vars` — but nothing may select it).
fn drop_gen_slot(vt: &mut VarTable, dead: u32) {
    if (dead as usize) >= vt.vars.len() {
        return;
    }
    let slot = vt.vars[dead as usize].slot as usize;
    if let Some(seg) = vt.by_slot.get_mut(slot) {
        seg.retain(|&(_, _, id)| id != dead);
    }
}

/// Type gate for `rename_gen_back`: merging gen `v` back onto base `k`
/// is only safe when the base can carry the gen's value — identical
/// types, or BOTH primitives (the bool/int register-reuse family that
/// booleanize + the cast fixers reconcile; the Kotlin bridge case,
/// kt5.b `int v18` receiving `boolean p6x`). A reference-vs-primitive
/// or cross-class merge would forge `String k = <boolean>`-style
/// assignments that never compile (the lark/weibo/reqable regression
/// of the ungated first cut: +8.4k `不兼容的类型`).
/// Pool/framework-aware least-upper-bound of two reference classes
/// (internal names): the most specific common ancestor through supers and
/// interfaces. `None` when nothing more specific than Object is common —
/// widening to Object would break member accesses on the base's readers,
/// so that case stays split.
/// Object-class name of a TypeRef without the JavaType clone `erased()`
/// performs (Array chains deep-clone) — the split-join candidate filter
/// ran two clones per candidate per diverged If and the clones dominated
/// the LUB change's wall cost.
/// Marker for a ref-pair candidate whose LUB is resolved lazily (after
/// the write-only read-gate); the empty string never collides with a real
/// internal class name (they always contain at least one '/').
fn lub_pending() -> Option<String> {
    Some(String::new())
}

/// Resolution of a split-undo candidate after the read-gate.
enum Undo {
    /// Rename the gen back onto the base unchanged.
    Direct,
    /// Broaden the base var's type to this LUB, then rename.
    Widen(String),
    /// No safe merge: leave the branch's gen as-is.
    Refuse,
}

fn obj_name(t: &TypeRef) -> Option<&std::sync::Arc<str>> {
    match t {
        TypeRef::J(JavaType::Object(o)) => Some(o),
        _ => None,
    }
}

fn pool_lub(a: &str, b: &str, pool: &DexPool) -> Option<String> {
    if a == b {
        return Some(a.to_string());
    }
    // The hierarchy data is immutable for the process lifetime; the two
    // full walks (ancestor set of `a` + BFS of `b`) with per-node String
    // allocations ran per diverged-If candidate and quadrupled wall time
    // on the big corpora (weixin 19s -> 75s) — a thread-local memo makes
    // the second call a hash hit.
    thread_local! {
        static MEMO: std::cell::RefCell<jdc_core::FxHashMap<(String, String), Option<String>>> =
            std::cell::RefCell::new(jdc_core::FxHashMap::default());
    }
    if let Some(hit) = MEMO.with(|m| m.borrow().get(&(a.to_string(), b.to_string())).cloned()) {
        return hit;
    }
    let r = pool_lub_uncached(a, b, pool);
    MEMO.with(|m| m.borrow_mut().insert((a.to_string(), b.to_string()), r.clone()));
    r
}

fn pool_lub_uncached(a: &str, b: &str, pool: &DexPool) -> Option<String> {
    let mut anc: jdc_core::FxHashSet<String> = jdc_core::FxHashSet::default();
    let mut stack: Vec<String> = vec![a.to_string()];
    while let Some(c) = stack.pop() {
        if !anc.insert(c.clone()) {
            continue;
        }
        if anc.len() > 1024 {
            return None;
        }
        if let Some(cls) = pool.get(&c) {
            if let Some(s) = &cls.super_name {
                stack.push(s.clone());
            }
            for i in &cls.interfaces {
                stack.push(i.clone());
            }
        } else {
            if let Some(s) = crate::fwdb::super_of(&c) {
                stack.push(s.to_string());
            }
            crate::fwdb::for_each_interface(&c, |i| stack.push(i.to_string()));
        }
    }
    let mut seen: jdc_core::FxHashSet<String> = jdc_core::FxHashSet::default();
    let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    queue.push_back(b.to_string());
    while let Some(c) = queue.pop_front() {
        if !seen.insert(c.clone()) {
            continue;
        }
        if c != "java/lang/Object" && anc.contains(&c) {
            return Some(c);
        }
        if seen.len() > 1024 {
            return None;
        }
        if let Some(cls) = pool.get(&c) {
            for i in &cls.interfaces {
                queue.push_back(i.clone());
            }
            if let Some(s) = &cls.super_name {
                queue.push_back(s.clone());
            }
        } else {
            crate::fwdb::for_each_interface(&c, |i| queue.push_back(i.to_string()));
            if let Some(s) = crate::fwdb::super_of(&c) {
                queue.push_back(s.to_string());
            }
        }
    }
    None
}

fn undo_type_safe(vt: &VarTable, k: u32, v: u32) -> bool {
    if (k as usize) >= vt.vars.len() || (v as usize) >= vt.vars.len() {
        return false;
    }
    let kt = vt.vars[k as usize].ty.erased();
    let vt_ty = vt.vars[v as usize].ty.erased();
    if kt == vt_ty {
        return true;
    }
    let is_prim = |t: &JavaType| {
        matches!(
            t,
            JavaType::Int
                | JavaType::Long
                | JavaType::Short
                | JavaType::Byte
                | JavaType::Char
                | JavaType::Float
                | JavaType::Double
                | JavaType::Boolean
        )
    };
    is_prim(&kt) && is_prim(&vt_ty)
}

fn split_walk_stmt(
    st: &mut Stmt,
    vt: &mut VarTable,
    gen: &mut std::collections::HashMap<u32, u32>,
    counter: &mut u32,
    pool: &DexPool,
) {
    match st {
        Stmt::Block(v) => {
            for x in v.iter_mut() {
                split_walk_stmt(x, vt, gen, counter, pool);
            }
        }
        Stmt::ExprStmt(e) => {
            rewrite_gen_reads(e, gen, vt);
            if let Expr::Assign {
                target,
                value,
                op: AssignOp::Plain,
                ..
            } = e
            {
                if let Expr::Local { var, .. } = &**target {
                    let old = *var;
                    if split_needed(vt, old, value, pool) {
                        let value_ty = value.type_ref().erased();
                        do_split(vt, target, old, &value_ty, gen, counter);
                    }
                }
            }
        }
        Stmt::LocalDef { var, init, .. } => {
            if let Some(e) = init {
                rewrite_gen_reads(e, gen, vt);
                if split_needed(vt, *var, e, pool) {
                    // The DEF mints its own generation: retarget the def
                    // to a fresh var (the bare decl of the ORIGINAL stays
                    // for its earlier readers).
                    let value_ty = e.type_ref().erased();
                    let old = *var;
                    let mut fake = Expr::Local {
                        var: old,
                        ty: vt.vars[old as usize].ty.clone(),
                    };
                    do_split(vt, &mut fake, old, &value_ty, gen, counter);
                    if let Expr::Local { var: nv, .. } = fake {
                        *var = nv;
                    }
                }
            }
        }
        Stmt::If {
            cond,
            then_stmt,
            else_stmt,
            ..
        } => {
            rewrite_gen_reads(cond, gen, vt);
            let then_floor = vt.vars.len() as u32;
            let mut g_then = gen.clone();
            split_walk_stmt(then_stmt, vt, &mut g_then, counter, pool);
            let else_floor = vt.vars.len() as u32;
            let mut g_else = gen.clone();
            if let Some(e) = else_stmt {
                split_walk_stmt(e, vt, &mut g_else, counter, pool);
            }
            // Undo splits that DIVERGED at this join (see
            // rename_gen_back), with TWO gates learned the hard way
            // (first cut regressed lark +8.4k):
            //  1. the SIBLING KEPT THE BASE — `g_else[k]` equals the
            //     entry mapping, i.e. only this branch minted a fresh
            //     gen for k. When both branches split, each orphan is
            //     declared by ensure_declared at its own value type and
            //     compiles; renaming them back onto the base would
            //     force cross-kind assignments (`int k = <boolean>`).
            //  2. the fresh gen is WRITE-ONLY in the branch — a gen
            //     read inside its branch renders fine on its own.
            // The gated shape is the Kotlin default-arg bridge phi
            // loss: then `v_g = p6x` / else `v = false` / confluence
            // reads v — the then value was silently dropped AND v_g
            // rendered undeclared (kt5.b). Undo restores the clean
            // diamond so fold_default_arg_bridge can fire.
            // Candidates first (the read-free conditions), then ONE
            // early-exit probe for the small candidate set — the old
            // read_counts_all rebuilt a HashMap over the WHOLE branch
            // subtree per diverged If (quadratic in nesting depth;
            // together with the split walk it was ~1/3 of worker CPU on
            // the QQ sample profile). The map was only ever consulted
            // as `reads(v) == 0` for these candidates.
            // Candidates carry an optional base widening: a REF pair the
            // strict type gate refuses can still merge when the pool
            // knows a common supertype (LinkedList + d96.p0 join to
            // java.util.List). The base widens to that LUB first, so
            // both branch writes and the confluence reads compile on
            // ONE var — the alternative (leaving the branch's gen) is
            // an undeclared write-only orphan and the branch value is
            // silently lost (weixin tp2/u2: the null fallback
            // `p03x = d96.p0.d` never reached addAll).
            // Stage the undo: cheap classification first, the
            // write-only read-gate second, and the pool LUB walks LAST —
            // only for survivors. Computing the LUB inside the filter ran
            // hierarchy walks for candidates the read-gate then dropped
            // (+40% wall on weixin).
            let mut undo_then: Vec<(u32, u32, Option<String>)> = g_then
                .iter()
                .filter(|(&k, &v)| {
                    v >= then_floor
                        && v < else_floor
                        && g_else.get(&k).copied() == gen.get(&k).copied()
                })
                .filter_map(|(&k, &v)| {
                    if undo_type_safe(vt, k, v) {
                        Some((v, k, None))
                    } else if let (Some(_), Some(_)) =
                        (obj_name(&vt.vars[k as usize].ty), obj_name(&vt.vars[v as usize].ty))
                    {
                        Some((v, k, lub_pending()))
                    } else {
                        None
                    }
                })
                .collect();
            if else_floor > then_floor && !undo_then.is_empty() {
                let vars: Vec<u32> = undo_then.iter().map(|&(v, _, _)| v).collect();
                let mut hits = vec![false; vars.len()];
                mark_read_vars(std::slice::from_ref(&**then_stmt), &vars, &mut hits);
                let mut i = 0usize;
                undo_then.retain(|_| {
                    let keep = !hits[i];
                    i += 1;
                    keep
                });
            }
            undo_then.sort_unstable_by_key(|&(v, _, _)| std::cmp::Reverse(v));
            // Resolve the widenings (only for read-gate survivors), apply
            // them to the base types, then ONE batched subtree rewrite.
            let mut pairs: Vec<(u32, u32)> = Vec::with_capacity(undo_then.len());
            for (v, k, pending) in undo_then {
                // Three-way resolution: Direct (types match, the primitive
                // family, or the gen value is assignable to the base)
                // renames as-is; Widen (a pool LUB exists) broadens the
                // base type first; Refused (no common supertype) SKIPS —
                // merging that pair would forge a cross-class assignment.
                let kind = match pending {
                    None => Undo::Direct,
                    Some(_) => {
                        let (a, b) = (
                            obj_name(&vt.vars[k as usize].ty),
                            obj_name(&vt.vars[v as usize].ty),
                        );
                        match (a, b) {
                            (Some(a), Some(b)) if pool.is_subtype(b, a) => Undo::Direct,
                            (Some(a), Some(b)) => match pool_lub(a, b, pool) {
                                Some(lub) => Undo::Widen(lub),
                                None => Undo::Refuse,
                            },
                            _ => Undo::Refuse,
                        }
                    }
                };
                match kind {
                    Undo::Refuse => continue,
                    Undo::Direct => pairs.push((v, k)),
                    Undo::Widen(lub) => {
                        WIDEN_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        vt.vars[k as usize].ty = TypeRef::J(JavaType::Object(lub.as_str().into()));
                        pairs.push((v, k));
                    }
                }
            }
            rename_gen_back_batch(then_stmt, &pairs, vt);
            for &(v, _) in &pairs {
                drop_gen_slot(vt, v);
            }
            if let Some(e) = else_stmt.as_deref_mut() {
                let mut undo_else: Vec<(u32, u32, Option<String>)> = g_else
                    .iter()
                    .filter(|(&k, &v)| {
                        v >= else_floor && g_then.get(&k).copied() == gen.get(&k).copied()
                    })
                    .filter_map(|(&k, &v)| {
                        if undo_type_safe(vt, k, v) {
                            Some((v, k, None))
                        } else if let (Some(_), Some(_)) =
                            (obj_name(&vt.vars[k as usize].ty), obj_name(&vt.vars[v as usize].ty))
                        {
                            Some((v, k, lub_pending()))
                        } else {
                            None
                        }
                    })
                    .collect();
                if (vt.vars.len() as u32) > else_floor && !undo_else.is_empty() {
                    let vars: Vec<u32> = undo_else.iter().map(|&(v, _, _)| v).collect();
                    let mut hits = vec![false; vars.len()];
                    mark_read_vars(std::slice::from_ref(&*e), &vars, &mut hits);
                    let mut i = 0usize;
                    undo_else.retain(|_| {
                        let keep = !hits[i];
                        i += 1;
                        keep
                    });
                }
                undo_else.sort_unstable_by_key(|&(v, _, _)| std::cmp::Reverse(v));
                let mut pairs: Vec<(u32, u32)> = Vec::with_capacity(undo_else.len());
                for (v, k, pending) in undo_else {
                    let kind = match pending {
                        None => Undo::Direct,
                        Some(_) => {
                            let (a, b) = (
                                obj_name(&vt.vars[k as usize].ty),
                                obj_name(&vt.vars[v as usize].ty),
                            );
                            match (a, b) {
                                (Some(a), Some(b)) if pool.is_subtype(b, a) => Undo::Direct,
                                (Some(a), Some(b)) => match pool_lub(a, b, pool) {
                                    Some(lub) => Undo::Widen(lub),
                                    None => Undo::Refuse,
                                },
                                _ => Undo::Refuse,
                            }
                        }
                    };
                    match kind {
                        Undo::Refuse => continue,
                        Undo::Direct => pairs.push((v, k)),
                        Undo::Widen(lub) => {
                            WIDEN_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            vt.vars[k as usize].ty = TypeRef::J(JavaType::Object(lub.as_str().into()));
                            pairs.push((v, k));
                        }
                    }
                }
                rename_gen_back_batch(e, &pairs, vt);
                for &(v, _) in &pairs {
                    drop_gen_slot(vt, v);
                }
            }
            // Join: keep only splits BOTH paths produced (an absent else
            // is a fall-through path that keeps the pre-if generation).
            gen.retain(|k, v| g_then.get(k) == Some(v) && g_else.get(k) == Some(v));
        }
        Stmt::While { cond, body, .. } | Stmt::DoWhile { cond, body, .. } => {
            // 3-round fixpoint: round N walks the cond+body with the
            // accumulated gen — the back-edge carries the tail
            // generation into the head reads of the next round.
            let entry = gen.clone();
            for _ in 0..3 {
                rewrite_gen_reads(cond, gen, vt);
                split_walk_stmt(body, vt, gen, counter, pool);
            }
            // After the loop the pre-loop generation is what a
            // zero-iteration execution left — conservative.
            *gen = entry;
        }
        Stmt::For { init, body, .. } => {
            for x in init.iter_mut() {
                split_walk_stmt(x, vt, gen, counter, pool);
            }
            let entry = gen.clone();
            for _ in 0..3 {
                split_walk_stmt(body, vt, gen, counter, pool);
            }
            *gen = entry;
        }
        Stmt::ForEach { body, .. } => {
            let entry = gen.clone();
            for _ in 0..3 {
                split_walk_stmt(body, vt, gen, counter, pool);
            }
            *gen = entry;
        }
        Stmt::Switch { cases, default, .. } => {
            let entry = gen.clone();
            for c in cases.iter_mut() {
                let mut g_case = entry.clone();
                for x in c.body.iter_mut() {
                    split_walk_stmt(x, vt, &mut g_case, counter, pool);
                }
                gen.retain(|k, v| g_case.get(k) == Some(v));
            }
            if let Some(d) = default {
                let mut g_def = entry.clone();
                split_walk_stmt(d, vt, &mut g_def, counter, pool);
                gen.retain(|k, v| g_def.get(k) == Some(v));
            }
        }
        Stmt::Try {
            body,
            catches,
            finally,
        } => {
            let entry = gen.clone();
            let mut g_try = entry.clone();
            split_walk_stmt(body, vt, &mut g_try, counter, pool);
            let mut g_all = g_try.clone();
            for c in catches.iter_mut() {
                // The handler runs with the PRE-try state (the exception
                // may fire anywhere in the try).
                let mut g_c = entry.clone();
                split_walk_stmt(&mut c.body, vt, &mut g_c, counter, pool);
                g_all.retain(|k, v| g_c.get(k) == Some(v));
            }
            *gen = g_all;
            if let Some(f) = finally {
                split_walk_stmt(f, vt, gen, counter, pool);
            }
        }
        Stmt::Synchronized { body, .. } | Stmt::Labeled { body, .. } => {
            split_walk_stmt(body, vt, gen, counter, pool);
        }
        Stmt::Return(Some(e)) | Stmt::Throw(e) => {
            rewrite_gen_reads(e, gen, vt);
        }
        _ => {}
    }
}

/// Framework exception super edge — the embedded API database
/// (fwdb) replaced the hand-written ~110-entry JDK/Android exception
/// supers table: real extends edges for EVERY framework class, so
/// multi-catch dedupe now covers chains the table missed (and cannot
/// carry a wrong entry — the table's failure mode was dropping a live
/// alternative).
fn fw_super(c: &str) -> Option<&'static str> {
    crate::fwdb::super_of(c)
}

/// `sub <: sup` for exception-class names, combining the pool
/// hierarchy (which also matches framework ancestor NAMES on the way
/// up, so pool subclasses of framework exceptions resolve) with the
/// tabled framework chains for pairs where both sides are outside the
/// dex.
fn exc_subtype(sub: &str, sup: &str, pool: &DexPool) -> bool {
    if sub == sup {
        return true;
    }
    if pool.is_subtype(sub, sup) {
        return true;
    }
    let mut cur = sub;
    for _ in 0..24 {
        let Some(next) = fw_super(cur) else { break };
        if next == sup {
            return true;
        }
        cur = next;
    }
    false
}

/// Drop multi-catch alternatives shadowed by another alternative in
/// the same handler (JLS 14.20.2 forbids subtype-related
/// alternatives; the dex verifier does not). Exact duplicates collapse
/// to the first occurrence. The caught set is unchanged, so this is
/// semantics-preserving by construction.
pub fn dedupe_multicatch(s: &mut Stmt, pool: &DexPool) {
    dedupe_multicatch_walk(s, pool);
}

fn dedupe_multicatch_walk(s: &mut Stmt, pool: &DexPool) {
    match s {
        Stmt::Try {
            body,
            catches,
            finally,
        }
        | Stmt::TryWithResources {
            body,
            catches,
            finally,
            ..
        } => {
            dedupe_multicatch_walk(body, pool);
            for c in catches.iter_mut() {
                if c.exc.len() > 1 {
                    let mut keep: Vec<std::sync::Arc<str>> = Vec::with_capacity(c.exc.len());
                    for e in c.exc.iter() {
                        let shadowed = c.exc.iter().any(|o| {
                            o.as_ref() != e.as_ref() && exc_subtype(e, o, pool)
                        });
                        if !shadowed && !keep.iter().any(|k| k.as_ref() == e.as_ref()) {
                            keep.push(e.clone());
                        }
                    }
                    c.exc = keep;
                }
                dedupe_multicatch_walk(&mut c.body, pool);
            }
            if let Some(f) = finally {
                dedupe_multicatch_walk(f, pool);
            }
        }
        Stmt::Block(v) => {
            for x in v.iter_mut() {
                dedupe_multicatch_walk(x, pool);
            }
        }
        Stmt::If {
            then_stmt,
            else_stmt,
            ..
        } => {
            dedupe_multicatch_walk(then_stmt, pool);
            if let Some(e) = else_stmt {
                dedupe_multicatch_walk(e, pool);
            }
        }
        Stmt::While { body, .. }
        | Stmt::DoWhile { body, .. }
        | Stmt::For { body, .. }
        | Stmt::ForEach { body, .. }
        | Stmt::Labeled { body, .. }
        | Stmt::Synchronized { body, .. } => dedupe_multicatch_walk(body, pool),
        Stmt::Switch { cases, default, .. } => {
            for c in cases.iter_mut() {
                for x in c.body.iter_mut() {
                    dedupe_multicatch_walk(x, pool);
                }
            }
            if let Some(d) = default {
                dedupe_multicatch_walk(d, pool);
            }
        }
        _ => {}
    }
}

/// Dangling `break L<id>`: a Goto whose paired Label/Labeled-wrap was
/// lost to structure degradation prints `break L<id>;` against an
/// undeclared label ("未定义的标签", weibo 371/lark 85/weixin 535).
/// Inside a loop the goto-to-loop-exit shape degrades to a plain
/// `break` — compilable, and the dominant original semantic. Outside
/// any loop the jump is irreducible flow (weixin protobuf state
/// machines: `goto` into a lost loop middle / dispatcher block — the
/// target exists but not at a position Java can express); it renders
/// as an honest `$DDC:` marker comment instead of a guaranteed
/// undefined-label error, and the block's tail after it is dropped
/// (the original control flow never reached those statements).
pub fn resolve_dangling_gotos(s: &mut Stmt) {
    let mut labels: jdc_core::FxHashSet<u32> = jdc_core::FxHashSet::default();
    // walk_all is read-only — the whole-body clone this used to make
    // (4.6k profile samples of Stmt::clone) bought nothing.
    walk_all(s, &mut |st| {
        if let Stmt::Label(id) = st {
            labels.insert(*id);
        }
    });
    resolve_gotos_walk(s, &labels, 0);
}

fn resolve_gotos_walk(s: &mut Stmt, labels: &jdc_core::FxHashSet<u32>, loop_depth: u32) {
    let depth = match s {
        Stmt::While { .. } | Stmt::DoWhile { .. } | Stmt::For { .. } | Stmt::ForEach { .. } => {
            loop_depth + 1
        }
        _ => loop_depth,
    };
    match s {
        Stmt::Goto(id) => {
            if labels.contains(id) {
                return; // valid pair — keep
            }
            if depth > 0 {
                *s = Stmt::Break(None);
            } else {
                *s = Stmt::Comment(format!(
                    "$DDC: unresolved jump to block {} (irreducible control flow)",
                    id
                ));
            }
        }
        Stmt::Block(v) => {
            let mut i = 0usize;
            while i < v.len() {
                // A depth-0 dangling Goto becomes a marker comment and
                // the original flow never reached the siblings after
                // it — dropping them keeps the rendered reachability
                // faithful (they were dead in the dex too).
                let dangling_here =
                    depth == 0 && matches!(&v[i], Stmt::Goto(id) if !labels.contains(id));
                resolve_gotos_walk(&mut v[i], labels, depth);
                if dangling_here {
                    v.truncate(i + 1);
                    break;
                }
                i += 1;
            }
        }
        Stmt::If { then_stmt, else_stmt, .. } => {
            resolve_gotos_walk(then_stmt, labels, depth);
            if let Some(e) = else_stmt {
                resolve_gotos_walk(e, labels, depth);
            }
        }
        Stmt::While { body, .. }
        | Stmt::DoWhile { body, .. }
        | Stmt::For { body, .. }
        | Stmt::ForEach { body, .. }
        | Stmt::Labeled { body, .. }
        | Stmt::Synchronized { body, .. } => resolve_gotos_walk(body, labels, depth),
        Stmt::Try {
            body,
            catches,
            finally,
        } => {
            resolve_gotos_walk(body, labels, depth);
            for c in catches.iter_mut() {
                resolve_gotos_walk(&mut c.body, labels, depth);
            }
            if let Some(f) = finally {
                resolve_gotos_walk(f, labels, depth);
            }
        }
        Stmt::Switch { cases, default, .. } => {
            for c in cases.iter_mut() {
                for x in c.body.iter_mut() {
                    resolve_gotos_walk(x, labels, depth);
                }
            }
            if let Some(d) = default {
                resolve_gotos_walk(d, labels, depth);
            }
        }
        _ => {}
    }
}

/// READS of `v` across the statements (assignment targets excluded).
/// All-var read counts in ONE pass (reads semantics of stmts_read_var)
/// — for gates that test many candidate vars against the same subtree
/// (split_walk's undo filters ran a full arm walk PER candidate).
/// Which of `vars` appear in a READ position inside `stmts`? One walk,
/// a small linear membership check per read, short-circuiting the check
/// once every candidate is found — no HashMap, no per-node hashing.
/// Replaces the full-subtree `read_counts_all` maps the split undo-gates
/// used to build per diverged If (their only query was `reads(v) == 0`
/// for a handful of candidate gens; the rebuild was quadratic in If
/// nesting depth and dominated the split pass on deep methods).
fn mark_read_vars(stmts: &[Stmt], vars: &[u32], hits: &mut [bool]) {
    let mut remaining = vars.len();
    for s in stmts {
        if remaining == 0 {
            break;
        }
        visit_stmt_exprs_ro(s, &mut |e| {
            if remaining == 0 {
                return;
            }
            visit_exprs_reads(e, &mut |x| {
                if remaining == 0 {
                    return;
                }
                if let Expr::Local { var, .. } = x {
                    for (i, v) in vars.iter().enumerate() {
                        if *v == *var && !hits[i] {
                            hits[i] = true;
                            remaining -= 1;
                        }
                    }
                }
            });
        });
    }
}

fn stmts_read_var(stmts: &[Stmt], v: u32) -> usize {
    let mut n = 0usize;
    for s in stmts {
        visit_stmt_exprs_ro(s, &mut |e| {
            visit_exprs_reads(e, &mut |x| {
                if let Expr::Local { var: vv, .. } = x {
                    if *vv == v {
                        n += 1;
                    }
                }
            });
        });
    }
    n
}

pub fn fix_ctor_conditional_super(body: &mut Stmt) {
    // A single-statement body can arrive UNWRAPPED (bare `If` — the
    // gb6/e throw-guard family); the shape scans need a statement list.
    if !matches!(body, Stmt::Block(_)) {
        let inner = std::mem::replace(body, Stmt::Block(Vec::new()));
        *body = Stmt::Block(vec![inner]);
    }
    let Stmt::Block(stmts) = body else { return };
    // Unwrap single-statement Block wrappers: the structurer can nest
    // the whole body one level deeper, hiding the top-level If from the
    // shape scans (cleanup would flatten it — but runs AFTER this pass).
    while stmts.len() == 1 {
        let Stmt::Block(inner) = &stmts[0] else { break };
        *stmts = inner.clone();
    }
    if stmts.is_empty() {
        return;
    }
    if is_bare_ctor_call(stmts.first().unwrap()) {
        dedupe_ctor_delegations(body);
        return;
    }
    if try_linear_super_inline(stmts) {
        return;
    }
    try_merge_branched_super(stmts);
}

/// Shape C: the delegation is already first — strip duplicate copies
/// left by R8 path-duplication (Kotlin default-arg bridge ctors: ft5/j
/// had `super()` leading PLUS another inside a branch — "对super的调用
/// 必须是构造器中的第一个语句" at the copy). Every path delegates
/// through the leading call, so structurally-identical (or empty-args,
/// same-kind) copies nested in blocks/ifs are removed, never moved.
/// Runs both ahead of the shape-A/B attempt and AFTER the plain hoist
/// (fix_ctor_super_first) — the hoist is what puts the leading
/// delegation in place for path-duplicated ctors.
/// A ctor that ALREADY delegates in its first statement can hold no
/// other `this(..)`/`super(..)` — Baidu Titan hotpatch instrumentation
/// injects a conditional re-delegation inside the `if ($ic != null)`
/// guard of every ctor (baidusearch ctor-not-first ×61,922 across
/// 21,813 files: `super(context); if ($ic != null) { ..; if
/// ((flag&1)!=0) { super((Context) callArgs[0]); ..; return; } }`).
/// The guard is dead code on the unpatched runtime path (and the
/// decompiled output has no patch runtime at all); dropping the
/// secondary delegation keeps the real semantics and restores the
/// Java first-statement rule. Runs after dedupe_ctor_delegations
/// (identical copies) and before extract_branched_delegation_helper
/// — gated on a LEADING delegation, so branched-delegation ctors
/// (no leading one) still get the helper treatment. Nested class
/// declarations are separate ctor scopes and are not entered.
pub fn strip_secondary_delegations(body: &mut Stmt) {
    let Stmt::Block(stmts) = body else { return };
    if !stmts.first().is_some_and(is_bare_ctor_call) {
        return;
    }
    fn strip_in(s: &mut Stmt) {
        if is_bare_ctor_call(s) {
            *s = Stmt::Block(Vec::new());
            return;
        }
        match s {
            Stmt::Block(v) => v.iter_mut().for_each(strip_in),
            Stmt::If {
                then_stmt,
                else_stmt,
                ..
            } => {
                strip_in(then_stmt);
                if let Some(e) = else_stmt {
                    strip_in(e);
                }
            }
            Stmt::While { body, .. }
            | Stmt::DoWhile { body, .. }
            | Stmt::ForEach { body, .. }
            | Stmt::Synchronized { body, .. }
            | Stmt::Labeled { body, .. } => strip_in(body),
            Stmt::For { init, body, .. } => {
                init.iter_mut().for_each(strip_in);
                strip_in(body);
            }
            Stmt::Switch { cases, default, .. } => {
                for c in cases.iter_mut() {
                    c.body.iter_mut().for_each(strip_in);
                }
                if let Some(d) = default {
                    strip_in(d);
                }
            }
            Stmt::Try {
                body,
                catches,
                finally,
            }
            | Stmt::TryWithResources {
                body,
                catches,
                finally,
                ..
            } => {
                strip_in(body);
                for c in catches.iter_mut() {
                    strip_in(&mut c.body);
                }
                if let Some(f) = finally {
                    strip_in(f);
                }
            }
            _ => {}
        }
    }
    for st in stmts.iter_mut().skip(1) {
        strip_in(st);
    }
}

pub fn dedupe_ctor_delegations(body: &mut Stmt) {
    let Stmt::Block(stmts) = body else { return };
    let Some(d0) = stmts.first().and_then(|s| match s {
        Stmt::ExprStmt(e) if is_delegation_expr(e) => Some(e.clone()),
        _ => None,
    }) else {
        return;
    };
    fn rec(s: &mut Stmt, d0: &Expr) {
        match s {
            Stmt::Block(v) => {
                v.retain(|x| {
                    !matches!(x, Stmt::ExprStmt(e) if is_delegation_expr(e) && same_delegation(e, d0))
                });
                for x in v.iter_mut() {
                    rec(x, d0);
                }
            }
            Stmt::If { then_stmt, else_stmt, .. } => {
                rec(then_stmt, d0);
                if let Some(e) = else_stmt {
                    rec(e, d0);
                }
            }
            _ => {}
        }
    }
    // Top-level copies first (rec only strips copies NESTED in
    // blocks/ifs — a duplicate `super()` sitting directly in the body
    // list fell through its `_` arm: weixin ft5/j's second super()).
    {
        let d0r = &d0;
        let mut idx = 0usize;
        stmts.retain(|s| {
            idx += 1;
            idx == 1
                || !(matches!(s, Stmt::ExprStmt(e)
                    if is_delegation_expr(e) && same_delegation(e, d0r)))
        });
    }
    for s in stmts.iter_mut().skip(1) {
        rec(s, &d0);
    }
}

/// Structural delegation identity for dedup: same kind/target and equal
/// args — or both arg-less (`super()` copies across duplicated paths
/// render with locally-renamed args yet delegate identically).
fn same_delegation(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (
            Expr::Method { name: n1, cls: c1, is_super: s1, args: a1, .. },
            Expr::Method { name: n2, cls: c2, is_super: s2, args: a2, .. },
        ) => {
            &**n1 == "<init>"
                && &**n2 == "<init>"
                && c1 == c2
                && s1 == s2
                && (a1 == a2 || (a1.is_empty() && a2.is_empty()))
        }
        _ => false,
    }
}

/// The `Method` expr of a bare ctor-delegation statement.
fn ctor_call_expr(s: &Stmt) -> Option<&Expr> {
    match s {
        Stmt::ExprStmt(e) if is_delegation_expr(e) => Some(e),
        _ => None,
    }
}

/// Same delegation kind ignoring args (super vs this, same target).
fn same_ctor_kind(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (
            Expr::Method { cls: c1, is_super: s1, owner: o1, .. },
            Expr::Method { cls: c2, is_super: s2, owner: o2, .. },
        ) => c1 == c2 && s1 == s2 && o1 == o2,
        _ => false,
    }
}

/// Statements of a branch body, recursively flattening pure Blocks —
/// the fix runs BEFORE cleanup, so structurer-emitted nesting is still
/// in place. Any non-Block statement (If/Try/...) stays an item and
/// fails the def/call classification, which is the intended abort.
fn flat_list(s: &Stmt) -> Vec<&Stmt> {
    fn rec<'a>(s: &'a Stmt, out: &mut Vec<&'a Stmt>) {
        match s {
            Stmt::Block(v) => {
                for x in v {
                    rec(x, out);
                }
            }
            other => out.push(other),
        }
    }
    let mut out = Vec::new();
    rec(s, &mut out);
    out
}

/// Blank the OWNER of un-folded `<init>` calls (lost allocation): the
/// printer renders them as `new X(..)` and never reads the owner, so its
/// register reference is not a real use (v7.a$a2: the dead
/// `new f(0,..)`'s owner was v1's register, inflating v1's use count
/// and blocking the inline-hoist).
fn strip_lost_alloc_owners(e: &mut Expr) {
    deep_rewrite(e, &mut |x| {
        if let Expr::Method { name, is_special: true, owner: Some(o), .. } = x {
            if &**name == "<init>" && !matches!(**o, Expr::This) {
                **o = Expr::Const(jdc_core::ir::expr::ConstVal::Int(0));
            }
        }
    });
}

/// Per-var `Local` occurrence counts over an expression (single pass;
/// lost-alloc `<init>` owners excluded — see strip_lost_alloc_owners).
fn count_locals_expr(e: &Expr) -> jdc_core::FxHashMap<u32, usize> {
    let mut m = jdc_core::FxHashMap::default();
    count_expr_ro(e, &mut m);
    m
}

pub(crate) fn count_locals_stmts(ss: &[Stmt]) -> jdc_core::FxHashMap<u32, usize> {
    let mut m = jdc_core::FxHashMap::default();
    for s in ss {
        visit_stmt_exprs_ro(s, &mut |e| count_expr_ro(e, &mut m));
    }
    m
}

fn cnt(m: &jdc_core::FxHashMap<u32, usize>, v: u32) -> usize {
    m.get(&v).copied().unwrap_or(0)
}

/// Substitute map-referenced locals, recursively expanding def chains
/// (depth-capped). Returns None when a referenced var has no def or the
/// chain is too deep.
fn inline_locals(
    e: &Expr,
    map: &std::collections::HashMap<u32, Expr>,
    depth: u32,
) -> Option<Expr> {
    if depth > 4 {
        return None;
    }
    let mut out = e.clone();
    let mut fail = false;
    deep_rewrite(&mut out, &mut |x| {
        if let Expr::Local { var, .. } = x {
            if let Some(def) = map.get(var) {
                match inline_locals(def, map, depth + 1) {
                    Some(r) => *x = r,
                    None => fail = true,
                }
            }
        }
    });
    if fail {
        None
    } else {
        Some(out)
    }
}

/// A leading statement that defines a local: (var, init expr).
fn def_of(s: &Stmt) -> Option<(u32, &Expr)> {
    match s {
        Stmt::LocalDef { var, init: Some(e), .. } => Some((*var, e)),
        Stmt::ExprStmt(Expr::Assign {
            target,
            value,
            op: jdc_core::ir::expr::AssignOp::Plain,
            ..
        }) => match &**target {
            Expr::Local { var, .. } => Some((*var, value)),
            _ => None,
        },
        _ => None,
    }
}

/// Shape A: straight-line prefix, delegation references prefix locals.
fn try_linear_super_inline(stmts: &mut Vec<Stmt>) -> bool {
    let Some(pos) = stmts.iter().position(is_bare_ctor_call) else {
        return false;
    };
    if pos == 0 {
        return false; // already first — nothing to do
    }
    // The prefix must be straight-line.
    if !stmts[..pos]
        .iter()
        .all(|s| matches!(s, Stmt::LocalDef { .. } | Stmt::ExprStmt(_)))
    {
        return false;
    }
    let Some(call) = ctor_call_expr(&stmts[pos]) else {
        return false;
    };
    let Expr::Method { args, .. } = call else {
        return false;
    };
    // Defs reachable from the args (last definition wins, bytecode order).
    let mut map: std::collections::HashMap<u32, Expr> =
        std::collections::HashMap::new();
    let mut def_idx: jdc_core::FxHashMap<u32, usize> =
        jdc_core::FxHashMap::default();
    for (i, s) in stmts[..pos].iter().enumerate() {
        if let Some((v, e)) = def_of(s) {
            map.insert(v, e.clone());
            def_idx.insert(v, i);
        }
    }
    // Which arg locals need inlining?
    let mut needed: Vec<u32> = Vec::new();
    for a in args {
        let mut probe = a.clone();
        strip_lost_alloc_owners(&mut probe);
        deep_rewrite(&mut probe, &mut |x| {
            if let Expr::Local { var, .. } = x {
                if map.contains_key(var) && !needed.contains(var) {
                    needed.push(*var);
                }
            }
        });
    }
    if needed.is_empty() {
        return false; // plain hoist handles it
    }
    // Guard: side-effecting defs inline only at a provably preserved
    // call count (exactly one use, the arg site).
    let mut consumed: Vec<usize> = Vec::new();
    let use_counts = count_locals_stmts(stmts);
    for &v in &needed {
        let def = &map[&v];
        let uses = cnt(&use_counts, v);
        if jdc_core::ir::build::has_side_effects(def) && uses != 1 {
            return false;
        }
        if uses == 1 {
            consumed.push(def_idx[&v]);
        }
    }
    // Build the inlined args.
    let mut new_args: Vec<Expr> = Vec::with_capacity(args.len());
    for a in args {
        match inline_locals(a, &map, 0) {
            Some(r) => new_args.push(r),
            None => return false,
        }
    }
    // Rebuild: super first (with inlined args), the unconsumed prefix
    // statements keep their order after it, then the original rest.
    let mut call_stmt = stmts.remove(pos);
    if let Stmt::ExprStmt(Expr::Method { args: slot, .. }) = &mut call_stmt {
        *slot = new_args;
    }
    let mut rest: Vec<Stmt> = Vec::with_capacity(stmts.len() + 1);
    rest.push(call_stmt);
    for (i, s) in stmts.drain(..).enumerate() {
        // Indices shift after `remove(pos)`: consumed holds pre-removal
        // indices; pos itself is already gone from `stmts`.
        let orig = if i >= pos { i + 1 } else { i };
        if !consumed.contains(&orig) {
            rest.push(s);
        }
    }
    *stmts = rest;
    true
}

/// Shape B: if/else branches each ending in the same-kind delegation.
fn try_merge_branched_super(stmts: &mut Vec<Stmt>) -> bool {
    // Candidate If positions — the prelude before the chosen If must be
    // straight-line; try each If until one merges (nested/multi-if
    // ctors exist but the first matching one is the delegation site).
    let if_positions: Vec<usize> = stmts
        .iter()
        .enumerate()
        .filter(|(_, s)| matches!(s, Stmt::If { .. }))
        .map(|(i, _)| i)
        .collect();
    for if_pos in if_positions {
        // Cheap allocation-free gate: at least one branch must hold a
        // delegation (most ctor ifs are field null-checks and must not
        // pay the merge machinery). The OTHER side may be delegation-free
        // (throw-guard shape D) or absent (then-only shape E — the
        // rendered if/else form often only exists after later passes).
        let gate = match &stmts[if_pos] {
            Stmt::If { then_stmt, else_stmt, .. } => {
                contains_delegation(then_stmt)
                    || else_stmt.as_ref().is_some_and(|e| contains_delegation(e))
            }
            _ => false,
        };
        if gate && merge_at(stmts, if_pos) {
            return true;
        }
    }
    false
}

/// Does this subtree contain a ctor delegation? (module-level: shared by
/// the merge gate and split_branch)
fn contains_delegation(s: &Stmt) -> bool {
    match s {
        Stmt::ExprStmt(e) => is_delegation_expr(e),
        Stmt::Block(v) => v.iter().any(contains_delegation),
        Stmt::If { then_stmt, else_stmt, .. } => {
            contains_delegation(then_stmt)
                || else_stmt.as_ref().is_some_and(|e| contains_delegation(e))
        }
        _ => false,
    }
}



/// One merge attempt at `stmts[if_pos]`. See fix_ctor_conditional_super
/// for the doctrine. Extensions over the pure two-call merge:
/// - branch pre-call SIDE-EFFECT statements (Kotlin `o.h(param,..)`
///   null-checks — the zz5/j0 family) are allowed; they move into the
///   branch remainder (after super — the same reordering doctrine the
///   plain hoist already applies; Java has no pre-super statement form).
/// - a branch WITHOUT a delegation (throw-guard: `if (x==null) throw
///   .. else { super(..); .. }` — the gb6/e family) merges as pure
///   remainder; exactly one branch carries the call then, and no
///   ternary args arise (the guard throws before any path needs them).
/// - prelude single-assignment if/else (`if (c) {v=e1} else {v=e2}`)
///   folds into a Cond def for inlining (the zz5/j0 v15/v16 preludes).
fn merge_at(stmts: &mut Vec<Stmt>, if_pos: usize) -> bool {
    /// Branch classification: Call(at, defs, pre-call extra idxs) when a
    /// delegation sits behind only defs/side-effect statements; Clean
    /// when the branch has no delegation at all (pure remainder); Dirty
    /// otherwise (nested/conditional delegation — out of scope).
    enum Br {
        /// (call index, pre-call defs, def statement indices)
        Call(usize, Vec<(u32, Expr)>, Vec<usize>),
        Clean,
        Dirty,
    }
    fn split_branch(list: &[&Stmt]) -> Br {
        let call_pos = list.iter().position(|s| ctor_call_expr(s).is_some());
        let Some(c) = call_pos else {
            // No delegation at this level: a pure remainder branch
            // (throw-guard) unless one hides in nested control flow.
            return if list.iter().any(|s| contains_delegation(s)) {
                Br::Dirty
            } else {
                Br::Clean
            };
        };
        if list.iter().skip(c + 1).any(|s| ctor_call_expr(s).is_some()) {
            return Br::Dirty; // two delegations in one branch
        }
        let mut defs: Vec<(u32, Expr)> = Vec::new();
        let mut def_idxs: Vec<usize> = Vec::new();
        for (i, s) in list.iter().enumerate().take(c) {
            match def_of(s) {
                Some((v, e)) => {
                    defs.push((v, e.clone()));
                    def_idxs.push(i);
                }
                None => match s {
                    // Side-effect statements and bare decls ahead of the
                    // delegation ride along into the remainder.
                    Stmt::ExprStmt(_) | Stmt::LocalDef { .. } => {}
                    _ => return Br::Dirty, // control flow pre-call
                },
            }
        }
        Br::Call(c, defs, def_idxs)
    }
    /// Prelude single-assignment if/else → `v = c ? e1 : e2` def.
    fn cond_def_of(s: &Stmt, scope: &[Stmt]) -> Option<(u32, Expr)> {
        let Stmt::If { cond, then_stmt, else_stmt: Some(e), .. } = s else {
            return None;
        };
        fn one<'x>(b: &'x Stmt, scope: &[Stmt]) -> Option<(u32, &'x Expr)> {
            let l = flat_list(b);
            if l.len() == 1 {
                return def_of(l[0]);
            }
            // A [w = E; v = w] forwarding PAIR — the shape the
            // structurer leaves when E is impure (`c v5 = new c(..);
            // v6 = v5`) and forward_single_use's adjacency pass missed
            // it (a duplicated read elsewhere in its raw body broke the
            // single-read gate). Collapse to `v = E`, gated on w being
            // method-unique: the merge DROPS this branch, so any other
            // read/write of w would dangle (lark gp2/d Kotlin default-
            // arg bridge, this-not-first family).
            if l.len() == 2 {
                let (w, ex) = def_of(l[0])?;
                let Stmt::ExprStmt(Expr::Assign {
                    target,
                    op: AssignOp::Plain,
                    value,
                }) = l[1]
                else {
                    return None;
                };
                let Expr::Local { var: tv, .. } = &**target else {
                    return None;
                };
                let Expr::Local { var: rv, .. } = &**value else {
                    return None;
                };
                if *rv != w || *tv == w {
                    return None;
                }
                // stmts_count_writes only sees Expr-level assigns; the
                // pair's own def is often an init-bearing LocalDef, so
                // count those separately for the uniqueness gate.
                let mut ldef_writes = 0usize;
                for st in scope {
                    walk_all(st, &mut |x| {
                        if let Stmt::LocalDef {
                            var,
                            init: Some(_),
                            ..
                        } = x
                        {
                            if *var == w {
                                ldef_writes += 1;
                            }
                        }
                    });
                }
                if stmts_read_var(scope, w) != 1
                    || stmts_count_writes(scope, w) + ldef_writes != 1
                {
                    return None;
                }
                return Some((*tv, ex));
            }
            None
        }
        let (v1, e1) = one(then_stmt, scope)?;
        let (v2, e2) = one(e, scope)?;
        if v1 != v2 {
            return None;
        }
        Some((
            v1,
            Expr::Cond {
                c: Box::new(cond.clone()),
                t: Box::new(e1.clone()),
                f: Box::new(e2.clone()),
            },
        ))
    }

    // Prelude: defs (flat or folded cond), bare decls, side-effect stmts.
    let mut pmap: std::collections::HashMap<u32, Expr> =
        std::collections::HashMap::new();
    let mut pidx: jdc_core::FxHashMap<u32, usize> =
        jdc_core::FxHashMap::default();
    for (i, s) in stmts[..if_pos].iter().enumerate() {
        match s {
            Stmt::LocalDef { .. } | Stmt::ExprStmt(_) => {
                if let Some((v, e)) = def_of(s) {
                    pmap.insert(v, e.clone());
                    pidx.insert(v, i);
                }
            }
            Stmt::If { .. } => match cond_def_of(s, stmts) {
                Some((v, e)) => {
                    pmap.insert(v, e);
                    pidx.insert(v, i);
                }
                None => return false,
            },
            _ => return false,
        }
    }
    let (cond, then_v, else_v, had_else) = match &stmts[if_pos] {
        Stmt::If { cond, then_stmt, else_stmt, .. } => (
            cond.clone(),
            then_stmt.as_ref().clone(),
            else_stmt.as_ref().map_or_else(|| Stmt::Block(Vec::new()), |e| e.as_ref().clone()),
            else_stmt.is_some(),
        ),
        _ => return false,
    };
    // Chained guards (Kotlin multi-param null-checks):
    // `if(a){throw..} else { if(b){throw..} else { super(..); .. } }` —
    // a branch that is a lone delegation-bearing If gets merged
    // recursively first (its delegation hoisted to the branch head), so
    // the outer split then sees [call, nested-guard-remainder].
    fn normalize_branch(b: &mut Stmt) {
        // MOVE-based (no deep clone): chained weixin Parcel ctors carry
        // huge tails; cloning per recursion level cost measurable wall
        // time. merge_at leaves stmts untouched on failure, so the taken
        // node can always go back.
        let slot: &mut Stmt = match b {
            Stmt::Block(v) if v.len() == 1 => &mut v[0],
            other => other,
        };
        if !matches!(slot, Stmt::If { .. }) || !contains_delegation(slot) {
            return;
        }
        let taken = std::mem::replace(slot, Stmt::Block(Vec::new()));
        let mut tmp = vec![taken];
        if merge_at(&mut tmp, 0) {
            *slot = Stmt::Block(tmp);
        } else {
            *slot = tmp.pop().unwrap();
        }
    }
    let mut then_v = then_v;
    let mut else_v = else_v;
    normalize_branch(&mut then_v);
    normalize_branch(&mut else_v);
    let then_list = flat_list(&then_v);
    let else_list = flat_list(&else_v);

    // Exactly: two Call branches (merge with ternaries) or one Call +
    // one Clean (single-delegation throw-guard). Owned branch payloads.
    enum Side {
        Call {
            at: usize,
            defs: Vec<(u32, Expr)>,
            def_idxs: Vec<usize>,
        },
        Clean,
    }
    let (t_side, e_side, two_sided) = match (split_branch(&then_list), split_branch(&else_list)) {
        (Br::Call(tc, td, ti), Br::Call(ec, ed, ei)) => (
            Side::Call { at: tc, defs: td, def_idxs: ti },
            Side::Call { at: ec, defs: ed, def_idxs: ei },
            true,
        ),
        (Br::Call(tc, td, ti), Br::Clean) => (
            Side::Call { at: tc, defs: td, def_idxs: ti },
            Side::Clean,
            false,
        ),
        (Br::Clean, Br::Call(ec, ed, ei)) => (
            Side::Clean,
            Side::Call { at: ec, defs: ed, def_idxs: ei },
            false,
        ),
        _ => return false,
    };
    // The delegation nodes (two-sided: both; single: one).
    let (t_call, e_call) = match (&t_side, &e_side) {
        (Side::Call { at: ta, .. }, Side::Call { at: ea, .. }) => (
            ctor_call_expr(then_list[*ta]),
            ctor_call_expr(else_list[*ea]),
        ),
        (Side::Call { at: ta, .. }, Side::Clean) => {
            (ctor_call_expr(then_list[*ta]), None)
        }
        (Side::Clean, Side::Call { at: ea, .. }) => {
            (None, ctor_call_expr(else_list[*ea]))
        }
        _ => return false,
    };
    // The node that becomes the merged call: then side when present.
    let (call_list, call_at) = match &t_side {
        Side::Call { at, .. } => (&then_list, *at),
        Side::Clean => match &e_side {
            Side::Call { at, .. } => (&else_list, *at),
            Side::Clean => return false,
        },
    };
    let (base_args, other_args) = if two_sided {
        let (Some(tc), Some(ec)) = (t_call, e_call) else {
            return false;
        };
        if !same_ctor_kind(tc, ec) {
            return false;
        }
        let (Expr::Method { args: a1, .. }, Expr::Method { args: a2, .. }) = (tc, ec) else {
            return false;
        };
        if a1.len() != a2.len() {
            return false;
        }
        (a1, Some(a2))
    } else {
        let c = t_call.or(e_call);
        let Some(c) = c else { return false };
        let Expr::Method { args, .. } = c else {
            return false;
        };
        (args, None)
    };

    // Def maps per side (prelude + own branch defs).
    let mut tmap: std::collections::HashMap<u32, Expr> = pmap.clone();
    let mut emap: std::collections::HashMap<u32, Expr> = pmap.clone();
    if let Side::Call { defs, .. } = &t_side {
        for (v, e) in defs {
            tmap.insert(*v, e.clone());
        }
    }
    if let Side::Call { defs, .. } = &e_side {
        for (v, e) in defs {
            emap.insert(*v, e.clone());
        }
    }
    let Some(cond_i) = inline_locals(&cond, &pmap, 0) else {
        return false;
    };
    // Inline each side's args through its own map.
    let mut then_args: Vec<Expr> = Vec::with_capacity(base_args.len());
    let mut else_args: Vec<Expr> = Vec::new();
    if two_sided {
        for a in base_args {
            match inline_locals(a, &tmap, 0) {
                Some(r) => then_args.push(r),
                None => return false,
            }
        }
        for a in other_args.unwrap() {
            match inline_locals(a, &emap, 0) {
                Some(r) => else_args.push(r),
                None => return false,
            }
        }
    } else {
        // Single-delegation side: base_args belong to whichever side
        // carries the call; pick that side's map.
        let from_then = matches!(&t_side, Side::Call { .. });
        for a in base_args {
            let r = if from_then {
                inline_locals(a, &tmap, 0)
            } else {
                inline_locals(a, &emap, 0)
            };
            match r {
                Some(r) => then_args.push(r),
                None => return false,
            }
        }
    }
    let primary_args = base_args;
    // Per-position merge.
    let mut merged: Vec<Expr> = Vec::with_capacity(then_args.len());
    let mut differ = 0usize;
    for (j, t) in then_args.iter().enumerate() {
        if two_sided {
            let e = &else_args[j];
            if t == e {
                merged.push(t.clone());
            } else {
                differ += 1;
                merged.push(Expr::Cond {
                    c: Box::new(cond_i.clone()),
                    t: Box::new(t.clone()),
                    f: Box::new(e.clone()),
                });
            }
        } else {
            merged.push(t.clone());
        }
    }

    // ---- side-effect preservation guards (batched counts) ----
    let vars_in = |e: &Expr| -> Vec<u32> {
        let mut vs = Vec::new();
        let mut probe = e.clone();
        strip_lost_alloc_owners(&mut probe);
        deep_rewrite(&mut probe, &mut |x| {
            if let Expr::Local { var, .. } = x {
                if !vs.contains(var) {
                    vs.push(*var);
                }
            }
        });
        vs
    };
    // Remainder statements per branch (extras + post-call tail; a Clean
    // branch contributes all of its statements).
    // The consumed defs LEAVE the remainder: their expressions were
    // inlined into the merged delegation args, and a dead `v = E` left
    // after the call both reads wrong and trips the u_t1/u_t2 guards
    // (ContinuationImpl's branch-local `context = null / getContext()`
    // — the coroutine family's 对this的调用必须是第一个语句 root).
    let branch_remainder = |list: &[&Stmt], side: &Side| -> Vec<Stmt> {
        match side {
            Side::Clean => list.iter().map(|s| (*s).clone()).collect(),
            Side::Call { at, def_idxs, .. } => {
                list.iter()
                    .enumerate()
                    .filter(|(i, _)| *i != *at && !def_idxs.contains(i))
                    .map(|(_, s)| (*s).clone())
                    .collect()
            }
        }
    };
    let tail_then_v = branch_remainder(&then_list, &t_side);
    let tail_else_v = branch_remainder(&else_list, &e_side);
    let post = &stmts[if_pos + 1..];
    let mut inlined: Vec<u32> = vars_in(&cond);
    for a in primary_args.iter().chain(else_args.iter()) {
        for v in vars_in(a) {
            if !inlined.contains(&v) {
                inlined.push(v);
            }
        }
    }
    inlined.retain(|v| pmap.contains_key(v) || tmap.contains_key(v) || emap.contains_key(v));
    // Prelude-and-branch redefinition: ambiguous provenance — reject.
    for &v in &inlined {
        let in_branch = match (&t_side, &e_side) {
            (Side::Call { defs: td, .. }, Side::Call { defs: ed, .. }) => {
                td.iter().any(|(d, _)| *d == v) || ed.iter().any(|(d, _)| *d == v)
            }
            (Side::Call { defs, .. }, Side::Clean)
            | (Side::Clean, Side::Call { defs, .. }) => {
                defs.iter().any(|(d, _)| *d == v)
            }
            _ => false,
        };
        if pmap.contains_key(&v) && in_branch {
            return false;
        }
    }
    type Counts = jdc_core::FxHashMap<u32, usize>;
    type GuardCounts = (
        Counts,
        Vec<Counts>,
        Vec<Counts>,
        Counts,
        Counts,
        Counts,
        Vec<(u32, Counts)>,
    );
    let (cond_counts, a1_counts, a2_counts, tail1_counts, tail2_counts, post_counts, def_counts): GuardCounts =
        if inlined.is_empty() {
        Default::default()
    } else {
        let all_defs: Vec<(u32, &Expr)> = pmap
            .iter()
            .map(|(k, v)| (*k, v))
            .chain(match &t_side {
                Side::Call { defs, .. } => defs.iter().map(|(k, v)| (*k, v)).collect(),
                Side::Clean => Vec::new(),
            })
            .chain(match &e_side {
                Side::Call { defs, .. } => defs.iter().map(|(k, v)| (*k, v)).collect(),
                Side::Clean => Vec::new(),
            })
            .collect();
        (
            count_locals_expr(&cond),
            primary_args.iter().map(count_locals_expr).collect(),
            else_args.iter().map(count_locals_expr).collect(),
            count_locals_stmts(&tail_then_v),
            count_locals_stmts(&tail_else_v),
            count_locals_stmts(post),
            all_defs.into_iter().map(|(v, e)| (v, count_locals_expr(e))).collect(),
        )
        };
    for &v in &inlined {
        let in_t = matches!(&t_side, Side::Call { defs, .. } if defs.iter().any(|(d, _)| *d == v));
        let in_e = matches!(&e_side, Side::Call { defs, .. } if defs.iter().any(|(d, _)| *d == v));
        let effectful = [
            in_t.then(|| tmap.get(&v)).flatten(),
            in_e.then(|| emap.get(&v)).flatten(),
            pmap.get(&v),
        ]
        .into_iter()
        .flatten()
        .any(jdc_core::ir::build::has_side_effects);
        if !effectful {
            continue;
        }
        let u_cond = cnt(&cond_counts, v);
        let u_post = cnt(&post_counts, v);
        let u_t1 = cnt(&tail1_counts, v);
        let u_t2 = cnt(&tail2_counts, v);
        let mut u_odef = 0usize;
        for (dv, dc) in &def_counts {
            if *dv != v && inlined.contains(dv) {
                u_odef += cnt(dc, v);
            }
        }
        let mut emit = u_cond * (1 + differ);
        let mut uses_args = 0usize;
        for j in 0..primary_args.len() {
            let c1 = cnt(&a1_counts[j], v);
            let c2 = if two_sided && j < a2_counts.len() {
                cnt(&a2_counts[j], v)
            } else {
                0
            };
            uses_args += c1 + c2;
            emit += if two_sided && then_args[j] != else_args[j] {
                c1 + c2
            } else {
                c1.max(c2)
            };
        }
        let ok = if in_t || in_e {
            u_cond == 0
                && u_post == 0
                && u_t1 == 0
                && u_t2 == 0
                && u_odef == 0
                && a1_counts.iter().map(|c| cnt(c, v)).sum::<usize>() <= 1
                && a2_counts.iter().map(|c| cnt(c, v)).sum::<usize>() <= 1
                && emit <= 2
                && (in_t || a1_counts.iter().all(|c| cnt(c, v) == 0))
                && (in_e || a2_counts.iter().all(|c| cnt(c, v) == 0))
        } else {
            u_post == 0
                && u_t1 == 0
                && u_t2 == 0
                && u_odef == 0
                && ((u_cond == 1 && uses_args == 0 && emit <= 2)
                    || (u_cond == 0 && emit <= 1))
        };
        if !ok {
            return false;
        }
    }

    // ---- assembly ----
    let mut call_stmt = call_list[call_at].clone();
    if let Stmt::ExprStmt(Expr::Method { args: slot, .. }) = &mut call_stmt {
        *slot = merged;
    }
    let residual = |v: u32| -> usize {
        cnt(&tail1_counts, v) + cnt(&tail2_counts, v) + cnt(&post_counts, v)
    };
    let mut kept_prelude: Vec<Stmt> = Vec::new();
    for (i, s) in stmts[..if_pos].iter().enumerate() {
        let consumed = match s {
            Stmt::LocalDef { var, .. } => Some(*var),
            Stmt::ExprStmt(Expr::Assign { target, .. }) => match &**target {
                Expr::Local { var, .. } => Some(*var),
                _ => None,
            },
            Stmt::If { .. } => cond_def_of(s, stmts).map(|(v, _)| v),
            _ => None,
        };
        if let Some(v) = consumed {
            if inlined.contains(&v) && pidx.get(&v) == Some(&i) && residual(v) == 0 {
                continue;
            }
        }
        kept_prelude.push(s.clone());
    }
    let build_keep = |list: &[&Stmt], side: &Side| -> Vec<Stmt> {
        match side {
            Side::Clean => list.iter().map(|s| (*s).clone()).collect(),
            Side::Call { at, .. } => {
                let mut out = Vec::new();
                for (i, s) in list.iter().enumerate() {
                    if i == *at {
                        continue;
                    }
                    if i < *at {
                        if let Some((v, _)) = def_of(s) {
                            if inlined.contains(&v) && residual(v) == 0 {
                                continue;
                            }
                        }
                    }
                    out.push((*s).clone());
                }
                out
            }
        }
    };
    let then_keep = build_keep(&then_list, &t_side);
    let else_keep = build_keep(&else_list, &e_side);
    let else_part = if else_keep.is_empty() && !had_else {
        None
    } else {
        Some(Box::new(Stmt::Block(else_keep)))
    };
    let new_if = Stmt::If {
        cond: cond_i,
        then_stmt: Box::new(Stmt::Block(then_keep)),
        else_stmt: else_part,
    };
    let if_empty = match &new_if {
        Stmt::If { then_stmt, else_stmt, .. } => {
            let be = |s: &Stmt| matches!(s, Stmt::Block(v) if v.is_empty());
            be(then_stmt) && else_stmt.as_ref().is_none_or(|e| be(e))
        }
        _ => false,
    };
    let mut out: Vec<Stmt> = Vec::with_capacity(stmts.len());
    out.push(call_stmt);
    out.extend(kept_prelude);
    if !if_empty {
        out.push(new_if);
    }
    out.extend(stmts[if_pos + 1..].to_vec());
    *stmts = out;
    true
}


pub fn strip_enum_ctor_super(body: &mut Stmt) {
    fn is_enum_super(s: &Stmt) -> bool {
        matches!(
            s,
            Stmt::ExprStmt(Expr::Method { name, is_super: true, .. }) if &**name == "<init>"
        )
    }
    // FULL depth: hotfix-guarded ctors hide a second super(String,int)
    // inside the proxy branch (alipay InstantRun: `if (proxy != null) {
    // super((String) v3[0], ..); proxy.afterSuper(this); return; }` at
    // depth 3+; the old two-level strip left it — super resolved against
    // Object once the fallback enum lost `extends Enum`, alipay
    // ctor-arity ×3,364). This path only runs for fallback enums whose
    // dex super is Enum/Object, so EVERY super call is a dead Enum-ctor
    // reference wherever it sits; constant subclasses (real supers)
    // take the other branch in method.rs and are untouched.
    fn strip_deep(stmts: &mut Vec<Stmt>) {
        stmts.retain(|s| !is_enum_super(s));
        for st in stmts.iter_mut() {
            strip_one(st);
        }
    }
    fn strip_one(st: &mut Stmt) {
        match st {
            Stmt::Block(v) => strip_deep(v),
            Stmt::If { then_stmt, else_stmt, .. } => {
                strip_one(then_stmt);
                if let Some(e) = else_stmt {
                    strip_one(e);
                }
            }
            Stmt::While { body, .. }
            | Stmt::DoWhile { body, .. }
            | Stmt::ForEach { body, .. }
            | Stmt::Synchronized { body, .. }
            | Stmt::Labeled { body, .. } => strip_one(body),
            Stmt::For { init, body, .. } => {
                strip_deep(init);
                strip_one(body);
            }
            Stmt::Switch { cases, default, .. } => {
                for c in cases.iter_mut() {
                    strip_deep(&mut c.body);
                }
                if let Some(d) = default {
                    strip_one(d);
                }
            }
            Stmt::Try { body, catches, finally }
            | Stmt::TryWithResources { body, catches, finally, .. } => {
                strip_one(body);
                for c in catches.iter_mut() {
                    strip_one(&mut c.body);
                }
                if let Some(f) = finally {
                    strip_one(f);
                }
            }
            _ => {}
        }
    }
    let Stmt::Block(stmts) = body else {
        return;
    };
    strip_deep(stmts);
}

pub fn ensure_declared(body: &mut Stmt, vt: &VarTable) {
    // (Perf: dense Vec<bool> tables instead of HashSets — var ids are
    // dense; the `assigned` set collected here was never read and cost a
    // full extra tree walk per method.)
    let n = vt.vars.len().max(1);

    // Demote extra LocalDefs (same var declared more than once) FIRST —
    // the scope analysis below must see exactly one def per var.
    let mut seen: HashSet<u32> = HashSet::default();
    demote_dup_decls(body, &mut seen, vt);

    // Scope hoist: a LocalDef inside a nested block whose var is also
    // referenced OUTSIDE that block is a Java scope violation (the
    // branch-local `int v8 = pi.versionCode;` read after the try —
    // "cannot find symbol" at every outside use). Demote those defs to
    // assignments and let the bare top-of-method declaration below cover
    // the var. Two cheap passes: pre-order block numbering + per-var
    // inside/outside occurrence counts (pre-order subtree = id range).
    let mut def_block: Vec<u32> = vec![u32::MAX; n];
    let mut sizes: Vec<u32> = vec![0];
    let mut next_id: u32 = 1;
    scope_pass_a(body, 0, &mut next_id, &mut def_block, &mut sizes);
    sizes[0] = next_id;
    let mut within: Vec<u32> = vec![0; n];
    let mut total: Vec<u32> = vec![0; n];
    let mut next_b: u32 = 1;
    scope_pass_b(body, 0, &mut next_b, &def_block, &sizes, &mut within, &mut total);
    let mut hoist: HashSet<u32> = HashSet::default();
    for v in 0..n {
        if def_block[v] != u32::MAX && def_block[v] != 0 && within[v] < total[v] {
            hoist.insert(v as u32);
        }
    }
    if !hoist.is_empty() {
        let mut seed: HashSet<u32> = hoist.iter().copied().collect();
        demote_dup_decls(body, &mut seed, vt);
    }

    let mut declared: Vec<bool> = vec![false; n];
    let mut is_param: Vec<bool> = vec![false; n];
    for v in &vt.vars {
        if v.is_param && (v.id as usize) < n {
            is_param[v.id as usize] = true;
        }
    }
    // Catch parameters are declared by the `catch (Type v)` clause —
    // bind_catches consumed their LocalDef before this pass ran, so
    // without this they re-declare at the top of the method and clash
    // with the clause (`Throwable th2;` + `catch (Throwable th2)` — a
    // syntax gate cannot see this, it is a semantic "already defined").
    let mut is_catch: Vec<bool> = vec![false; n];
    walk_all(body, &mut |st| {
        if let Stmt::Try { catches, .. } = st {
            for c in catches {
                if (c.var as usize) < n {
                    is_catch[c.var as usize] = true;
                }
            }
        }
    });
    walk_all(body, &mut |st| {
        if let Stmt::LocalDef { var, .. } = st {
            if (*var as usize) < n {
                declared[*var as usize] = true;
            }
        }
    });
    let mut used: HashSet<u32> = HashSet::default();
    // assignments=true: a var that only ever appears as an assign
    // TARGET still needs a declaration (`x = 5;` alone does not
    // declare x in Java).
    stmt_collect_vars(body, &mut used, true);

    // Params are declared by the signature; hoisted vars lost their
    // LocalDef to the demotion above and always need the bare decl.
    let mut needs_decl: Vec<u32> = used
        .into_iter()
        .filter(|v| {
            let i = *v as usize;
            i >= n
                || ((!declared[i] && !is_param[i] && !is_catch[i]) || hoist.contains(v))
        })
        .collect();
    needs_decl.sort_unstable();
    needs_decl.dedup();

    if !needs_decl.is_empty() {
        let mut decls: Vec<Stmt> = needs_decl
            .into_iter()
            .map(|v| Stmt::LocalDef {
                var: v,
                init: None,
                is_final: false,
                force_type: true,
            })
            .collect();
        decls.reverse();
        match body {
            Stmt::Block(v) => {
                for d in decls {
                    v.insert(0, d);
                }
            }
            other => {
                let inner = std::mem::replace(other, Stmt::Block(vec![]));
                *other = Stmt::Block({
                    let mut vs = decls;
                    vs.push(inner);
                    vs
                });
            }
        }
    }
}

fn demote_dup_decls(s: &mut Stmt, seen: &mut HashSet<u32>, vt: &VarTable) {
    match s {
        // Exactly one visit per node: the Block arm consumes its children,
        // the fallthrough walk would double-visit them.
        Stmt::Block(v) => {
            for x in v.iter_mut() {
                demote_dup_decls(x, seen, vt);
            }
        }
        Stmt::LocalDef { var, init, .. } => {
            if seen.contains(var) {
                let ty = vt.var(*var).ty.clone();
                if let Some(e) = init.take() {
                    *s = Stmt::ExprStmt(Expr::Assign {
                        target: Box::new(Expr::Local { var: *var, ty }),
                        op: AssignOp::Plain,
                        value: Box::new(e),
                    });
                } else {
                    *s = Stmt::Block(vec![]);
                }
            } else {
                seen.insert(*var);
            }
        }
        other => {
            walk_mut(other, &mut |x| demote_dup_decls(x, seen, vt));
        }
    }
}

#[allow(dead_code)]
fn unused(_: &Vec<CaseGroup>, _: &Catch) {}

// ---------------------------------------------------------------------------
// jadx-style local names (ApplyVariableNames + dexdec's port of it)
// ---------------------------------------------------------------------------

/// A synthetic `v12`/`p3` never survives when a better name exists.
/// Priority: (1) a Kotlin `Intrinsics.checkNotNullParameter(x, "name")`
/// names `x` from the message string (the string IS the parameter name);
/// (2) the single defining call — `getFoo()` → `foo`, `isFinishing()` →
/// `finishing`, `new File(…)` → `file`; (3) a type alias or the
/// lowercased class simple name. Collisions take `2`, `3`, …; a var with
/// two DISAGREEING defining calls stays unnamed (dexdec's
/// RelationalNameInference rule). Debug-info names are never touched
/// (`synthetic_name` gates the whole pass).
pub fn apply_local_names(vt: &mut VarTable, body: &Stmt) {
    use std::collections::{HashMap, HashSet};

    // ---- proposals ------------------------------------------------------
    // var → (name, source-priority); disagreeing call names are dropped.
    let mut by_call: HashMap<u32, Vec<String>> = HashMap::new();
    let mut by_intrinsics: HashMap<u32, String> = HashMap::new();

    visit_all_exprs(body, &mut |e| {
        if let Expr::Method { cls, name, args, .. } = e {
            if (cls.as_ref() == "kotlin/jvm/internal/Intrinsics"
                || cls.as_ref() == "kotlin/jvm/internal/IntrinsicsKt")
                && matches!(name.as_ref(), "checkNotNullParameter" | "checkParameterIsNotNull")
                && args.len() == 2
            {
                if let (Expr::Local { var, .. }, Expr::Const(ConstVal::Str(s))) = (&args[0], &args[1])
                {
                    by_intrinsics
                        .entry(*var)
                        .or_insert_with(|| sanitize_name(s).unwrap_or_default());
                }
            }
        }
    });

    walk_all(body, &mut |st| {
        let (var, value) = match st {
            Stmt::LocalDef { var, init: Some(e), .. } => (*var, e),
            Stmt::ExprStmt(Expr::Assign { target, value, .. })
                if matches!(&**target, Expr::Local { .. }) =>
            {
                let Expr::Local { var, .. } = &**target else { return };
                (*var, value.as_ref())
            }
            _ => return,
        };
        if let Some(n) = defining_call_name(value) {
            by_call.entry(var).or_default().push(n);
        }
    });

    // ---- reservation + application --------------------------------------
    // Real (debug-info) names can legally REPEAT across sibling source
    // scopes (`int i` in two loops) but our flat declaration hoisting
    // puts them in one scope — de-duplicate with the same numeric
    // suffixes the claim path uses.
    // Uniqueness operates on the SANITIZED display name: the identifier
    // sanitizer maps non-ASCII to `_`, so distinct debug names
    // (`ERROR_token参数缺失` / `ERROR_channelId参数缺失`) both RENDER as
    // `ERROR________` and collide as declarations. Tracking raw names
    // here would let the collision through.
    let sanit = |n: &str| crate::classdec::java_ident(n).into_owned();
    let mut taken: HashSet<String> = HashSet::default();
    // Parameter names are occupied REGARDLESS of syntheticness: claim()
    // hands out rename candidates, and without the synthetic params in
    // the pool a type fallback (`l7.p0` → "p0") renamed one parameter
    // ONTO another's slot-name (`b(v6.l p0, Object obj, l7.p0 p0)`).
    for v in vt.vars.iter() {
        // ALL synthetic names occupy their names, not just parameters:
        // a type fallback (`pc5.v56` → "v56") otherwise renames a local
        // ONTO another local's slot-name (`int v56;` beside
        // `pc5.v56 v56;`).
        if v.synthetic_name {
            taken.insert(sanit(&v.name));
        }
    }
    for v in vt.vars.iter_mut() {
        if v.synthetic_name {
            continue;
        }
        if !taken.insert(sanit(&v.name)) {
            let base = sanit(&v.name);
            for i in 2.. {
                let cand = format!("{base}{i}");
                if taken.insert(cand.clone()) {
                    v.name = cand;
                    break;
                }
            }
        }
    }
    let claim = |taken: &mut HashSet<String>, want: &str| -> String {
        let w = crate::classdec::java_ident(want).into_owned();
        if taken.insert(w.clone()) {
            return w;
        }
        for i in 2.. {
            let cand = format!("{w}{i}");
            if taken.insert(cand.clone()) {
                return cand;
            }
        }
        w
    };

    for info in vt.vars.iter_mut() {
        if !info.synthetic_name {
            continue;
        }
        // (1) Intrinsics string — the Kotlin compiler wrote the real
        // parameter name right into the check.
        if let Some(n) = by_intrinsics.get(&info.id) {
            if !n.is_empty() {
                info.name = claim(&mut taken, n);
                continue;
            }
        }
        // (2) One consistent defining call.
        if let Some(names) = by_call.get(&info.id) {
            let unique: HashSet<&String> = names.iter().collect();
            if unique.len() == 1 {
                let n = unique.into_iter().next().unwrap();
                if !is_java_keyword(n) {
                    info.name = claim(&mut taken, n);
                    continue;
                }
            }
        }
        // (3) Type alias, else the lowercased simple class name
        // (jadx names a `Looper` local `looper`).
        let want = type_alias(&info.ty).map(str::to_string).or_else(|| simple_type_name(&info.ty));
        if let Some(n) = want {
            info.name = claim(&mut taken, &n);
        }
    }

    // Final uniquification across ALL vars (params, locals, catch): the
    // reservation, claim and debug-name domains above are checked
    // pairwise but a name claimed LAST can still equal a debug name
    // that never re-checked (`zd1.v2 v2` beside `zd1.v2[] v2`).
    let mut seen: HashSet<String> = HashSet::default();
    for v in vt.vars.iter_mut() {
        if !seen.insert(sanit(&v.name)) {
            let base = sanit(&v.name);
            for i in 2.. {
                let cand = format!("{base}{i}");
                if seen.insert(cand.clone()) {
                    v.name = cand;
                    break;
                }
            }
        }
    }
}

/// `com/android/.../Looper` → `looper` — the alias-table fallback.
fn simple_type_name(ty: &TypeRef) -> Option<String> {
    let TypeRef::J(JavaType::Object(n)) = ty else {
        return None;
    };
    let simple = n.rsplit('/').next().unwrap_or(n);
    let simple = simple.rsplit('$').next().unwrap_or(simple);
    // Lowercase the first ALPHABETIC char — `$`/`_`-prefixed names
    // (R8's `$$$_Thread`) must not survive capitalized.
    let mut chars = simple.chars();
    let mut out = String::with_capacity(simple.len());
    let mut lowered = false;
    for c in chars.by_ref() {
        if c.is_ascii_alphabetic() && !lowered {
            out.push(c.to_ascii_lowercase());
            lowered = true;
        } else {
            out.push(c);
        }
        if lowered {
            break;
        }
    }
    out.extend(chars);
    sanitize_name(&out)
}

/// `getFoo()` → `foo`, `isFinishing()` → `finishing`, `new File(…)` →
/// `file`. The get/is prefixes only strip when the remainder starts
/// uppercase (so `issues()` keeps its name).
fn defining_call_name(e: &Expr) -> Option<String> {
    match e {
        Expr::Method { name, .. } => {
            let base = name
                .strip_prefix("get")
                .or_else(|| name.strip_prefix("is"))
                .filter(|rest| rest.chars().next().is_some_and(|c| c.is_ascii_uppercase()))
                .unwrap_or(name);
            let first = base.chars().next()?;
            let mut out = String::with_capacity(base.len());
            out.push(first.to_ascii_lowercase());
            out.extend(base.chars().skip(1));
            sanitize_name(&out)
        }
        Expr::New { cls, .. } => {
            let simple = cls.rsplit('/').next().unwrap_or(cls);
            let simple = simple.rsplit('$').next().unwrap_or(simple);
            let first = simple.chars().next()?;
            let mut out = String::with_capacity(simple.len());
            out.push(first.to_ascii_lowercase());
            out.extend(simple.chars().skip(1));
            sanitize_name(&out)
        }
        _ => None,
    }
}

/// Valid java identifier, not a keyword/restricted name, ≥2 chars.
fn sanitize_name(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.len() < 2
        || out.chars().next().is_some_and(|c| c.is_ascii_digit())
        || is_java_keyword(&out)
    {
        return None;
    }
    Some(out)
}

/// jadx's alias table for the common types, with the Android globals the
/// corpus actually shows; anything else falls back in the caller.
fn type_alias(ty: &TypeRef) -> Option<&'static str> {
    let TypeRef::J(JavaType::Object(n)) = ty else {
        return None;
    };
    Some(match n.as_ref() {
        "java/lang/String" | "kotlin/String" => "str",
        "java/lang/Class" => "cls",
        "java/lang/Throwable" => "th",
        "java/lang/Object" | "kotlin/Any" => "obj",
        "java/util/Iterator" | "kotlin/collections/Iterator" => "it",
        "java/lang/Boolean" => "bool",
        "java/lang/Integer" => "num",
        "java/lang/Character" => "ch",
        "java/lang/Byte" => "b",
        "java/lang/Short" => "sh",
        "java/lang/Float" => "f",
        "java/lang/Double" => "d",
        "java/lang/Long" => "i",
        "java/lang/StringBuilder" => "sb",
        "java/util/ArrayList" => "list",
        "java/util/HashMap" => "map",
        "android/content/Context" => "context",
        "android/content/Intent" => "intent",
        "android/os/Bundle" => "bundle",
        "android/view/View" => "view",
        "android/graphics/Bitmap" => "bitmap",
        "android/view/ViewGroup" => "viewGroup",
        _ => return None,
    })
}

/// Every expression in the tree, statements included (read-only).
pub(crate) fn visit_all_exprs<F: FnMut(&Expr)>(s: &Stmt, f: &mut F) {
    walk_all(s, &mut |st| {
        let exprs: Vec<&Expr> = match st {
            Stmt::ExprStmt(Expr::Assign { target, value, .. }) => {
                let mut v: Vec<&Expr> = vec![value];
                if !matches!(&**target, Expr::Local { .. }) {
                    v.push(target);
                }
                v
            }
            Stmt::ExprStmt(e) | Stmt::Throw(e) | Stmt::MonitorEnter(e) | Stmt::MonitorExit(e) => {
                vec![e]
            }
            Stmt::Return(Some(e)) => vec![e],
            Stmt::LocalDef { init: Some(e), .. } => vec![e],
            Stmt::If { cond, .. } | Stmt::While { cond, .. } | Stmt::DoWhile { cond, .. } => {
                vec![cond]
            }
            _ => Vec::new(),
        };
        for e in exprs {
            visit_exprs(e, f);
        }
    });
}

/// IntDef/LongDef exact-match rendering: a literal argument to a
/// platform method whose parameter carries a constant domain renders as
/// the named constant (`setVisibility(8)` → `android.view.View.GONE`).
/// Combined flag values and literals outside the domain stay numeric.
pub fn platform_constants(body: &mut Stmt) {
    if !crate::platform::installed() {
        return;
    }
    rewrite_exprs(body, &mut |e| {
        if let Expr::Method { cls, name, desc, args, .. } = e {
            for a in args.iter_mut() {
                let (value, is_long) = match a {
                    Expr::Const(ConstVal::Int(v)) => (*v as i64, false),
                    Expr::Const(ConstVal::Long(v)) => (*v, true),
                    _ => continue,
                };
                if let Some(named) =
                    crate::platform::named_constant(cls, name, &desc_to_string(desc), 0, value)
                {
                    let _ = is_long;
                    *a = Expr::Raw(named);
                }
            }
        }
    });
}

/// MethodDescriptor → dex descriptor string (`(I)V`).
fn desc_to_string(d: &jdc_core::types::MethodDescriptor) -> String {
    let mut out = String::from("(");
    for a in &d.args {
        type_to_desc(a, &mut out);
    }
    out.push(')');
    type_to_desc(&d.ret, &mut out);
    out
}

fn type_to_desc(t: &jdc_core::types::JavaType, out: &mut String) {
    match t {
        JavaType::Void => out.push('V'),
        JavaType::Boolean => out.push('Z'),
        JavaType::Byte => out.push('B'),
        JavaType::Char => out.push('C'),
        JavaType::Short => out.push('S'),
        JavaType::Int => out.push('I'),
        JavaType::Float => out.push('F'),
        JavaType::Long => out.push('J'),
        JavaType::Double => out.push('D'),
        JavaType::Object(n) => {
            out.push('L');
            out.push_str(n);
            out.push(';');
        }
        JavaType::Array(inner) => {
            out.push('[');
            type_to_desc(inner, out);
        }
    }
}

/// Kotlin `Intrinsics` null-check elision (jadx's ProcessKotlinInternals):
/// statement-position `checkNotNullParameter(p, "name")` /
/// `checkParameterIsNotNull` / `checkNotNull…` calls are runtime
/// assertions — drop them. The naming pass harvests the parameter
/// strings FIRST, so run this after `apply_local_names`.
pub fn remove_kotlin_checks(s: &mut Stmt) {
    walk_mut_deep(s, &mut |st| {
        let Stmt::Block(v) = st else { return };
        v.retain(|x| !is_kotlin_check(x));
    });
}

fn is_kotlin_check(s: &Stmt) -> bool {
    let Stmt::ExprStmt(e) = s else { return false };
    let Expr::Method { cls, name, .. } = e else { return false };
    (cls.as_ref() == "kotlin/jvm/internal/Intrinsics" || cls.as_ref() == "kotlin/jvm/internal/IntrinsicsKt__Jdk7Kt")
        && matches!(
            name.as_ref(),
            "checkNotNullParameter"
                | "checkParameterIsNotNull"
                | "checkExpressionValueIsNotNull"
                | "checkNotNullExpressionValue"
                | "checkReturnedValueIsNotNull"
                | "checkFieldIsNotNull"
                | "checkNotNull"
        )
}

// ---------------------------------------------------------------------------
// Synthetic-accessor inlining (jadx's MarkMethodsForInline, the safe subset)
// ---------------------------------------------------------------------------

/// Inline `access$NNN`-style synthetic static bridges at their call
/// sites: an identity (`return pN`), a getter (`iget pX, field;
/// return vR`), or a method forwarder (`invoke; return`) — with the
/// d8 APM trace wrappers (`MethodCollector.i/.o` const-only static
/// calls) tolerated around the core. Only STATIC + SYNTHETIC callees
/// qualify (compiler-generated bridges: no override semantics, no
/// side effects beyond the forwarded operation).
/// Can `host` (internal class name) source-level reference `target`'s
/// class chain and member? Cross-package inlining of a synthetic
/// accessor used to EXPOSE package-private targets the raw dex never
/// referenced cross-package (BuildersKt.launch$default forwards to
/// kotlinx.coroutines.k.e — the widening census cannot see post-inline
/// calls; lark's 1,019-line k不是公共的 family). Non-public chain link
/// or non-public member across packages → the inline must not happen
/// (the public synthetic bridge stays and compiles).
fn inline_ref_ok(pool: &DexPool, host: &str, target: &str, member: Option<(&str, bool)>) -> bool {
    let hpkg = match host.rfind('/') {
        Some(i) => &host[..i],
        None => "",
    };
    let mut cur = target;
    loop {
        let tpkg = match cur.rfind('/') {
            Some(i) => &cur[..i],
            None => "",
        };
        if tpkg != hpkg {
            if let Some(pc) = pool.get(cur) {
                if pc.access & crate::access::ACC_PUBLIC == 0 {
                    return false;
                }
                if cur == target {
                    if let Some((mname, is_method)) = member {
                        let ok = if is_method {
                            pc.all_methods().any(|m| {
                                &*m.name == mname
                                    && m.access & crate::access::ACC_PUBLIC != 0
                            })
                        } else {
                            pc.static_fields
                                .iter()
                                .chain(pc.instance_fields.iter())
                                .any(|f| {
                                    f.name.as_ref() == mname
                                        && f.access & crate::access::ACC_PUBLIC != 0
                                })
                        };
                        // A member match requires the exact member to be
                        // public; an absent member (renamed/collapsed)
                        // stays allowed — the render will show what the
                        // pool has.
                        if !ok
                            && (if is_method {
                                pc.all_methods().any(|m| &*m.name == mname)
                            } else {
                                pc.static_fields
                                    .iter()
                                    .chain(pc.instance_fields.iter())
                                    .any(|f| f.name.as_ref() == mname)
                            })
                        {
                            return false;
                        }
                    }
                }
            }
        }
        match cur.rfind('$') {
            Some(i) if i > 0 => cur = &cur[..i],
            _ => break,
        }
    }
    true
}

pub fn inline_accessors(s: &mut Stmt, pool: &DexPool, host: &str) {
    rewrite_exprs(s, &mut |e| {
        let Expr::Method { cls, name, desc, args, is_static, .. } = e else { return };
        if !*is_static || args.is_empty() {
            return;
        }
        let Some(target) = pool.get(cls) else { return };
        let desc_s = desc_to_string(desc);
        let Some(m) = target.find_method(name, &desc_s) else { return };
        if !(m.is_static() && m.access & access::ACC_SYNTHETIC != 0) {
            return;
        }
        let Some(dex) = pool.dex(m.dex_idx) else { return };
        // Snapshot first: the accessor's owning image may already be
        // retired, and a live read's success would depend on worker
        // completion interleaving — nondeterministic inlining (whether
        // the accessor folds at all) between identical runs.
        let code = match pool.accessor_code(m.dex_idx, m.code_off) {
            Some(bytes) => ddc_dex::CodeItem::parse(&bytes, 0),
            None => dex.code_at(m.code_off),
        };
        let Some(code) = code else { return };
        let insns: Vec<&Insn> =
            code.insns.iter().filter(|i| !matches!(i.kind, InsnKind::Nop)).collect();
        // Strip the APM trace wrappers (const-only invoke-static).
        let core: Vec<&Insn> = insns
            .iter()
            .copied()
            .filter(|i| !is_const_only_trace(&i.kind, &insns))
            .collect();
        match accessor_shape(&dex, &core, &code) {
            Some(Shape::Identity(arg_i)) => {
                if let Some(a) = args.get(arg_i) {
                    *e = a.clone();
                }
            }
            Some(Shape::FieldRead { arg, cls: field_cls, field, field_ty }) => {
                if !inline_ref_ok(pool, host, &field_cls, Some((&field, false))) {
                    return;
                }
                if let Some(owner) = args.get(arg).cloned().map(Box::new) {
                    let ty = TypeRef::J(desc_type(&field_ty));
                    // The inlined read must consult the member rename
                    // registry like every lifted field ref does — the
                    // raw dex name desynced from renamed declarations
                    // (lark uu4/f$b's `a`→`ax6`: 找不到符号 变量 a ×4.4k
                    // when the nested-collision deshadow fired).
                    let name = jdc_core::rename::field_display(
                        &field_cls,
                        &field,
                        &field_ty,
                    )
                    .map(std::sync::Arc::from)
                    .unwrap_or_else(|| field.into());
                    *e = Expr::Field {
                        owner: Some(owner),
                        cls: field_cls.into(),
                        name,
                        ty,
                        is_static: false,
                    };
                }
            }
            Some(Shape::Forward { cls: tcls, name: tname, desc: tdesc, instance }) => {
                if !inline_ref_ok(pool, host, &tcls, Some((&tname, true))) {
                    return;
                }
                // Same registry duty as FieldRead: the forwarded member
                // must render under its renamed display, not the raw
                // dex name.
                let disp = jdc_core::rename::field_display(
                    &tcls,
                    &tname,
                    &tdesc.to_string(),
                )
                .map(std::sync::Arc::from)
                .unwrap_or_else(|| tname.into());
                *cls = tcls.into();
                *name = disp;
                *desc = std::sync::Arc::new(tdesc);
                if instance && !args.is_empty() {
                    // The first param (the receiver) becomes the owner.
                    let recv = args.remove(0);
                    if let Expr::Method { owner, .. } = e {
                        *owner = Some(Box::new(recv));
                    }
                }
            }
            None => {}
        }
    });
}

/// What an accessor body reduces to.
enum Shape {
    /// `return pN` — the call becomes argument N.
    Identity(usize),
    /// `iget vR, pX, field; return vR` — becomes `argN.field`.
    FieldRead { arg: usize, cls: String, field: String, field_ty: String },
    /// `invoke {pX, args…}, method@M; (move-result;)? return` —
    /// becomes the forwarded call.
    Forward { cls: String, name: String, desc: MethodDescriptor, instance: bool },
}

/// Resolve the IGet field's owner class, name and type descriptor.
fn field_of(dex: &ddc_dex::DexFile, field_idx: u32) -> Option<(String, String, String)> {
    let f = dex.field(field_idx);
    Some((
        // class_name strips the `L...;` shell; type_name would leak a
        // descriptor into the Field's owner class (`La3.a.e_`).
        dex.class_name(f.class_idx),
        dex.string(f.name_idx).to_string(),
        // The FIELD TYPE stays a descriptor: desc_type parses it.
        dex.type_name(f.type_idx).to_string(),
    ))
}

/// Map a register to a parameter index (static method: params occupy
/// the LAST ins_size registers).
fn param_index(code: &ddc_dex::CodeItem, reg: u16) -> Option<usize> {
    let rs = code.registers_size as usize;
    let ins = code.ins_size as usize;
    let r = reg as usize;
    let first = rs.checked_sub(ins)?;
    (r >= first && r < rs).then(|| r - first)
}

fn accessor_shape(
    dex: &ddc_dex::DexFile,
    core: &[&Insn],
    code: &ddc_dex::CodeItem,
) -> Option<Shape> {
    // Dead consts that fed the stripped trace wrappers remain in the
    // core — skip them. ONLY when actually dead (dst unread by the
    // rest): a const-returning synthetic (`lambda$new$4(String) {
    // return false; }` = `const/4 v0,0; return v0`, registers=1 so v0
    // IS the param) lost its const and matched Identity(param0) —
    // inline_accessors then replaced the CALL with its ARGUMENT
    // (`return (String) obj` against a boolean SAM — weibo
    // IntentSanitizer lambdas ×116, and silently WRONG renders
    // wherever the types happened to agree).
    let reads = |rest: &[&Insn], r: u16| {
        rest.iter()
            .any(|i| crate::lift::src_regs(&i.kind).contains(&r))
    };
    let core: &[&Insn] = match core.split_first() {
        Some((first, rest))
            if matches!(first.kind, InsnKind::Const { dst, .. } if !reads(rest, dst)) =>
        {
            rest
        }
        _ => core,
    };
    let core: &[&Insn] = match core.split_last() {
        Some((last, rest))
            if matches!(last.kind, InsnKind::Const { dst, .. } if !reads(rest, dst)) =>
        {
            rest
        }
        _ => core,
    };
    match core {
        // return pN
        [Insn { kind: InsnKind::Return { src }, .. }] => {
            let idx = param_index(code, *src)?;
            Some(Shape::Identity(idx))
        }
        // iget vR, pX, field; return vR
        [
            Insn { kind: InsnKind::IGet { dst, obj, field_idx }, .. },
            Insn { kind: InsnKind::Return { src }, .. },
        ] if dst == src => {
            let arg = param_index(code, *obj)?;
            let (cls, field, field_ty) = field_of(dex, *field_idx)?;
            Some(Shape::FieldRead { arg, cls, field, field_ty })
        }
        // invoke {…}, method@M; (move-result; return)? — forwarder.
        [Insn { kind: InsnKind::Invoke { method_idx, regs, kind, .. }, .. }, rest @ ..] if rest.len() <= 2 => {
            // Only forward when every register is a param (no locals).
            if !regs.iter().all(|r| param_index(code, *r).is_some()) || regs.is_empty() {
                return None;
            }
            // Trailing must be move-result+return or return/void —
            // anything else (e.g. trace calls) already filtered upstream.
            if rest.len() == 2 {
                let ok = matches!(rest[0].kind, InsnKind::MoveResult { .. })
                    && matches!(rest[1].kind, InsnKind::Return { .. } | InsnKind::ReturnVoid);
                if !ok {
                    return None;
                }
            }
            let mid = dex.method(*method_idx);
            // class_name strips the `L...;` shell — type_name leaks a
            // descriptor into the forwarded call's class (`La3.a.e_`).
            let cls = dex.class_name(mid.class_idx);
            let name = dex.string(mid.name_idx).to_string();
            let mut s = String::from("(");
            for t in dex.proto_params(mid.proto_idx) {
                s.push_str(dex.type_name(*t));
            }
            s.push(')');
            s.push_str(dex.type_name(dex.proto(mid.proto_idx).return_type_idx));
            let desc = parse_method_descriptor(&s)?;
            Some(Shape::Forward { cls, name, desc, instance: !matches!(kind, InvokeKind::Static) })
        }
        _ => None,
    }
}


/// A const-only invoke-static (the d8 APM `i(732046)` / `o(732046)`
/// trace wrapper): every argument register is loaded by a Const (or a
/// Const-into-move chain) in the SAME body. Scoped to synthetic
/// accessors, so a genuinely side-effecting const call is never
/// mistaken for a trace.
fn is_const_only_trace(kind: &InsnKind, insns: &[&Insn]) -> bool {
    let InsnKind::Invoke { kind, regs, .. } = kind else { return false };
    if !matches!(kind, InvokeKind::Static) || regs.is_empty() {
        return false;
    }
    regs.iter().all(|r| const_loaded(*r, insns, 0))
}

/// Const-loaded directly, or through a Move chain from a const-loaded
/// register (the APM `const v0, id` → `invoke {v0}` shape).
fn const_loaded(r: u16, insns: &[&Insn], depth: u8) -> bool {
    if depth > 3 {
        return false;
    }
    insns.iter().any(|i| match &i.kind {
        InsnKind::Const { dst, .. } => *dst == r,
        InsnKind::Move { dst, src } => *dst == r && const_loaded(*src, insns, depth + 1),
        _ => false,
    })
}
