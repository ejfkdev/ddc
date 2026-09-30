//! DEX container: header, id tables, class defs.

use std::collections::HashMap;

use crate::annotations::{self, EncodedValue};
use crate::code::CodeItem;
use crate::reader::Cursor;

/// Index sentinel for "absent".
pub const NO_INDEX: u32 = 0xffff_ffff;

#[derive(Debug, Clone)]
pub struct ProtoId {
    /// Index into string ids: the shorty descriptor.
    pub shorty_idx: u32,
    /// Index into type ids: return type.
    pub return_type_idx: u32,
    /// Offset of the parameter type list, or 0.
    pub parameters_off: u32,
}

#[derive(Debug, Clone)]
pub struct FieldId {
    pub class_idx: u32,
    pub type_idx: u32,
    pub name_idx: u32,
}

#[derive(Debug, Clone)]
pub struct MethodId {
    pub class_idx: u32,
    pub proto_idx: u32,
    pub name_idx: u32,
}

/// One `class_def_item`.
#[derive(Debug, Clone)]
pub struct ClassDef {
    /// Index into type ids.
    pub class_idx: u32,
    pub access_flags: u32,
    pub superclass_idx: u32,
    pub interfaces_off: u32,
    pub source_file_idx: u32,
    pub annotations_off: u32,
    pub class_data_off: u32,
    pub static_values_off: u32,
}

/// One `encoded_field`.
#[derive(Debug, Clone)]
pub struct EncodedField {
    pub field_idx: u32,
    pub access_flags: u32,
}

/// One `encoded_method`.
#[derive(Debug, Clone)]
pub struct EncodedMethod {
    pub method_idx: u32,
    pub access_flags: u32,
    pub code_off: u32,
}

/// One `class_data_item`, fully expanded (index diffs applied).
#[derive(Debug, Clone, Default)]
pub struct ClassData {
    pub static_fields: Vec<EncodedField>,
    pub instance_fields: Vec<EncodedField>,
    pub direct_methods: Vec<EncodedMethod>,
    pub virtual_methods: Vec<EncodedMethod>,
}

/// A parsed DEX image.
///
/// Id tables and class defs are parsed eagerly; code items, class data and
/// annotations are materialized on demand. All accessors degrade to
/// sentinels/`None` on out-of-range indices instead of panicking.
pub struct DexFile {
    /// The raw image. `None` after `release_data`/`mark_released` —
    /// retired images drop their inflated bytes (the full-decompile
    /// pipeline retires a dex once every class in it has been emitted;
    /// on weibo/lark that is ~360MB of images that used to stay resident
    /// for the whole run).
    data: std::sync::RwLock<Option<Vec<u8>>>,
    /// Set by `mark_released` from a shared (&) reference — the actual
    /// bytes drop immediately through the mutex.
    released: std::sync::atomic::AtomicBool,
    /// MUTF-8 string table as shared `Arc<str>` slices: pool-side
    /// structures (PoolMethod names, resolved names) clone the Arc
    /// instead of copying heap bytes per method — a dex string idx is
    /// already the dedup key.
    strings: Vec<std::sync::Arc<str>>,
    /// Type ids: descriptor string indices.
    types: Vec<u32>,
    protos: Vec<ProtoId>,
    /// Parameter type-id lists per proto, resolved once at parse: the
    /// invoke decode path used to re-read each `type_list` from the
    /// image on EVERY call (`read_type_list` hit the `raw()` mutex per
    /// u16 — a full lock cycle per parameter, per invoke, per method).
    proto_tys: Vec<Box<[u32]>>,
    /// Method-descriptor string per proto (`(ILjava/lang/String;)V`),
    /// formatted once at parse: pool-side PoolMethod descs clone the Arc
    /// instead of re-joining per method (a dex's proto table is the
    /// descriptor dedup layer — methods share protos heavily).
    proto_descs: Vec<std::sync::Arc<str>>,
    fields: Vec<FieldId>,
    methods: Vec<MethodId>,
    pub class_defs: Vec<ClassDef>,
    pub version: String,
    /// class_idx → index into class_defs.
    by_type: HashMap<u32, usize>,
    /// `method_handle_item`s (DEX 037+): (kind, field/method id).
    method_handles: Vec<MethodHandleItem>,
    /// `call_site_id_item`s (DEX 037+), resolved.
    call_sites: Vec<CallSiteInfo>,
}

/// One `method_handle_item`.
#[derive(Debug, Clone)]
pub struct MethodHandleItem {
    /// 0-3: field static-put/static-get/instance-put/instance-get;
    /// 4: invoke-static; 5: invoke-instance; 6: invoke-constructor;
    /// 7: invoke-direct; 8: invoke-interface.
    pub kind: u16,
    /// Field or method id (widened from u2).
    pub target_id: u32,
    pub is_field: bool,
}

