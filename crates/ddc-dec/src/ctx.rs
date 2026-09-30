//! The DEX front-end's implementation of `jdc_core::Ctx`.
//!
//! Every method here answers a *semantic* question about class metadata
//! using the pooled DEX class table; generics are absent (erased), so all
//! generic-aware queries answer "unknown" and the core degrades gracefully.

use jdc_core::ir::expr::{Expr, TypeRef};
use jdc_core::types::{
    ClassAccessFlags, FieldAccessFlags, GenericType, JavaType, MethodAccessFlags, MethodDescriptor,
};
use jdc_core::{Ctx, Family, NestedClass, NestedKind};

use crate::access::*;
use crate::{desc_type, DexPool, PoolClass};

/// Front-end context handed to the core's structurer and printer.
pub struct DexCtx<'a> {
    pub pool: &'a DexPool,
    pub class: &'a PoolClass,
    /// Field display names visible in this class's lexical scope by
    /// INHERITANCE or ENCLOSURE (supers/interfaces of the class and of
    /// every `$`-outer, transitively). `declares_field` only saw own
    /// fields: an inherited int `h` captured the class ref `h.e`
    /// (无法取消引用int, weixin v2/j family) because the shadow check
    /// missed it and the simple render stayed unqualified.
    inherited_fields: jdc_core::FxHashSet<String>,
}

impl<'a> DexCtx<'a> {
    pub fn new(pool: &'a DexPool, class: &'a PoolClass) -> Self {
        let inherited_fields = {
            let mut set: jdc_core::FxHashSet<String> = jdc_core::FxHashSet::default();
            let mut level = class.name.clone();
            loop {
                let mut queue = vec![level.clone()];
                let mut seen: jdc_core::FxHashSet<String> =
                    jdc_core::FxHashSet::default();
                let mut hops = 0u32;
                while let Some(c) = queue.pop() {
                    if hops >= 64 || !seen.insert(c.clone()) {
                        continue;
                    }
                    hops += 1;
                    let Some(pc) = pool.get(&c) else { continue };
                    if c != class.name {
                        for f in pc.static_fields.iter().chain(pc.instance_fields.iter()) {
                            set.insert(crate::classdec::java_ident(&f.name).into_owned());
                        }
                    }
                    if let Some(sup) = &pc.super_name {
                        queue.push(sup.clone());
                    }
                    queue.extend(pc.interfaces.iter().cloned());
                }
                let Some(i) = level.rfind('$') else { break };
                level.truncate(i);
            }
            set
        };
        DexCtx { pool, class, inherited_fields }
    }

    fn find_class(&self, internal: &str) -> Option<&PoolClass> {
        self.pool.get(internal)
    }

    /// The javac-convention enclosing field: named `this$0` AND typed
    /// as the outer. The pure type match over-fires on STATIC nested
    /// classes that legitimately hold an outer-typed field (wcdb
    /// CancellationSignal$Transport's mCancellationSignal — rendered as
    /// an inner class, `new Transport(null)` from a static method is
    /// "需要包含...的封闭实例"). An obfuscated-renamed this$0 falls to
    /// the static render, which stays compilable: the ctor keeps its
    /// outer param and every call site passes it explicitly.
    /// JLS inner-ctor shape: the synthetic outer instance rides as
    /// formal-0 typed as the DIRECT enclosing class. Field evidence
    /// alone misclassifies Kotlin lambda classes — kotlinc names the
    /// captured `this` field this$0 too, but at an ARBITRARY param
    /// position (lark SearchResultView$q: `(int $requestSeq,
    /// SearchResultView this$0)`; the int capture was hijacked as the
    /// qualified-new outer — `(this.w + 1).new q17(this)`, lark
    /// 意外的类型 ×295). Mirrors the decl-side strip guard in classdec
    /// (`d.args.first()` must BE the enclosing type).
    fn outer_is_formal0(&self, internal: &str, pc: &PoolClass) -> bool {
        let Some(outer) = self.find_outer(internal) else {
            return false;
        };
        pc.all_methods().any(|m| {
            &*m.name == "<init>"
                && m.parsed_desc().is_some_and(|d| {
                    d.args.first().is_some_and(|t| {
                        matches!(t, JavaType::Object(n) if n.as_ref() == outer.as_str())
                    })
                })
        })
    }

