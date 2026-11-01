//! Cross-reference search over DEX images WITHOUT decompiling.
//!
//! The rasc playbook, ported: the image is scanned DIRECTLY (zero-copy
//! header/table-range validation only — no DexFile::parse, no materialized
//! tables, no decoded string table); targets resolve in BYTE space with
//! SIMD substring search (memchr) over the raw MUTF-8; a raw-code prefilter
//! (memmem for the encoded index bytes) skips whole methods when the query
//! has few targets; per-method names decode lazily, only for hits. Matching
//! is case-sensitive substring (literal), like grep.

use std::collections::BTreeSet;

use anyhow::{bail, Result};
use memchr::{memchr, memmem};

use ddc_dex::insn::scan_instructions;

/// What the user is looking for.
#[derive(Debug, Clone)]
pub enum FindQuery {
    /// Substring match (literal, case-sensitive) against string literals.
    String(String),
    /// Substring match against type names (`com.poc.Main`,
    /// `com/poc/Main`, `Lcom/poc/Main;` all normalize).
    Type(String),
    Method {
        class: Option<String>,
        name: String,
        fuzzy_class: bool,
    },
    Field {
        class: Option<String>,
        name: String,
        fuzzy_class: bool,
    },
}

impl FindQuery {
    pub fn kind(&self) -> &'static str {
        match self {
            FindQuery::String(_) => "string",
            FindQuery::Type(_) => "type",
            FindQuery::Method { .. } => "method",
            FindQuery::Field { .. } => "field",
        }
    }
}

/// One reference hit.
pub struct Hit {
    /// Source dex image label.
    pub dex: String,
    /// Owner class, slashed internal form.
    pub class: String,
    /// Owner method, `name(descriptor)`.
    pub method: String,
    /// Instruction kind of the first hit (`const-string`, `invoke`...).
    pub insn: &'static str,
    /// All matched targets in this method (deduped, first-hit order):
    /// string literals or member descriptors.
    pub targets: Vec<String>,
}

/// The four reference-carrying opcode families, one bit each.
const K_STRING: u8 = 1;
const K_TYPE: u8 = 2;
const K_FIELD: u8 = 4;
const K_METHOD: u8 = 8;

const fn opcode_kinds(op: u8) -> u8 {
    let mut mask = 0;
    if matches!(op, 0x1a | 0x1b) {
        mask |= K_STRING;
    }
    if matches!(op, 0x1c | 0x1f | 0x20 | 0x22..=0x25) {
        mask |= K_TYPE;
    }
    if matches!(op, 0x52..=0x6d) {
        mask |= K_FIELD;
    }
    if matches!(op, 0x6e..=0x72 | 0x74..=0x78) {
        mask |= K_METHOD;
    }
    mask
}

/// Kind mask per opcode — one array load decides whether an instruction can
/// carry the queried reference kind.
const OPCODE_KINDS: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut op = 0usize;
    while op < 256 {
        table[op] = opcode_kinds(op as u8);
        op += 1;
    }
    table
};

/// A raw DEX image view: header fields + validated table ranges, nothing
/// decoded.
pub(crate) struct RawDex<'a> {
    pub(crate) d: &'a [u8],
    pub(crate) str_n: usize,
    pub(crate) str_off: usize,
    pub(crate) type_n: usize,
    pub(crate) type_off: usize,
    pub(crate) proto_off: usize,
    pub(crate) field_n: usize,
    pub(crate) field_off: usize,
    pub(crate) method_n: usize,
    pub(crate) method_off: usize,
    pub(crate) cls_n: usize,
    pub(crate) cls_off: usize,
}