/// A resolved `call_site_id_item`: the `call_site_off` encoded array.
#[derive(Debug, Clone)]
pub struct CallSiteInfo {
    /// Bootstrap method handle index.
    pub bootstrap_handle: u32,
    /// Call-site method name (string index).
    pub name_idx: u32,
    /// Call-site method type (proto index).
    pub proto_idx: u32,
    /// Linker arguments (raw encoded values).
    pub linker_args: Vec<crate::annotations::EncodedValue>,
}

impl Drop for DexFile {
    fn drop(&mut self) {
        *self.data.write().unwrap() = None;
    }
}

impl DexFile {
    pub fn parse(data: Vec<u8>) -> Result<DexFile, String> {
        if data.len() < crate::DEX_HEADER_SIZE {
            return Err("file too small for DEX header".into());
        }
        let magic = &data[..8];
        if &magic[..4] != b"dex\n" || magic[7] != 0 {
            return Err(format!("not a DEX file: magic {:02x?}", magic));
        }
        let version = String::from_utf8_lossy(&magic[4..7]).into_owned();
        // 036 was an unofficial odex-era marker; ART accepts 035, 037-041.
        if !matches!(
            version.as_str(),
            "035" | "037" | "038" | "039" | "040" | "041"
        ) {
            return Err(format!("unsupported DEX version {}", version));
        }
        let endian = u32::from_le_bytes(data[40..44].try_into().unwrap());
        if endian != 0x1234_5678 {
            return Err(format!("unexpected endian tag {endian:#x}"));
        }

        let u32at = |off: usize| -> u32 {
            if off + 4 > data.len() {
                return 0;
            }
            u32::from_le_bytes(data[off..off + 4].try_into().unwrap())
        };
        let string_ids_size = u32at(0x38) as usize;
        let string_ids_off = u32at(0x3c) as usize;
        let type_ids_size = u32at(0x40) as usize;
        let type_ids_off = u32at(0x44) as usize;
        let proto_ids_size = u32at(0x48) as usize;
        let proto_ids_off = u32at(0x4c) as usize;
        let field_ids_size = u32at(0x50) as usize;
        let field_ids_off = u32at(0x54) as usize;
        let method_ids_size = u32at(0x58) as usize;
        let method_ids_off = u32at(0x5c) as usize;
        let class_defs_size = u32at(0x60) as usize;
        let class_defs_off = u32at(0x64) as usize;

        // Strings: MUTF-8 with uleb length.
        let mut strings = Vec::with_capacity(string_ids_size);
        if string_ids_off + 4 * string_ids_size <= data.len() {
            for i in 0..string_ids_size {
                let so = u32at(string_ids_off + 4 * i) as usize;
                let mut c = Cursor::at(&data, so);
                let s = c
                    .read_uleb128()
                    .and_then(|len| c.read_mutf8(c.pos, len))
                    .unwrap_or_default();
                strings.push(std::sync::Arc::from(s));
            }
        }

        // Type ids: u4 descriptor string indices.
        let mut types = Vec::with_capacity(type_ids_size);
        if type_ids_off + 4 * type_ids_size <= data.len() {
            for i in 0..type_ids_size {
                types.push(u32at(type_ids_off + 4 * i));
            }
        }

        // Proto ids: 12 bytes each.
        let mut protos = Vec::with_capacity(proto_ids_size);
        if proto_ids_off + 12 * proto_ids_size <= data.len() {
            for i in 0..proto_ids_size {
                let base = proto_ids_off + 12 * i;
                protos.push(ProtoId {
                    shorty_idx: u32at(base),
                    return_type_idx: {
                        let v = u16::from_le_bytes(data[base + 4..base + 6].try_into().unwrap());
                        v as u32
                    },
                    parameters_off: u32at(base + 8),
                });
            }
        }
        // Resolve every proto's parameter list once, straight off the
        // local image slice (no `raw()` mutex on this path at all).
        let proto_tys: Vec<Box<[u32]>> = protos
            .iter()
            .map(|p| read_type_list_at(&data, p.parameters_off as usize))
            .collect();
        // Descriptor per proto, straight off the local tables.
        let ty_name = |t: u32| -> &str {
            match types.get(t as usize) {
                Some(&si) => strings.get(si as usize).map(|s| &**s).unwrap_or(""),
                None => "",
            }
        };
        let proto_descs: Vec<std::sync::Arc<str>> = protos
            .iter()
            .zip(&proto_tys)
            .map(|(p, tys)| {
                let ret = ty_name(p.return_type_idx);
                let mut d = String::with_capacity(2 + ret.len() + tys.len() * 12);
                d.push('(');
                for &t in tys.iter() {
                    d.push_str(ty_name(t));
                }
                d.push(')');
                d.push_str(ret);
                std::sync::Arc::from(d)
            })
            .collect();

        // Field ids: 8 bytes each.
        let mut fields = Vec::with_capacity(field_ids_size);
        if field_ids_off + 8 * field_ids_size <= data.len() {
            for i in 0..field_ids_size {
                let base = field_ids_off + 8 * i;
                fields.push(FieldId {
                    class_idx: u16::from_le_bytes(data[base..base + 2].try_into().unwrap()) as u32,
                    type_idx: u16::from_le_bytes(data[base + 2..base + 4].try_into().unwrap())
                        as u32,
                    name_idx: u32at(base + 4),
                });
            }
        }

        // Method ids: 8 bytes each.
        let mut methods = Vec::with_capacity(method_ids_size);
        if method_ids_off + 8 * method_ids_size <= data.len() {
            for i in 0..method_ids_size {
                let base = method_ids_off + 8 * i;
                methods.push(MethodId {
                    class_idx: u16::from_le_bytes(data[base..base + 2].try_into().unwrap()) as u32,
                    proto_idx: u16::from_le_bytes(data[base + 2..base + 4].try_into().unwrap())
                        as u32,
                    name_idx: u32at(base + 4),
                });
            }
        }

        // Class defs: 32 bytes each.
        let mut class_defs = Vec::with_capacity(class_defs_size);
        if class_defs_off + 32 * class_defs_size <= data.len() {
            for i in 0..class_defs_size {
                let base = class_defs_off + 32 * i;
                class_defs.push(ClassDef {
                    class_idx: u32at(base),
                    access_flags: u32at(base + 4),
                    superclass_idx: u32at(base + 8),
                    interfaces_off: u32at(base + 12),
                    source_file_idx: u32at(base + 16),
                    annotations_off: u32at(base + 20),
                    class_data_off: u32at(base + 24),
                    static_values_off: u32at(base + 28),
                });
            }
        }

        let mut by_type = HashMap::with_capacity(class_defs.len());
        for (i, cd) in class_defs.iter().enumerate() {
            by_type.insert(cd.class_idx, i);
        }

        // DEX 037+ tables (call sites / method handles) live ONLY in the
        // map list — the header carries no offsets for them.
        let (method_handles, call_sites) = parse_map_tables(&data);

        Ok(DexFile {
            data: std::sync::RwLock::new(Some(data)),
            released: std::sync::atomic::AtomicBool::new(false),
            strings,
            types,
            protos,
            proto_tys,
            proto_descs,
            fields,
            methods,
            class_defs,
            version,
            by_type,
            method_handles,
            call_sites,
        })
    }