    fn holds_this0(&self, internal: &str, pc: &PoolClass) -> bool {
        let Some(outer) = self.find_outer(internal) else {
            return false;
        };
        pc.instance_fields.iter().any(|f| {
            f.name == "this$0"
                && f
                    .desc
                    .strip_prefix('L')
                    .and_then(|d| d.strip_suffix(';'))
                    .is_some_and(|ty| ty == outer)
        })
    }

    /// Nesting evidence (see `find_outer_name`).
    fn outer_of(&self, internal: &str) -> Option<String> {
        crate::find_outer_name(self.pool, internal)
    }

    fn class_access_flags(&self, internal: &str) -> ClassAccessFlags {
        let mut f = ClassAccessFlags::empty();
        let Some(pc) = self.find_class(internal) else {
            return f;
        };
        let a = pc.access;
        if a & ACC_PUBLIC != 0 {
            f |= ClassAccessFlags::PUBLIC;
        }
        if a & ACC_FINAL != 0 {
            f |= ClassAccessFlags::FINAL;
        }
        if a & ACC_INTERFACE != 0 {
            f |= ClassAccessFlags::INTERFACE;
        }
        if a & ACC_ABSTRACT != 0 {
            f |= ClassAccessFlags::ABSTRACT;
        }
        if a & ACC_SYNTHETIC != 0 {
            f |= ClassAccessFlags::SYNTHETIC;
        }
        if a & ACC_ANNOTATION != 0 {
            f |= ClassAccessFlags::ANNOTATION;
        }
        if a & ACC_ENUM != 0 {
            f |= ClassAccessFlags::ENUM;
        }
        f
    }
}

impl<'a> Ctx for DexCtx<'a> {
    fn pool_id(&self) -> u64 {
        // Printer::shorten memo cache key: every DexCtx in a run shares
        // one DexPool, and shorten's ctx queries (has_class/find_outer/
        // class_bases) are pool-level — one cache entry set per pool.
        self.pool as *const DexPool as u64
    }

    fn obscured_simple(&self, internal: &str) -> Option<String> {
        crate::classdec::obscured_render_pub(internal)
    }

    fn class_name(&self) -> &str {
        &self.class.name
    }

    fn source_level(&self) -> u16 {
        52
    }

    fn find_outer(&self, internal: &str) -> Option<String> {
        // ddcroot root-package relocation, SYMMETRIC with has_class's
        // root_pkg clause: a relocated NESTED class (`ddcroot/agv$b`)
        // must resolve its outer (`ddcroot/agv`) so nested_display dots
        // the `$` into `ddcroot.agv.b`. has_class already un-prefixes
        // `ddcroot/agv$b` to the pool key `agv$b` (→ true), but without
        // this the outer walk fails (`ddcroot/agv` is not a pool key),
        // so keep_dollar_pool saw (has_class, no-outer) and treated the
        // genuine nestee as a flat `$` emission unit — rimet rendered
        // `new ddcroot.agv$b(..)` / `ddcroot.agv$b[]`, unresolvable
        // (找不到符号 类 agv$b/qse$b/bik$a ×1,100+). A relocated FLAT unit
        // (R8 deleted the outer, lark UserCustomStatusExtraParams$*)
        // still returns None here — the stripped outer is off-pool — so
        // it correctly keeps its `$`.
        if let Some(rp) = crate::root_pkg_display() {
            if internal.len() > rp.len() + 1
                && internal.starts_with(rp.as_str())
                && internal.as_bytes()[rp.len()] == b'/'
            {
                let stripped = &internal[rp.len() + 1..];
                return crate::find_outer_name(self.pool, stripped)
                    .map(|o| format!("{rp}/{o}"));
            }
        }
        self.outer_of(internal)
    }

