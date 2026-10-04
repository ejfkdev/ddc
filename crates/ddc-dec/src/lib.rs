//! ddc-dec — DEX → Java decompiler front-end over `jdc-core`.
//!
//! Pipeline per method: DEX code units → CFG → per-block register→IR
//! lifting → `jdc-core` structuring/conversion → refinement passes →
//! emission. Class-level rendering mirrors jcdc's `classdec` but reads
//! DEX metadata (no signatures, no generics).

pub mod cfg;
pub mod classdec;
pub mod fwdb;

pub use classdec::{sanitize_fq, ClassOptions};
pub mod ctx;
pub mod lift;
pub mod method;
pub mod passes;
pub mod platform;
mod refscan;
pub mod ssa_census;

use jdc_core::FxHashMap as HashMap;

use ddc_dex::annotations::{self, EncodedValue};

/// A resolved static-field initializer.
#[derive(Debug, Clone)]
pub enum StaticValue {
    Int(i64),
    Float(f32),
    Double(f64),
    Str(String),
    Type(String),
    Boolean(bool),
    Null,
    /// (declaring class, field name, field descriptor) — enum constants /
    /// const field refs. The desc participates in the rename-registry key
    /// (owner, name, desc) so the render site can consult field_display.
    Field(String, String, String),
    Other,
}
use ddc_dex::{ClassDef, DexFile};
use jdc_core::types::{
    parse_field_descriptor, parse_method_descriptor, JavaType, MethodDescriptor,
};

// ---------------------------------------------------------------------------
// Access flags (Dalvik numbering, JVM-compatible subset).
// ---------------------------------------------------------------------------
pub mod access {
    pub const ACC_PUBLIC: u32 = 0x1;
    pub const ACC_PRIVATE: u32 = 0x2;
    pub const ACC_PROTECTED: u32 = 0x4;
    pub const ACC_STATIC: u32 = 0x8;
    pub const ACC_FINAL: u32 = 0x10;
    pub const ACC_SYNCHRONIZED: u32 = 0x20;
    pub const ACC_BRIDGE: u32 = 0x40;
    pub const ACC_VARARGS: u32 = 0x80;
    pub const ACC_NATIVE: u32 = 0x100;
    pub const ACC_INTERFACE: u32 = 0x200;
    pub const ACC_ABSTRACT: u32 = 0x400;
    pub const ACC_STRICT: u32 = 0x800;
    pub const ACC_SYNTHETIC: u32 = 0x1000;
    pub const ACC_ANNOTATION: u32 = 0x2000;
    pub const ACC_ENUM: u32 = 0x4000;
    pub const ACC_CONSTRUCTOR: u32 = 0x1_0000;
    pub const ACC_DECLARED_SYNCHRONIZED: u32 = 0x2_0000;
}

/// One field of a pooled class.
#[derive(Debug, Clone)]
pub struct PoolField {
    /// Shared with the dex string table (field names repeat massively
    /// under R8's single-letter namespace — one Arc per unique name).
    pub name: std::sync::Arc<str>,
    /// Field descriptor (`I`, `Ljava/lang/String;`, ...) — shared per
    /// unique descriptor through the same table.
    pub desc: std::sync::Arc<str>,
    pub access: u32,
    pub is_static: bool,
}

/// One method of a pooled class.
#[derive(Debug, Clone)]
pub struct PoolMethod {
    /// Shared with the dex string table (the idx IS the dedup key).
    pub name: std::sync::Arc<str>,
    /// Method descriptor, shared per proto (a dex's proto table is the
    /// descriptor dedup layer — 1.9M methods on lark share ~1 proto
    /// table's worth of unique descriptors).
    pub desc: std::sync::Arc<str>,
    pub access: u32,
    pub code_off: u32,
    pub debug_info_off: u32,
    /// Which DEX image the body lives in.
    pub dex_idx: usize,
}

impl PoolMethod {
    pub fn parsed_desc(&self) -> Option<MethodDescriptor> {
        parse_method_descriptor(&self.desc)
    }
    pub fn is_static(&self) -> bool {
        self.access & access::ACC_STATIC != 0
    }
    pub fn is_abstract_or_native(&self) -> bool {
        self.access & (access::ACC_ABSTRACT | access::ACC_NATIVE) != 0
    }
}

/// One pool entry: either already built, or name-registered with its
/// (dex index, class_def index) locator for on-demand construction.
enum ClassEntry {
    Eager(PoolClass),
    Lazy {
        at: (usize, usize),
        pc: std::sync::OnceLock<PoolClass>,
    },
}

/// A class materialized from one `class_def_item` (with ids resolved to
/// names and the class data expanded).
#[derive(Debug, Clone)]
pub struct PoolClass {
    pub name: String,
    pub access: u32,
    /// `None` for `java/lang/Object` roots.
    pub super_name: Option<String>,
    pub interfaces: Vec<String>,
    pub source_file: Option<String>,
    pub static_fields: Vec<PoolField>,
    pub instance_fields: Vec<PoolField>,
    pub direct_methods: Vec<PoolMethod>,
    pub virtual_methods: Vec<PoolMethod>,
    /// Static initial values aligned with the head of `static_fields`.
    pub static_values: Vec<StaticValue>,
    /// Nesting evidence with annotation ids resolved to names.
    pub nesting: ResolvedNesting,
    /// Which DEX image the class body lives in (provenance header).
    pub dex_idx: usize,
}

/// Nesting evidence, names resolved at pooling time.
#[derive(Debug, Clone, Default)]
pub struct ResolvedNesting {
    pub enclosing_class: Option<String>,
    /// (class internal name, method name) of the enclosing method.
    pub enclosing_method: Option<(String, String)>,
    pub member_classes: Vec<String>,
}

impl PoolClass {
    pub fn is_interface(&self) -> bool {
        self.access & access::ACC_INTERFACE != 0
    }

    pub fn is_enum(&self) -> bool {
        self.access & access::ACC_ENUM != 0
    }

    pub fn is_synthetic(&self) -> bool {
        self.access & access::ACC_SYNTHETIC != 0
    }

    pub fn is_static_nested(&self) -> bool {
        self.access & access::ACC_STATIC != 0
    }

    /// All methods in declaration order.
    pub fn all_methods(&self) -> impl Iterator<Item = &PoolMethod> {
        self.direct_methods
            .iter()
            .chain(self.virtual_methods.iter())
    }

    /// Find a method by name + descriptor.
    pub fn find_method(&self, name: &str, desc: &str) -> Option<&PoolMethod> {
        self.all_methods()
            .find(|m| &*m.name == name && &*m.desc == desc)
    }

    /// Constructors `<init>` matching `arity` descriptor arguments.
    pub fn ctors_by_arity(&self, arity: usize) -> Vec<&PoolMethod> {
        self.all_methods()
            .filter(|m| &*m.name == "<init>")
            .filter(|m| {
                m.parsed_desc()
                    .map(|d| d.args.len() == arity)
                    .unwrap_or(false)
            })
            .collect()
    }

    pub fn field_flags_of(&self, name: &str) -> Option<u32> {
        self.static_fields
            .iter()
            .chain(self.instance_fields.iter())
            .find(|f| f.name.as_ref() == name)
            .map(|f| f.access)
    }
}

/// Multi-DEX class pool: classes from all images, first definition wins.
/// One facade-map entry: the public base plus, for PARTIALLY covered
/// parts, the exact static members the base declares (None = the whole
/// part rewrites; Some = per-member gate). Single map = one hash probe
/// per static reference (two maps measured +2.4s on weibo).
type FacadeTarget = (
    String,
    Option<
        std::sync::Arc<(
            jdc_core::FxHashSet<(String, String)>,
            jdc_core::FxHashSet<String>,
        )>,
    >,
);

pub struct DexPool {
    /// Images as Arcs: `dex()` hands out snapshots whose borrows stay
    /// valid for the Arc's lifetime (decompile workers hold them), while
    /// `release_images` empties the image data through exclusive access
    /// when all of an image's classes are done (full-decompile driver
    /// only — progressive callers keep images whole).
    dexes: Vec<std::sync::Arc<DexFile>>,
    /// Human label per image ("weibo!classes.dex") for the provenance
    /// header; defaults to "dex N" until the CLI names the inputs.
    pub dex_labels: Vec<String>,
    /// Name → class entry. Eager pools materialize everything at add
    /// time (full decompile, tests); lazy pools register names only and
    /// materialize on first `get` (progressive `getclass` — one class's
    /// PoolClass costs annotation reads, 145k of them cost ~0.2s).
    classes: HashMap<String, ClassEntry>,
    /// Class names in insertion order (stable for output).
    pub order: Vec<String>,
    /// Per-image remaining class count once `arm_retirement` fires.
    retire_counts: std::sync::Mutex<Vec<u64>>,
    retire_armed: std::sync::atomic::AtomicBool,
    /// name → outer (computed once; `$` heuristic + dalvik annotations).
    outers: std::sync::OnceLock<HashMap<String, Option<String>>>,
    /// outer → direct children (computed once).
    children: std::sync::OnceLock<HashMap<String, Vec<String>>>,
    /// Per-image hot-reference interning (parallel to `dexes`).
    ref_caches: Vec<DexRefCache>,
    /// Synthetic-static accessor code snapshots keyed by (dex_idx,
    /// code_off). `inline_accessors` reads accessor bodies ACROSS images
    /// during worker runs; image retirement can release those bytes
    /// mid-run, making the inline decision — and with it var numbering
    /// downstream — dependent on worker completion interleaving
    /// (observed: reqable enum-constant args flipping `var0`/`var1`
    /// between identical runs; DDC_NORETIRE=1 stable). The full-decompile
    /// driver fills this BEFORE `arm_retirement`; progressive/lazy pools
    /// never retire and fall through to live reads.
    accessor_code: std::sync::OnceLock<AccessorSnapshots>,
    /// Cached package → direct-class simple names (import-collision gate).
    pkg_simples:
        std::sync::OnceLock<jdc_core::FxHashMap<String, std::sync::Arc<jdc_core::FxHashSet<String>>>>,
    /// First path segments of all packages (`v2` for `v2/n`): local or
    /// field names equal to one capture package-qualified renders
    /// (`v2.n.a`) — deshadow_locals consults this.
    root_segs: std::sync::OnceLock<jdc_core::FxHashSet<String>>,
    /// Partially-covered facade parts (see kotlin_facade_partial_map).
    /// Kotlin multi-file facade parts → public facade (see
    /// `kotlin_facade_map`).
    kotlin_facades: std::sync::OnceLock<jdc_core::FxHashMap<String, FacadeTarget>>,
    /// Per class internal name: the set of `<init>` ARGUMENT descriptors
    /// (the `(..)` inner text) REFERENCED through that class anywhere in
    /// the pool. Dex resolves a `<init>` method-ref up the superclass
    /// chain, so a subclass `super(..)` may target `C.<init>(sig)` that C
    /// does NOT declare (an ancestor does) — Java has no inherited ctors,
    /// so C needs a bridge. Computed once from the method-id tables
    /// (parsed, survive image retirement). See `inherited_ctor_bridges`.
    ctor_refs: std::sync::OnceLock<jdc_core::FxHashMap<String, jdc_core::FxHashSet<String>>>,
}

/// Raw code_item bytes of every synthetic-static accessor, keyed by
/// (dex_idx, code_off) — see `snapshot_accessor_code`.
type AccessorSnapshots = HashMap<(usize, u32), std::sync::Arc<[u8]>>;

/// Lazily-filled, thread-shared reference interning for one image.
///
/// The lifter used to rebuild class names, field/method names and WHOLE
/// method descriptors per INSTRUCTION (`method_ref` alone: N param
/// Strings → `format!` descriptor → re-parse — 6-10 allocations per
/// invoke). Every entry here is built at most once per (image, table
/// index); lifts and the endless IR clones downstream are refcount
/// bumps. Slots are `OnceLock` so worker threads fill them race-free
/// without a lock on the hot path.
pub(crate) struct DexRefCache {
    proto_descs: Box<[std::sync::OnceLock<std::sync::Arc<jdc_core::types::MethodDescriptor>>]>,
    type_names: Box<[std::sync::OnceLock<std::sync::Arc<str>>]>,
    type_javas: Box<[std::sync::OnceLock<std::sync::Arc<jdc_core::types::JavaType>>]>,
}

impl DexRefCache {
    fn new(protos: usize, types: usize) -> Self {
        DexRefCache {
            proto_descs: (0..protos).map(|_| std::sync::OnceLock::new()).collect(),
            type_names: (0..types).map(|_| std::sync::OnceLock::new()).collect(),
            type_javas: (0..types).map(|_| std::sync::OnceLock::new()).collect(),
        }
    }
}

impl DexPool {
    pub fn new() -> Self {
        DexPool {
            dexes: Vec::new(),
            retire_armed: std::sync::atomic::AtomicBool::new(false),
            dex_labels: Vec::new(),
            classes: HashMap::default(),
            order: Vec::new(),
            retire_counts: std::sync::Mutex::new(Vec::new()),
            outers: std::sync::OnceLock::new(),
            children: std::sync::OnceLock::new(),
            ref_caches: Vec::new(),
            accessor_code: std::sync::OnceLock::new(),
            pkg_simples: std::sync::OnceLock::new(),
            root_segs: std::sync::OnceLock::new(),
            kotlin_facades: std::sync::OnceLock::new(),
            ctor_refs: std::sync::OnceLock::new(),
        }
    }

    /// Referenced `<init>` argument-descriptor set for a class internal
    /// name (see `ctor_refs`). Builds the whole index on first call by
    /// scanning every image's method-id table for `<init>` refs — cheap
    /// (flat table, integer name-idx compare against the interned
    /// "<init>" string) and done once for the whole decompile.
    pub fn ctor_ref_sigs(&self, class: &str) -> Option<&jdc_core::FxHashSet<String>> {
        self.ctor_refs
            .get_or_init(|| {
                let mut map: jdc_core::FxHashMap<String, jdc_core::FxHashSet<String>> =
                    jdc_core::FxHashMap::default();
                for dex in &self.dexes {
                    let n = dex.method_count();
                    for i in 0..n {
                        let m = dex.method(i as u32);
                        if dex.string(m.name_idx) != "<init>" {
                            continue;
                        }
                        let desc = dex.proto_desc(m.proto_idx);
                        let (Some(lo), Some(hi)) = (desc.find('('), desc.find(')')) else {
                            continue;
                        };
                        map.entry(dex.class_name(m.class_idx))
                            .or_default()
                            .insert(desc[lo + 1..hi].to_string());
                    }
                }
                map
            })
            .get(class)
    }

    /// Shared `MethodDescriptor` of a proto (one parse per image).
    pub fn proto_desc_parsed(
        &self,
        di: usize,
        proto_idx: u32,
    ) -> std::sync::Arc<jdc_core::types::MethodDescriptor> {
        let Some(dex) = self.dexes.get(di) else {
            return std::sync::Arc::new(jdc_core::types::MethodDescriptor {
                args: Vec::new(),
                ret: jdc_core::types::JavaType::Void,
            });
        };
        let build = || {
            std::sync::Arc::new(parse_proto_desc(dex, proto_idx))
        };
        match self
            .ref_caches
            .get(di)
            .and_then(|c| c.proto_descs.get(proto_idx as usize))
        {
            Some(slot) => slot.get_or_init(build).clone(),
            None => build(),
        }
    }

    /// Shared internal class name of a type id (one strip+alloc per image).
    pub fn type_name_arc(&self, di: usize, type_idx: u32) -> std::sync::Arc<str> {
        let Some(dex) = self.dexes.get(di) else {
            return std::sync::Arc::from("");
        };
        let build = || std::sync::Arc::from(dex.class_name(type_idx).as_str());
        match self
            .ref_caches
            .get(di)
            .and_then(|c| c.type_names.get(type_idx as usize))
        {
            Some(slot) => slot.get_or_init(build).clone(),
            None => build(),
        }
    }

    /// Shared parsed type of a type id.
    pub fn type_java(&self, di: usize, type_idx: u32) -> std::sync::Arc<jdc_core::types::JavaType> {
        let Some(dex) = self.dexes.get(di) else {
            return std::sync::Arc::new(jdc_core::types::JavaType::Object(
                std::sync::Arc::from("java/lang/Object"),
            ));
        };
        let build = || std::sync::Arc::new(desc_type(dex.type_name(type_idx)));
        match self
            .ref_caches
            .get(di)
            .and_then(|c| c.type_javas.get(type_idx as usize))
        {
            Some(slot) => slot.get_or_init(build).clone(),
            None => build(),
        }
    }

    /// name → outer map, computed lazily once.
    fn outer_map(&self) -> &HashMap<String, Option<String>> {
        self.outers.get_or_init(|| {
            self.order
                .iter()
                .map(|n| (n.clone(), find_outer_name(self, n)))
                .collect()
        })
    }

    /// The outer class of `name`, from the cached map (borrowed).
    pub fn outer_of(&self, name: &str) -> Option<&str> {
        self.outer_map().get(name).and_then(|o| o.as_deref())
    }