    /// The raw image, or empty once retired.
    ///
    /// SAFETY of the lifetime: the bytes live in an `Arc<Vec<u8>>` whose
    /// clone is moved into a self-owned stash (`raw_keep`) that outlives
    /// every later call — each call swaps the stash, keeping the previous
    /// slice alive only for the previous caller, which is unsound in
    /// general. The callers here hold the slice only inside one method
    /// (`code_at` / parse-time table walks) and never across calls, and
    /// single-call scope is the documented contract; the alternative
    /// (returning a guard) would infect every parser signature.
    pub fn raw(&self) -> &[u8] {
        // RwLock read: `raw()` ran once PER CLASS through an exclusive
        // mutex while 18 materialize threads hammered the same per-image
        // lock (3.9% of weixin's CPU in mutexwait/mutexdrop). Readers
        // never exclude each other; only retirement takes the write lock.
        let guard = self.data.read().unwrap();
        match guard.as_ref() {
            Some(bytes) => unsafe { std::mem::transmute::<&[u8], &[u8]>(bytes.as_slice()) },
            None => &[],
        }
    }

    /// Drop the inflated image (tables stay usable; code accessors return
    /// empty). Call only when no further code decoding will happen.
    pub fn release_data(&mut self) {
        *self.data.write().unwrap() = None;
        self.released
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Mark the image retired through a shared reference: code accessors
    /// go empty immediately; the bytes themselves drop when the last Arc
    /// snapshot drops (workers hold snapshots only mid-class, so the
    /// memory is reclaimed as chunks complete).
    pub fn mark_released(&self) {
        self.released
            .store(true, std::sync::atomic::Ordering::Release);
        // Free the bytes NOW (through the write lock, &self-safe).
        *self.data.write().unwrap() = None;
    }

    #[inline]
    #[allow(dead_code)] // debug hook for retirement postmortems
    fn is_released(&self) -> bool {
        self.released.load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn string(&self, idx: u32) -> &str {
        self.strings
            .get(idx as usize)
            .map(|s| &**s)
            .unwrap_or("")
    }

    pub fn string_count(&self) -> usize {
        self.strings.len()
    }

    /// Type id → CLASS internal name (`java/lang/String`), descriptor
    /// stripped; array descriptors pass through unchanged.
    pub fn class_name(&self, idx: u32) -> String {
        let d = self.type_name(idx);
        if d.len() > 2 && d.starts_with('L') && d.ends_with(';') {
            d[1..d.len() - 1].to_string()
        } else {
            d.to_string()
        }
    }

    /// Type id → descriptor string (`Ljava/lang/String;`, `[I`, ...).
    /// Number of entries in the type-id table (bounds for `type_name`).
    pub fn num_types(&self) -> usize {
        self.types.len()
    }

    pub fn type_name(&self, idx: u32) -> &str {
        match self.types.get(idx as usize) {
            Some(&si) => self.string(si),
            None => "",
        }
    }

    /// Type descriptor as a shared clone (the pool's field descriptors —
    /// the string table IS the dedup layer).
    pub fn type_name_arc(&self, idx: u32) -> std::sync::Arc<str> {
        match self.types.get(idx as usize) {
            Some(&si) => self.string_arc(si),
            None => std::sync::Arc::from(""),
        }
    }

    pub fn type_count(&self) -> usize {
        self.types.len()
    }

    pub fn proto_count(&self) -> usize {
        self.protos.len()
    }

    pub fn proto(&self, idx: u32) -> &ProtoId {
        static EMPTY: std::sync::OnceLock<ProtoId> = std::sync::OnceLock::new();
        self.protos.get(idx as usize).unwrap_or_else(|| {
            EMPTY.get_or_init(|| ProtoId {
                shorty_idx: 0,
                return_type_idx: 0,
                parameters_off: 0,
            })
        })
    }

    /// Parameter type ids of a proto (empty for `(V)`).
    /// Shared clone of a table string (the pool's method names).
    pub fn string_arc(&self, idx: u32) -> std::sync::Arc<str> {
        self.strings
            .get(idx as usize)
            .cloned()
            .unwrap_or_else(|| std::sync::Arc::from(""))
    }

    /// Method descriptor of a proto as a shared clone.
    pub fn proto_desc(&self, idx: u32) -> std::sync::Arc<str> {
        self.proto_descs
            .get(idx as usize)
            .cloned()
            .unwrap_or_else(|| std::sync::Arc::from("()V"))
    }

    /// Parameter type ids of a proto, resolved once at parse (shared
    /// slice; the old per-call `type_list` re-read is gone).
    pub fn proto_params(&self, idx: u32) -> &[u32] {
        self.proto_tys
            .get(idx as usize)
            .map(|b| &**b)
            .unwrap_or(&[])
    }

    /// `type_list` at `off` (interface lists): one `raw()` snapshot for
    /// the whole walk — the shape below used to lock per u16 read.
    fn read_type_list(&self, off: u32) -> Option<Vec<u32>> {
        let raw = self.raw();
        Some(read_type_list_at(raw, off as usize).into())
    }

    pub fn field(&self, idx: u32) -> &FieldId {
        static EMPTY: std::sync::OnceLock<FieldId> = std::sync::OnceLock::new();
        self.fields.get(idx as usize).unwrap_or_else(|| {
            EMPTY.get_or_init(|| FieldId {
                class_idx: 0,
                type_idx: 0,
                name_idx: 0,
            })
        })
    }

    pub fn field_count(&self) -> usize {
        self.fields.len()
    }

    pub fn method(&self, idx: u32) -> &MethodId {
        static EMPTY: std::sync::OnceLock<MethodId> = std::sync::OnceLock::new();
        self.methods.get(idx as usize).unwrap_or_else(|| {
            EMPTY.get_or_init(|| MethodId {
                class_idx: 0,
                proto_idx: 0,
                name_idx: 0,
            })
        })
    }

    pub fn method_count(&self) -> usize {
        self.methods.len()
    }

    /// Class def index for a type descriptor, when this DEX defines it.
    pub fn class_def_of(&self, type_idx: u32) -> Option<usize> {
        self.by_type.get(&type_idx).copied()
    }

    /// Interfaces of a class def as type ids.
    pub fn interfaces_of(&self, cd: &ClassDef) -> Vec<u32> {
        if cd.interfaces_off == 0 {
            return Vec::new();
        }
        self.read_type_list(cd.interfaces_off).unwrap_or_default()
    }

    pub fn class_data(&self, cd: &ClassDef) -> ClassData {
        if cd.class_data_off == 0 {
            return ClassData::default();
        }
        self.read_class_data(cd.class_data_off).unwrap_or_default()
    }

    fn read_class_data(&self, off: u32) -> Option<ClassData> {
        let mut c = Cursor::at(self.raw(), off as usize);
        let sf = c.read_uleb128()? as usize;
        let inf = c.read_uleb128()? as usize;
        let dm = c.read_uleb128()? as usize;
        let vm = c.read_uleb128()? as usize;

        let read_fields = |n: usize, cur: &mut Cursor| -> Option<Vec<EncodedField>> {
            let mut out = Vec::with_capacity(n);
            let mut idx: u64 = 0;
            for _ in 0..n {
                idx += cur.read_uleb128()?;
                out.push(EncodedField {
                    field_idx: idx as u32,
                    access_flags: cur.read_uleb128()? as u32,
                });
            }
            Some(out)
        };
        let static_fields = read_fields(sf, &mut c)?;
        let instance_fields = read_fields(inf, &mut c)?;

        let read_methods = |n: usize, cur: &mut Cursor| -> Option<Vec<EncodedMethod>> {
            let mut out = Vec::with_capacity(n);
            let mut idx: u64 = 0;
            for _ in 0..n {
                idx += cur.read_uleb128()?;
                out.push(EncodedMethod {
                    method_idx: idx as u32,
                    access_flags: cur.read_uleb128()? as u32,
                    code_off: cur.read_uleb128()? as u32,
                });
            }
            Some(out)
        };
        let direct_methods = read_methods(dm, &mut c)?;
        let virtual_methods = read_methods(vm, &mut c)?;

        Some(ClassData {
            static_fields,
            instance_fields,
            direct_methods,
            virtual_methods,
        })
    }

    /// (registers_size, insns_size) read straight from the code_item header
    /// — no instruction decoding. Hot path for risk checks.
    pub fn code_stats(&self, off: u32) -> Option<(u16, u32)> {
        if off == 0 {
            return None;
        }
        let o = off as usize;
        // One snapshot: each raw() takes the RwLock read side (this ran
        // 5 locks per call on per-method hot paths).
        let d = self.raw();
        if o + 16 > d.len() {
            return None;
        }
        let regs = u16::from_le_bytes([d[o], d[o + 1]]);
        let insns = u32::from_le_bytes([d[o + 12], d[o + 13], d[o + 14], d[o + 15]]);
        Some((regs, insns))
    }

    /// debug_info_off straight from the code_item header (pool building).
    pub fn debug_info_off_at(&self, off: u32) -> Option<u32> {
        if off == 0 {
            return None;
        }
        let o = off as usize;
        let d = self.raw();
        if o + 12 > d.len() {
            return None;
        }
        Some(u32::from_le_bytes([d[o + 8], d[o + 9], d[o + 10], d[o + 11]]))
    }

    pub fn code_at(&self, off: u32) -> Option<CodeItem> {
        if off == 0 {
            return None;
        }
        CodeItem::parse(self.raw(), off as usize)
    }

    /// Class descriptors straight from an inflated image, WITHOUT a full
    /// `DexFile::parse` (which decodes the whole string table — the class
    /// names are a small slice of it). Class listing pays for exactly what
    /// it prints.
    pub fn class_names_from_image(image: &[u8]) -> Vec<String> {
        if image.len() < 0x70 || &image[..4] != b"dex\n" {
            return Vec::new();
        }
        let u4 = |o: usize| -> u32 {
            u32::from_le_bytes([image[o], image[o + 1], image[o + 2], image[o + 3]])
        };
        let strings_size = u4(0x38) as usize;
        let strings_off = u4(0x3c) as usize;
        let types_size = u4(0x40) as usize;
        let types_off = u4(0x44) as usize;
        let classes_size = u4(0x60) as usize;
        let classes_off = u4(0x64) as usize;
        if strings_off + 4 * strings_size > image.len()
            || types_off + 4 * types_size > image.len()
            || classes_off + 32 * classes_size > image.len()
        {
            return Vec::new();
        }
        let u4at = |o: usize| -> u32 {
            u32::from_le_bytes([image[o], image[o + 1], image[o + 2], image[o + 3]])
        };
        let string_at = |idx: u32| -> Option<String> {
            let so = u4at(strings_off + 4 * idx as usize) as usize;
            if so >= image.len() {
                return None;
            }
            // mutf8 string_data_item: uleb len (utf16 units), bytes, NUL.
            let mut p = so;
            // uleb128
            let mut len = 0u64;
            let mut shift = 0;
            loop {
                if p >= image.len() {
                    return None;
                }
                let b = image[p];
                p += 1;
                len |= ((b & 0x7f) as u64) << shift;
                shift += 7;
                if b & 0x80 == 0 {
                    break;
                }
            }
            let end = image[p..].iter().position(|&b| b == 0)? + p;
            crate::reader::mutf8_decode(&image[p..end], len)
        };
        let mut out = Vec::with_capacity(classes_size);
        for i in 0..classes_size {
            let cd = classes_off + 32 * i;
            let class_idx = u4at(cd) as usize;
            if class_idx >= types_size {
                continue;
            }
            let string_idx = u4at(types_off + 4 * class_idx);
            if let Some(name) = string_at(string_idx) {
                // type strings are descriptors (Lcom/foo/Bar;): strip the
                // L; shell to match DexFile::class_name's plain form.
                let plain = name
                    .strip_prefix('L')
                    .and_then(|s| s.strip_suffix(';'))
                    .map(str::to_string)
                    .unwrap_or(name);
                out.push(plain);
            }
        }
        out
    }

    /// The raw insns byte section of one code item (zero-copy) for the
    /// boundary-walking scan path — skips try tables and full decode.
    pub fn code_insns_bytes_at(&self, off: u32) -> Option<&[u8]> {
        if off == 0 {
            return None;
        }
        let off = off as usize;
        let d = self.raw();
        if off + 16 > d.len() {
            return None;
        }
        // code_item: 4×u2 header, u4 debug_info_off, u4 insns_size(units).
        let insns_size =
            u32::from_le_bytes([d[off + 12], d[off + 13], d[off + 14], d[off + 15]]) as usize;
        let start = off + 16;
        let end = start + 2 * insns_size;
        if end > d.len() {
            return None;
        }
        Some(&d[start..end])
    }

    /// Static field initial values (aligned with `static_fields` order).
    pub fn static_values(&self, off: u32) -> Vec<EncodedValue> {
        if off == 0 {
            return Vec::new();
        }
        annotations::read_encoded_array(self.raw(), off as usize).unwrap_or_default()
    }

    /// `method_handle_item` by index.
    pub fn method_handle(&self, idx: u32) -> Option<&MethodHandleItem> {
        self.method_handles.get(idx as usize)
    }

    pub fn method_handle_count(&self) -> usize {
        self.method_handles.len()
    }

    /// Resolved call site by index.
    pub fn call_site(&self, idx: u32) -> Option<&CallSiteInfo> {
        self.call_sites.get(idx as usize)
    }

    pub fn call_site_count(&self) -> usize {
        self.call_sites.len()
    }

    /// Parameter names from debug info (NO_INDEX → absent). `debug_info_off`
    /// comes from a code item; the entry layout is `line_start` uleb,
    /// `parameters_size` uleb, then `uleb128p1` names.
    /// Local-variable table of a `debug_info_item` (DBG_START_LOCAL /
    /// END / RESTART ranges). Release APKs strip it (empty result, zero
    /// cost); debug-enabled builds recover the ORIGINAL source names
    /// (`bArr`, `i3`, …) instead of synthetic `vN` registers.
    pub fn debug_locals(&self, debug_info_off: u32) -> Vec<DebugLocal> {
        if debug_info_off == 0 {
            return Vec::new();
        }
        let raw = self.raw();
        let mut c = Cursor::at(raw, debug_info_off as usize);
        if c.read_uleb128().is_none() {
            return Vec::new();
        }
        let n = match c.read_uleb128() {
            Some(v) => v as usize,
            None => return Vec::new(),
        };
        // Skip the parameter-name indices.
        for _ in 0..n {
            if c.read_uleb128p1().is_none() {
                return Vec::new();
            }
        }
        let mut out: Vec<DebugLocal> = Vec::new();
        // Open ranges per register + the last closed name per register
        // (DBG_RESTART_LOCAL resumes it). Small linear tables: a method
        // touches a handful of registers at any address.
        // (register, start pc, name, optional type descriptor)
        type OpenLocal = (u16, u32, std::sync::Arc<str>, Option<std::sync::Arc<str>>);
        let mut open: Vec<OpenLocal> = Vec::new();
        let mut last: Vec<(u16, std::sync::Arc<str>, Option<std::sync::Arc<str>>)> = Vec::new();
        let mut addr: u32 = 0;
        // Corrupt-stream guard: the bytecode is variable-length; cap the
        // opcode count far above any real method (tolerant-sentinel rule).
        for _ in 0..2_000_000u32 {
            let Some(op) = c.u1() else { break };
            match op {
                0x00 => break, // DBG_END_SEQUENCE
                0x01 => {
                    // DBG_ADVANCE_PC
                    let Some(d) = c.read_uleb128() else { break };
                    addr = addr.saturating_add(d as u32);
                }
                0x02 => {
                    // DBG_ADVANCE_LINE (line numbers are not recovered)
                    if c.read_sleb128().is_none() {
                        break;
                    }
                }
                0x03 | 0x04 => {
                    // DBG_START_LOCAL[_EXTENDED]
                    let (Some(reg), Some(ni)) = (c.read_uleb128(), c.read_uleb128p1()) else {
                        break;
                    };
                    let ti = match c.read_uleb128p1() {
                        Some(v) => v,
                        None => break,
                    };
                    if op == 0x04 && c.read_uleb128p1().is_none() {
                        break; // signature_idx
                    }
                    // read_uleb128p1 ALREADY applies the -1 (NO_INDEX =
                    // -1); decrementing again shifted every name one
                    // string-table slot down (pm→pi, name→line).
                    if ni >= 0 {
                        let reg = reg as u16;
                        if let Some(pos) = open.iter().position(|(r, ..)| *r == reg) {
                            let (_, start, name, ty) = open.remove(pos);
                            out.push(DebugLocal { reg, start, end: addr, name, ty });
                        }
                        let name = self.string_arc(ni as u32);
                        let ty = if ti >= 0 {
                            Some(std::sync::Arc::from(self.type_name(ti as u32)))
                        } else {
                            None
                        };
                        if let Some(pos) = last.iter().position(|(r, ..)| *r == reg) {
                            last[pos] = (reg, name.clone(), ty.clone());
                        } else {
                            last.push((reg, name.clone(), ty.clone()));
                        }
                        open.push((reg, addr, name, ty));
                    }
                }
                0x05 => {
                    // DBG_END_LOCAL
                    let Some(reg) = c.read_uleb128() else { break };
                    let reg = reg as u16;
                    if let Some(pos) = open.iter().position(|(r, ..)| *r == reg) {
                        let (_, start, name, ty) = open.remove(pos);
                        out.push(DebugLocal { reg, start, end: addr, name, ty });
                    }
                }
                0x06 => {
                    // DBG_RESTART_LOCAL
                    let Some(reg) = c.read_uleb128() else { break };
                    let reg = reg as u16;
                    if let Some((_, name, ty)) = last.iter().find(|(r, ..)| *r == reg) {
                        let name = name.clone();
                        let ty = ty.clone();
                        if !open.iter().any(|(r, ..)| *r == reg) {
                            open.push((reg, addr, name, ty));
                        }
                    }
                }
                0x07 | 0x08 => {} // prologue-end / epilogue-begin
                0x09 => {
                    // DBG_SET_FILE
                    if c.read_uleb128p1().is_none() {
                        break;
                    }
                }
                // Special opcodes (>= 0x0a): DWARF-style fused
                // (address, line) advance in ONE byte — addr +=
                // (op - 0x0a) / 15, line += -4 + (op - 0x0a) % 15.
                // Line numbers are not recovered; the address advance
                // positions the local ranges. Treating these as invalid
                // (an earlier shape broke here) lost EVERY local after
                // the first special opcode — i.e. all of them.
                c => {
                    let adjusted = (c - 0x0a) as u32;
                    addr = addr.saturating_add(adjusted / 15);
                }
            }
        }
        for (reg, start, name, ty) in open {
            out.push(DebugLocal {
                reg,
                start,
                end: u32::MAX,
                name,
                ty,
            });
        }
        out
    }

    pub fn parameter_names(&self, debug_info_off: u32) -> Vec<Option<String>> {
        if debug_info_off == 0 {
            return Vec::new();
        }
        let mut c = Cursor::at(self.raw(), debug_info_off as usize);
        let _line_start = match c.read_uleb128() {
            Some(v) => v,
            None => return Vec::new(),
        };
        let n = match c.read_uleb128() {
            Some(v) => v as usize,
            None => return Vec::new(),
        };
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let idx = c.read_uleb128p1().unwrap_or(-1);
            if idx < 0 {
                out.push(None);
            } else {
                out.push(Some(self.string(idx as u32).to_string()));
            }
        }
        out
    }
}

impl std::fmt::Debug for DexFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DexFile")
            .field("version", &self.version)
            .field("strings", &self.strings.len())
            .field("types", &self.types.len())
            .field("protos", &self.protos.len())
            .field("fields", &self.fields.len())
            .field("methods", &self.methods.len())
            .field("class_defs", &self.class_defs.len())
            .finish()
    }
}