    fn family(&self, root: &str) -> Family {
        let mut fam = Family {
            root: root.to_string(),
            ..Default::default()
        };
        // Cached child index: BFS over the `$` chain from `root`.
        let mut queue: std::collections::VecDeque<String> =
            self.pool.children_of(root).iter().cloned().collect();
        let mut seen: jdc_core::FxHashSet<String> = queue.iter().cloned().collect();
        while let Some(name) = queue.pop_front() {
            for c in self.pool.children_of(&name) {
                if seen.insert(c.clone()) {
                    queue.push_back(c.clone());
                }
            }
            let Some(_) = self.find_class(&name) else {
                continue;
            };
            let root_prefix = format!("{}$", root);
            let rest: String = if name.starts_with(&root_prefix) {
                name[root_prefix.len()..].to_string()
            } else {
                name.rsplit('$').next().unwrap_or(&name).to_string()
            };
            let simple = rest.rsplit('$').next().unwrap_or(&rest).to_string();
            let kind = classify_nested(&rest);
            let access = self.class_access_flags(&name);
            fam.nested.insert(
                name.clone(),
                NestedClass {
                    name: name.clone(),
                    simple,
                    kind,
                    access,
                    sig_header: None,
                },
            );
            match kind {
                NestedKind::Anonymous => {
                    fam.anonymous.insert(name.clone());
                }
                NestedKind::Local => {
                    fam.locals.insert(name.clone());
                }
                NestedKind::Lambda => {
                    fam.lambdas.insert(name.clone());
                }
                NestedKind::Member => {}
            }
        }
        fam
    }

    fn nested_is_static(&self, internal: &str) -> bool {
        // ACC_STATIC (annotation evidence) OR the structural signal:
        // no instance field typed as the outer class. javac ALWAYS
        // gives a non-static inner class an enclosing-instance field
        // (this$0), and the field TYPE survives obfuscation that
        // renames the field itself. Plain d8 output carries no
        // nesting annotations at all — without the structural
        // fallback every static nested class rendered as an inner
        // one (`str.new Report(...)` swallowing the first ctor arg).
        // A this$0 field WITHOUT the formal-0 outer ctor param is a
        // capture, not a JLS outer (Kotlin lambdas) — static too.
        // Logic lives in the free `nested_is_static` below (shared
        // with method.rs passes that run without a ctx).
        match self.find_class(internal) {
            Some(pc) => nested_is_static(self.pool, pc),
            None => true,
        }
    }

    fn class_has_this0(&self, internal: &str) -> bool {
        // Inner classes without ACC_STATIC carry an enclosing instance.
        match self.find_class(internal) {
            Some(pc) => {
                !pc.is_static_nested()
                    && self.holds_this0(internal, pc)
                    && self.outer_is_formal0(internal, pc)
            }
            None => false,
        }
    }

    fn is_subtype_of(&self, sub: &JavaType, sup: &str) -> bool {
        if let JavaType::Object(n) = sub {
            self.pool.is_subtype(n, sup)
        } else {
            false
        }
    }

    fn is_interface(&self, internal: &str) -> bool {
        self.find_class(internal)
            .map(|pc| pc.is_interface())
            .unwrap_or(false)
    }

    fn is_sealed(&self, _internal: &str) -> bool {
        false
    }

    fn super_name(&self, internal: &str) -> Option<String> {
        self.find_class(internal)
            .and_then(|pc| pc.super_name.clone())
    }

    fn has_class(&self, internal: &str) -> bool {
        self.pool.get(internal).is_some()
            || internal == self.class.name
            // The ddcroot ROOT-PACKAGE RELOCATION only: the display
            // prepends the synthetic package, the pool key lacks it.
            // Deliberately NOT a general un-rename: collision-renamed
            // nested families (weibo x0$a$b) render dotted against
            // their renamed emission owners — a general un-rename
            // flipped them to flat `$` renders (weibo 5,357 → 77,783
            // cannot-find cascade). Flat `$` here is right ONLY for
            // relocated root classes, which ARE their own emission
            // unit (lark UserCustomStatusExtraParams$* "不可见" ×138).
            || crate::root_pkg_display().is_some_and(|rp| {
                internal.len() > rp.len() + 1
                    && internal.starts_with(rp.as_str())
                    && internal.as_bytes()[rp.len()] == b'/'
                    && self.pool.get(&internal[rp.len() + 1..]).is_some()
            })
    }

    fn class_supers_args(
        &self,
        internal: &str,
        _args: &[GenericType],
    ) -> Vec<(String, Vec<GenericType>)> {
        let mut out = Vec::new();
        let mut cur = self.find_class(internal).map(|pc| pc.name.clone());
        let mut hops = 0;
        while let Some(name) = cur {
            hops += 1;
            if hops > 64 {
                break;
            }
            if name == "java/lang/Object" {
                break;
            }
            out.push((name.clone(), Vec::new()));
            cur = self.find_class(&name).and_then(|pc| pc.super_name.clone());
        }
        out
    }