    /// Direct nested children of `internal` (any `$` depth 1), cached.
    /// (Perf: returns a borrowed slice — the owned-Vec clone ran once per
    /// class on 98k-class runs.)
    pub fn children_of(&self, internal: &str) -> &[String] {
        if self.children.get().is_none() {
            // Build FIRST, set ONCE: the previous shape set an empty map
            // to claim the OnceLock and then silently failed to store the
            // real index (`let _ = set(...)` on an initialized lock) —
            // children_of returned [] forever, so member nested classes
            // were neither inlined nor emitted as files (Guard$Report
            // vanished whole; every corpus run since the borrow refactor
            // dropped them, invisible to the syntax-only gate).
            let mut idx: HashMap<String, Vec<String>> = HashMap::default();
            for name in &self.order {
                if let Some(outer) = self.outer_of(name).map(str::to_string) {
                    idx.entry(outer).or_default().push(name.clone());
                }
            }
            // A racing thread may have set an identical map first — the
            // index is a pure function of `order`, so either copy is right.
            let _ = self.children.set(idx);
        }
        self.children
            .get()
            .and_then(|m| m.get(internal))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Adds one image; duplicate class names keep their first definition.
    /// Returns the image's index — call `set_dex_label` with it to give the
    /// provenance header a real origin instead of the default "dex N".
    pub fn add_dex(&mut self, dex: DexFile) -> usize {
        let dex_idx = self.dexes.len();
        // Pre-size the name map: 240k-entry growth rehashed repeatedly
        // (reserve_rehash showed up in corpus profiles).
        self.classes.reserve(dex.class_defs.len());
        // The annotation reader borrows the image; pool classes are built
        // before ownership moves into `self.dexes` (no full-image copy).
        for cd in &dex.class_defs {
            let name = dex.class_name(cd.class_idx);
            if self.classes.contains_key(&name) {
                continue;
            }
            let pc = pool_class_of(&dex, dex.raw(), cd, dex_idx);
            self.classes.insert(name.clone(), ClassEntry::Eager(pc));
            self.order.push(name);
        }
        self.ref_caches
            .push(DexRefCache::new(dex.proto_count(), dex.type_count()));
        self.dexes.push(std::sync::Arc::new(dex));
        self.retire_counts.lock().unwrap().push(0);
        self.dex_labels.push(format!("dex {}", dex_idx));
        dex_idx
    }

    /// Registers class NAMES only; the PoolClass (annotation reads
    /// included) is materialized on first `get`. Duplicate class names
    /// keep their first definition, matching `add_dex`.
    pub fn add_dex_lazy(&mut self, dex: DexFile) -> usize {
        let dex_idx = self.dexes.len();
        self.classes.reserve(dex.class_defs.len());
        for (ci, cd) in dex.class_defs.iter().enumerate() {
            let name = dex.class_name(cd.class_idx);
            if self.classes.contains_key(&name) {
                continue;
            }
            self.classes.insert(
                name.clone(),
                ClassEntry::Lazy {
                    at: (dex_idx, ci),
                    pc: std::sync::OnceLock::new(),
                },
            );
            self.order.push(name);
        }
        self.ref_caches
            .push(DexRefCache::new(dex.proto_count(), dex.type_count()));
        self.dexes.push(std::sync::Arc::new(dex));
        self.retire_counts.lock().unwrap().push(0);
        self.dex_labels.push(format!("dex {}", dex_idx));
        dex_idx
    }

    /// Label the image at `idx` (see `add_dex`).
    pub fn set_dex_label(&mut self, idx: usize, label: impl Into<String>) {
        if let Some(slot) = self.dex_labels.get_mut(idx) {
            *slot = label.into();
        }
    }

    pub fn len(&self) -> usize {
        self.classes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.classes.is_empty()
    }

    pub fn get(&self, internal: &str) -> Option<&PoolClass> {
        self.get_inner(internal, None)
    }

    /// `get` with optional pre-taken raw-image snapshots (one per
    /// image). `materialize_all` hoists the `DexFile::raw()` lock out of
    /// the per-class loop — the parallel materialize used to take the
    /// same per-image lock once PER CLASS across all threads.
    fn get_inner(&self, internal: &str, raws: Option<&[&[u8]]>) -> Option<&PoolClass> {
        match self.classes.get(internal)? {
            ClassEntry::Eager(pc) => Some(pc),
            ClassEntry::Lazy { at, pc } => {
                if let Some(built) = pc.get() {
                    return Some(built);
                }
                let (di, ci) = *at;
                let dex = self.dexes.get(di)?;
                let cd = dex.class_defs.get(ci)?;
                let raw = match raws {
                    Some(r) => r.get(di).copied().unwrap_or(&[]),
                    None => dex.raw(),
                };
                let built = pool_class_of(dex, raw, cd, di);
                Some(pc.get_or_init(move || built))
            }
        }
    }

    /// Materialize every lazy entry (parallel across the given chunk
    /// count). After this the pool is observationally identical to an
    /// eagerly-built one — including the annotation-aware outer-map —
    /// while the annotation reads ran on worker threads instead of the
    /// serial pool build (~0.2s on weibo).
    pub fn materialize_all(&self, threads: usize) {
        let keys: Vec<String> = self
            .classes
            .iter()
            .filter(|(_, e)| matches!(e, ClassEntry::Lazy { .. }))
            .map(|(k, _)| k.clone())
            .collect();
        if keys.is_empty() {
            return;
        }
        // One raw() snapshot per image for the whole sweep (see
        // get_inner). Safe window: materialize_all completes before
        // retirement is armed, so no image releases while these slices
        // are alive.
        let raws: Vec<&[u8]> = self.dexes.iter().map(|d| d.raw()).collect();
        let threads = threads.max(1);
        let next = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(k) = keys.get(i) else { break };
                    let _ = self.get_inner(k, Some(&raws));
                });
            }
        });
    }

    /// `get` without materializing: None for not-yet-built lazy entries.
    fn get_if_materialized(&self, internal: &str) -> Option<&PoolClass> {
        match self.classes.get(internal)? {
            ClassEntry::Eager(pc) => Some(pc),
            ClassEntry::Lazy { pc, .. } => pc.get(),
        }
    }

    /// Name registered (no materialization).
    pub fn has_name(&self, internal: &str) -> bool {
        self.classes.contains_key(internal)
    }

    /// One image as an Arc snapshot (borrows of the snapshot live as
    /// long as the caller holds the Arc).
    pub fn dex(&self, idx: usize) -> Option<std::sync::Arc<DexFile>> {
        self.dexes.get(idx).cloned()
    }

    pub fn dex_count(&self) -> usize {
        self.dexes.len()
    }

    /// Raw image list (install-phase whole-program scans).
    pub fn dexes(&self) -> &[std::sync::Arc<ddc_dex::DexFile>] {
        &self.dexes
    }

    /// First path segments of every package in the pool (cached once).
    pub fn root_pkg_segs(&self) -> &jdc_core::FxHashSet<String> {
        self.root_segs.get_or_init(|| {
            self.order
                .iter()
                .filter_map(|n| n.find('/').map(|i| n[..i].to_string()))
                .collect()
        })
    }

    /// Package → simple names of its direct classes, cached once per
    /// pool (the import-collision gate consults it per class — the
    /// uncached scan was O(classes²) and 10×'d weixin's wall time).
    pub fn package_simples(
        &self,
    ) -> &jdc_core::FxHashMap<String, std::sync::Arc<jdc_core::FxHashSet<String>>> {
        // Arc'd sets: the per-file import machinery BORROWS a package's
        // set (an Arc clone) instead of deep-copying it — WhatsApp's
        // ~10k-class package paid two full copies per class (~200M
        // String allocations), the #2/#3 CPU frames on its profile.
        self.pkg_simples.get_or_init(|| {
            let mut m: jdc_core::FxHashMap<String, std::sync::Arc<jdc_core::FxHashSet<String>>> =
                jdc_core::FxHashMap::default();
            let mut raw: jdc_core::FxHashMap<String, jdc_core::FxHashSet<String>> =
                jdc_core::FxHashMap::default();
            for n in &self.order {
                if let Some((pkg, simple)) = n.rsplit_once('/') {
                    raw.entry(pkg.to_string())
                        .or_default()
                        .insert(simple.to_string());
                }
            }
            m.extend(raw.into_iter().map(|(k, v)| (k, std::sync::Arc::new(v))));
            m
        })
    }

    /// Kotlin multi-file class facades: the compiler splits a file facade
    /// (`kotlin.text.StringsKt`) into package-private parts
    /// (`StringsKt__StringsKt`, `StringsKt__StringsJVMKt`) and call sites
    /// target the PARTS directly. Java cannot access a package-private
    /// class cross-package ("StringsKt__StringsKt在kotlin.text中不是公共的"
    /// — 1.4k lark root errors), but the public facade EXTENDS the parts,
    /// so static members resolve through it: rewrite the reference's owner
    /// class to the facade. part → base, gated on base existing, being
    /// public, and transitively extending the part (static inheritance is
    /// the resolution guarantee).
    pub fn kotlin_facade_map(&self) -> &jdc_core::FxHashMap<String, FacadeTarget> {
        self.kotlin_facades.get_or_init(|| {
            let t_fb = std::time::Instant::now();
            let mut m: jdc_core::FxHashMap<String, FacadeTarget> =
                jdc_core::FxHashMap::default();
            let mut npartial = 0usize;
            for name in &self.order {
                let Some(i) = name.find("__") else { continue };
                let base = &name[..i];
                if base.is_empty() {
                    continue;
                }
                let Some(basec) = self.get(base) else {
                    continue;
                };
                if basec.access & crate::access::ACC_PUBLIC == 0 {
                    continue;
                }
                // base must transitively extend the part...
                let mut cur = basec.super_name.clone();
                let mut hops = 0u32;
                let mut reaches = false;
                while let Some(c) = cur {
                    if c == *name {
                        reaches = true;
                        break;
                    }
                    if hops >= 8 {
                        break;
                    }
                    hops += 1;
                    cur = self.get(&c).and_then(|cc| cc.super_name.clone());
                }
                // ...OR cover the part's static members: the Kotlin
                // compiler MERGES every part's statics into the public
                // facade, but the facade's super chain only includes ONE
                // part — lark's `CollectionsKt extends
                // CollectionsKt___CollectionsKt` leaves
                // `CollectionsKt__CollectionsJVMKt` uncovered (463
                // 不是公共的 errors). Member coverage (name+desc for
                // methods, name for fields) is the precise rewrite
                // condition: every reference through the facade then
                // resolves exactly as through the part.
                if reaches {
                    m.insert(name.clone(), (base.to_string(), None));
                    continue;
                }
                // Member-coverage fallback: the Kotlin compiler MERGES
                // the parts' statics into the public facade, but R8 can
                // sever the super chain (lark: `CollectionsKt extends
                // CollectionsKt___CollectionsKt extends
                // kotlin/collections/s` — `__CollectionsJVMKt` is off
                // the chain, 463 不是公共的 errors) and may drop facade
                // members no direct facade call needed. Map at (name,
                // desc) granularity then: covered members rewrite, the
                // rest keep the part owner (a latent error either way —
                // Java cannot access the package-private part).
                let Some(part) = self.get(name) else {
                    continue;
                };
                let base_methods: jdc_core::FxHashSet<(&str, &str)> = basec
                    .direct_methods
                    .iter()
                    .chain(basec.virtual_methods.iter())
                    .map(|mm| (&*mm.name, &*mm.desc))
                    .collect();
                let base_fields: jdc_core::FxHashSet<&str> =
                    basec.static_fields.iter().map(|f| &*f.name).collect();
                let mut methods: jdc_core::FxHashSet<(String, String)> =
                    jdc_core::FxHashSet::default();
                let mut fields: jdc_core::FxHashSet<String> =
                    jdc_core::FxHashSet::default();
                let mut all = true;
                for mm in part
                    .direct_methods
                    .iter()
                    .chain(part.virtual_methods.iter())
                    .filter(|mm| mm.access & crate::access::ACC_STATIC != 0)
                {
                    if base_methods.contains(&(&*mm.name, &*mm.desc)) {
                        methods.insert((mm.name.to_string(), mm.desc.to_string()));
                    } else {
                        all = false;
                    }
                }
                for pf in &part.static_fields {
                    if base_fields.contains(&*pf.name) {
                        fields.insert(pf.name.to_string());
                    } else {
                        all = false;
                    }
                }
                if all {
                    m.insert(name.clone(), (base.to_string(), None));
                } else if !methods.is_empty() || !fields.is_empty() {
                    npartial += 1;
                    m.insert(
                        name.clone(),
                        (base.to_string(), Some(std::sync::Arc::new((methods, fields)))),
                    );
                }
            }
            if std::env::var("DDC_STATS").is_ok() {
                eprintln!(
                    "[renames] facade map: entries={} partial={} build={:?}",
                    m.len(),
                    npartial,
                    t_fb.elapsed()
                );
            }
            m
        })
    }

    pub fn class_names(&self) -> impl Iterator<Item = &str> {
        self.order.iter().map(|s| s.as_str())
    }

    /// True when `sub` is assignable to `sup` (internal names), walking the
    /// pool's hierarchy. Unknown classes are never assignable (conservative).
    pub fn is_subtype(&self, sub: &str, sup: &str) -> bool {
        if sub == sup {
            return true;
        }
        // Arrays: covariance by element type when both are arrays.
        if let (Some(sub_elem), Some(sup_elem)) = (array_elem(sub), array_elem(sup)) {
            if sub_elem.starts_with('L')
                && sup_elem.starts_with('L')
                && sub_elem.ends_with(';')
                && sup_elem.ends_with(';')
            {
                return self.is_subtype(
                    &sub_elem[1..sub_elem.len() - 1],
                    &sup_elem[1..sup_elem.len() - 1],
                );
            }
            // Multi-dimensional: recurse on the element descriptors
            // (`[[LSavedState;` <: `[[LObject;` peels one dimension at a
            // time); the equality fallback below stays for primitive
            // elements (`[I` vs `[J` are invariant).
            if sub_elem.starts_with('[') && sup_elem.starts_with('[') {
                return self.is_subtype(sub_elem, sup_elem);
            }
            return sub_elem == sup_elem;
        }
        // Everything reference-typed is Object-assignable (the pool walk
        // used to fall off the chain end and answer false for
        // String <: Object — the hand-written Object-formal special case
        // in the ambiguity pin existed for exactly this hole).
        if sup == "java/lang/Object" {
            return !matches!(
                sub,
                "I" | "Z" | "B" | "S" | "C" | "J" | "F" | "D" | "V"
            );
        }
        let mut cur = self.get(sub);
        if cur.is_none() {
            // `sub` is a framework (or phantom) type: the embedded API
            // database decides (phantoms are absent there too → false).
            return crate::fwdb::is_subtype(sub, sup);
        }
        let mut hops = 0;
        while let Some(c) = cur {
            hops += 1;
            if hops > 64 {
                return false;
            }
            for i in &c.interfaces {
                if i == sup || self.is_subtype(i, sup) {
                    return true;
                }
            }
            match &c.super_name {
                Some(s) if s == sup => return true,
                Some(s) if s != "java/lang/Object" => {
                    match self.get(s) {
                        Some(sc) => cur = Some(sc),
                        // Framework boundary: continue in the embedded DB.
                        None => return crate::fwdb::is_subtype(s, sup),
                    }
                }
                _ => return false,
            }
        }
        false
    }
}

impl Default for DexPool {
    fn default() -> Self {
        Self::new()
    }
}

fn array_elem(desc: &str) -> Option<&str> {
    desc.strip_prefix('[')
}

fn resolve_static_value(v: &EncodedValue, dex: &DexFile) -> StaticValue {
    match v {
        EncodedValue::Byte(x) => StaticValue::Int(*x as i64),
        EncodedValue::Short(x) => StaticValue::Int(*x as i64),
        EncodedValue::Char(x) => StaticValue::Int(*x as i64),
        EncodedValue::Int(x) => StaticValue::Int(*x as i64),
        EncodedValue::Long(x) => StaticValue::Int(*x),
        EncodedValue::Float(x) => StaticValue::Float(*x),
        EncodedValue::Double(x) => StaticValue::Double(*x),
        EncodedValue::String(i) => StaticValue::Str(dex.string(*i).to_string()),
        EncodedValue::Type(i) => StaticValue::Type(dex.class_name(*i)),
        EncodedValue::Boolean(b) => StaticValue::Boolean(*b),
        EncodedValue::Null => StaticValue::Null,
        EncodedValue::Enum(i) | EncodedValue::Field(i) => {
            let f = dex.field(*i);
            StaticValue::Field(
                dex.class_name(f.class_idx),
                dex.string(f.name_idx).to_string(),
                dex.type_name(f.type_idx).to_string(),
            )
        }
        _ => StaticValue::Other,
    }
}

fn pool_class_of(dex: &DexFile, raw: &[u8], cd: &ClassDef, dex_idx: usize) -> PoolClass {
    let name = dex.class_name(cd.class_idx);
    let super_name = if cd.superclass_idx == ddc_dex::NO_INDEX {
        None
    } else {
        let s = dex.class_name(cd.superclass_idx);
        if s.is_empty() || s == "java/lang/Object" {
            None
        } else {
            Some(s)
        }
    };
    let interfaces = dex
        .interfaces_of(cd)
        .into_iter()
        .map(|t| dex.class_name(t))
        .collect();
    let source_file = if cd.source_file_idx == ddc_dex::NO_INDEX {
        None
    } else {
        Some(dex.string(cd.source_file_idx).to_string())
    };

    let data = dex.class_data(cd);
    let mk_field = |ef: &ddc_dex::EncodedField, is_static: bool| {
        let f = dex.field(ef.field_idx);
        PoolField {
            name: dex.string_arc(f.name_idx),
            desc: dex.type_name_arc(f.type_idx),
            access: ef.access_flags,
            is_static,
        }
    };
    let static_fields: Vec<PoolField> = data
        .static_fields
        .iter()
        .map(|f| mk_field(f, true))
        .collect();
    let instance_fields: Vec<PoolField> = data
        .instance_fields
        .iter()
        .map(|f| mk_field(f, false))
        .collect();
    let mk_method = |em: &ddc_dex::EncodedMethod| {
        let m = dex.method(em.method_idx);
        let (code_off, debug_info_off) = match dex.debug_info_off_at(em.code_off) {
            Some(d) => (em.code_off, d),
            None => (0, 0),
        };
        PoolMethod {
            name: dex.string_arc(m.name_idx),
            desc: dex.proto_desc(m.proto_idx),
            access: em.access_flags,
            code_off,
            debug_info_off,
            dex_idx,
        }
    };
    let direct_methods: Vec<PoolMethod> = data.direct_methods.iter().map(&mk_method).collect();
    let virtual_methods: Vec<PoolMethod> = data.virtual_methods.iter().map(&mk_method).collect();

    let static_values: Vec<StaticValue> = dex
        .static_values(cd.static_values_off)
        .into_iter()
        .map(|v| resolve_static_value(&v, dex))
        .collect();
    let anns = if cd.annotations_off != 0 {
        annotations::read_class_annotations(raw, cd.annotations_off)
    } else {
        Vec::new()
    };
    let raw = annotations::nesting_from(&anns, &|t: u32| dex.type_name(t).to_string());
    let nesting = ResolvedNesting {
        enclosing_class: raw.enclosing_class.map(|t| dex.class_name(t)),
        enclosing_method: raw.enclosing_method.map(|m| {
            let mid = dex.method(m);
            (
                dex.class_name(mid.class_idx),
                dex.string(mid.name_idx).to_string(),
            )
        }),
        member_classes: raw
            .member_classes
            .into_iter()
            .map(|t| dex.class_name(t))
            .collect(),
    };

    PoolClass {
        name,
        access: cd.access_flags,
        super_name,
        interfaces,
        source_file,
        static_fields,
        instance_fields,
        direct_methods,
        virtual_methods,
        static_values,
        nesting,
        dex_idx,
    }
}

/// JavaType for a field/method descriptor segment.
/// Parse a proto's descriptor into a MethodDescriptor straight from the
/// tables (no string round-trip).
fn parse_proto_desc(
    dex: &ddc_dex::DexFile,
    proto_idx: u32,
) -> jdc_core::types::MethodDescriptor {
    let proto = dex.proto(proto_idx);
    let args = dex
        .proto_params(proto_idx)
        .iter()
        .map(|&t| desc_type(dex.type_name(t)))
        .collect();
    jdc_core::types::MethodDescriptor {
        args,
        ret: desc_type(dex.type_name(proto.return_type_idx)),
    }
}

pub fn desc_type(desc: &str) -> JavaType {
    parse_field_descriptor(desc).unwrap_or(JavaType::Object("java/lang/Object".into()))
}

/// Nesting evidence for `internal`: dalvik annotations first, then the
/// `$`-name heuristic against the pool.
pub fn find_outer_name(pool: &DexPool, internal: &str) -> Option<String> {
    // Annotation refinement only for ALREADY-MATERIALIZED classes: eager
    // pools (full decompile) behave exactly as before; lazy pools
    // (progressive getclass) fall straight to the `$` chain without
    // materializing the world.
    if let Some(pc) = pool.get_if_materialized(internal) {
        if let Some(enc) = &pc.nesting.enclosing_class {
            // The annotation outer must still EXIST: R8 deletes whole
            // outer classes and leaves the nestlings as flat `$` files
            // (weixin matrix JiffiesMonitorFeature deleted,
            // $JiffiesSnapshot survives) — an unresolved annotation
            // outer made refs render dotted THROUGH a missing type
            // ("是在不可访问的类或接口中定义的"). Fall through to the
            // existence-checked `$` walk.
            if pool.has_name(enc) {
                return Some(enc.clone());
            }
        }
    }
    let mut rest = internal;
    while let Some(d) = rest.rfind('$') {
        // A `$` followed by `/` is a PACKAGE-path dollar (desugar's
        // `j$/util/...`), never a nesting boundary: without this the
        // whole j$ tree attached to a root-package class `j` as family
        // members (deepseek's 62k-line j.java absorbing 549 desugar
        // classes, double-emitted against their flat j$/ files).
        if rest.as_bytes().get(d + 1) == Some(&b'/') {
            rest = &rest[..d];
            continue;
        }
        let cand = &rest[..d];
        if pool.has_name(cand) {
            return Some(cand.to_string());
        }
        rest = cand;
    }
    None
}

/// True when the `$` tail names a plain member class (not anonymous/local/
/// lambda), i.e. the class renders inside its outer's compilation unit.
fn clean_member_tail(rest: &str) -> bool {
    if rest.starts_with('-') {
        return false;
    }
    // A member tail can never contain `/` — that is a package-path
    // fragment from a package-dollar name (`j$/util/X` stripped against
    // a root class `j`); defensive backstop for the find_outer_name
    // guard.
    if rest.contains('/') {
        return false;
    }
    // d8's synthetic outline/lambda/backport classes (`X$$ExternalSynthetic…`)
    // are TOP-LEVEL synthetics, not nested members: `$$` is d8's marker, and
    // find_outer_name's `$`-chain walk mistakes them for children of `X`.
    // Emitting them nested strands every cross-package reference on a bare
    // simple name ("找不到符号 变量 ExternalSyntheticBackportWithForwarding0",
    // weibo ×467). Targeted at `ExternalSynthetic` only — Kotlin's
    // `$$serializer` / `$$delegate` stay nested members.
    if rest.starts_with("$ExternalSynthetic") {
        return false;
    }
    let tail = rest.rsplit('$').next().unwrap_or(rest);
    if tail.is_empty() || tail.starts_with(|c: char| c.is_ascii_digit()) {
        return false;
    }
    true
}