impl<'a> RawDex<'a> {
    pub(crate) fn parse(d: &'a [u8]) -> Result<Self> {
        if d.len() < 0x70 || !d.starts_with(b"dex\n") {
            bail!(crate::lang::bi!("not a DEX image", "不是 DEX 镜像"));
        }
        let u4 = |o: usize| -> usize {
            u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]) as usize
        };
        let dex = RawDex {
            d,
            str_n: u4(0x38),
            str_off: u4(0x3c),
            type_n: u4(0x40),
            type_off: u4(0x44),
            proto_off: u4(0x4c),
            field_n: u4(0x50),
            field_off: u4(0x54),
            method_n: u4(0x58),
            method_off: u4(0x5c),
            cls_n: u4(0x60),
            cls_off: u4(0x64),
        };
        let ok = |off: usize, n: usize, w: usize| off + w * n <= d.len();
        if !ok(dex.str_off, dex.str_n, 4)
            || !ok(dex.type_off, dex.type_n, 4)
            || !ok(dex.field_off, dex.field_n, 8)
            || !ok(dex.method_off, dex.method_n, 8)
            || !ok(dex.cls_off, dex.cls_n, 32)
        {
            bail!(crate::lang::bi!(
                "DEX table ranges out of bounds",
                "DEX 表范围越界"
            ));
        }
        Ok(dex)
    }

    #[inline]
    pub(crate) fn u4(&self, o: usize) -> u32 {
        u32::from_le_bytes([self.d[o], self.d[o + 1], self.d[o + 2], self.d[o + 3]])
    }

    /// Raw MUTF-8 bytes of string `idx` (uleb length skipped).
    pub(crate) fn string_bytes(&self, idx: u32) -> Option<&'a [u8]> {
        // 指令操作数是索引而非保证：畸形/混淆镜像会给出越界值。
        if idx as usize >= self.str_n {
            return None;
        }
        let mut off = self.u4(self.str_off + 4 * idx as usize) as usize;
        if off >= self.d.len() {
            return None;
        }
        loop {
            if off >= self.d.len() {
                return None;
            }
            let b = self.d[off];
            off += 1;
            if b & 0x80 == 0 {
                break;
            }
        }
        let end = memchr(0, &self.d[off..])? + off;
        Some(&self.d[off..end])
    }

    fn string(&self, idx: u32) -> Option<String> {
        Some(decode_mutf8_lossy(self.string_bytes(idx)?))
    }

    /// The raw descriptor bytes of a type (`Lcom/foo/Bar;`).
    pub(crate) fn type_bytes(&self, idx: u32) -> Option<&'a [u8]> {
        if idx as usize >= self.type_n {
            return None;
        }
        let sidx = self.u4(self.type_off + 4 * idx as usize);
        self.string_bytes(sidx)
    }

    /// Class internal name from a type index (descriptor shell stripped).
    pub(crate) fn class_name(&self, type_idx: u32) -> String {
        match self.type_bytes(type_idx) {
            Some(b) => {
                let s = decode_mutf8_lossy(b);
                s.strip_prefix('L')
                    .and_then(|t| t.strip_suffix(';'))
                    .map(str::to_string)
                    .unwrap_or(s)
            }
            None => String::new(),
        }
    }

    /// One class_def row: (class_type_idx, super_type_idx, interfaces_off,
    /// class_data_off, source_file_idx). NO_INDEX = u32::MAX.
    pub(crate) fn class_def_parts(&self, ci: usize) -> Option<(u32, u32, u32, u32, u32)> {
        let o = self.cls_off + 32 * ci;
        if o + 32 > self.d.len() {
            return None;
        }
        let u4 = |p: usize| -> u32 {
            u32::from_le_bytes([self.d[p], self.d[p + 1], self.d[p + 2], self.d[p + 3]])
        };
        // class_def: class_idx, access_flags, superclass_idx, interfaces_off,
        // source_file_idx, annotations_off, class_data_off, static_values_off.
        Some((u4(o), u4(o + 8), u4(o + 12), u4(o + 24), u4(o + 16)))
    }

    /// The type indexes of a class's interfaces (from interfaces_off).
    pub(crate) fn interface_types(&self, interfaces_off: u32) -> Vec<u32> {
        if interfaces_off == 0 || interfaces_off as usize + 4 > self.d.len() {
            return Vec::new();
        }
        let p = interfaces_off as usize;
        let n =
            u32::from_le_bytes([self.d[p], self.d[p + 1], self.d[p + 2], self.d[p + 3]]) as usize;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let q = p + 4 + 2 * i;
            if q + 2 > self.d.len() {
                break;
            }
            out.push(u16::from_le_bytes([self.d[q], self.d[q + 1]]) as u32);
        }
        out
    }

    /// (class_type_idx, super_type_idx, interfaces_off, class_data_off, source_file_idx)
    /// of a class found by its internal name.
    pub(crate) fn find_class(&self, internal: &str) -> Option<usize> {
        for ci in 0..self.cls_n {
            let (ty, _, _, _, _) = self.class_def_parts(ci)?;
            if self.class_name(ty) == internal {
                return Some(ci);
            }
        }
        None
    }

    /// Iterate the class's methods: (method_idx, access, code_off).
    pub(crate) fn methods_of(&self, class_data_off: usize) -> Option<Vec<(u32, u32, u32)>> {
        if class_data_off == 0 {
            return Some(Vec::new());
        }
        let d = self.d;
        let mut cur = class_data_off;
        let mut uleb = move || -> Option<u32> {
            let mut v: u32 = 0;
            let mut sh = 0;
            loop {
                if cur >= d.len() {
                    return None;
                }
                let b = d[cur];
                cur += 1;
                v |= ((b & 0x7f) as u32) << sh;
                sh += 7;
                if b & 0x80 == 0 {
                    return Some(v);
                }
            }
        };
        let (sf, inf, dm, vm) = (uleb()?, uleb()?, uleb()?, uleb()?);
        for _ in 0..sf + inf {
            uleb()?;
            uleb()?;
        }
        let mut out: Vec<(u32, u32, u32)> = Vec::new();
        // Direct and virtual lists EACH restart the method_idx delta at 0.
        for count in [dm, vm] {
            let mut midx: u32 = 0;
            for _ in 0..count {
                let diff = uleb()?;
                let access = uleb()?;
                let code_off = uleb()?;
                midx = midx.wrapping_add(diff);
                out.push((midx, access, code_off));
            }
        }
        Some(out)
    }

    /// (class_idx, proto_idx, name bytes) of a method id.
    pub(crate) fn method_parts(&self, idx: u32) -> Option<(u32, u32, &'a [u8])> {
        let o = self.method_off + 8 * idx as usize;
        if o + 8 > self.d.len() {
            return None;
        }
        let class_idx = u16::from_le_bytes([self.d[o], self.d[o + 1]]) as u32;
        let proto_idx = u16::from_le_bytes([self.d[o + 2], self.d[o + 3]]) as u32;
        let name_idx = self.u4(o + 4);
        Some((class_idx, proto_idx, self.string_bytes(name_idx)?))
    }

    /// (class_idx, name bytes, type bytes) of a field id
    /// (field_id: class_idx u2, type_idx u2, name_idx u4).
    pub(crate) fn field_parts(&self, idx: u32) -> Option<(u32, &'a [u8], &'a [u8])> {
        let o = self.field_off + 8 * idx as usize;
        if o + 8 > self.d.len() {
            return None;
        }
        let class_idx = u16::from_le_bytes([self.d[o], self.d[o + 1]]) as u32;
        let type_idx = u16::from_le_bytes([self.d[o + 2], self.d[o + 3]]) as u32;
        let name_idx = self.u4(o + 4);
        Some((
            class_idx,
            self.string_bytes(name_idx)?,
            self.type_bytes(type_idx)?,
        ))
    }

    /// `name(params)ret` from a proto id (12-byte proto_id items).
    pub(crate) fn proto_desc(&self, proto_idx: u32) -> String {
        let o = self.proto_off + 12 * proto_idx as usize;
        if o + 12 > self.d.len() {
            return String::new();
        }
        let ret = self.u4(o + 4);
        let params_off = self.u4(o + 8) as usize;
        let mut out = String::from("(");
        if params_off != 0 && params_off + 4 <= self.d.len() {
            let n = u32::from_le_bytes([
                self.d[params_off],
                self.d[params_off + 1],
                self.d[params_off + 2],
                self.d[params_off + 3],
            ]) as usize;
            for i in 0..n {
                let p = params_off + 4 + 2 * i;
                if p + 2 > self.d.len() {
                    break;
                }
                let t = u16::from_le_bytes([self.d[p], self.d[p + 1]]) as u32;
                if let Some(b) = self.type_bytes(t) {
                    out.push_str(&decode_mutf8_lossy(b));
                }
            }
        }
        out.push(')');
        if let Some(b) = self.type_bytes(ret) {
            out.push_str(&decode_mutf8_lossy(b));
        }
        out
    }

    /// String indices whose raw bytes contain `needle` (SIMD memmem).
    pub(crate) fn matching_strings(&self, needle: &[u8]) -> Vec<u32> {
        if needle.is_empty() {
            return Vec::new();
        }
        let finder = memmem::Finder::new(needle);
        let mut out = Vec::new();
        for idx in 0..self.str_n as u32 {
            if let Some(bytes) = self.string_bytes(idx) {
                if finder.find(bytes).is_some() {
                    out.push(idx);
                }
            }
        }
        out
    }

    /// Type indices whose type string contains `needle`.
    fn matching_types(&self, needle: &[u8]) -> Vec<u32> {
        let strs: BTreeSet<u32> = self.matching_strings(needle).into_iter().collect();
        let mut out = Vec::new();
        for idx in 0..self.type_n as u32 {
            let sidx = self.u4(self.type_off + 4 * idx as usize);
            if strs.contains(&sidx) {
                out.push(idx);
            }
        }
        out
    }
}