/// `type_list` at `off` straight off a byte slice: parse-time proto
/// resolution and the interfaces read both go through here — one
/// snapshot of the slice, no per-element locking. Reads past the end
/// are truncated (the tolerant-sentinel rule for corrupt images).
fn read_type_list_at(data: &[u8], off: usize) -> Box<[u32]> {
    if off == 0 || off + 4 > data.len() {
        return Vec::new().into_boxed_slice();
    }
    let size = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
    let mut p = off + 4;
    let mut out = Vec::with_capacity(size);
    for _ in 0..size {
        if p + 2 > data.len() {
            break;
        }
        out.push(u16::from_le_bytes(data[p..p + 2].try_into().unwrap()) as u32);
        p += 2;
    }
    out.into_boxed_slice()
}

/// Sections 0x0007 (call_site_id) and 0x0008 (method_handle_id) from the
/// map list. Layouts (dx `ItemType`/`MethodHandleItem`):
/// * `call_site_id_item`: u4 offset → `call_site_off` encoded array
///   [bootstrap METHOD_HANDLE, name STRING, type METHOD_TYPE, linker args…].
/// * `method_handle_item`: u2 kind, u2 reserved, u2 target id, u2 reserved.
fn parse_map_tables(data: &[u8]) -> (Vec<MethodHandleItem>, Vec<CallSiteInfo>) {
    let mut handles = Vec::new();
    let mut sites = Vec::new();
    if data.len() < 0x70 {
        return (handles, sites);
    }
    let map_off = u32le(data, 0x34) as usize;
    if map_off + 4 > data.len() {
        return (handles, sites);
    }
    let n = u32le(data, map_off) as usize;
    let mut p = map_off + 4;
    for _ in 0..n {
        if p + 12 > data.len() {
            break;
        }
        let t = u16le(data, p);
        let size = u32le(data, p + 4) as usize;
        let off = u32le(data, p + 8) as usize;
        p += 12;
        match t {
            0x0007 => {
                // call_site_id_item: u4 call_site_off each.
                for i in 0..size {
                    let q = off + 4 * i;
                    if q + 4 > data.len() {
                        break;
                    }
                    let cs_off = u32le(data, q) as usize;
                    if let Some(info) = parse_call_site(data, cs_off) {
                        sites.push(info);
                    }
                }
            }
            0x0008 => {
                // method_handle_item: (u2 kind, u2 res, u2 id, u2 res).
                for i in 0..size {
                    let q = off + 8 * i;
                    if q + 8 > data.len() {
                        break;
                    }
                    let kind = u16le(data, q);
                    let id = u16le(data, q + 4);
                    handles.push(MethodHandleItem {
                        kind,
                        target_id: id as u32,
                        is_field: kind <= 3,
                    });
                }
            }
            _ => {}
        }
    }
    (handles, sites)
}