/// Classes to emit as their own compilation units: top-level classes plus
/// anonymous/local/lambda-shaped ones (clean members render inline).
impl DexPool {
    /// Arm image retirement (full-decompile driver only): count one
    /// pending class per image by pool ownership.
    /// Snapshot every synthetic-static accessor body (the exact predicate
    /// `inline_accessors` uses) as RAW code_item bytes, so cross-image
    /// accessor reads during worker runs survive image retirement.
    /// Call AFTER materialization and BEFORE `arm_retirement`; idempotent
    /// (first fill wins). Raw memcpy, no decode — accessors are numerous
    /// (weixin ~100k) but only a fraction are ever inlined, so decoding
    /// stays lazy at the use site. The slice bound covers the fixed
    /// header + insns + try items with generous handler slack (accessor
    /// tries are vanishingly rare; the snapshot is a deterministic
    /// function of the image either way).
    pub fn snapshot_accessor_code(&self) {
        let mut snap: AccessorSnapshots = HashMap::default();
        let mut bytes_total = 0usize;
        for name in &self.order {
            let Some(pc) = self.get_if_materialized(name) else {
                continue;
            };
            for m in pc.direct_methods.iter().chain(pc.virtual_methods.iter()) {
                if m.code_off == 0
                    || !m.is_static()
                    || m.access & crate::access::ACC_SYNTHETIC == 0
                {
                    continue;
                }
                if snap.contains_key(&(m.dex_idx, m.code_off)) {
                    continue;
                }
                let Some(dex) = self.dex(m.dex_idx) else {
                    continue;
                };
                let raw = dex.raw();
                let off = m.code_off as usize;
                if off + 16 > raw.len() {
                    continue;
                }
                let u2 = |o: usize| u16::from_le_bytes([raw[o], raw[o + 1]]) as usize;
                let u4 = |o: usize| {
                    u32::from_le_bytes([raw[o], raw[o + 1], raw[o + 2], raw[o + 3]]) as usize
                };
                let tries = u2(off + 6);
                let insns = u4(off + 12);
                let mut end = off + 16 + 2 * insns;
                if tries > 0 {
                    // pad to 4-align + try_item[tries] + handler blobs
                    // (uleb-coded; slack covers realistic accessor shapes).
                    end = end.next_multiple_of(4) + 8 * tries + 256 + 64 * tries;
                }
                let end = end.min(raw.len());
                let slice: std::sync::Arc<[u8]> = raw[off..end].into();
                bytes_total += slice.len();
                snap.insert((m.dex_idx, m.code_off), slice);
            }
        }
        if std::env::var("DDC_WALL").is_ok() {
            eprintln!(
                "[wall] accessor snapshot: {} entries, {} KB",
                snap.len(),
                bytes_total / 1024
            );
        }
        let _ = self.accessor_code.set(snap);
    }

    /// Snapshotted accessor code_item bytes, when the snapshot was taken.
    /// Parse with `CodeItem::parse(&bytes, 0)`.
    pub fn accessor_code(&self, dex_idx: usize, code_off: u32) -> Option<std::sync::Arc<[u8]>> {
        self.accessor_code.get()?.get(&(dex_idx, code_off)).cloned()
    }

    pub fn arm_retirement(&self) {
        self.retire_armed
            .store(true, std::sync::atomic::Ordering::Release);
        let mut counts = self.retire_counts.lock().unwrap();
        for c in counts.iter_mut() {
            *c = 0;
        }
        for entry in self.classes.values() {
            let di = match entry {
                ClassEntry::Eager(pc) => pc.dex_idx,
                ClassEntry::Lazy { at, .. } => at.0,
            };
            if let Some(c) = counts.get_mut(di) {
                *c += 1;
            }
        }
    }

    /// Report one class finished; Some(image) when it was the image's
    /// last class (driver batches these into release_images).
    pub fn report_class_done(&self, class_name: &str) -> Option<usize> {
        if !self.retire_armed.load(std::sync::atomic::Ordering::Acquire) {
            return None;
        }
        let entry = self.classes.get(class_name)?;
        let di = match entry {
            ClassEntry::Eager(pc) => pc.dex_idx,
            ClassEntry::Lazy { at, .. } => at.0,
        };
        let mut counts = self.retire_counts.lock().unwrap();
        match counts.get_mut(di) {
            Some(c) => {
                *c = c.saturating_sub(1);
                if *c == 0 {
                    Some(di)
                } else {
                    None
                }
            }
            None => None,
        }
    }

    /// Release the images' inflated bytes. Safe: the driver calls this
    /// only when every class of each image is emitted or failed; new
    /// `dex()` snapshots still work (tables valid, code accessors empty).
    pub fn release_images(&self, indexes: &[usize]) {
        let mut counts = self.retire_counts.lock().unwrap();
        for &i in indexes {
            if let Some(dex) = self.dexes.get(i) {
                // Mark the shared image: code accessors go empty
                // immediately; the bytes drop when the last snapshot
                // drops (workers hold snapshots only mid-class).
                dex.mark_released();
            }
            if let Some(c) = counts.get_mut(i) {
                *c = u64::MAX; // released marker
            }
        }
    }
}

pub fn top_level_classes(pool: &DexPool) -> Vec<String> {
    pool.class_names()
        .filter(|name| match pool.outer_of(name) {
            None => true,
            Some(outer) => {
                if pool.get(outer).is_none() {
                    return true;
                }
                // The outer can come from an ANNOTATION (EnclosingClass)
                // with no naming relationship to this class — obfuscated
                // apps pair a 1-char name with a long enclosing descriptor
                // (weixin), which made the old blind slice panic. Only a
                // real `outer$tail` prefix yields a member tail; anything
                // else is a standalone unit.
                let rest = name
                    .strip_prefix(outer)
                    .and_then(|t| t.strip_prefix('$'))
                    .unwrap_or("");
                !clean_member_tail(rest)
            }
        })
        .map(|n| n.to_string())
        .collect()
}

/// Sanitized form of one internal-name segment — the shared mapping the
/// declaration site (`print_class_name` → `sanitize_ref`), the CLASS
/// lookup and the CLI writer must all agree on. Exported so the writer
/// cannot drift from the declaration (see `sanitize_fq`).
pub fn sanitize_seg(seg: &str) -> String {
    classdec::sanitize_fq(seg)
}

/// Sanitized internal PATH form of a class name: the exact on-disk
/// identity `source_path` writes and the declaration claims.
pub fn sanitize_internal(internal: &str) -> String {
    internal
        .split('/')
        .map(classdec::sanitize_fq)
        .collect::<Vec<_>>()
        .join("/")
}

/// Deterministic case-collision renames over the FILE-emission set:
/// classes whose internal names differ only in letter case cannot share
/// one case-insensitive directory; the first (sorted) member of each
/// group keeps its name, the others gain `_2`, `_3`, … on the simple
/// segment. The map carries identity entries for unrenamed file-level
/// classes (they anchor nested prefix walks in apply_class_rename).
pub fn case_rename_map(pool: &DexPool) -> HashMap<String, String> {
    use std::collections::HashMap;
    let mut groups: HashMap<String, Vec<String>> = HashMap::default();
    for t in top_level_classes(pool) {
        groups.entry(t.to_lowercase()).or_default().push(t);
    }
    // No `folds` clone: the fold check consults `groups` itself (the old
    // full HashSet<String> of every lowercase key was a ~25-30MB pure
    // duplicate on the large corpora). Sort in place first, then an
    // IMMUTABLE walk keeps the keys queryable for the fold lookup.
    for v in groups.values_mut() {
        v.sort();
    }
    let mut map = HashMap::default();
    for members in groups.values() {
        for (i, m) in members.iter().enumerate() {
            if i == 0 {
                map.insert(m.clone(), m.clone());
                continue;
            }
            // Suffix the simple segment; keep incrementing if the fold
            // of the suffixed name is already taken by a real class.
            let cut = m.rfind('/').map(|x| x + 1).unwrap_or(0);
            let mut n = i + 1;
            loop {
                let cand = format!("{}{}_{}", &m[..cut], &m[cut..], n);
                let fold = cand.to_lowercase();
                if !groups.contains_key(&fold) {
                    map.insert(m.clone(), cand);
                    break;
                }
                n += 1;
            }
        }
    }
    map
}

/// Display-level renames for two nested-class shapes that render
/// uncompilable or inconsistent output:
///
/// 1. A member class whose simple tail equals its rendered parent's
///    tail (`x0$a$a` inside `x0$a`): javac rejects a member class with
///    the same simple name as its immediately enclosing class ("已在类
///    x0中定义了类 x0.a") — a shape Kotlin lambda families hit by the
///    thousand (lark alone: 22k). Bump the tail: `a` → `a2`, `a3`, …
/// 2. An orphaned intermediate (`x0$a$b` with `x0$a` absent from the
///    pool): the member DECLARES as `b` inside `x0` but references
///    printed `x0.a$b` — a type that exists nowhere. Flatten the
///    display name to `x0$b` so every site agrees.
///
/// Both are pure display renames keyed by internal name; declarations,
/// ctor names, file names and every type reference funnel through
/// apply_class_rename, so one map keeps all sites consistent. Children
/// Display-level renames for nested member classes that render
/// uncompilable or inconsistent output:
///
/// 1. A member whose simple tail equals ANY enclosing class's simple
///    name in its nesting chain. javac rejects more than the immediate
///    parent (`class a { static class a2 { static class a {} } }` —
///    member `a` two levels under `a` — is "已在类 X中定义了类 X.a");
///    Kotlin lambda families (`x0$a$a` inside `x0$a`) hit it by the
///    thousand (lark alone: 22k). Bump the tail until it clears every
///    ancestor name: `a` → `a3`.
/// 2. An orphaned intermediate (`x0$a$b` with `x0$a` absent): the
///    member DECLARES as `b` inside `x0` but references printed
///    `x0.a$b` — a type that exists nowhere. Flatten the display to
///    `x0$b` so every site agrees.
///
/// Candidates build on the parent's DISPLAY name (ancestors are
/// renamed first — ascending `$`-depth order), so declarations,
/// constructor names, file names and every type reference stay
/// consistent through apply_class_rename. All are pure display
/// renames keyed by internal name.
/// In-progress-map display composition, mirroring
/// `rename::apply_class_rename`'s ancestor walk (the registry is not
/// installed yet during map construction, so mint rules must compose
/// through renamed ancestors themselves). A nested name whose DIRECT
/// parent has no entry still inherits every renamed higher prefix:
/// grandparent `yN5` -> `yN5_3` must turn minted `yN5$a$a2` into
/// `yN5_3$a$a2` (xingye case-quads: the raw-prefix entry short-
/// circuited the render walk — 程序包ddcroot.yN5.a不存在 ×134-family).
fn display_of(name: &str, map: &HashMap<String, String>) -> String {
    // EXACT hit first — the walk truncates the full name before ever
    // consulting it, and an identity entry on an ancestor prefix
    // (case_rename_map seeds those for every fold-group first member)
    // would short-circuit to the raw name (lark aa2/d$c17 minted, then
    // aa2/d$c$a composed against the identity of aa2/d — the child
    // stranded on the dead `c` prefix, 找不到符号 类 a ×1,874).
    if let Some(d) = map.get(name) {
        return d.clone();
    }
    let mut s = name;
    while let Some(i) = s.rfind('$') {
        s = &s[..i];
        match map.get(s) {
            Some(d) if d.as_str() != s => return format!("{d}{}", &name[s.len()..]),
            Some(_) => return name.to_string(),
            None => continue,
        }
    }
    map.get(name).cloned().unwrap_or_else(|| name.to_string())
}

fn nested_collision_renames(
    pool: &DexPool,
    map: &mut HashMap<String, String>,
    fam_segs: &SegMap,
) {
    // Ancestors before descendants: a parent's rename must be in the
    // map when its children compute their display chains.
    let mut names: Vec<&String> = pool.order.iter().collect();
    names.sort_by_key(|n| n.matches('$').count());
    let mut assigned: jdc_core::FxHashSet<String> = jdc_core::FxHashSet::default();
    // Top-level package segments. A nested member type DISPLAYED as `a2`
    // shadows package `a2` for every qualified `a2.x` reference in its
    // file (member types outrank package names in class scope), so a
    // renamed tail must not become a package shadow — round 47's failure
    // mode at nested level: candidate `a2` (for j2/g$a) broke the SAME
    // file's `extends a2.a` ("找不到符号 类 a" + a 9450-error Object
    // cascade as every dependent of the corrupt head failed too).
    let mut pkg_segments: jdc_core::FxHashSet<String> = jdc_core::FxHashSet::default();
    for n in &pool.order {
        if let Some((seg, _)) = n.split_once('/') {
            pkg_segments.insert(seg.to_string());
        }
    }
    // LOCAL-OK gate (round-59 Design B): a field-obscuring rename may
    // only fire when every class referencing the candidate lives inside
    // the root file's family, so the rename perturbs exactly ONE rendered
    // file. Ungated (Design A) the rule netted reqable −1653 / lark −5221
    // but weibo +36916: renames there flipped ambiguous type-vs-package
    // resolutions across thousands of pre-existing conflict families and
    // javac's error recovery turned "missing supertype, body suppressed"
    // files into full Object cascades. Lazy (progressive-browse) pools
    // skip the gate AND the rule — the multi-second image scan must not
    // stall single-class queries; the full-decompile pipeline always
    // materializes everything before installing renames.
    // Round-59 Design B stands: ungated field-clash renames were
    // retried AFTER the package-obscuring rules landed (weibo gate,
    // DDC_FC_UNGATED A/B) and still exploded to the 500k error cap —
    // fifth falsification of the ungated rename route.
    let local_ok: jdc_core::FxHashSet<String> = if pool_majority_materialized(pool) {
        refscan::local_ok(&pool.dexes, &field_clash_cands(pool, map))
    } else {
        jdc_core::FxHashSet::default()
    };
    let mut taken_sibling_displays: jdc_core::FxHashSet<String> =
        map.values().cloned().collect();
    // Stage-2 queue: nested tails equal to a top-level package segment
    // whose family showed NO descriptor-level trigger — the victim ref
    // may live in method bodies only (weixin e32/a2's `a.b.c()` bound
    // to its own nested `class a`, 位置: 类 a ×169).
    let mut body_cands: Vec<(String, String, String)> = Vec::new();
    for name in names {
        if !name.contains('$') || map.contains_key(name) {
            continue;
        }
        let Some(parent) = find_outer_name(pool, name) else {
            continue;
        };
        // Only a REAL `parent$rest` name renders as an inline member;
        // anonymous/local/lambda tails keep their flat own-file names.
        let Some(rest) = name
            .strip_prefix(parent.as_str())
            .and_then(|t| t.strip_prefix('$'))
        else {
            continue;
        };
        if !clean_member_tail(rest) {
            continue;
        }
        // Display simple names of every enclosing level, bottom-up; `cur`
        // ends at the ROOT (the top-level class whose file renders this
        // nested type — the scope the renamed tail lives in).
        let mut chain: Vec<String> = Vec::new();
        let mut cur = parent.clone();
        loop {
            let disp = map.get(&cur).cloned().unwrap_or_else(|| cur.clone());
            let simple = disp.rsplit('/').next().unwrap_or(&disp);
            chain.push(simple.rsplit('$').next().unwrap_or(simple).to_string());
            match find_outer_name(pool, &cur) {
                Some(o) if o != cur => cur = o,
                _ => break,
            }
        }
        let root: &str = &cur;
        let disp_parent = display_of(&parent, map);
        let tail = rest.rsplit('$').next().unwrap_or(rest);
        let orphan = rest.contains('$');
        // Field obscuring (JLS 6.4.2): a nested type whose simple name
        // equals an enclosing-class field is OBSCURED by it in a
        // `<Parent>.<tail>` qualified reference — `io/flutter/view/g$i`
        // (a `static enum i`) renders `g.i.t`, but g also declares an
        // instance field `i`, so javac resolves `g.i` to the field:
        // "无法从静态上下文中引用非静态 变量 i" (975 of 1140 reqable
        // static-context errors). The fix renames the NESTED TYPE (the
        // registry carries every render path: shorten, inner_simple —
        // whose bypass was the round-55 blowup, fixed in jdc-core —
        // classdec declarations), gated to LOCAL-OK candidates by the
        // refscan above. PoC: in-file rename of flutter g's `enum i`
        // took the class from 2452 errors to 1.
        let field_clash = local_ok.contains(name.as_str());
        // Package shadow (JLS 6.4.2 at nested level): a nested type
        // DISPLAYED with a simple name equal to the first segment of a
        // package its FAMILY references binds every `s.x` qualified ref
        // in the root file to the member type — the family's own outer
        // rename can EXPOSE this (e/b → e/b2 un-collided the nested `b`,
        // which then hijacked `b.b2` — 循环继承). The round-47 candidate
        // guard only kept RENAMED tails off package segments; pre-
        // existing shadows need the rename too, family-gated: fire only
        // when this family's own descriptors reference the segment.
        // Orphans INCLUDED: an `Outer$$x` orphan normalizes to the
        // nested display `x` (k=1 invisible rename), and a clean tail
        // renders BARE in the family file — same package shadow as a
        // true nested (weixin li/u0$$r → `class r` hijacked `r.a` for
        // root package r).
        let pkg_shadow = fam_segs
            .get(root)
            .is_some_and(|segs| segs.contains(tail));
        let clash = chain.iter().any(|c| c == tail) || field_clash || pkg_shadow;
        // Stage-2 queue: NO descriptor trigger, but the tail equals a
        // top-level package segment — the victim ref may be body-level
        // only. ORPHANS included: their pass-1 k=1 mint is the invisible
        // normalization (`a$$a` → display `a`), which leaves the package
        // shadow fully alive (weixin e32/a$$a hijacking `a.b.c()` ×169).
        if !clash && pkg_segments.contains(tail) {
            body_cands.push((name.clone(), root.to_string(), tail.to_string()));
        }
        if !orphan && !clash {
            continue;
        }
        // The enclosing class's field names (sanitized like the renderer)
        // — only needed once a rename actually fires, to keep the new
        // tail from being obscured again. Computed lazily so progressive
        // pools don't materialize parents for rules that don't rename.
        let mut enc_fields: jdc_core::FxHashSet<String> = jdc_core::FxHashSet::default();
        if field_clash || pkg_shadow {
            if let Some(pc) = pool.get(&parent) {
                for f in pc.static_fields.iter().chain(pc.instance_fields.iter()) {
                    enc_fields.insert(crate::classdec::java_ident(&f.name).into_owned());
                }
            }
        }
        let mut k = 0u32;
        loop {
            k += 1;
            let cand_tail = if k == 1 {
                tail.to_string()
            } else {
                format!("{tail}{k}")
            };
            // The candidate must clear every ancestor display name (javac
            // checks the whole chain) AND every enclosing field name (else
            // the renamed type is just obscured again). Sibling nested
            // tails are covered by the pool.has_name check below.
            if chain.contains(&cand_tail) || enc_fields.contains(&cand_tail) {
                continue;
            }
            // Package shadow: the renamed tail becomes a member type of
            // the root file's class, outranking any package of the same
            // name for qualified refs in that file. Same-package top-level
            // classes likewise outrank-then-break in-file simple refs.
            // BOTH checks apply to field-clash renames only: for the
            // orphan rule the k=1 candidate is the natural display name
            // (`g0$$r` → `g0$r` renders identically — an invisible rename),
            // and rejecting it forced weixin's whole `g0$$X` Runnable
            // family to visible `X17` renames, breaking every lucky
            // `g0.r`-style resolution corpus-wide (+2911 `变量 r` — the
            // orphan rule is ungated by design, predating LOCAL-OK).
            if field_clash || pkg_shadow {
                if pkg_segments.contains(&cand_tail) {
                    continue;
                }
                if pkg_shadow
                    && fam_segs
                        .get(root)
                        .is_some_and(|segs| segs.contains(cand_tail.as_str()))
                {
                    continue;
                }
                if let Some((root_pkg, _)) = root.rsplit_once('/') {
                    let sib = format!("{root_pkg}/{cand_tail}");
                    if pool.has_name(&sib) || taken_sibling_displays.contains(&sib)
                    {
                        // A top-level sibling exists — or an EARLIER rule
                        // minted that display (obscuring_class_renames
                        // moved ssosdk/b → ssosdk/b2 while this loop was
                        // about to mint nested $b → b2: two `b2`s in one
                        // scope, member type outranks the package sibling
                        // and hijacked every bare `b2.b(..)` call).
                        continue;
                    }
                    taken_sibling_displays.insert(sib);
                }
            }
            let cand = format!("{disp_parent}${cand_tail}");
            if pool.has_name(&cand) || assigned.contains(&cand) {
                continue;
            }
            assigned.insert(cand.clone());
            map.insert(name.clone(), cand);
            break;
        }
    }
    // Stage 2 (body-gated package shadow): one body-level image scan
    // over the queued families (family_ref_segments — the field-
    // deshadow mechanism; instruction-level, cross-package refs), then
    // mint for the families whose members actually reference the
    // segment. Mirrors obscuring_class_renames' two-stage design at
    // family level. Lazy pools skip (same gate as stage 1's scan).
    let mut body_renamed = 0usize;
    if !body_cands.is_empty() && pool_majority_materialized(pool) {
        let roots: jdc_core::FxHashSet<&str> =
            body_cands.iter().map(|(_, r, _)| r.as_str()).collect();
        let mut members: jdc_core::FxHashSet<String> = jdc_core::FxHashSet::default();
        for n in &pool.order {
            let r = match n.find('$') {
                Some(i) => &n[..i],
                None => n.as_str(),
            };
            if roots.contains(r) {
                members.insert(n.clone());
            }
        }
        let t0 = std::time::Instant::now();
        let (by_referrer, _) = refscan::family_ref_segments(&pool.dexes, &members);
        // Referrer-keyed → family-root-keyed: every member renders in
        // the root file's scope.
        let mut root_segs: jdc_core::FxHashMap<&str, jdc_core::FxHashSet<&str>> =
            jdc_core::FxHashMap::default();
        for (referrer, segs) in &by_referrer {
            let r = referrer.split('$').next().unwrap_or(referrer);
            root_segs.entry(r).or_default().extend(segs.iter().map(|s| s.as_str()));
        }
        if std::env::var("DDC_STATS").is_ok() {
            eprintln!(
                "[renames] nested body-shadow scan: cands={} families={} scan={:?}",
                body_cands.len(),
                root_segs.len(),
                t0.elapsed()
            );
        }
        for (name, root, tail) in &body_cands {
            if !root_segs
                .get(root.as_str())
                .is_some_and(|s| s.contains(tail.as_str()))
            {
                    continue;
            }
            if let Some(d) = map.get(name) {
                // Pass-1 orphan mints are invisible (display tail ==
                // tail) — those get re-minted visibly here. Anything
                // already visibly renamed stays as the earlier rule set it.
                let cur_tail = d.rsplit(['/', '$']).next().unwrap_or("");
                if cur_tail != tail.as_str() {
                    continue;
                }
            }
            // Fresh display context: stage-1 renames may have moved the
            // parent/chain since this candidate was queued.
            let Some(parent) = find_outer_name(pool, name) else {
                continue;
            };
            let Some(rest) = name
                .strip_prefix(parent.as_str())
                .and_then(|t| t.strip_prefix('$'))
            else {
                continue;
            };
            if !clean_member_tail(rest) {
                continue;
            }
            let mut chain: Vec<String> = Vec::new();
            let mut cur = parent.clone();
            loop {
                let disp = map.get(&cur).cloned().unwrap_or_else(|| cur.clone());
                let simple = disp.rsplit('/').next().unwrap_or(&disp);
                chain.push(simple.rsplit('$').next().unwrap_or(simple).to_string());
                match find_outer_name(pool, &cur) {
                    Some(o) if o != cur => cur = o,
                    _ => break,
                }
            }
            let disp_parent = display_of(&parent, map);
            let mut enc_fields: jdc_core::FxHashSet<String> =
                jdc_core::FxHashSet::default();
            if let Some(pc) = pool.get(&parent) {
                for f in pc.static_fields.iter().chain(pc.instance_fields.iter()) {
                    enc_fields.insert(crate::classdec::java_ident(&f.name).into_owned());
                }
            }
            let mut k = 0u32;
            loop {
                k += 1;
                let cand_tail = if k == 1 {
                    tail.clone()
                } else {
                    format!("{tail}{k}")
                };
                if chain.contains(&cand_tail) || enc_fields.contains(&cand_tail) {
                    continue;
                }
                // The renamed tail stays a member type of the root
                // file — it must not shadow ANOTHER package segment the
                // family references (descriptor OR body level) nor a
                // same-package top-level sibling.
                if pkg_segments.contains(&cand_tail) {
                    continue;
                }
                if fam_segs
                    .get(root.as_str())
                    .is_some_and(|segs| segs.contains(cand_tail.as_str()))
                {
                    continue;
                }
                if root_segs
                    .get(root.as_str())
                    .is_some_and(|segs| segs.contains(cand_tail.as_str()))
                {
                    continue;
                }
                if let Some((root_pkg, _)) = root.rsplit_once('/') {
                    let sib = format!("{root_pkg}/{cand_tail}");
                    if pool.has_name(&sib) || taken_sibling_displays.contains(&sib) {
                        continue;
                    }
                    taken_sibling_displays.insert(sib);
                }
                let cand = format!("{disp_parent}${cand_tail}");
                if pool.has_name(&cand) || assigned.contains(&cand) {
                    continue;
                }
                // Repair pass-1 mints whose display chain ran through
                // THIS name's old display: stage 2 renames ancestors
                // after descendants were minted (weibo mobileads k$a →
                // a5 left pass-1's k$a$b$a → "k$a$b$a2" pointing through
                // the dead `a` — 找不到符号 类 a ×48). Ancestor-first
                // within each pass, but stage 2 as a whole runs last.
                let old_disp = format!("{disp_parent}${tail}");
                if old_disp != cand {
                    for (mk, mv) in map.iter_mut() {
                        if mk == name {
                            continue;
                        }
                        if let Some(rest) = mv.strip_prefix(old_disp.as_str()) {
                            if rest.starts_with('$') {
                                let repaired = format!("{cand}{rest}");
                                assigned.insert(repaired.clone());
                                *mv = repaired;
                            }
                        }
                    }
                }
                assigned.insert(cand.clone());
                map.insert(name.clone(), cand);
                body_renamed += 1;
                break;
            }
        }
    }
    if body_renamed > 0 && std::env::var("DDC_STATS").is_ok() {
        eprintln!("[renames] nested body-shadow renames: {body_renamed}");
    }
}