pub(crate) fn decode_mutf8_lossy(b: &[u8]) -> String {
    // Standard UTF-8 first (covers ASCII and files written by tools that
    // emit plain UTF-8); DEX string data is MUTF-8 (CESU-8), whose
    // supplementary characters are surrogate pairs that strict UTF-8
    // rejects — `from_utf8_lossy` would replace them with U+FFFD.
    match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(_) => ddc_dex::reader::mutf8_decode_lossy(b),
    }
}

/// Normalize any of `com.poc.Main`, `com/poc/Main`, `Lcom/poc/Main;` to
/// the slashed form.
/// Byte needles for a text query: its UTF-8 form and, when they differ,
/// its MUTF-8 form. DEX strings store supplementary characters as
/// surrogate pairs, so a plain UTF-8 needle can never reach them.
pub(crate) fn byte_needles(s: &str) -> Vec<Vec<u8>> {
    let utf8 = s.as_bytes().to_vec();
    let mutf8 = ddc_dex::reader::mutf8_encode(s);
    if mutf8 == utf8 {
        vec![utf8]
    } else {
        vec![utf8, mutf8]
    }
}

/// Does `hay` contain any of the needles as a byte substring?
pub(crate) fn bytes_contain(hay: &[u8], needles: &[Vec<u8>]) -> bool {
    needles.iter().any(|n| memmem::find(hay, n).is_some())
}