fn parse_call_site(data: &[u8], off: usize) -> Option<CallSiteInfo> {
    let vals = crate::annotations::read_encoded_array(data, off)?;
    if vals.len() < 3 {
        return None;
    }
    let bootstrap_handle = match &vals[0] {
        crate::annotations::EncodedValue::MethodHandle(h) => *h,
        _ => return None,
    };
    let name_idx = match &vals[1] {
        crate::annotations::EncodedValue::String(s) => *s,
        _ => return None,
    };
    let proto_idx = match &vals[2] {
        crate::annotations::EncodedValue::MethodType(p) => *p,
        _ => return None,
    };
    Some(CallSiteInfo {
        bootstrap_handle,
        name_idx,
        proto_idx,
        linker_args: vals[3..].to_vec(),
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_dex() {
        assert!(DexFile::parse(vec![0u8; 128]).is_err());
    }
}


/// One source-level local from the debug-info local table: the name is
/// valid for `reg` over `[start, end)` in code units.
#[derive(Debug, Clone)]
pub struct DebugLocal {
    pub reg: u16,
    pub start: u32,
    pub end: u32,
    pub name: std::sync::Arc<str>,
    /// Type descriptor from the local table (`[C`, `Ljava/lang/String;`),
    /// when present — lets the consumer reject a range whose type does
    /// not match the value being named (register reuse: `new-array v0,
    // v0` reads the OLD v0 as a size inside the NEW v0's range).
    pub ty: Option<std::sync::Arc<str>>,
}