/// Field-obscuring rename candidates: nested member types whose simple
/// name equals a field of the enclosing class, mapped to the top-level
/// class whose file renders them (the LOCAL-OK family root). Mirrors the
/// skip conditions of `nested_collision_renames` (same-name entries must
/// agree, or the gate proves locality for a rename that never fires — or
/// worse, skips one that does).
fn field_clash_cands(pool: &DexPool, map: &HashMap<String, String>) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::default();
    for name in &pool.order {
        if !name.contains('$') || map.contains_key(name) {
            continue;
        }
        let Some(parent) = find_outer_name(pool, name) else {
            continue;
        };
        let Some(rest) = name
            .strip_prefix(parent.as_str())
            .and_then(|t| t.strip_prefix('$'))
        else {
            continue;
        };
        if !clean_member_tail(rest) {
            continue;
        }
        let tail = rest.rsplit('$').next().unwrap_or(rest);
        let Some(pc) = pool.get(&parent) else {
            continue;
        };
        let clash = pc
            .static_fields
            .iter()
            .chain(pc.instance_fields.iter())
            .any(|f| crate::classdec::java_ident(&f.name).as_ref() == tail);
        if !clash {
            continue;
        }
        let mut root = parent.clone();
        loop {
            match find_outer_name(pool, &root) {
                Some(o) if o != root => root = o,
                _ => break,
            }
        }
        out.insert(name.clone(), root);
    }
    out
}

/// Whether the pool is (majority-)materialized: full-decompile pipelines
/// call `materialize_all` before installing renames, progressive browse
/// pools materialize a handful of classes on demand. The refscan gate
/// must not run (or stall startup) in the progressive case.
fn pool_majority_materialized(pool: &DexPool) -> bool {
    let (mut mat, mut lazy) = (0usize, 0usize);
    for e in pool.classes.values() {
        match e {
            ClassEntry::Eager(_) => mat += 1,
            ClassEntry::Lazy { pc, .. } => {
                if pc.get().is_some() {
                    mat += 1;
                } else {
                    lazy += 1;
                }
            }
        }
    }
    lazy == 0 || mat >= lazy
}