/// `bytes_contain` for a single query string (allocates only for
/// non-ASCII queries, i.e. never on the common path).
pub(crate) fn bytes_contain_str(hay: &[u8], s: &str) -> bool {
    if s.is_ascii() {
        return memmem::find(hay, s.as_bytes()).is_some();
    }
    bytes_contain(hay, &byte_needles(s))
}

fn normalize_type(q: &str) -> String {
    let mut s = q.trim();
    if let Some(stripped) = s.strip_prefix('L') {
        if s.ends_with(';') {
            s = &stripped[..stripped.len().saturating_sub(1)];
        }
    }
    s.replace('.', "/")
}

/// Class-side match for method/field queries against a type descriptor.
fn class_matches(type_bytes: &[u8], class: &str, fuzzy: bool) -> bool {
    let hay = decode_mutf8_lossy(type_bytes);
    let hay = hay.trim_start_matches('L').trim_end_matches(';');
    let needle = normalize_type(class);
    if fuzzy {
        hay.contains(&needle)
    } else {
        hay == needle
    }
}

/// Encoded target indices for the raw-code prefilter: when the query has
/// few targets, memmem for the index's own bytes skips whole methods
/// before any opcode walk.
struct Prefilter {
    pairs: Vec<[u8; 2]>,
    quads: Vec<[u8; 4]>,
}

impl Prefilter {
    fn new(targets: &BTreeSet<u32>) -> Option<Self> {
        // Above ~4 targets the filter costs more than the decode it skips.
        if targets.len() > 4 {
            return None;
        }
        let mut pairs = Vec::new();
        let mut quads = Vec::new();
        for &t in targets {
            if t <= u16::MAX as u32 {
                pairs.push((t as u16).to_le_bytes());
            } else {
                quads.push(t.to_le_bytes());
            }
        }
        Some(Prefilter { pairs, quads })
    }

    fn might_hit(&self, code: &[u8]) -> bool {
        self.pairs.iter().any(|p| contains_pair(code, *p))
            || self.quads.iter().any(|q| memmem::find(code, q).is_some())
    }
}