    fn class_bases(&self, internal: &str) -> Option<(Vec<String>, Option<String>)> {
        let pc = self.find_class(internal)?;
        Some((pc.interfaces.clone(), pc.super_name.clone()))
    }

    fn field_flags(&self, internal: &str, name: &str) -> Option<FieldAccessFlags> {
        let pc = self.find_class(internal)?;
        let raw = pc
            .static_fields
            .iter()
            .chain(pc.instance_fields.iter())
            .find(|f| f.name == name)?
            .access;
        Some(raw_field_flags(raw))
    }

    fn method_flags(&self, internal: &str, name: &str, desc: &str) -> Option<MethodAccessFlags> {
        let pc = self.find_class(internal)?;
        let m = pc.find_method(name, desc)?;
        Some(raw_method_flags(m.access))
    }

    fn declares_field(&self, internal: &str, name: &str) -> bool {
        if internal == self.class.name {
            return self.inherited_fields.contains(name)
                || self.class.field_flags_of(name).is_some();
        }
        self.find_class(internal)
            .map(|pc| pc.field_flags_of(name).is_some())
            .unwrap_or(false)
    }

    fn is_fw_shadow(&self, internal: &str) -> bool {
        crate::is_fw_shadow(internal)
    }

    fn declares_field_display(&self, internal: &str, display: &str) -> bool {
        let pc = if internal == self.class.name {
            self.class
        } else {
            match self.find_class(internal) {
                Some(pc) => pc,
                None => return false,
            }
        };
        pc.static_fields
            .iter()
            .chain(pc.instance_fields.iter())
            .any(|f| {
                let disp = jdc_core::rename::field_display(internal, &f.name, &f.desc)
                    .map(|d| d.to_string())
                    .unwrap_or_else(|| crate::classdec::java_ident(&f.name).into_owned());
                disp == display
            })
    }

    fn declares_method_named(&self, internal: &str, name: &str) -> bool {
        self.find_class(internal)
            .map(|pc| pc.all_methods().any(|m| &*m.name == name))
            .unwrap_or(false)
    }

    fn is_generic_call(&self, _e: &Expr) -> bool {
        false
    }

    fn generic_call_formals(&self, _e: &Expr) -> Option<(Vec<GenericType>, Vec<String>)> {
        None
    }

    fn polymorphic_ret_cast(
        &self,
        _cls: &str,
        _name: &str,
        _desc: &MethodDescriptor,
    ) -> Option<JavaType> {
        None
    }

    fn ctor_formals_by_arity(&self, internal: &str, arity: usize) -> Option<Vec<GenericType>> {
        let pc = self.find_class(internal)?;
        let ctors = pc.ctors_by_arity(arity);
        let m = ctors.first()?;
        let md = m.parsed_desc()?;
        Some(md.args.iter().map(java_type_to_generic).collect())
    }

    fn ctor_param_types(
        &self,
        internal: &str,
        skip: usize,
        n: usize,
        args: &[Expr],
    ) -> Option<Vec<JavaType>> {
        let _ = args;
        let pc = self.find_class(internal)?;
        let ctors = pc.ctors_by_arity(skip + n);
        let m = ctors.first()?;
        let md = m.parsed_desc()?;
        Some(md.args.iter().skip(skip).cloned().collect())
    }

    fn outer_param_via_super(&self, _internal: &str) -> bool {
        false
    }

    fn nested_method(
        &self,
        _l: &jdc_core::ir::expr::LambdaExpr,
        _outer_vt: &jdc_core::var::VarTable,
    ) -> Option<jdc_core::MethodBody> {
        None
    }
}

/// `$`-suffix classification (javac conventions d8 preserves).
fn classify_nested(rest: &str) -> NestedKind {
    // rest is the text after `Outer$`.
    let tail = rest.rsplit('$').next().unwrap_or(rest);
    if rest.starts_with("-$$Lambda$") || tail.starts_with("-$$Lambda$") {
        return NestedKind::Lambda;
    }
    if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
        return NestedKind::Anonymous;
    }
    if tail.starts_with(|c: char| c.is_ascii_digit()) {
        return NestedKind::Local;
    }
    NestedKind::Member
}