/// Compute and install the registry (call before worker threads spawn).
/// CURRENT resident size (MB) — ru_maxrss is monotone (historical peak),
/// so purge effects are invisible there; live-vs-retained attribution
/// needs the instantaneous figure (macOS mach task_info).
#[cfg(target_vendor = "apple")]
fn current_rss_mb() -> f64 {
    #[repr(C)]
    struct TaskBasicInfo {
        virtual_size: u64,
        resident_size: u64,
        resident_size_max: u64,
        // time_value_t = {i32 secs, i32 usec} = 8B each.
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
    // count is in natural_t (4B) units; a smaller-than-kernel version
    // still fills the prefix fields we read.
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

/// ru_maxrss peak so far (MB). macOS reports bytes, Linux KB; Windows
/// has no getrusage (the old not(apple) fallback referenced it and
/// failed the MSVC link — LNK2019 — in the v0.1.18 CI).
#[cfg(unix)]
fn peak_rss_mb() -> f64 {
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

/// DDC_STATS phase attribution of the RSS high-water mark: peak AND
/// instantaneous live (ru_maxrss is monotone, so purge effects are
/// invisible there).
pub(crate) fn stats_rss(phase: &str) {
    if std::env::var("DDC_STATS").is_ok() {
        eprintln!(
            "[rss] {phase}: peak {:.0} MB, live {:.0} MB",
            peak_rss_mb(),
            current_rss_mb()
        );
    }
}

pub fn install_case_renames(pool: &DexPool) {
    let dbg = std::env::var("DDC_DBG_REN").is_ok();
    let mut tick = std::time::Instant::now();
    let mut lap = |tag: &str| {
        if dbg {
            eprintln!("[ren] {:?} {}", tick.elapsed(), tag);
            tick = std::time::Instant::now();
        }
    };
    let mut map = case_rename_map(pool);
    lap("case_rename_map");
    crate::stats_rss("  case_rename_map");
    // Cross-package reference first segments (descriptor level). Lazy
    // (progressive-browse) pools skip the scan — materializing every
    // class must not stall single-class queries; the obscuring rules
    // then simply never fire (browse mode has no whole-program javac).
    let (pkg_segs, fam_segs) = if pool_majority_materialized(pool) {
        ref_segments(pool)
    } else {
        (
            jdc_core::FxHashMap::default(),
            jdc_core::FxHashMap::default(),
        )
    };
    // Package-leaf shadows FIRST: the nested-collision renames compute
    // display chains off the (renamed) parent display names — running
    // them before the parent rename left both rules minting the same
    // display (`a2` twice in weibo's AIDL families).
    crate::stats_rss("  ref_segments");
    lap("ref_segments");
    pkg_leaf_shadow_renames(pool, &mut map);
    lap("pkg_leaf_shadow");
    class_pkg_collision_renames(pool, &mut map);
    lap("class_pkg_collision");
    obscuring_class_renames(pool, &mut map, &pkg_segs);
    lap("obscuring");
    nested_collision_renames(pool, &mut map, &fam_segs);
    lap("nested_collision");
    // LAST: lossy-sanitize collisions key on the display form every rule
    // above has already settled (`l.᩻ܶ` → `l/__.java`, 22,636 classes onto
    // 3 paths on bin.mt.plus — the silent-overwrite data loss).
    crate::stats_rss("  shadow+collision+obscuring+nested rules");
    lap("rules");
    lossy_sanitize_renames(pool, &mut map);
    // Default-package relocation keys on the settled displays too (it
    // PREFIXES them with the synthetic package) — runs after every
    // simple-name rule has minted.
    root_pkg_relocation(pool, &mut map);
    fw_shadow_relocation(pool, &mut map);
    crate::stats_rss("  lossy+root-pkg rules");
    lap("lossy+root");
    jdc_core::rename::set_class_renames(map);
    crate::stats_rss("class-renames installed");
    lap("set_class_renames");
    // Field-rename registry build and the access-widening image scan are
    // independent: both only READ the pool, they install into disjoint
    // statics (FIELD_RENAMES vs WIDEN), the WIDEN accessors are
    // is_some_and-guarded with no reader before render time, and lazy
    // materialization is OnceLock-safe. Run them concurrently — the 1.6s
    // QQ widening scan fully overlaps the field-rename build.
    if pool_majority_materialized(pool) {
        std::thread::scope(|s| {
            let h = s.spawn(|| crate::classdec::install_access_widening(pool));
            jdc_core::rename::set_field_renames(combined_field_renames(pool));
            let _ = h.join();
        });
        crate::stats_rss("field-renames + widening installed");
    lap("field+widen");
    } else {
        jdc_core::rename::set_field_renames(combined_field_renames(pool));
        lap("field_renames(lazy)");
    }
}

/// A top-level class `P/s` whose simple name equals the FIRST PACKAGE
/// SEGMENT of types that other classes in P reference cross-package.
/// Such a reference renders as the FQN `s.rest...`, and javac binds the
/// first segment to the nearest type in scope — the same-package class
/// `P/s` (JLS 6.4.2): `n91.f.a(x)` inside pc5/bd looks up member `f` OF
/// CLASS pc5.n91 (weixin's n0/n91/uc6/vc6 protobuf cluster: ~12k root
/// files, ~90k cascade lines; weibo's `a.g.b.a.b` chains root the same
/// way — class a beside subpackage a/ inside package a/g/b). No Java
/// syntax escapes an obscured first segment (imports can bypass it, but
/// single-type imports are blocked exactly where obfuscated leaf names
/// collide with same-package siblings), so the class moves: the r66
/// rename mechanism at a new trigger. Gated on DESCRIPTOR-level refs of
/// the candidate's own package (supers/fields/protos) — body-only refs
/// are missed by design; the import layer covers unblocked leaves there.
/// Descriptor-level cross-package reference first segments, aggregated
/// per PACKAGE (top-level obscurers: every file in P shares the scope)
/// and per FAMILY root (nested obscurers: a nested simple name is in
/// scope only inside its own root file). Raw `L..;` scans — no JavaType
/// parsing. Body-level refs are missed by design (the import layer
/// catches unblocked leaves there); descriptors cover supers, field
/// types and method protos — the header positions whose breakage starts
/// the error-type cascades.
/// Package (or family) → first segments of the cross-package types its
/// classes reference at descriptor level. Obscuring-rename trigger sets.
type SegMap<'a> = jdc_core::FxHashMap<&'a str, jdc_core::FxHashSet<&'a str>>;

fn ref_segments(pool: &DexPool) -> (SegMap<'_>, SegMap<'_>) {
    fn pkg_of(n: &str) -> &str {
        match n.rsplit_once('/') {
            Some((p, _)) => p,
            None => "",
        }
    }
    fn note<'a>(
        pkg_segs: &mut jdc_core::FxHashMap<&'a str, jdc_core::FxHashSet<&'a str>>,
        fam_segs: &mut jdc_core::FxHashMap<&'a str, jdc_core::FxHashSet<&'a str>>,
        pkg: &'a str,
        fam: &'a str,
        t: &'a str,
    ) {
        if !t.contains('/') {
            return; // root-package target: unreferenceable from named pkgs
        }
        let _ = pkg; // same-package refs COUNT: ddc renders headers/field
                     // types fully qualified (print_class_name), so `a.g.b.a.b`
                     // inside package a/g/b is obscured by class a/g/b/a just
                     // like a cross-package FQN (weibo's whole `a`-tree family).
        let seg = t.split('/').next().unwrap_or("");
        if !seg.is_empty() {
            pkg_segs.entry(pkg).or_default().insert(seg);
            fam_segs.entry(fam).or_default().insert(seg);
        }
    }
    fn scan<'a>(
        pkg_segs: &mut jdc_core::FxHashMap<&'a str, jdc_core::FxHashSet<&'a str>>,
        fam_segs: &mut jdc_core::FxHashMap<&'a str, jdc_core::FxHashSet<&'a str>>,
        pkg: &'a str,
        fam: &'a str,
        desc: &'a str,
    ) {
        let b = desc.as_bytes();
        let mut i = 0usize;
        while i < b.len() {
            if b[i] == b'L' {
                if let Some(end) = desc[i + 1..].find(';') {
                    note(pkg_segs, fam_segs, pkg, fam, &desc[i + 1..i + 1 + end]);
                    i += end + 2;
                    continue;
                }
            }
            i += 1;
        }
    }
    let mut pkg_segs: jdc_core::FxHashMap<&str, jdc_core::FxHashSet<&str>> =
        jdc_core::FxHashMap::default();
    let mut fam_segs: jdc_core::FxHashMap<&str, jdc_core::FxHashSet<&str>> =
        jdc_core::FxHashMap::default();
    for name in &pool.order {
        let Some(c) = pool.get(name) else { continue };
        let pkg = pkg_of(name);
        let fam = match name.find('$') {
            Some(i) => &name[..i],
            None => name.as_str(),
        };
        if let Some(sup) = &c.super_name {
            note(&mut pkg_segs, &mut fam_segs, pkg, fam, sup);
        }
        for sup in c.interfaces.iter() {
            note(&mut pkg_segs, &mut fam_segs, pkg, fam, sup);
        }
        for f in c.static_fields.iter().chain(c.instance_fields.iter()) {
            scan(&mut pkg_segs, &mut fam_segs, pkg, fam, &f.desc);
        }
        for m in c.direct_methods.iter().chain(c.virtual_methods.iter()) {
            scan(&mut pkg_segs, &mut fam_segs, pkg, fam, &m.desc);
        }
    }
    (pkg_segs, fam_segs)
}

/// A top-level class `P/s` whose simple name equals the FIRST PACKAGE
/// SEGMENT of types that classes in P reference cross-package. Such a
/// reference renders as the FQN `s.rest...`, and javac binds the first
/// segment to the nearest type in scope — the same-package class `P/s`
/// (JLS 6.4.2, no package fallback — verified): `n91.f.a(x)` inside
/// pc5/bd looks up member `f` OF CLASS pc5.n91 (weixin's n0/n91/uc6/vc6
/// protobuf cluster: ~12k root files, ~90k cascade lines; weibo's
/// `a.g.b.a.b` chains root the same way). No Java syntax escapes an
/// obscured first segment, so the class moves: the r66 rename mechanism
/// at a new trigger, gated on DESCRIPTOR-level refs of the candidate's
/// package (body-only refs go to the import layer). Nested obscurers
/// (a nested simple name shadowing a package inside its family file)
/// are the companion trigger in nested_collision_renames.
fn obscuring_class_renames(
    pool: &DexPool,
    map: &mut HashMap<String, String>,
    pkg_segs: &SegMap,
) {
    fn pkg_of(n: &str) -> &str {
        match n.rsplit_once('/') {
            Some((p, _)) => p,
            None => "",
        }
    }
    // Root package first segments (segments some class lives under).
    let mut root_segs: jdc_core::FxHashSet<&str> = jdc_core::FxHashSet::default();
    for n in &pool.order {
        if let Some(i) = n.find('/') {
            root_segs.insert(&n[..i]);
        }
    }
    let mut cands: Vec<&String> = Vec::new();
    for name in &pool.order {
        let simple = name.rsplit('/').next().unwrap_or(name);
        let pkg = pkg_of(name);
        if simple.is_empty() || simple.contains('$') || pkg.is_empty() {
            continue;
        }
        if !root_segs.contains(simple) {
            continue;
        }
        if map.get(name).is_some_and(|v| v != name) {
            continue; // an earlier rule moved it
        }
        // NB: NO "class lives under the s tree" exclusion. weibo's
        // a/a/b/c/l/a shadowed the FQN refs `a.a.a.a2` that its own
        // package files render for cross-subpackage targets (the
        // starts_with("a/") skip left the WHOLE a-tree unrenamed —
        // 找不到符号 类 a chains). The two-stage refsegs gates below
        // require an actual cross-package FQN ref for the trigger.
        cands.push(name);
    }
    let ncands = cands.len();
    if std::env::var("DDC_STATS").is_ok() {
        eprintln!(
            "[renames] obscuring candidates={ncands} pkgs-with-segs={}",
            pkg_segs.len()
        );
    }
    if cands.is_empty() {
        return;
    }
    // Fast collision oracle: sorted names + prefix range scan (the r66
    // candidate loop's `order.iter().any(starts_with)` is O(n) per
    // candidate — this rule can mint tens of thousands of renames).
    let mut sorted: Vec<&String> = pool.order.iter().collect();
    sorted.sort_unstable();
    let prefix_free = |cand: &str| -> bool {
        // No class named cand, nothing nested under cand$..., no package cand/...
        let idx = sorted.partition_point(|n| n.as_str() < cand);
        !sorted[idx..].first().is_some_and(|n| n.starts_with(cand))
    };
    let pkg_simples = pool.package_simples();
    let mut taken_displays: jdc_core::FxHashSet<String> = map.values().cloned().collect();
    let mut renamed = 0usize;
    // Stage 1: descriptor-level hits rename now; the rest queue for the
    // BODY-level image scan (weixin pc5's `invoke-static Ln91/f;.a` —
    // 7.8k+6.6k error lines the descriptor gate cannot see).
    let mut stage2: Vec<&String> = Vec::new();
    for name in cands {
        let pkg = pkg_of(name);
        let simple = name.rsplit('/').next().unwrap_or(name);
        if !pkg_segs.get(pkg).is_some_and(|segs| segs.contains(simple)) {
            stage2.push(name);
            continue;
        }
        let siblings = pkg_simples.get(pkg);
        let mut k = 1u32;
        loop {
            k += 1;
            let new_simple = format!("{simple}{k}");
            let cand = if pkg.is_empty() {
                new_simple.clone()
            } else {
                format!("{pkg}/{new_simple}")
            };
            // Exact/prefix pool clash, another rule's display target, the
            // package's own referenced segments (the new name must not
            // obscure ANOTHER package the same files reference), or a
            // case-insensitive clash with a same-package sibling
            // (case-insensitive filesystems fold the files).
            let clash = !prefix_free(&cand)
                || taken_displays.contains(&cand)
                || pkg_segs
                    .get(pkg)
                    .is_some_and(|segs| segs.contains(new_simple.as_str()))
                || siblings.is_some_and(|s| {
                    s.iter()
                        .any(|p| p.eq_ignore_ascii_case(&new_simple) && *p != simple)
                });
            if !clash {
                taken_displays.insert(cand.clone());
                map.insert(name.clone(), cand);
                renamed += 1;
                break;
            }
        }
    }
    let mut body_renamed = 0usize;
    if !stage2.is_empty() && pool_majority_materialized(pool) {
        let stage2_pkgs: jdc_core::FxHashSet<String> =
            stage2.iter().map(|n| pkg_of(n).to_string()).collect();
        let body_segs = crate::refscan::body_ref_segments(&pool.dexes, &stage2_pkgs);
        for name in stage2 {
            let pkg = pkg_of(name);
            let simple = name.rsplit('/').next().unwrap_or(name);
            if !body_segs
                .get(pkg)
                .is_some_and(|segs| segs.contains(simple))
            {
                continue;
            }
            let siblings = pkg_simples.get(pkg);
            let mut k = 1u32;
            loop {
                k += 1;
                let new_simple = format!("{simple}{k}");
                let cand = format!("{pkg}/{new_simple}");
                let clash = !prefix_free(&cand)
                    || taken_displays.contains(&cand)
                    || pkg_segs
                        .get(pkg)
                        .is_some_and(|segs| segs.contains(new_simple.as_str()))
                    || body_segs
                        .get(pkg)
                        .is_some_and(|segs| segs.contains(new_simple.as_str()))
                    || siblings.is_some_and(|s| {
                        s.iter()
                            .any(|p| p.eq_ignore_ascii_case(&new_simple) && *p != simple)
                    });
                if !clash {
                    taken_displays.insert(cand.clone());
                    map.insert(name.clone(), cand);
                    renamed += 1;
                    body_renamed += 1;
                    break;
                }
            }
        }
    }
    if std::env::var("DDC_STATS").is_ok() {
        eprintln!(
            "[renames] obscuring-class renames: {renamed} (body-gated {body_renamed}; pkgs-with-segs={}, cands={ncands})",
            pkg_segs.len()
        );
    }
}

/// Class-level counterpart of the non-ASCII rule already applied to
/// MEMBERS by `member_collision_renames`, kept as the SAFETY NET behind
/// the injective `sanitize_fq`.
///
/// `sanitize_fq` escapes non-identifier characters as `_u<hex>`, so two
/// distinct classes normally reach two distinct file names. It cannot be
/// injective against *lookalikes*, because ident-safe ASCII must pass
/// through unchanged: a literal class named `_u1a7b` collides with the
/// class whose single character is U+1A7B, and the pre-existing keyword
/// repair makes `_do` collide with `do`. Without this pass those pairs
/// repeat the original disaster — the writer opens with `create_new`,
/// takes EEXIST, unlinks and rewrites, so one class is destroyed with
/// exit code 0 (on bin.mt.plus the lossy predecessor of this rule hid
/// 22,633 of 30,768 classes exactly this way).
///
/// Groups are keyed by the SANITIZED path — the exact string both the
/// declaration site (`sanitize_ref`) and the writer (`source_path`)
/// produce. The first (sorted) member of each group keeps its name; the
/// rest become `<sanitized>_2`, `<sanitized>_3`, … Pure ASCII, so a
/// minted name is a fixed point of the sanitizer: it cannot silently
/// join a second collision group, and the declaration, every reference
/// and the file name agree by construction through `apply_class_rename`.
///
/// Costs nothing on a healthy pool: one pass over the emission set, and
/// it returns before touching the registry when every key is unique.
/// Runs LAST in `install_case_renames`: it keys on the display form the
/// earlier rules have already settled.
fn lossy_sanitize_renames(pool: &DexPool, map: &mut HashMap<String, String>) {
    let names = top_level_classes(pool);
    // Current display of an emission-set name. `case_rename_map` has
    // already inserted an entry for every one of these (identity or
    // renamed) and an exact registry hit short-circuits
    // `apply_class_rename`, so `map` already holds the final display —
    // no `$`-prefix walk is needed here.
    fn key_of(map: &HashMap<String, String>, n: &str) -> String {
        let disp = map.get(n).map(|s| s.as_str()).unwrap_or(n);
        sanitize_internal(disp)
    }
    let mut counts: HashMap<String, u32> = HashMap::default();
    for n in &names {
        *counts.entry(key_of(map, n)).or_insert(0) += 1;
    }
    if counts.values().all(|c| *c == 1) {
        return;
    }
    // Every sanitized path in the POOL is off-limits for a minted name,
    // not just the emission set: an inlined nested class still owns the
    // display it renders under inside its outer's file.
    let mut taken: jdc_core::FxHashSet<String> = pool
        .order
        .iter()
        .map(|n| sanitize_internal(map.get(n).map(|s| s.as_str()).unwrap_or(n)))
        .collect();
    let mut sorted: Vec<&String> = names.iter().collect();
    sorted.sort();
    let mut seen: jdc_core::FxHashSet<String> = jdc_core::FxHashSet::default();
    let mut renamed = 0usize;
    for n in sorted {
        let key = key_of(map, n);
        if counts.get(&key).copied().unwrap_or(0) < 2 {
            continue;
        }
        if seen.insert(key.clone()) {
            continue; // first (sorted) member of the group keeps its name
        }
        let (pkg, simple) = match key.rsplit_once('/') {
            Some((p, s)) => (p, s),
            None => ("", key.as_str()),
        };
        let mut i = 1u32;
        loop {
            i += 1;
            let cand = if pkg.is_empty() {
                format!("{simple}_{i}")
            } else {
                format!("{pkg}/{simple}_{i}")
            };
            if !taken.insert(cand.clone()) {
                continue; // already owned by a real class or another mint
            }
            // The minted display IS the sanitized name, so
            // sanitize_internal(cand) == cand and the writer's file name
            // matches the declaration it writes into it.
            map.insert(n.clone(), cand);
            renamed += 1;
            break;
        }
    }
    if std::env::var("DDC_STATS").is_ok() {
        eprintln!("[renames] lossy-sanitize collisions renamed={renamed}");
    }
}

/// A class named `P/s` while OTHER classes live under `P/s/` — the dex
/// namespace is flat, so obfuscators freely mint a class whose simple
/// name equals a subpackage of its own package (`ptr.a` +
/// `ptr.a.a`, weibo rsplay/ptr families). That namespace is
/// unrepresentable in Java: the declaration itself is
/// "类 a与带有相同名称的程序包冲突" and every qualified reference
/// through the segment becomes unresolvable (the 53.8k weibo
/// missing-class errors root here, with Object/getClass cascades on
/// top). Rename the class — the registry carries declarations and
/// references — with a candidate that cannot recreate the shape (not a
/// class, not a package, not a nested-class prefix, not another rule's
/// target).
fn class_pkg_collision_renames(pool: &DexPool, map: &mut HashMap<String, String>) {
    // Every package that exists in the pool — ALL ancestor prefixes of
    // every class path, not just direct parents: Java sees package
    // `z` as existing when any class lives at `z/a/b` (the weibo
    // `com.sina.weibo.z` interface collides with a subpackage-only
    // `z/` that has no directly-resident class).
    // Borrowed slices: the owned set allocated a fresh String per
    // ancestor prefix of every class (~600k allocations on weibo).
    let mut pkgs: jdc_core::FxHashSet<&str> = jdc_core::FxHashSet::default();
    for n in &pool.order {
        let mut rest = n.as_str();
        while let Some(i) = rest.rfind('/') {
            rest = &rest[..i];
            pkgs.insert(rest);
        }
    }
    // Exact-class and minted-display sets make the clash checks O(1):
    // the old `order.iter().any(starts_with(&cand))` scanned every name
    // per candidate per k-step (weibo: the second quadratic of the
    // single-class getclass path, ~1s) — and its prefix match also
    // counted SIBLING classes (`a/b2` matching `a/b23`) that map to
    // different files and never collide.
    let taken: jdc_core::FxHashSet<&str> = pool.order.iter().map(|s| s.as_str()).collect();
    let mut used: jdc_core::FxHashSet<String> = map.values().cloned().collect();
    for name in &pool.order {
        let Some((pkg, simple)) = name.rsplit_once('/') else {
            continue;
        };
        if simple.is_empty() || pkg.is_empty() || simple.contains('$') {
            // Nested classes render dotted (`Outer.a`) — no package
            // effect from their `$` tail.
            continue;
        }
        if !pkgs.contains(name.as_str()) || map.get(name).is_some_and(|v| v != name) {
            continue; // no subpackage under it, or an earlier rule moved it
        }
        let mut k = 1u32;
        loop {
            k += 1;
            let cand = format!("{pkg}/{simple}{k}");
            let clash = pkgs.contains(cand.as_str())
                || taken.contains(cand.as_str())
                || used.contains(&cand);
            if !clash {
                used.insert(cand.clone());
                map.insert(name.clone(), cand);
                break;
            }
        }
    }
}

/// Obfuscators can name a class after its own package leaf (`package k;`
/// `class k extends EditText`). Every SAME-PACKAGE reference to `k.Anything`
/// then resolves the first segment to the CLASS, not the package —
/// javac looks for `Anything` as a member of class k ("找不到符号 class p2,
/// location: class k"). Renaming the class (display-level, the registry
/// carries declarations and references) makes the package the only
/// thing the leaf name resolves to.
fn pkg_leaf_shadow_renames(pool: &DexPool, map: &mut HashMap<String, String>) {
    let taken: jdc_core::FxHashSet<&str> = pool.order.iter().map(|s| s.as_str()).collect();
    let mut used: jdc_core::FxHashSet<String> = map.values().cloned().collect();
    // Classes per package in ONE pass — the per-candidate
    // `order.iter().filter(starts_with).count()` was a quadratic scan
    // on obfuscated corpora (weibo: 2124 leaf-shadow candidates × 98k
    // names ≈ 2×10⁸ memcmp — the dominant single-class getclass cost,
    // and paid again by every full run).
    let mut under_pkg: jdc_core::FxHashMap<&str, usize> = jdc_core::FxHashMap::default();
    for n in &pool.order {
        if let Some(i) = n.rfind('/') {
            *under_pkg.entry(&n[..i]).or_default() += 1;
        }
    }
    for name in &pool.order {
        let Some((pkg, simple)) = name.rsplit_once('/') else {
            continue;
        };
        if simple.is_empty() || pkg.is_empty() {
            continue;
        }
        let leaf = pkg.rsplit('/').next().unwrap_or(pkg);
        // class name == package leaf, and the package really exists
        // (more classes than just this one live under it).
        if simple != leaf {
            continue;
        }
        let siblings = under_pkg.get(pkg).copied().unwrap_or(0).saturating_sub(1);
        if siblings == 0 || map.get(name).is_some_and(|v| v != name) {
            continue; // already renamed by an earlier rule; identity anchors are fine to overwrite
        }
        let mut k = 1u32;
        loop {
            k += 1;
            let cand = format!("{pkg}/{simple}{k}");
            if !taken.contains(cand.as_str()) && !used.contains(&cand) {
                used.insert(cand.clone());
                map.insert(name.clone(), cand);
                break;
            }
        }
    }
}

/// Member-level collision renames (fields AND methods of one class).
/// Two sources collapse onto one display name: obfuscators renaming a
/// synthetic outer reference (`this$0`) onto a real field's name, and
/// the identifier sanitizer mapping non-ASCII to `_` (whole
/// `ERROR_中文` families render one `ERROR________`).
/// Both are legal in bytecode (members resolve by index) and both are
/// javac "already defined" errors in source. Policy per colliding group
/// (fields keep the first non-synthetic name; a synthetic outer
/// reference restores `this$0`; everything else gets `2`/`3` suffixes;
/// method keys include erased parameter types):
/// Keys stay ORIGINAL (name, descriptor) so references resolve exactly;
/// only the rendered identifier changes.
/// Descriptor type string → internal class name (arrays/primitives →
/// None).
/// Can a method returning descriptor `m_ret` override an ancestor
/// declaration returning `r`? Equal, or a covariant subtype. ARRAY
/// returns must reach is_subtype in DESCRIPTOR form (it peels `[` for
/// element covariance): the old `strip L…;`-only comparison returned
/// false for `SavedState[] <: Object[]`, so Parcelable.Creator's real
/// `newArray(I)[LSavedState;` was judged an IMPOSTOR and renamed
/// (newArray3 — m_taken already held newArray + the bridge's newArray2),
/// breaking the interface override: "X$1不是抽象的, 并且未覆盖…
/// newArray(int)" (weibo ×504, battery ~789, rimet/news/uuyc ~655).
/// Object returns keep the internal-name form is_subtype's pool/fwdb
/// walk expects; an array m_ret against an object r resolves through
/// is_subtype's `sup == Object` / fwdb path (arrays ARE Objects).
fn ret_satisfies(pool: &DexPool, m_ret: &str, r: &str) -> bool {
    if m_ret == r {
        return true;
    }
    if m_ret.starts_with('[') {
        let sup: std::borrow::Cow<str> = if r.starts_with('[') {
            std::borrow::Cow::Borrowed(r)
        } else {
            match r.strip_prefix('L').and_then(|x| x.strip_suffix(';')) {
                Some(rc) => std::borrow::Cow::Borrowed(rc),
                None => return false, // primitive sup: only equality (above)
            }
        };
        return pool.is_subtype(m_ret, &sup);
    }
    if r.starts_with('[') {
        return false; // nothing but an array satisfies an array return
    }
    match (
        r.strip_prefix('L').and_then(|x| x.strip_suffix(';')),
        m_ret.strip_prefix('L').and_then(|x| x.strip_suffix(';')),
    ) {
        (Some(rc), Some(mc)) => pool.is_subtype(mc, rc),
        _ => false,
    }
}

/// Raw name → (argsig, ret) JVM-descriptor slices of one class's own
/// virtual methods — the key space the collision queries use.
type AncMethods = jdc_core::FxHashMap<
    std::sync::Arc<str>,
    Vec<(std::sync::Arc<str>, std::sync::Arc<str>)>,
>;

/// One provenance bucket of an analyzed class's ancestor chain (cached
/// layers, no merge — see `layer_rets`).
type AncLayers = Vec<std::sync::Arc<AncEntry>>;

/// Parsed direct virtual methods of one ancestor class + its chain
/// edges, cached per class name for the member-collision walk. The
/// framework chains (View/Activity/Fragment…) and pool hub supers repeat
/// across thousands of analyzed classes — parsing each ancestor's
/// descriptors ONCE per run (instead of per analyzed subclass) removes
/// the dominant main-loop cost.
struct AncEntry {
    methods: AncMethods,
    /// Provenance bucket: FRAMEWORK ancestors (fwdb classes AND
    /// desugared `java.*`/`javax.*` pool copies — javac compiles against
    /// the android.jar originals, the covariant source truth) vs POOL
    /// ancestors (rendered RAW — the erased bridge is what satisfies the
    /// raw interface). The keeper preference flips on this axis (weibo
    /// Function0.invoke bridge must keep the name; WhatsApp SortedMap
    /// .keySet real override must).
    is_fw: bool,
    /// super + direct interfaces (chain continuation), Object-stripped.
    supers: Vec<std::sync::Arc<str>>,
}

fn parse_anc_entry(pool: &DexPool, cn: &str) -> std::sync::Arc<AncEntry> {
    let mut methods: AncMethods = jdc_core::FxHashMap::default();
    let mut supers: Vec<std::sync::Arc<str>> = Vec::new();
    let is_fw;
    if let Some(ac) = pool.get_if_materialized(cn) {
        // Desugared `java.*`/`javax.*` copies in the dex are
        // pool-materialized, but javac compiles against the android.jar
        // ORIGINALS (covariant generics truth) — their declarations
        // belong to the fw bucket (WhatsApp bundles desugared
        // java.util.SortedMap; the pool bucket kept the
        // flattened-erasure bridge and renamed the real SortedSet
        // keySet).
        is_fw = cn.starts_with("java/") || cn.starts_with("javax/");
        for m in ac.all_methods() {
            if m.is_static() || &*m.name == "<init>" || &*m.name == "<clinit>"
            {
                continue;
            }
            let d: &str = &m.desc;
            let lo = d.find('(').map(|i| i + 1).unwrap_or(0);
            let hi = d.find(')').unwrap_or(d.len());
            methods
                .entry(m.name.clone())
                .or_default()
                .push((
                    std::sync::Arc::from(&d[lo..hi]),
                    std::sync::Arc::from(&d[hi + 1..]),
                ));
        }
        if let Some(s) = &ac.super_name {
            if s != "java/lang/Object" {
                supers.push(std::sync::Arc::from(s.as_str()));
            }
        }
        supers.extend(ac.interfaces.iter().map(|i| std::sync::Arc::from(i.as_str())));
    } else {
        // FRAMEWORK ancestors bind names too: a group member
        // implementing a framework declaration must keep the original
        // name (renaming WhatsApp's Map.keySet-style overrides broke
        // the interface chain — cannot-find:变量 ×18.6k). fwdb
        // descriptors are erased JVM form — directly comparable with
        // dex descs. A class that is NEITHER pool nor fwdb (phantom)
        // parses to an empty entry — a chain dead-end, same as before.
        is_fw = true;
        crate::fwdb::for_each_method(cn, |d, f| {
            if f & crate::fwdb::MF_STATIC != 0 {
                return;
            }
            let Some(open) = d.find('(') else {
                return;
            };
            let lo = open + 1;
            let hi = d.find(')').unwrap_or(0);
            // NOTE: hi == lo is the zero-arg case, not malformed; and
            // the name slice ends at `open` (a `d[..lo]` cut kept the
            // `(` — every fw key missed its gkey lookup).
            if hi == 0 || hi + 1 >= d.len() {
                return;
            }
            methods
                .entry(std::sync::Arc::from(&d[..open]))
                .or_default()
                .push((
                    std::sync::Arc::from(&d[lo..hi]),
                    std::sync::Arc::from(&d[hi + 1..]),
                ));
        });
        if let Some(sup) = crate::fwdb::super_of(cn) {
            if sup != "java/lang/Object" {
                supers.push(std::sync::Arc::from(sup));
            }
        }
        crate::fwdb::for_each_interface(cn, |i| supers.push(std::sync::Arc::from(i)));
    }
    std::sync::Arc::new(AncEntry {
        methods,
        is_fw,
        supers,
    })
}

/// Rets declared for `(name, argsig)` by any layer of a chain — lazy,
/// no merge (every consumer is an "any ancestor declares this" query,
/// so a per-layer scan replaces the old per-class eager map build).
/// Alloc-free: `Arc<str>: Borrow<str>` keys the hash lookup by `&str`.
fn layer_rets<'a>(
    layers: &'a [std::sync::Arc<AncEntry>],
    name: &'a str,
    argsig: &'a str,
) -> impl Iterator<Item = &'a str> {
    layers
        .iter()
        .filter_map(move |e| e.methods.get(name))
        .flatten()
        .filter(move |(a, _)| &**a == argsig)
        .map(|(_, r)| &**r)
}