/// Two-byte search: memchr the first byte, check both neighbor positions
/// (a 16-bit operand can start one byte before the found position).
fn contains_pair(code: &[u8], [first, second]: [u8; 2]) -> bool {
    let mut from = 0;
    while let Some(found) = memchr(first, &code[from..]) {
        let at = from + found;
        if code.get(at + 1) == Some(&second) || (at > 0 && code[at - 1] == second) {
            return true;
        }
        from = at + 1;
    }
    false
}

/// Whether the query resolves to ZERO targets on a (possibly partial)
/// image. `None` when the prefix cannot answer yet (incomplete tables or
/// string data beyond its end) — the caller keeps inflating.
pub fn resolve_on_prefix(image: &[u8], query: &FindQuery) -> Option<(u8, BTreeSet<u32>)> {
    let dex = RawDex::parse(image).ok()?;
    // Completeness: every string must sit fully inside the prefix —
    // otherwise the resolution would silently miss targets.
    for idx in 0..dex.str_n as u32 {
        dex.string_bytes(idx)?;
    }
    Some(resolve_targets(&dex, query))
}

/// Scan one inflated DEX image for `query`. `pre` carries targets the
/// producer already resolved on the string-data-complete prefix (single
/// pass: the scanner then skips resolution entirely).
pub fn scan_image(
    label: &str,
    image: &[u8],
    query: &FindQuery,
    pre: Option<(u8, BTreeSet<u32>)>,
) -> Result<Vec<Hit>> {
    let dex_name = label
        .rsplit_once('!')
        .map(|(_, e)| e)
        .unwrap_or(label)
        .to_string();
    let dex = RawDex::parse(image)?;
    let (kind_bit, targets): (u8, BTreeSet<u32>) =
        pre.unwrap_or_else(|| resolve_targets(&dex, query));
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    let prefilter = Prefilter::new(&targets);
    let mut hits = Vec::new();
    for ci in 0..dex.cls_n {
        let cd = dex.cls_off + 32 * ci;
        let class_type_idx = dex.u4(cd);
        let class = dex.class_name(class_type_idx);
        let class_data_off = dex.u4(cd + 24) as usize;
        if class_data_off == 0 {
            continue;
        }
        scan_class(
            &dex,
            &dex_name,
            class_data_off,
            &class,
            kind_bit,
            &targets,
            &prefilter,
            &mut hits,
            false,
            &mut 0,
        );
    }
    Ok(hits)
}

/// Summary-only sibling of [`scan_image`]: counts matched methods per owner
/// class without rendering method names or target descriptors. A broad query
/// (845k hits on qq) otherwise allocates a full Hit for every match.
pub fn scan_image_counts(
    _label: &str,
    image: &[u8],
    query: &FindQuery,
    pre: Option<(u8, BTreeSet<u32>)>,
) -> Result<std::collections::BTreeMap<String, usize>> {
    let dex = RawDex::parse(image)?;
    let (kind_bit, targets): (u8, BTreeSet<u32>) =
        pre.unwrap_or_else(|| resolve_targets(&dex, query));
    let mut out: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    if targets.is_empty() {
        return Ok(out);
    }
    let prefilter = Prefilter::new(&targets);
    let mut no_hits = Vec::new();
    for ci in 0..dex.cls_n {
        let cd = dex.cls_off + 32 * ci;
        let class_type_idx = dex.u4(cd);
        let class = dex.class_name(class_type_idx);
        let class_data_off = dex.u4(cd + 24) as usize;
        if class_data_off == 0 {
            continue;
        }
        let mut count = 0usize;
        scan_class(
            &dex,
            "",
            class_data_off,
            &class,
            kind_bit,
            &targets,
            &prefilter,
            &mut no_hits,
            true,
            &mut count,
        );
        if count > 0 {
            *out.entry(class.replace('/', ".")).or_default() += count;
        }
    }
    Ok(out)
}