fn raw_field_flags(raw: u32) -> FieldAccessFlags {
    let mut f = FieldAccessFlags::empty();
    if raw & ACC_PUBLIC != 0 {
        f |= FieldAccessFlags::PUBLIC;
    }
    if raw & ACC_PRIVATE != 0 {
        f |= FieldAccessFlags::PRIVATE;
    }
    if raw & ACC_PROTECTED != 0 {
        f |= FieldAccessFlags::PROTECTED;
    }
    if raw & ACC_STATIC != 0 {
        f |= FieldAccessFlags::STATIC;
    }
    if raw & ACC_FINAL != 0 {
        f |= FieldAccessFlags::FINAL;
    }
    if raw & ACC_SYNTHETIC != 0 {
        f |= FieldAccessFlags::SYNTHETIC;
    }
    if raw & ACC_ENUM != 0 {
        f |= FieldAccessFlags::ENUM;
    }
    f
}

fn raw_method_flags(raw: u32) -> MethodAccessFlags {
    let mut f = MethodAccessFlags::empty();
    if raw & ACC_PUBLIC != 0 {
        f |= MethodAccessFlags::PUBLIC;
    }
    if raw & ACC_PRIVATE != 0 {
        f |= MethodAccessFlags::PRIVATE;
    }
    if raw & ACC_PROTECTED != 0 {
        f |= MethodAccessFlags::PROTECTED;
    }
    if raw & ACC_STATIC != 0 {
        f |= MethodAccessFlags::STATIC;
    }
    if raw & ACC_FINAL != 0 {
        f |= MethodAccessFlags::FINAL;
    }
    if raw & ACC_SYNCHRONIZED != 0 || raw & ACC_DECLARED_SYNCHRONIZED != 0 {
        f |= MethodAccessFlags::SYNCHRONIZED;
    }
    if raw & ACC_BRIDGE != 0 {
        f |= MethodAccessFlags::BRIDGE;
    }
    if raw & ACC_VARARGS != 0 {
        f |= MethodAccessFlags::VARARGS;
    }
    if raw & ACC_NATIVE != 0 {
        f |= MethodAccessFlags::NATIVE;
    }
    if raw & ACC_ABSTRACT != 0 {
        f |= MethodAccessFlags::ABSTRACT;
    }
    if raw & ACC_STRICT != 0 {
        f |= MethodAccessFlags::STRICT;
    }
    if raw & ACC_SYNTHETIC != 0 {
        f |= MethodAccessFlags::SYNTHETIC;
    }
    f
}

/// Erased JavaType → generic type view (no signature data in DEX).
pub fn java_type_to_generic(t: &JavaType) -> GenericType {
    match t {
        JavaType::Object(n) => GenericType::Class(class_sig_of(n)),
        JavaType::Array(inner) => GenericType::Array(Box::new(java_type_to_generic(inner))),
        other => GenericType::Primitive(other.primitive_char().unwrap_or('V')),
    }
}

fn class_sig_of(internal: &str) -> jdc_core::types::ClassSig {
    let (package, name) = match internal.rfind('/') {
        Some(i) => (internal[..i].to_string(), internal[i + 1..].to_string()),
        None => (String::new(), internal.to_string()),
    };
    jdc_core::types::ClassSig {
        package,
        parts: vec![jdc_core::types::ClassSigPart { name, args: vec![] }],
    }
}

/// Render a type reference for signatures where the printer is not used.
pub fn type_ref_of(desc: &str) -> TypeRef {
    TypeRef::J(desc_type(desc))
}

/// Pool-level mirror of the `DexCtx::nested_is_static` render decision
/// for passes that run without a ctx (method.rs). ACC_STATIC annotation
/// evidence OR the structural signals — no outer-typed `this$0` field,
/// or no formal-0 outer ctor param (a this$0 field without the formal-0
/// param is a Kotlin lambda capture, not a JLS outer).
pub fn nested_is_static(pool: &DexPool, pc: &PoolClass) -> bool {
    if pc.is_static_nested() {
        return true;
    }
    let Some(outer) = crate::find_outer_name(pool, &pc.name) else {
        return true;
    };
    let holds = pc.instance_fields.iter().any(|f| {
        f.name == "this$0"
            && f
                .desc
                .strip_prefix('L')
                .and_then(|d| d.strip_suffix(';'))
                .is_some_and(|ty| ty == outer)
    });
    if !holds {
        return true;
    }
    !pc.all_methods().any(|m| {
        &*m.name == "<init>"
            && m.parsed_desc().is_some_and(|d| {
                d.args.first().is_some_and(|t| {
                    matches!(t, JavaType::Object(n) if n.as_ref() == outer.as_str())
                })
            })
    })
}