fn member_collision_renames(
    pool: &DexPool,
) -> HashMap<std::sync::Arc<str>, Vec<jdc_core::rename::FieldRename>> {
    let mut out: HashMap<std::sync::Arc<str>, Vec<jdc_core::rename::FieldRename>> =
        HashMap::default();
    // Direct nested tails per base class (`C$a` → a for base C), for
    // the companion-field rule below.
    let mut child_tails: jdc_core::FxHashMap<&str, Vec<&str>> =
        jdc_core::FxHashMap::default();
    // Per-package RENAMED class display heads: a MINTED field display
    // equal to one of these shadows same-package qualified type refs
    // (`a2.changeQuickRedirect` binds to the field when the class a2's
    // display is a2 — the inherited rule minting field a→a2 ONTO the
    // pkg-renamed class display created exactly this: weibo +194
    // cannot-find). Keyed on the POST-rename display (what refs render),
    // covering each class's own display too.
    let root_segs = pool.root_pkg_segs();
    // Lazy single-class pools build the global tables from MATERIALIZED
    // classes only (the target + its on-demand family): unmateralized
    // siblings matter to whole-program javac, which a one-class stdout
    // dump does not have — the same reduced-fidelity contract the
    // obscuring rules already document for lazy pools. The walk over
    // every registered name was the dominant field-rename cost of
    // getclass (~0.5s on weibo for one class).
    let lazy = !pool_majority_materialized(pool);
    let mut pkg_displays: jdc_core::FxHashMap<&str, jdc_core::FxHashSet<String>> =
        jdc_core::FxHashMap::default();
    for n in &pool.order {
        if lazy && pool.get_if_materialized(n).is_none() {
            continue;
        }
        let renamed = crate::apply_class_rename(n);
        let (pkg, simple) = match renamed.rfind('/') {
            Some(i) => (renamed[..i].to_string(), renamed[i + 1..].to_string()),
            None => (String::new(), renamed.to_string()),
        };
        let head = simple.split('$').next().unwrap_or(&simple).to_string();
        if !head.is_empty() {
            let pkg_key = n.rfind('/').map(|i| &n[..i]).unwrap_or("");
            let _ = pkg;
            pkg_displays.entry(pkg_key).or_default().insert(head);
        }
    }
    // Reverse super map — the obscuring rename must register under every
    // transitive subclass (dex refs of an inherited field may name any
    // hierarchy class as owner).
    let mut subs: jdc_core::FxHashMap<&str, Vec<&str>> = jdc_core::FxHashMap::default();
    for n in &pool.order {
        if lazy && pool.get_if_materialized(n).is_none() {
            continue;
        }
        // EVERY `$` boundary: each prefix is an owner and the segment
        // after it one of its direct member-type tails. The old
        // first-segment-only map left NESTED owners (ta8/t$a, androidx
        // i0$e) with no member displays — their fields kept or minted
        // names colliding with their own member types, and in qualified
        // chains a VARIABLE OBSCURES a member type (JLS 6.4.2):
        // `ta8.t.a.a100.a` (Kotlin-object singleton sget, nova
        // static-ctx ×1,281) and `i0.e.b.ADDING` (lark ×145) bound to
        // the field instead of the nested class.
        for (i, _) in n.match_indices('$') {
            let owner = &n[..i];
            let rest = &n[i + 1..];
            let tail = match rest.find('$') {
                Some(j) => &rest[..j],
                None => rest,
            };
            if owner.is_empty() || tail.is_empty() {
                continue;
            }
            let v = child_tails.entry(owner).or_default();
            if !v.contains(&tail) {
                v.push(tail);
            }
        }
        if let Some(pc) = pool.get_if_materialized(n) {
            if let Some(sup) = &pc.super_name {
                if sup != "java/lang/Object" {
                    subs.entry(sup.as_str()).or_default().push(n.as_str());
                }
            }
        }
    }
    // Per-ancestor parse cache (function-local: lives exactly as long
    // as this rename install, so materialization state is consistent by
    // construction; freed on return — no cross-run staleness, no RSS
    // tail).
    let mut anc_cache: jdc_core::FxHashMap<
        std::sync::Arc<str>,
        std::sync::Arc<AncEntry>,
    > = jdc_core::FxHashMap::default();
    for name in &pool.order {
        let Some(pc) = pool.get_if_materialized(name) else {
            continue;
        };
        let outer_ref_desc = find_outer_name(pool, name)
            .filter(|o| o != name)
            .map(|o| format!("L{o};"));
        // ---- fields: one shared namespace (static + instance) ----
        let fields: Vec<&PoolField> = pc
            .static_fields
            .iter()
            .chain(pc.instance_fields.iter())
            .collect();
        let fgroups: HashMap<String, Vec<&PoolField>> = HashMap::default();
        let mut fgroups = fgroups;
        for f in &fields {
            let disp = crate::classdec::java_ident(&f.name).into_owned();
            fgroups.entry(disp).or_default().push(f);
        }
        let mut f_taken: jdc_core::FxHashSet<String> = fields
            .iter()
            .map(|f| crate::classdec::java_ident(&f.name).into_owned())
            .collect();
        // Displays already registered under THIS class by ancestor
        // subs-propagation (an ancestor processed earlier in pool.order
        // pushed its renames down): the own mints must avoid them or
        // two fields in one class render the same display
        // ("已在类中定义了变量 a2", weibo bx$b: subs-pushed bv→a2 beside
        // its own String→a2).
        if let Some(v) = out.get(&std::sync::Arc::from(name.as_str())) {
            for fr in v {
                f_taken.insert(fr.display.to_string());
            }
        }
        for group in fgroups.values() {
            if group.len() < 2 {
                continue;
            }
            let keeper_pos = group
                .iter()
                .position(|f| f.access & crate::access::ACC_SYNTHETIC == 0)
                .unwrap_or(0);
            for (gi, f) in group.iter().enumerate() {
                if gi == keeper_pos {
                    continue;
                }
                let base = crate::classdec::java_ident(&f.name).into_owned();
                let display: String = match &outer_ref_desc {
                    Some(ord)
                        if f.access & crate::access::ACC_SYNTHETIC != 0
                            && !f.is_static
                            && *ord == *f.desc =>
                    {
                        let mut k = 0u32;
                        loop {
                            let cand = if k == 0 {
                                "this$0".to_string()
                            } else {
                                format!("this$0{k}")
                            };
                            if f_taken.insert(cand.clone()) {
                                break cand;
                            }
                            k += 1;
                        }
                    }
                    _ => suffix_unique(&base, &mut f_taken),
                };
                // Register with subclass propagation: dex refs of an
                // inherited field may name ANY hierarchy class as owner
                // (R8 writes the referrer), and without the propagation
                // the declaring class's rename does not reach the
                // subclass-keyed lookups — the ref renders the RAW name
                // against a renamed declaration (bx$b read its inherited
                // `a:Lcom/xiaomi/push/bv;` as `this.a` while bx$a
                // declared it `a2`). Mirrors the method-side hierarchy
                // consistency rule.
                register_field_rename_with_subs(
                    pool, &subs, &mut out, name, &f.name, &f.desc, &display,
                );
            }
        }
        // ---- fields obscuring nested types (JLS 6.4.2) ----
        // `C.b.a(..)` with BOTH a field `b` and a member class `C$b`
        // binds to the FIELD — the qualified type reading is obscured
        // ("无法从静态上下文中引用非静态 变量 b" + the paired cannot-find
        // cascade; lark LKEvent/MotionLayout family, static-ctx ×1,154).
        // The CHILD-side rule (nested_collision_renames) already renames
        // colliding children when its local_ok gate passes (`a$a` renders
        // as `a2`); this field-side rule covers the gate-REJECTED residual
        // (child keeps display `b`, program-wide refs block its rename).
        // A field's refs all funnel through field_display (lift call
        // sites, declarations, const inits), so the field side needs no
        // locality gate. TWO invariants the first attempt violated (lark
        // +8,129): fire on the child's FINAL DISPLAY last segment, not
        // the raw tail (a child the child-side rule already renamed does
        // NOT obscure), and mint avoiding every child display (`a`→`a2`
        // landed on the existing `interface a2`, minting a fresh
        // obscuring pair).
        {
            let tails: &[&str] = child_tails
                .get(name.as_str())
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            let mut member_displays: jdc_core::FxHashSet<String> =
                jdc_core::FxHashSet::default();
            let mut obscuring: jdc_core::FxHashSet<String> =
                jdc_core::FxHashSet::default();
            for t in tails {
                let full = format!("{name}${t}");
                let renamed = crate::apply_class_rename(&full);
                let last = renamed
                    .rsplit('/')
                    .next()
                    .unwrap_or(&renamed)
                    .rsplit('$')
                    .next()
                    .unwrap_or(t)
                    .to_string();
                member_displays.insert(last.clone());
                // Only an INLINE-rendered member is reachable by the bare
                // simple name (own-file children are referenced by their
                // flat `$` form, which fields cannot obscure).
                let member_rendered = clean_member_tail(t)
                    && crate::find_outer_name(pool, &full).as_deref()
                        == Some(name.as_str());
                if member_rendered {
                    obscuring.insert(last);
                }
            }
            // The class's OWN simple display name joins the obscuring
            // set: a field named like its class captures every
            // qualified self-static ref from nested scopes
            // (`a2.changeQuickRedirect` bound to the String FIELD a2 —
            // weibo robust refs ×317; bare refs survive via the
            // variable namespace, the qualified form does not).
            let self_renamed = crate::apply_class_rename(name);
            let self_simple = self_renamed
                .rsplit('/')
                .next()
                .unwrap_or(&self_renamed)
                .rsplit('$')
                .next()
                .unwrap_or("")
                .to_string();
            if !self_simple.is_empty() {
                obscuring.insert(crate::classdec::java_ident(&self_simple).into_owned());
            }
            let self_pkg = name.rfind('/').map(|i| &name[..i]).unwrap_or("");
            let pkg_names = pkg_displays.get(self_pkg);
            for f in &fields {
                let fdisp = crate::classdec::java_ident(&f.name).into_owned();
                if fdisp.starts_with("this$") || !obscuring.contains(fdisp.as_str()) {
                    continue;
                }
                // Already renamed by the same-class duplicate-group rule?
                // Its display no longer equals the obscuring simple name,
                // so this rule's target collision does not exist — and
                // minting a SECOND entry for the same (name, desc) with
                // subs-propagation put a CONFLICTING display under every
                // subclass: refs naming the subclass resolved to one name
                // while the declaring class's declaration rendered the
                // first entry's (weibo bx$b read its inherited
                // `a:Lcom/xiaomi/push/bv;` as `this.a8` while bx$a
                // declared it `a2` — the R8 same-name-field family).
                let already = out
                    .get(&std::sync::Arc::from(name.as_str()))
                    .is_some_and(|v| {
                        v.iter().any(|fr| *fr.name == *f.name && *fr.desc == *f.desc)
                    });
                if already {
                    continue;
                }
                let mut k = 1u32;
                let display = loop {
                    k += 1;
                    let cand = format!("{fdisp}{k}");
                    // root_segs: a minted display equal to a PACKAGE
                    // first segment captures every qualified ref to
                    // that package inside the family's file (field e2
                    // ate `e2.c.b(..)` — lark pq/e "变量 c" ×47). The
                    // deshadow builder cannot repair this after the
                    // fact: it scans RAW names, and the collision is
                    // born here in display space.
                    if !f_taken.contains(&cand)
                        && !member_displays.contains(&cand)
                        && !root_segs.contains(&cand)
                        && !pkg_names.is_some_and(|p| p.contains(cand.as_str()))
                    {
                        f_taken.insert(cand.clone());
                        member_displays.insert(cand.clone());
                        break cand;
                    }
                };
                register_field_rename_with_subs(
                    pool, &subs, &mut out, name, &f.name, &f.desc, &display,
                );
                if std::env::var("DDC_OBSCURE_STATS").is_ok() {
                    eprintln!("[obscure] {name} field {} -> {}", f.name, display);
                }
            }
        }
        // ---- companion-holder fields vs nested types (JLS 6.4.2 in
        // expression context): Kotlin compiles `object`/companion
        // holders as a static field whose name EQUALS the nested class
        // it instances (`static final j$a a`), and static members of the
        // nested are invoked as `j.a.c(..)` — but `j.a` in an EXPRESSION
        // resolves to the FIELD (variables outrank member types), so the
        // call becomes a member lookup on the instance type... which is
        // the same class, yet javac reports 找不到符号 for statics
        // reached through the shadowed path, and the whole file
        // error-types (weibo feed/business/j: 654 errors + closure
        // cascade; the nested-side rename for this is the LOCAL-OK-gated
        // rule that explodes ungated — the FIELD side is safe because
        // dex field refs always carry the declaring class, so this
        // registry keys every consumer). Gate: the nested class has
        // static members (companion shape) and the field's descriptor
        // is exactly the nested type.
        if let Some(tails) = child_tails.get(name.as_str()) {
            for tail in tails {
                let nested = format!("{name}${tail}");
                let Some(nc) = pool.get_if_materialized(&nested) else {
                    continue;
                };
                // Only CLEAN tails render dotted (`j.a`); flat-rendered
                // tails (`j$1`) carry the `$` and never clash.
                if !clean_member_tail(tail) {
                    continue;
                }
                let has_statics = nc
                    .static_fields
                    .iter()
                    .chain(nc.instance_fields.iter())
                    .any(|f| f.is_static)
                    || nc.all_methods().any(|m| m.access & crate::access::ACC_STATIC != 0);
                if !has_statics {
                    continue;
                }
                // STRICT companion shape: the field's descriptor IS the
                // nested type (`static final j$a a`). The relaxed form
                // (any same-named field beside a statics-bearing nested)
                // minted thousands of renames on weibo and exploded the
                // battery to the 500k cap — the field funnel does not
                // hold at that scale (改名面 vs 引用覆盖面, again).
                let want_desc = format!("L{nested};");
                for f in pc.static_fields.iter().chain(pc.instance_fields.iter()) {
                    if f.desc.as_ref() != want_desc.as_str() {
                        continue;
                    }
                    let disp = crate::classdec::java_ident(&f.name).into_owned();
                    if disp != crate::classdec::java_ident(tail) {
                        continue;
                    }
                    // Already renamed by the duplicate-group rule above?
                    let already = out
                        .get(&std::sync::Arc::from(name.as_str()))
                        .is_some_and(|v| {
                            v.iter().any(|fr| *fr.name == *f.name && *fr.desc == *f.desc)
                        });
                    if already {
                        continue;
                    }
                    let display = suffix_unique(&disp, &mut f_taken);
                    f_taken.insert(display.clone());
                    out.entry(std::sync::Arc::from(name.as_str()))
                        .or_default()
                        .push(jdc_core::rename::FieldRename {
                            name: f.name.clone(),
                            desc: f.desc.clone(),
                            display: std::sync::Arc::from(display.as_str()),
                        });
                }
            }
        }
        // ---- methods: display key = sanitized name + erased params ----
        let methods: Vec<&PoolMethod> = pc.all_methods().collect();
        // Ancestor virtual-method returns per (name, argsig) — one
        // chain walk per non-interface class, shared by the
        // covariant-keeper selection and the impostor-override
        // detection below. Layers stay PER-ANCESTOR (cached Arcs, no
        // merge): every consumer is an "any ancestor declares
        // (name, argsig) → ret" query, so a per-layer scan replaces
        // the old per-class eager map build. Provenance splits the
        // chain into fw / pool buckets (see AncEntry.is_fw).
        let mut anc_chain: Option<(AncLayers, AncLayers)> = None;
        if pc.access & crate::access::ACC_INTERFACE == 0 {
            let mut fw_layers: AncLayers = Vec::new();
            let mut pool_layers: AncLayers = Vec::new();
            let mut stack: Vec<std::sync::Arc<str>> = Vec::new();
            if let Some(sup) = &pc.super_name {
                if sup != "java/lang/Object" {
                    stack.push(std::sync::Arc::from(sup.as_str()));
                }
            }
            stack.extend(
                pc.interfaces
                    .iter()
                    .map(|i| std::sync::Arc::from(i.as_str())),
            );
            let mut seen_cls: jdc_core::FxHashSet<std::sync::Arc<str>> =
                jdc_core::FxHashSet::default();
            while let Some(cn) = stack.pop() {
                if !seen_cls.insert(cn.clone()) {
                    continue;
                }
                let entry = match anc_cache.get(cn.as_ref()) {
                    Some(e) => e.clone(),
                    None => {
                        let e = parse_anc_entry(pool, &cn);
                        anc_cache.insert(cn.clone(), e.clone());
                        e
                    }
                };
                if !entry.methods.is_empty() {
                    if entry.is_fw {
                        fw_layers.push(entry.clone());
                    } else {
                        pool_layers.push(entry.clone());
                    }
                }
                stack.extend(entry.supers.iter().cloned());
            }
            anc_chain = Some((fw_layers, pool_layers));
        }
        let mut m_taken: jdc_core::FxHashSet<String> = methods
            .iter()
            .map(|m| crate::classdec::java_ident(&m.name).into_owned())
            .collect();
        let mut mgroups: HashMap<(String, String), Vec<&PoolMethod>> = HashMap::default();
        for m in &methods {
            let d: &str = &m.desc;
            let lo = d.find('(').map(|i| i + 1).unwrap_or(0);
            let hi = d.find(')').unwrap_or(d.len());
            let key = (
                crate::classdec::java_ident(&m.name).into_owned(),
                d[lo..hi].to_string(),
            );
            mgroups.entry(key).or_default().push(m);
        }
        for ((base, _), group) in &mgroups {
            if group.len() < 2 {
                continue;
            }
            // R8 return-type specialization: a non-isolated class carries
            // `M(args)void` (the real logic) beside the interface bridge
            // `M(args)R` (unbox → call void → return Unit). Same erasure,
            // so Java source can't hold both, and `void` never satisfies
            // the interface's `R` — the claim map keeps the void side and
            // drops the bridge, yielding "X不是抽象的, 并且未覆盖…invoke"
            // (Kotlin suspend lambdas, weixin/lark ×1.1k). This is NOT the
            // covariant-override shape below (void has no subtype
            // relation): keep the BRIDGE under the original name so it
            // satisfies the interface, and rename the void specialization
            // so both render and the bridge's call to it resolves to the
            // renamed method.
            // `)Ljava/lang/Void;` counts as void-like: a Kotlin `Nothing`
            // function (an unsupported-op that always throws) compiles to
            // the Void CLASS return, not the `V` primitive, but it IS the
            // real-logic side of the same specialization pair. Missing it
            // sent the group down the covariant path, which kept the Void
            // method and renamed the bridge, then the impostor pass renamed
            // the Void method too — leaving NO `addAll` to satisfy Set
            // (weibo compose SnapshotMapEntrySet "未覆盖Set中的addAll").
            let ret_void = |m: &PoolMethod| {
                m.desc.ends_with(")V") || m.desc.ends_with(")Ljava/lang/Void;")
            };
            let isolated = pc
                .super_name
                .as_ref()
                .is_none_or(|s| s == "java/lang/Object")
                && pc.interfaces.is_empty();
            let has_void = group.iter().any(|m| ret_void(m) && &*m.name != "<init>");
            let has_nonvoid = group.iter().any(|m| !ret_void(m));
            if !isolated && has_void && has_nonvoid {
                let keeper_desc: Option<&str> = group
                    .iter()
                    .copied()
                    .find(|m| {
                        m.access & crate::access::ACC_BRIDGE != 0
                            && !ret_void(m)
                            && &*m.name != "<init>"
                    })
                    .or_else(|| {
                        group
                            .iter()
                            .copied()
                            .find(|m| !ret_void(m) && &*m.name != "<init>")
                    })
                    .map(|m| &*m.desc);
                let mut seen_orig: jdc_core::FxHashSet<(&str, &str)> =
                    jdc_core::FxHashSet::default();
                for m in group.iter().copied() {
                    if keeper_desc == Some(&*m.desc) {
                        seen_orig.insert((&*m.name, &*m.desc));
                        continue; // bridge keeps the interface name
                    }
                    if !seen_orig.insert((&*m.name, &*m.desc)) {
                        continue; // exact duplicate: emitter drops it
                    }
                    let display = suffix_unique(base, &mut m_taken);
                    out.entry(std::sync::Arc::from(name.as_str()))
                        .or_default()
                        .push(jdc_core::rename::FieldRename {
                            name: m.name.clone(),
                            desc: m.desc.clone(),
                            display: std::sync::Arc::from(display.as_str()),
                        });
                }
                continue;
            }
            // A COVARIANT override pair (same ORIGINAL name, different
            // descriptors — the compiler's bridge shape: the interface's
            // `deserialize(e)` plus the narrower implementation). Renaming
            // either side breaks @Override ("is not abstract and does not
            // override abstract method deserialize(e)", ~2.2k on reqable);
            // the emitter's claim logic already renders only the
            // non-bridge member of the pair. An ISOLATED class (extends
            // Object, implements nothing — d8's ExternalSyntheticApiModel
            // Outline shells) overrides nothing: its same-(name,params)
            // pairs differing only by RETURN type are dex-legal distinct
            // methods, and dropping one strands its callers on the
            // survivor (`HalfKt$$…0.m()Class` vs `.m()V` — "void无法
            // 转换为Class", weibo ×85+).
            let covariant = !isolated
                && group
                    .iter()
                    .any(|m| group.iter().any(|o| o.name == m.name && o.desc != m.desc));
            if covariant {
                // Return-type-only clash in a class WITH a hierarchy:
                // Java cannot declare both, and the claim logic would
                // keep an arbitrary one — when that is NOT the member
                // an ancestor declares, every concrete subclass fails
                // "X不是抽象的, 并且未覆盖…" (weibo ×590: viewmodel.a
                // kept getUsageInfo()e while interface d declares
                // getUsageInfo()g; the kept side also strands the
                // dropped side's callers). Keep the ancestor-declared
                // member under the original name and rename the rest —
                // the registry carries every call site, and the
                // propagation below shares the display down the family.
                // 0 or 2+ ancestor-matched members = the shape Java
                // cannot express at all (two interfaces demanding
                // different returns): keep the old claim behavior.
                let keepers: Vec<&&PoolMethod> = group
                    .iter()
                    .filter(|m| {
                        let d: &str = &m.desc;
                        let lo = d.find('(').map(|i| i + 1).unwrap_or(0);
                        let hi = d.find(')').unwrap_or(d.len());
                        if let Some((fwl, pll)) = &anc_chain {
                            layer_rets(fwl, &m.name, &d[lo..hi])
                                .chain(layer_rets(pll, &m.name, &d[lo..hi]))
                                .any(|r| r == &d[hi + 1..])
                        } else {
                            false
                        }
                    })
                    .collect();
                if keepers.len() >= 2 {
                    // The shape Java cannot express at all (2+
                    // ancestors demanding different returns): no
                    // rename target — keep the claim behavior.
                    continue;
                }
                // fwdb flattens JDK covariant overrides to the erased
                // declaring return (SortedMap.keySet records Set, the
                // source return is SortedSet) — the sole "keeper" can
                // be the synthetic BRIDGE while the real override is
                // the non-bridge sibling whose return is a subtype of
                // the recorded one. Keeping the bridge renamed the
                // real method and broke the interface chain (WhatsApp
                // X/Jl2 SortedMap, cannot-find:变量 ×18.6k). Prefer the
                // non-bridge in that shape: the claim map dropped the
                // bridge before this machinery existed anyway.
                // Keeper selection by ancestor PROVENANCE:
                // - POOL ancestor matches (raw-rendered): the member
                //   whose return EXACTLY matches a pool erasure that
                //   is NOT purely framework-sourced keeps the name —
                //   the erased bridge is what satisfies the raw
                //   interface (weibo Function0.invoke / GlideRequest
                //   .load; renaming the real one instead cost +375).
                // - FRAMEWORK-only match: javac reads the compiled
                //   covariant truth (SortedMap.keySet→SortedSet) while
                //   fwdb records the flattened erasure (→Set) that the
                //   BRIDGE matches — prefer the non-bridge sibling
                //   whose return is a subtype (WhatsApp X/Jl2).
                // - no ancestor: first non-bridge (the old claim
                //   survivor), else dex order.
                fn ret_desc(m: &PoolMethod) -> &str {
                    let d: &str = &m.desc;
                    &d[d.find(')').map(|i| i + 1).unwrap_or(d.len())..]
                }
                let gkey = {
                    let d: &str = &group[0].desc;
                    let lo = d.find('(').map(|i| i + 1).unwrap_or(0);
                    let hi = d.find(')').unwrap_or(d.len());
                    (
                        crate::classdec::java_ident(&group[0].name).into_owned(),
                        d[lo..hi].to_string(),
                    )
                };
                // fw match: a NON-BRIDGE member whose return is
                // assignable to a framework ret (covariant source
                // truth — it satisfies the raw pool ancestor too,
                // overrides being covariant-legal in Java).
                let fw_i = anc_chain.as_ref().and_then(|(fwl, _)| {
                    let fws: Vec<&str> =
                        layer_rets(fwl, &gkey.0, &gkey.1).collect();
                    if fws.is_empty() {
                        return None;
                    }
                    group.iter().position(|m| {
                        m.access & crate::access::ACC_BRIDGE == 0
                            && fws.iter().any(|fr| {
                                // ret_satisfies: descriptor-level, so
                                // ARRAY covariant returns match too (the
                                // old ret_internal strip returned None
                                // for `[LX;` and fell back to descriptor
                                // equality — Creator newArray's real
                                // `[LSavedState;` never matched fw's
                                // `[LObject;`).
                                ret_satisfies(pool, ret_desc(m), fr)
                            })
                    })
                });
                let pool_i = anc_chain.as_ref().and_then(|(_, pll)| {
                    let rets: Vec<&str> =
                        layer_rets(pll, &gkey.0, &gkey.1).collect();
                    if rets.is_empty() {
                        return None;
                    }
                    group
                        .iter()
                        .position(|m| rets.iter().any(|r| *r == ret_desc(m)))
                });
                let keeper_idx = match (fw_i, pool_i) {
                    (Some(fi), _) => fi,
                    (None, Some(pi)) => pi,
                    (None, None) => group
                        .iter()
                        .position(|m| m.access & crate::access::ACC_BRIDGE == 0)
                        .unwrap_or(0),
                };
                // keepers==1: the ancestor-declared member keeps the
                // original name. keepers==0: NOTHING in the POOL binds
                // the name — keep the member the old claim map would
                // have kept (first NON-BRIDGE, else dex order) and
                // rename the rest through the registry: letting the
                // claim map silently drop a side stranded its callers
                // on the survivor's incompatible return (weibo xiaomi
                // push gk/dq/hj/hf/gt: `boolean b()Z` dropped beside
                // `String b()` — every `boolean v = this.b()` bound to
                // the String twin, "String无法转换为boolean" ×300+ in
                // the push package alone). NON-bridge preference is
                // load-bearing: a framework-ancestor covariant pair
                // (emoji2 n extends SpannableStringBuilder, Editable-
                // bridge beside the real override) satisfies the
                // ancestor through the REAL method — bridge-preference
                // renamed it and broke the override chain (reqable +8
                // "无法覆盖…返回类型不兼容").
                let keep_desc: &str = &group[keeper_idx].desc;
                let mut seen_orig: jdc_core::FxHashSet<(&str, &str)> =
                    jdc_core::FxHashSet::default();
                for m in group.iter().copied() {
                    if m.desc.as_ref() == keep_desc {
                        seen_orig.insert((&*m.name, &*m.desc));
                        continue;
                    }
                    if !seen_orig.insert((&*m.name, &*m.desc)) {
                        continue;
                    }
                    let display = suffix_unique(base, &mut m_taken);
                    out.entry(std::sync::Arc::from(name.as_str()))
                        .or_default()
                        .push(jdc_core::rename::FieldRename {
                            name: m.name.clone(),
                            desc: m.desc.clone(),
                            display: std::sync::Arc::from(display.as_str()),
                        });
                }
                continue;
            }
            // Real re-declarations (same original name AND descriptor —
            // R8 duplicates) are already deduped by the emitter's claim
            // logic; only SANITIZER collapses reach here and every one of
            // them is a distinct method.
            let mut base = base.clone();
            let mut seen_orig: jdc_core::FxHashSet<(&str, &str)> =
                jdc_core::FxHashSet::default();
            for m in group {
                if !seen_orig.insert((&m.name, &m.desc)) {
                    continue; // exact duplicate: emitter drops it
                }
                let display = suffix_unique(&base, &mut m_taken);
                out.entry(std::sync::Arc::from(name.as_str()))
                    .or_default()
                    .push(jdc_core::rename::FieldRename {
                        name: m.name.clone(),
                        desc: m.desc.clone(),
                        display: std::sync::Arc::from(display.as_str()),
                    });
                // The base stays the sanitized ORIGINAL so the first
                // occurrence keeps the plain name.
                let _ = &mut base;
            }
        }
        // ---- impostor overrides (R8 generic specialization) ----
        // A class method whose (name, argsig) matches an ancestor
        // declaration but whose return type satisfies NONE of the
        // ancestor returns is not an override Java can express: the
        // pair shares an erasure, so the class cannot declare both, and
        // ART never resolved it either (interface dispatch matches
        // name+descriptor — the missing proto throws AbstractMethodError
        // at runtime; weibo IStreamPresenter.getView()IStreamView vs the
        // impl's getView()IPageView with no subtype edge, ×258 across
        // getView/getWidget/getListView families). Rename the impostor
        // (the registry carries every caller and the propagation below
        // carries subclasses) and let synth_missing_interface_stubs add
        // the ART-faithful stub — rename-aware provision makes the
        // requirement missing again.
        if let Some((fwl, pll)) = &anc_chain {
            for m in &methods {
                if m.is_static()
                    || &*m.name == "<init>"
                    || &*m.name == "<clinit>"
                {
                    continue;
                }
                let d: &str = &m.desc;
                let lo = d.find('(').map(|i| i + 1).unwrap_or(0);
                let hi = d.find(')').unwrap_or(d.len());
                let m_ret = &d[hi + 1..];
                // Lazy per-layer scan: `found` replaces the old
                // `map.get(key)` presence gate (no ancestor declares
                // (name, argsig) ⇒ nothing to violate ⇒ skip).
                let mut found = false;
                let mut satisfied = false;
                'lay: for e in fwl.iter().chain(pll.iter()) {
                    let Some(rets) = e.methods.get(m.name.as_ref()) else {
                        continue;
                    };
                    for (a, r) in rets {
                        if **a != d[lo..hi] {
                            continue;
                        }
                        found = true;
                        // ret_satisfies handles ARRAY covariance in
                        // descriptor form — the old L…;-strip match
                        // answered false for `SavedState[] <: Object[]`
                        // and renamed the REAL Parcelable.Creator
                        // newArray as an impostor (newArray3), breaking
                        // the override ("不是抽象的…未覆盖 newArray(int)",
                        // weibo ×504 / battery ~789).
                        if ret_satisfies(pool, m_ret, r) {
                            satisfied = true;
                            break 'lay;
                        }
                    }
                }
                if !found || satisfied {
                    continue;
                }
                // Already renamed by a clash-group rule above?
                let already = out
                    .get(&std::sync::Arc::from(name.as_str()))
                    .is_some_and(|v| {
                        v.iter().any(|fr| *fr.name == *m.name && *fr.desc == *m.desc)
                    });
                if already {
                    continue;
                }
                let base = crate::classdec::java_ident(&m.name).into_owned();
                let display = suffix_unique(&base, &mut m_taken);
                out.entry(std::sync::Arc::from(name.as_str()))
                    .or_default()
                    .push(jdc_core::rename::FieldRename {
                        name: m.name.clone(),
                        desc: m.desc.clone(),
                        display: std::sync::Arc::from(display.as_str()),
                    });
            }
        }
    }
    // ---- method-rename propagation down the hierarchy ----
    // A renamed method must keep ONE display across its whole override
    // family: the parent's abstract declaration and every subclass
    // implementation/caller render through the registry, and a missed
    // link re-breaks the override ("未覆盖" at each concrete subclass)
    // or the call site (cannot-find). Register each method entry on the
    // full transitive subclass + implementer closure — a subclass that
    // does NOT declare the method still serves as a dex ref owner for
    // the inherited method, so it needs the entry too (same reasoning
    // as register_field_rename_with_subs for fields). Ancestor-forced
    // displays win; a colliding local entry re-mints off the original
    // method name and re-propagates.
    {
        let mut down: jdc_core::FxHashMap<&str, Vec<&str>> =
            jdc_core::FxHashMap::default();
        for (sup, children) in subs.iter() {
            down.entry(*sup).or_default().extend(children.iter().copied());
        }
        for n in &pool.order {
            if let Some(pc) = pool.get_if_materialized(n) {
                for i in pc.interfaces.iter() {
                    down.entry(i.as_str()).or_default().push(n.as_str());
                }
            }
        }
        type PropEntry = (
            std::sync::Arc<str>,
            std::sync::Arc<str>,
            std::sync::Arc<str>,
            std::sync::Arc<str>,
        );
        let mut queue: std::collections::VecDeque<PropEntry> =
            std::collections::VecDeque::new();
        {
            // Sorted seed: `out` is a std HashMap (RandomState) — an
            // unsorted seed would make conflicting ancestor forces
            // resolve in per-process order (display names flipping
            // run-to-run).
            let mut seed: Vec<PropEntry> = Vec::new();
            for (cls, entries) in out.iter() {
                for fr in entries {
                    if fr.desc.contains('(') {
                        seed.push((
                            cls.clone(),
                            fr.name.clone(),
                            fr.desc.clone(),
                            fr.display.clone(),
                        ));
                    }
                }
            }
            seed.sort();
            for t in seed {
                queue.push_back(t);
            }
        }
        let mut guard = 0usize;
        while let Some((cls, n, d, disp)) = queue.pop_front() {
            guard += 1;
            if guard > 2_000_000 {
                break;
            }
            let Some(children) = down.get(cls.as_ref()).cloned() else {
                continue;
            };
            for child in children {
                let mut changed = false;
                let mut reminted: Vec<PropEntry> = Vec::new();
                {
                    let child_key: std::sync::Arc<str> = std::sync::Arc::from(child);
                    let entries = out.entry(child_key.clone()).or_default();
                    // A raw method of the child literally named `disp`
                    // with a different signature blocks the force —
                    // renaming that raw method instead is the over-reach
                    // class (500k blowup history); skip this child.
                    let raw_blocked = pool
                        .get_if_materialized(child)
                        .is_some_and(|pc| {
                            pc.all_methods().any(|m| {
                                crate::classdec::java_ident(&m.name) == disp.as_ref()
                                    && m.desc != d
                            })
                        });
                    if raw_blocked {
                        continue;
                    }
                    match entries
                        .iter_mut()
                        .find(|fr| *fr.name == *n && *fr.desc == *d)
                    {
                        Some(e) if e.display == disp => {}
                        Some(e) => {
                            e.display = disp.clone();
                            changed = true;
                        }
                        None => {
                            entries.push(jdc_core::rename::FieldRename {
                                name: n.clone(),
                                desc: d.clone(),
                                display: disp.clone(),
                            });
                            changed = true;
                        }
                    }
                    if changed {
                        // Display collisions inside the child: the
                        // ancestor-forced entry keeps `disp`; others
                        // re-mint from their original method name.
                        let mut taken: jdc_core::FxHashSet<String> =
                            jdc_core::FxHashSet::default();
                        if let Some(pc) = pool.get_if_materialized(child) {
                            for m in pc.all_methods() {
                                taken.insert(
                                    crate::classdec::java_ident(&m.name).into_owned(),
                                );
                            }
                        }
                        for e in entries.iter() {
                            taken.insert(e.display.to_string());
                        }
                        let mut seen: jdc_core::FxHashSet<String> =
                            jdc_core::FxHashSet::default();
                        seen.insert(disp.to_string());
                        for e in entries.iter_mut() {
                            // FIELD entries never re-mint here: this is
                            // the METHOD-name propagation, and a field
                            // display colliding with a forced method
                            // display is resolved at the field's own mint
                            // (f_taken seeding above), not by renaming the
                            // field out from under its subs-registered
                            // references (bx$b's inherited bv→a2 was
                            // reminted to a7 while every ref kept a2).
                            if !e.desc.contains('(') {
                                continue;
                            }
                            if *e.name == *n && *e.desc == *d {
                                continue;
                            }
                            if seen.insert(e.display.to_string()) {
                                continue;
                            }
                            let base =
                                crate::classdec::java_ident(&e.name).into_owned();
                            let nd: std::sync::Arc<str> = std::sync::Arc::from(
                                suffix_unique(&base, &mut taken).as_str(),
                            );
                            e.display = nd.clone();
                            reminted.push((
                                child_key.clone(),
                                e.name.clone(),
                                e.desc.clone(),
                                nd,
                            ));
                        }
                    }
                }
                if changed {
                    let child_key: std::sync::Arc<str> = std::sync::Arc::from(child);
                    queue.push_back((child_key.clone(), n.clone(), d.clone(), disp.clone()));
                    for r in reminted {
                        queue.push_back(r);
                    }
                }
            }
        }
    }
    inherited_obscuring_renames(pool, &mut out, &pkg_displays);
    out
}