/// Target resolution + the opcode-kind bit, split out so the producer's
/// prefix pass can run it once and hand the result to the scanner.
pub fn resolve_targets(dex: &RawDex, query: &FindQuery) -> (u8, BTreeSet<u32>) {
    match query {
        FindQuery::String(p) => (
            K_STRING,
            byte_needles(p)
                .iter()
                .flat_map(|n| dex.matching_strings(n))
                .collect(),
        ),
        FindQuery::Type(p) => (
            K_TYPE,
            byte_needles(&normalize_type(p))
                .iter()
                .flat_map(|n| dex.matching_types(n))
                .collect(),
        ),
        FindQuery::Method {
            class,
            name,
            fuzzy_class,
        } => (
            K_METHOD,
            matching_members(dex, true, name, class.as_deref(), *fuzzy_class),
        ),
        FindQuery::Field {
            class,
            name,
            fuzzy_class,
        } => (
            K_FIELD,
            matching_members(dex, false, name, class.as_deref(), *fuzzy_class),
        ),
    }
}

fn matching_members(
    dex: &RawDex,
    is_method: bool,
    name: &str,
    class: Option<&str>,
    fuzzy_class: bool,
) -> BTreeSet<u32> {
    let mut set = BTreeSet::new();
    let n = if is_method { dex.method_n } else { dex.field_n };
    for idx in 0..n as u32 {
        let parts = if is_method {
            dex.method_parts(idx).map(|(c, _p, nb)| (c, nb))
        } else {
            dex.field_parts(idx).map(|(c, nb, _t)| (c, nb))
        };
        if let Some((class_idx, name_b)) = parts {
            if !bytes_contain_str(name_b, name) {
                continue;
            }
            if let Some(c) = class {
                let cb = match dex.type_bytes(class_idx) {
                    Some(b) => b,
                    None => continue,
                };
                if !class_matches(cb, c, fuzzy_class) {
                    continue;
                }
            }
            set.insert(idx);
        }
    }
    set
}

/// One class's methods, walked from raw class_data bytes.
#[allow(clippy::too_many_arguments)]
fn scan_class(
    dex: &RawDex,
    dex_name: &str,
    class_data_off: usize,
    class: &str,
    kind_bit: u8,
    targets: &BTreeSet<u32>,
    prefilter: &Option<Prefilter>,
    hits: &mut Vec<Hit>,
    count_only: bool,
    count_out: &mut usize,
) {
    let mut cur = class_data_off;
    let d = dex.d;
    let mut uleb = move || -> Option<u32> {
        let mut v: u32 = 0;
        let mut shift = 0;
        loop {
            if cur >= d.len() {
                return None;
            }
            let b = d[cur];
            cur += 1;
            v |= ((b & 0x7f) as u32) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
    };
    let (Some(sf), Some(inf), Some(dm), Some(vm)) = (uleb(), uleb(), uleb(), uleb()) else {
        return;
    };
    for _ in 0..sf + inf {
        uleb();
        uleb();
    }
    for count in [dm, vm] {
        let mut method_idx = 0u32;
        for _ in 0..count {
            let Some(diff) = uleb() else { break };
            method_idx = method_idx.wrapping_add(diff);
            uleb();
            let code_off = uleb().unwrap_or(0) as usize;
            if code_off == 0 {
                continue;
            }
            // code_item header: 4×u2, u4 debug, u4 insns_size.
            if code_off + 16 > dex.d.len() {
                continue;
            }
            let insns_size = dex.u4(code_off + 12) as usize;
            let start = code_off + 16;
            let end = (start + 2 * insns_size).min(dex.d.len());
            if start >= end {
                continue;
            }
            let code = &dex.d[start..end];
            if let Some(pf) = &prefilter {
                if !pf.might_hit(code) {
                    continue;
                }
            }
            // Per-method aggregation: one Hit with every matched
            // target (a method with several hits stays ONE line —
            // the instruction kind is that of the first hit).
            let mut owner: Option<String> = None;
            let mut matched: Vec<String> = Vec::new();
            let mut first_insn: Option<&'static str> = None;
            let mut count_hit = false;
            scan_instructions(code, &mut |op, pc, bytes| {
                if OPCODE_KINDS[op as usize] & kind_bit == 0 {
                    return;
                }
                let idx = if op == 0x1b {
                    (unit_at(bytes, pc + 1) as u32) | ((unit_at(bytes, pc + 2) as u32) << 16)
                } else {
                    unit_at(bytes, pc + 1) as u32
                };
                if !targets.contains(&idx) {
                    return;
                }
                if count_only {
                    count_hit = true;
                    return;
                }
                if owner.is_none() {
                    owner = Some(render_owner(dex, method_idx));
                }
                if first_insn.is_none() {
                    first_insn = Some(kind_name(op));
                }
                let target = match kind_bit {
                    K_STRING => match dex.string(idx) {
                        Some(s) => format!("{s:?}"),
                        None => return,
                    },
                    K_TYPE => match dex.type_bytes(idx) {
                        Some(b) => decode_mutf8_lossy(b),
                        None => return,
                    },
                    K_METHOD => render_member(dex, idx, true),
                    _ => render_member(dex, idx, false),
                };
                if !matched.contains(&target) {
                    matched.push(target);
                }
            });
            if count_only {
                if count_hit {
                    *count_out += 1;
                }
                continue;
            }
            if owner.is_some() {
                hits.push(Hit {
                    dex: dex_name.to_string(),
                    class: class.to_string(),
                    method: owner.unwrap_or_default(),
                    insn: first_insn.unwrap_or("ref"),
                    targets: matched,
                });
            }
        }
    }
}

#[inline]
fn unit_at(bytes: &[u8], i: usize) -> u16 {
    let o = 2 * i;
    if o + 1 < bytes.len() {
        u16::from_le_bytes([bytes[o], bytes[o + 1]])
    } else {
        0
    }
}

fn kind_name(op: u8) -> &'static str {
    match op {
        0x1a => "const-string",
        0x1b => "const-string/jumbo",
        0x1c => "const-class",
        0x1f => "check-cast",
        0x20 => "instance-of",
        0x22 => "new-instance",
        0x23 => "new-array",
        0x24 => "filled-new-array",
        0x25 => "filled-new-array/range",
        0x52..=0x58 => "iget",
        0x59..=0x5f => "iput",
        0x60..=0x66 => "sget",
        0x67..=0x6d => "sput",
        _ => "invoke",
    }
}