/// Inherited-field variant of the obscuring rule: class C renders a
/// member nested type `d` while INHERITING a field `d` from a pool
/// superclass — `C.d.b(..)` binds to the inherited variable (fields
/// obscure types, JLS 6.4.2) and fails ("无法从静态上下文中引用非静态
/// 变量 d" + "无法取消引用int" pairs, LynxBaseScrollViewDragging ×84 —
/// latent behind the broken-superclass hub until the same-class rule
/// healed it). The rename lands on the DECLARING class (one declaration
/// to re-render) and is registered under every transitive subclass as
/// well, because dex field refs may name any subclass as owner and the
/// lift-time consult keys on the ref's owner.
/// Register a field rename under the declaring class AND every transitive
/// subclass: dex field refs for an inherited access may name ANY class in
/// the hierarchy as owner (`iput Lh$a;->a` inside the constant-subclass
/// ctor of `h`), and the lift-time consult keys on the ref's owner — a
/// declaring-class-only entry leaves those refs stale (org/a/c/h ×3).
/// Subclasses that REDECLARE (name, desc) hide the field — their own
/// field is distinct and must keep its name (and their own obscuring,
/// if any, is handled when the loop reaches them).
fn register_field_rename_with_subs(
    pool: &DexPool,
    subs: &jdc_core::FxHashMap<&str, Vec<&str>>,
    out: &mut HashMap<std::sync::Arc<str>, Vec<jdc_core::rename::FieldRename>>,
    owner: &str,
    fname: &str,
    fdesc: &str,
    display: &str,
) {
    let mut queue: Vec<&str> = vec![owner];
    let mut seen: jdc_core::FxHashSet<&str> = jdc_core::FxHashSet::default();
    seen.insert(owner);
    let mut first = true;
    while let Some(c) = queue.pop() {
        let redeclares = if first {
            first = false;
            false // the declaring class itself: always register
        } else {
            pool.get_if_materialized(c)
                .map(|pc| {
                    pc.static_fields
                        .iter()
                        .chain(pc.instance_fields.iter())
                        .any(|f| f.name.as_ref() == fname && f.desc.as_ref() == fdesc)
                })
                .unwrap_or(false)
        };
        if redeclares {
            continue; // hidden below this point: its subtree refs bind to its own field
        }
        out.entry(std::sync::Arc::from(c))
            .or_default()
            .push(jdc_core::rename::FieldRename {
                name: std::sync::Arc::from(fname),
                desc: std::sync::Arc::from(fdesc),
                display: std::sync::Arc::from(display),
            });
        if let Some(children) = subs.get(c) {
            for ch in children {
                if seen.insert(*ch) {
                    queue.push(*ch);
                }
            }
        }
    }
}

fn inherited_obscuring_renames(
    pool: &DexPool,
    out: &mut HashMap<std::sync::Arc<str>, Vec<jdc_core::rename::FieldRename>>,
    pkg_displays: &jdc_core::FxHashMap<&str, jdc_core::FxHashSet<String>>,
) {
    let root_segs = pool.root_pkg_segs();
    // Reverse super map over materialized classes.
    let mut subs: jdc_core::FxHashMap<&str, Vec<&str>> = jdc_core::FxHashMap::default();
    for n in &pool.order {
        let Some(pc) = pool.get_if_materialized(n) else {
            continue;
        };
        if let Some(sup) = &pc.super_name {
            if sup != "java/lang/Object" {
                subs.entry(sup.as_str()).or_default().push(n.as_str());
            }
        }
    }
    let mut renamed: jdc_core::FxHashSet<(String, String, String)> =
        jdc_core::FxHashSet::default();
    for n in &pool.order {
        let Some(pc) = pool.get_if_materialized(n) else {
            continue;
        };
        if pc.is_interface() {
            continue;
        }
        // Member-rendered child displays of THIS class.
        let mut child_displays: Vec<String> = Vec::new();
        for child in pool.children_of(n.as_str()) {
            let Some(rest) = child
                .strip_prefix(n.as_str())
                .and_then(|t| t.strip_prefix('$'))
            else {
                continue;
            };
            if !clean_member_tail(rest) {
                continue;
            }
            if crate::find_outer_name(pool, child).as_deref() != Some(n.as_str()) {
                continue;
            }
            let renamed_disp = crate::apply_class_rename(child);
            let last = renamed_disp
                .rsplit('/')
                .next()
                .unwrap_or(&renamed_disp)
                .rsplit('$')
                .next()
                .unwrap_or(rest)
                .to_string();
            child_displays.push(last);
        }
        if child_displays.is_empty() {
            continue;
        }
        // Walk the super chain for an inherited field whose display
        // collides; own fields are the same-class rule's job.
        let own: jdc_core::FxHashSet<String> = pc
            .static_fields
            .iter()
            .chain(pc.instance_fields.iter())
            .map(|f| crate::classdec::java_ident(&f.name).into_owned())
            .collect();
        let mut sup = pc.super_name.clone();
        while let Some(sname) = sup {
            if sname == "java/lang/Object" {
                break;
            }
            let Some(sc) = pool.get_if_materialized(&sname) else {
                break;
            };
            for f in sc.static_fields.iter().chain(sc.instance_fields.iter()) {
                let fdisp = crate::classdec::java_ident(&f.name).into_owned();
                if fdisp.starts_with("this$") || !child_displays.contains(&fdisp) {
                    continue;
                }
                if !renamed.insert((sname.clone(), f.name.to_string(), f.desc.to_string())) {
                    continue;
                }
                // Mint in the DECLARING class's namespace: avoid its
                // fields + its own member-rendered child displays.
                let mut avoid: jdc_core::FxHashSet<String> = sc
                    .static_fields
                    .iter()
                    .chain(sc.instance_fields.iter())
                    .map(|g| crate::classdec::java_ident(&g.name).into_owned())
                    .collect();
                // The declaring class's PACKAGE class displays (post-
                // rename): minting onto one shadows its qualified refs
                // from inside the class (the a→a2-onto-display-a2 bug).
                if let Some(names) = pkg_displays
                    .get(sname.rfind('/').map(|i| &sname[..i]).unwrap_or(""))
                {
                    avoid.extend(names.iter().cloned());
                }
                for child in pool.children_of(sname.as_str()) {
                    let Some(rest) = child
                        .strip_prefix(sname.as_str())
                        .and_then(|t| t.strip_prefix('$'))
                    else {
                        continue;
                    };
                    if !clean_member_tail(rest) {
                        continue;
                    }
                    let rd = crate::apply_class_rename(child);
                    avoid.insert(
                        rd.rsplit('/')
                            .next()
                            .unwrap_or(&rd)
                            .rsplit('$')
                            .next()
                            .unwrap_or(rest)
                            .to_string(),
                    );
                }
                let mut k = 1u32;
                let display = loop {
                    k += 1;
                    let cand = format!("{fdisp}{k}");
                    // root_segs: see the own-field mint site — a
                    // package first segment must never be minted.
                    if !avoid.contains(&cand) && !root_segs.contains(&cand) {
                        break cand;
                    }
                };
                register_field_rename_with_subs(
                    pool, &subs, out, &sname, &f.name, &f.desc, &display,
                );
                if std::env::var("DDC_OBSCURE_STATS").is_ok() {
                    eprintln!("[obscure-inh] {sname} field {} -> {display} (child of {n})", f.name);
                }
            }
            sup = sc.super_name.clone();
        }
        let _ = &own;
    }
}

fn suffix_unique(base: &str, taken: &mut jdc_core::FxHashSet<String>) -> String {
    let mut k = 1u32;
    loop {
        k += 1;
        let cand = format!("{base}{k}");
        if taken.insert(cand.clone()) {
            return cand;
        }
    }
}

/// member_collision_renames + field_deshadow_renames, collision-registry
/// entries winning per (owner, name, desc).
fn combined_field_renames(
    pool: &DexPool,
) -> HashMap<std::sync::Arc<str>, Vec<jdc_core::rename::FieldRename>> {
    let t = std::time::Instant::now();
    let mut base = member_collision_renames(pool);
    if std::env::var("DDC_DBG_REN").is_ok() {
        eprintln!("[ren] {:?} member_collision_renames", t.elapsed());
    }
    let t = std::time::Instant::now();
    crate::stats_rss("  member_collision_renames");
    let extra = field_deshadow_renames(pool);
    if std::env::var("DDC_DBG_REN").is_ok() {
        eprintln!("[ren] {:?} field_deshadow_renames", t.elapsed());
    }
    let mut merged = 0usize;
    for (owner, v) in extra {
        let e = base.entry(owner).or_default();
        for fr in v {
            if !e.iter().any(|x| x.name == fr.name && x.desc == fr.desc) {
                e.push(fr);
                merged += 1;
            }
        }
    }
    if std::env::var("DDC_STATS").is_ok() {
        eprintln!("[renames] field-deshadow renames: {merged}");
    }
    crate::stats_rss("  field_deshadow merged");
    base
}

/// Field deshadow (JLS 6.4.2, expression position): a FIELD whose display
/// name equals a root package first segment captures package-qualified
/// renders inside its family's file — androidx z2's field `j` binds the
/// `j` of `j.a.a` (找不到符号 变量 a). Rename through the field registry
/// (declaration and every reference funnel through field_display).
/// Gate: only when the family actually references that segment
/// (family_ref_segments body scan) or the segment is the family's OWN
/// package root (same-package refs fall back to `root.x` FQN renders) —
/// ungated field renames are the six-times-falsified blast radius.
fn field_deshadow_renames(
    pool: &DexPool,
) -> HashMap<std::sync::Arc<str>, Vec<jdc_core::rename::FieldRename>> {
    let mut out: HashMap<std::sync::Arc<str>, Vec<jdc_core::rename::FieldRename>> =
        HashMap::default();
    let root_segs = pool.root_pkg_segs();
    if root_segs.is_empty() {
        return out;
    }
    // Candidate fields per family (display, original name, desc).
    let mut cand: HashMap<
        String,
        Vec<(String, String, String, u32)>,
    > = HashMap::default();
    for name in &pool.order {
        let Some(pc) = pool.get_if_materialized(name) else {
            continue;
        };
        for f in pc.static_fields.iter().chain(pc.instance_fields.iter()) {
            let disp = crate::classdec::java_ident(&f.name).into_owned();
            if root_segs.contains(&disp) {
                cand
                    .entry(name.clone())
                    .or_default()
                    .push((disp, f.name.to_string(), f.desc.to_string(), f.access));
            }
        }
    }
    if cand.is_empty() {
        return out;
    }
    let cand_set: jdc_core::FxHashSet<String> = cand.keys().cloned().collect();
    let t0 = std::time::Instant::now();
    let (fam_segs, fam_nest) =
        crate::refscan::family_ref_segments(&pool.dexes, &cand_set);
    if std::env::var("DDC_STATS").is_ok() {
        eprintln!(
            "[renames] field-deshadow scan: cands={} scan={:?}",
            cand_set.len(),
            t0.elapsed()
        );
    }
    for (fam, fields) in cand {
        let own_root = fam.find('/').map(|i| fam[..i].to_string());
        let scanned = fam_segs.get(&fam);
        // NO nested-tail collision trigger here: a private field named
        // like a nested class does capture `Fam.b.member` in expression
        // position (lark LKEvent static-ctx family, ~1k lines), but
        // renaming it is corpus-toxic even gated private+body-
        // referenced — lark +32k (43,643 renames), weixin/weibo −200.
        // Eighth falsification of loose rename gates: the resolution
        // perturbation surface of mass field renames dwarfs the target
        // family. The fam_nest scan stays wired for a future
        // render-side fix (the capture is only unfixable INSIDE the
        // family file; foreign files could import the nested type).
        let _ = &fam_nest;
        let hits: Vec<(String, String, String)> = fields
            .into_iter()
            .filter(|(disp, _, _, _access)| {
                scanned.is_some_and(|s| s.contains(disp))
                    || own_root.as_deref() == Some(disp.as_str())
            })
            .map(|(d, n, de, _)| (d, n, de))
            .collect();
        if hits.is_empty() {
            continue;
        }
        // Names already taken in the family's member scope: field and
        // method displays plus direct nested-class tails (member types
        // share the lexical scope of the rendered file). Built only for
        // hit families — the per-family setup must stay off the 175k-
        // candidate loop (an O(pool.order) nested scan there cost 60s on
        // weixin).
        let mut taken: jdc_core::FxHashSet<String> = jdc_core::FxHashSet::default();
        if let Some(pc) = pool.get_if_materialized(&fam) {
            for f in pc.static_fields.iter().chain(pc.instance_fields.iter()) {
                taken.insert(crate::classdec::java_ident(&f.name).into_owned());
            }
            for m in pc.direct_methods.iter().chain(pc.virtual_methods.iter()) {
                taken.insert(crate::classdec::java_ident(&m.name).into_owned());
            }
        }
        let mut nested_tails: jdc_core::FxHashSet<String> =
            jdc_core::FxHashSet::default();
        for child in pool.children_of(&fam) {
            if let Some(tail) = child.rsplit('$').next() {
                if !tail.is_empty() {
                    taken.insert(tail.to_string());
                    nested_tails.insert(tail.to_string());
                }
            }
        }
        for (disp, name, desc) in hits {
            let mut k = 0u32;
            let new_disp = loop {
                k += 1;
                let c = if k == 1 {
                    format!("{disp}x")
                } else {
                    format!("{disp}x{k}")
                };
                if !taken.contains(&c) && !root_segs.contains(&c) {
                    break c;
                }
            };
            taken.insert(new_disp.clone());
            out.entry(std::sync::Arc::from(fam.as_str()))
                .or_default()
                .push(jdc_core::rename::FieldRename {
                    name: std::sync::Arc::from(name.as_str()),
                    desc: std::sync::Arc::from(desc.as_str()),
                    display: std::sync::Arc::from(new_disp.as_str()),
                });
        }
    }
    out
}
pub use jdc_core::rename::apply_class_rename;

// ---------------------------------------------------------------------------
// Default-package relocation.
// ---------------------------------------------------------------------------

/// The synthetic package the default-package cohort currently relocates
/// to (Mutex, not OnceLock: `set_class_renames` replaces the registry per
/// pool, so a second pool in one process must be able to re-key this).
static ROOT_PKG: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// The synthetic package display root-package classes were moved under
/// (`None` when no relocation ran). Consumers keying per-package caches
/// by the ORIGINAL package map this display back to `""`.
pub fn root_pkg_display() -> Option<String> {
    ROOT_PKG
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Default-package relocation: javac cannot resolve ANY reference from a
/// named package to an unnamed-package class (JLS 7.4.2 — `private it0 f;`
/// inside com.amap.X is 找不到符号 whatever the syntax; xingye ~198k +
/// Amap ~81k errors ≈ 73% of the whole-directory sweep total, and
/// kotlin.jvm.Function0 even EXTENDS a root class on xingye). No source
/// syntax escapes it — but the rename registry escapes the NAME: move the
/// whole root cohort under a synthetic package (`ddcroot/a.java`,
/// `package ddcroot;`) and every cross-package ref renders the resolvable
/// FQN `ddcroot.a`. Cohort-uniform: in-cohort refs keep resolving (one
/// new shared package), the case/lossy dedupes inside the cohort still
/// hold (a directory move preserves relative collisions), and the
/// synthetic segment is collision-bumped against every pool package and
/// class so it can never shadow or be shadowed. Runs LAST (prefixes the
/// settled displays). Fidelity note: declared package no longer matches
/// the binary name — the same trade every rename rule here makes, on a
/// cohort whose cross-package render is otherwise dead code.
/// Pool classes that shadow a FRAMEWORK class (d8 desugar stubs at the
/// framework FQN). The FILE identity relocates to the synthetic package,
/// but every REFERENCE renders the raw framework name — the bytecode's
/// FQN means the platform class (the stub is dead code the boot
/// classpath outranks).
pub(crate) static FW_SHADOW: std::sync::OnceLock<
    std::sync::Arc<jdc_core::FxHashSet<String>>,
> = std::sync::OnceLock::new();

/// True when `internal` is a framework-shadow stub (fast empty-set
/// check when no corpus stubs exist).
pub fn is_fw_shadow(internal: &str) -> bool {
    let Some(s) = FW_SHADOW.get() else { return false };
    // Exact hit, or any $-chain PREFIX hits: a nested FRAMEWORK member
    // of a stub outer (`ImageDecoder$Source` with the stub
    // `ImageDecoder` in the pool) renders through the $-walk of the
    // stub's map entry — it must keep the raw framework name too.
    if s.contains(internal) {
        return true;
    }
    let mut idx = internal.len();
    while let Some(i) = internal[..idx].rfind('$') {
        if s.contains(&internal[..i]) {
            return true;
        }
        idx = i;
    }
    false
}

/// d8's library/API desugaring embeds STUB classes under the framework's
/// own names (`android.graphics.Insets`, `android.app.Person` — minimal
/// shapes the runtime never loads, the boot classpath wins). Emitted at
/// their FQN they SHADOW the real framework class for every OTHER file
/// at compile time: javac resolves `android.graphics.Insets` to the
/// field-less stub and `insets.top` dies (reqable r2: the whole Insets
/// family). Relocate every pool class whose head resolves in the
/// embedded framework DB to a synthetic package; the rename registry
/// carries every reference.
fn fw_shadow_relocation(pool: &DexPool, map: &mut HashMap<String, String>) {
    let root_segs = pool.root_pkg_segs();
    // ANDROID-namespace only: a framework-shadow stub in another
    // namespace is usually a REAL bundled implementation (weibo ships
    // its own full org.json — relocating it broke every org.json user,
    // +86). d8's API-desugar stubs live under android/.
    let shadow_set: std::sync::Arc<jdc_core::FxHashSet<String>> =
        std::sync::Arc::new(
            pool.order
                .iter()
                .filter(|n| n.starts_with("android/"))
                .filter(|n| {
                    let head = n.split('$').next().unwrap_or(n);
                    crate::fwdb::find(head).is_some()
                })
                .map(|n| n.to_string())
                .collect(),
        );
    let _ = FW_SHADOW.set(shadow_set.clone());
    jdc_core::rename::set_fw_shadow(
        shadow_set.iter().cloned().collect(),
    );
    let mut pkg = "ddcfwstub".to_string();
    while pool.has_name(&pkg) || root_segs.contains(&pkg) {
        pkg.push('_');
    }
    let mut moved = 0usize;
    for name in &pool.order {
        // The relocation decision keys on the $-chain HEAD (a nested
        // stub rides its outer's identity); nested-of-pool-outer shapes
        // stay entry-less so apply_class_rename's $-walk composes them
        // through the relocated outer (the VasUIToken exact-entry
        // lesson from root_pkg_relocation). Android-namespace only —
        // see the set build above.
        if !name.starts_with("android/") {
            continue;
        }
        let head = name.split('$').next().unwrap_or(name);
        if crate::fwdb::find(head).is_none() {
            continue;
        }
        if head != name && pool.has_name(head) {
            continue;
        }
        let disp = map.get(name).cloned().unwrap_or_else(|| name.clone());
        if disp != *name {
            // An earlier rule renamed this class (obscuring/collision
            // family) — its display already diverges from the FQN and
            // the relocation prefix applies to the settled display.
            map.insert(name.clone(), format!("{pkg}/{disp}"));
        } else {
            map.insert(name.clone(), format!("{pkg}/{name}"));
        }
        moved += 1;
    }
    if std::env::var("DDC_STATS").is_ok() {
        eprintln!("[renames] fw-shadow relocation: {moved} classes -> {pkg}");
    }
}

fn root_pkg_relocation(pool: &DexPool, map: &mut HashMap<String, String>) {
    // Gate on NAMES only (identical verdict in lazy browse and full
    // decompile — the display must not depend on materialization):
    // relocation matters only when BOTH cohorts exist — the JLS-dead
    // direction is named-package → root-package, so an all-root app
    // (hello fixtures, pure-demo dexes) keeps the faithful layout.
    let has_root = pool.order.iter().any(|n| !n.contains('/'));
    let has_named = pool.order.iter().any(|n| n.contains('/'));
    if !has_root || !has_named {
        *ROOT_PKG.lock().unwrap_or_else(|e| e.into_inner()) = None;
        return;
    }
    let root_segs = pool.root_pkg_segs();
    let mut pkg = "ddcroot".to_string();
    while pool.has_name(&pkg) || root_segs.contains(&pkg) {
        pkg.push('_');
    }
    // A root class whose simple name IS a top-level package segment
    // (`com`, `org`, ...) cannot simply MOVE into the synthetic
    // package: a same-package type shadows the package name in every
    // qualified ref from every cohort file (JLS 6.4.1 — rimet's root
    // `com`/`org` hijacked 235k `com.alibaba...` refs as 类 alibaba
    // 位置: 类 com). Those get a bumped first segment (`com` →
    // ddcroot/com2) — a display rename the registry carries through
    // every ref path, like the obscuring renames.
    const FW_SEGS: [&str; 10] = [
        "java", "javax", "android", "org", "junit", "dalvik", "libcore", "androidx", "kotlin",
        "kotlinx",
    ];
    let mut taken: jdc_core::FxHashSet<String> = jdc_core::FxHashSet::default();
    for name in &pool.order {
        if name.contains('/') {
            continue;
        }
        let disp = map.get(name).cloned().unwrap_or_else(|| name.clone());
        taken.insert(format!("{pkg}/{disp}"));
    }
    let mut moved = 0usize;
    let mut bumped = 0usize;
    for name in &pool.order {
        if name.contains('/') {
            continue;
        }
        let disp = map.get(name).cloned().unwrap_or_else(|| name.clone());
        if disp.contains('/') {
            continue; // defensive: some rule already packaged it
        }
        if disp == *name && name.contains('$') && find_outer_name(pool, name).is_some() {
            // An UNRENAMED nested name with a pool outer must stay
            // entry-less so apply_class_rename's $-walk composes it
            // through the outer's relocated+renamed display — an exact
            // entry short-circuits the walk and freezes the outer's
            // pre-rename simple (xingye's case-collision quads:
            // `ue5$a` exact-mapped to ddcroot/ue5$a instead of
            // ddcroot/ue5_4$a — 程序包ddcroot.ue5不存在 ×61k).
            // The case-collision machinery pre-inserts an IDENTITY
            // entry for every emission-set name, which short-circuits
            // the same walk and froze `$`-tails in the ROOT package
            // while their outer moved to ddcroot — family split: QQ's
            // VasUIToken$1..$1xx husks stayed at tree root, and every
            // `VasUIToken$N.INSTANCE` ref from ddcroot/VasUIToken.java
            // died (找不到符号 变量 VasUIToken$N ×202 in that one file).
            // Remove the pin; the walk composes through the outer.
            map.remove(name);
            continue;
        }
        let head = disp.split('$').next().unwrap_or(&disp);
        let collide = root_segs.contains(head) || FW_SEGS.contains(&head);
        if !collide {
            map.insert(name.clone(), format!("{pkg}/{disp}"));
            moved += 1;
            continue;
        }
        let rest = &disp[head.len()..];
        let mut k = 1u32;
        loop {
            k += 1;
            let nh = format!("{head}{k}");
            if root_segs.contains(&nh) || FW_SEGS.contains(&nh.as_str()) {
                continue;
            }
            let cand = format!("{pkg}/{nh}{rest}");
            if taken.contains(&cand) {
                continue;
            }
            taken.insert(cand.clone());
            map.insert(name.clone(), cand);
            moved += 1;
            bumped += 1;
            break;
        }
    }
    *ROOT_PKG.lock().unwrap_or_else(|e| e.into_inner()) = Some(pkg.clone());
    if std::env::var("DDC_STATS").is_ok() {
        eprintln!("[renames] root-pkg relocation: {moved} classes -> {pkg}/ ({bumped} segment-bumped)");
    }
}