fn render_owner(dex: &RawDex, method_idx: u32) -> String {
    match dex.method_parts(method_idx) {
        Some((_cls, proto, name)) => {
            let mut s = decode_mutf8_lossy(name);
            s.push_str(&dex.proto_desc(proto));
            s
        }
        None => String::new(),
    }
}

/// `Lcls;->name(desc)` (method) or `Lcls;->name:type` (field) — full
/// Dalvin descriptor form, ASC parity.
fn render_member(dex: &RawDex, idx: u32, is_method: bool) -> String {
    let plain = |b: Option<&[u8]>| -> String {
        match b {
            Some(bytes) => decode_mutf8_lossy(bytes),
            None => String::new(),
        }
    };
    if is_method {
        match dex.method_parts(idx) {
            Some((class_idx, proto, name)) => {
                let cls = plain(dex.type_bytes(class_idx));
                format!(
                    "{}->{}{}",
                    cls,
                    decode_mutf8_lossy(name),
                    dex.proto_desc(proto)
                )
            }
            None => String::new(),
        }
    } else {
        match dex.field_parts(idx) {
            Some((class_idx, name, ty)) => format!(
                "{}->{}:{}",
                plain(dex.type_bytes(class_idx)),
                decode_mutf8_lossy(name),
                decode_mutf8_lossy(ty)
            ),
            None => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_needles_cover_supplementary_forms() {
        // BMP-only queries have a single encoding (UTF-8 and MUTF-8 agree).
        assert_eq!(byte_needles("abc"), vec![b"abc".to_vec()]);
        assert_eq!(byte_needles("中文"), vec!["中文".as_bytes().to_vec()]);
        // Supplementary characters differ: a UTF-8 needle cannot match the
        // surrogate-pair bytes stored in the dex string table.
        let n = byte_needles("a\u{10000}b");
        assert_eq!(n.len(), 2);
        assert_eq!(n[0], "a\u{10000}b".as_bytes());
        assert_eq!(n[1], ddc_dex::reader::mutf8_encode("a\u{10000}b"));
        assert!(!bytes_contain(&n[1], &n[..1]), "utf8 must not match mutf8");
        assert!(bytes_contain(&n[1], &n));
        assert!(bytes_contain_str(&n[1], "a\u{10000}b"));
    }
}
