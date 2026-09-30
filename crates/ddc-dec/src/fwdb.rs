//! Embedded Android framework API database.
//!
//! Built offline by `scripts/extract-android-api.py` from the SDK's
//! `android.jar` (class files: names, descriptors, supers, interfaces,
//! ACCESS FLAGS) merged with `data/api-versions.xml` (classes REMOVED
//! from the modern jar but still referenced by old APKs — the
//! org.apache.http family — flagged UNKNOWN so flag-sensitive consumers
//! skip them while subtype/exists queries resolve).
//!
//! Runtime cost is zero-parse by design: the blob is `include_bytes!`'d
//! and queried in place — binary search over the class table, NUL-scan
//! string reads, section slices for members. No decompression, no
//! deserialization, no startup work (the rejected alternatives: runtime
//! zlib+JSON ≈ 300ms first query; generated Rust source ≈ multi-MB of
//! literals, slower builds, bigger binary than the raw pool).
//!
//! Consumers: DexPool::is_subtype's framework-boundary fallback
//! (String <: CharSequence <: Object chains the dex pool cannot see),
//! the multi-catch / Throwable-bridge / interface-stub synthesizers
//! (replacing hand-written tables), obscured-render field-shadow
//! detection (View.X/Y/Z and friends), and the framework overload
//! enumeration behind the descriptor-exact ambiguity pin.

const BLOB: &[u8] = include_bytes!("../data/android-api.fwdb");

const MAGIC: u32 = 0x42445746; // 'FWDB'
const REC: usize = 36; // 9 × u32 per class record
const SUPER_NONE: u32 = u32::MAX;

// Compile-time blob validation (const panic = build failure): wrong or
// truncated data can never ship.
const _: () = assert!(
    u32::from_le_bytes([BLOB[0], BLOB[1], BLOB[2], BLOB[3]]) == MAGIC
        && u32::from_le_bytes([BLOB[4], BLOB[5], BLOB[6], BLOB[7]]) == 1
        && BLOB.len() > 40,
    "android-api.fwdb: bad magic/version — rerun scripts/extract-android-api.py"
);

// Class flag bits.
pub const CF_INTERFACE: u32 = 1;
pub const CF_ABSTRACT: u32 = 2;
pub const CF_ENUM: u32 = 4;
pub const CF_UNKNOWN: u32 = 1 << 31; // xml-only (removed API): no flag info

// Member flag bits.
pub const MF_PUBLIC: u32 = 1;
pub const MF_PROTECTED: u32 = 2;
pub const MF_PRIVATE: u32 = 4;
pub const MF_STATIC: u32 = 8;
pub const MF_ABSTRACT: u32 = 16;
pub const MF_FINAL: u32 = 32;
pub const MF_BRIDGE: u32 = 64;
pub const MF_SYNTHETIC: u32 = 128;
pub const MF_UNKNOWN: u32 = 1 << 31;

#[inline(always)]
fn u32_at(off: usize) -> u32 {
    u32::from_le_bytes([BLOB[off], BLOB[off + 1], BLOB[off + 2], BLOB[off + 3]])
}

#[inline(always)]
fn off_pool() -> usize {
    u32_at(16) as usize
}

#[inline(always)]
fn class_count() -> u32 {
    u32_at(8)
}

#[inline(always)]
fn off_classes() -> usize {
    u32_at(20) as usize
}

#[inline(always)]
fn off_impls() -> usize {
    u32_at(24) as usize
}

#[inline(always)]
fn off_methods() -> usize {
    u32_at(28) as usize
}

#[inline(always)]
fn off_fields() -> usize {
    u32_at(32) as usize
}

/// String-pool read: NUL-terminated slice (the builder guarantees ASCII-
/// compatible UTF-8; from_utf8 cannot fail on the generated data but the
/// safe call costs one linear pass we already paid via the NUL scan).
#[inline]
fn s_at(off: u32) -> &'static str {
    let start = off_pool() + off as usize;
    let mut end = start;
    while BLOB[end] != 0 {
        end += 1;
    }
    std::str::from_utf8(&BLOB[start..end]).unwrap_or("")
}

/// Byte-wise compare of a pool string against `name` (no &str materialized).
#[inline]
fn cmp_name(off: u32, name: &[u8]) -> std::cmp::Ordering {
    let start = off_pool() + off as usize;
    let mut i = 0usize;
    loop {
        let b = BLOB[start + i];
        match (b, name.get(i)) {
            (0, None) => return std::cmp::Ordering::Equal,
            (0, Some(_)) => return std::cmp::Ordering::Less, // pool str is a prefix
            (_, None) => return std::cmp::Ordering::Greater,
            (x, Some(&y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                i += 1;
            }
        }
    }
}

/// Binary search: internal name -> class record index.
pub fn find(name: &str) -> Option<u32> {
    let n = class_count();
    let base = off_classes();
    let nb = name.as_bytes();
    let (mut lo, mut hi) = (0u32, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let name_off = u32_at(base + REC * mid as usize);
        match cmp_name(name_off, nb) {
            std::cmp::Ordering::Equal => return Some(mid),
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
        }
    }
    None
}

#[inline]
fn rec(i: u32) -> [u32; 9] {
    let b = off_classes() + REC * i as usize;
    [
        u32_at(b),
        u32_at(b + 4),
        u32_at(b + 8),
        u32_at(b + 12),
        u32_at(b + 16),
        u32_at(b + 20),
        u32_at(b + 24),
        u32_at(b + 28),
        u32_at(b + 32),
    ]
}

pub fn exists(name: &str) -> bool {
    find(name).is_some()
}

pub fn class_flags(name: &str) -> u32 {
    find(name).map(|i| rec(i)[2]).unwrap_or(0)
}

pub fn super_of(name: &str) -> Option<&'static str> {
    let i = find(name)?;
    let so = rec(i)[1];
    if so == SUPER_NONE {
        None
    } else {
        Some(s_at(so))
    }
}

/// Iterate the direct interfaces of `name`.
pub fn for_each_interface(name: &str, mut f: impl FnMut(&'static str)) {
    let Some(i) = find(name) else { return };
    let r = rec(i);
    let (start, len) = (r[3] as usize, r[4] as usize);
    let base = off_impls();
    for k in 0..len {
        f(s_at(u32_at(base + 4 * (start + k))));
    }
}

/// Iterate `(descriptor, flags)` of every method of `name`. The descriptor
/// is the combined `"name(args)ret"` form.
pub fn for_each_method(name: &str, mut f: impl FnMut(&'static str, u32)) {
    let Some(i) = find(name) else { return };
    let r = rec(i);
    let (start, len) = (r[5] as usize, r[6] as usize);
    let base = off_methods();
    for k in 0..len {
        let b = base + 8 * (start + k);
        f(s_at(u32_at(b)), u32_at(b + 4));
    }
}

/// Iterate `(field_name, flags)` of every field of `name`.
pub fn for_each_field(name: &str, mut f: impl FnMut(&'static str, u32)) {
    let Some(i) = find(name) else { return };
    let r = rec(i);
    let (start, len) = (r[7] as usize, r[8] as usize);
    let base = off_fields();
    for k in 0..len {
        let b = base + 8 * (start + k);
        f(s_at(u32_at(b)), u32_at(b + 4));
    }
}

/// Framework-world subtype check: walks supers and interfaces inside the
/// embedded DB. `sub == sup` is true by contract (callers may rely on it).
pub fn is_subtype(sub: &str, sup: &str) -> bool {
    if sub == sup {
        return true;
    }
    if find(sub).is_none() {
        return false;
    }
    // The builder strips java/lang/Object super edges (space); every
    // class in the DB is Object-assignable (interfaces included).
    if sup == "java/lang/Object" {
        return true;
    }
    let mut stack: Vec<&str> = vec![sub];
    let mut visited: jdc_core::FxHashSet<&str> = jdc_core::FxHashSet::default();
    let mut hops = 0usize;
    while let Some(c) = stack.pop() {
        hops += 1;
        if hops > 4096 || !visited.insert(c) {
            continue;
        }
        if let Some(s) = super_of(c) {
            if s == sup {
                return true;
            }
            stack.push(s);
        }
        let mut hit = false;
        for_each_interface(c, |i| {
            if i == sup {
                hit = true;
            } else {
                stack.push(i);
            }
        });
        if hit {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_header_is_valid() {
        assert_eq!(u32_at(0), MAGIC);
        assert_eq!(u32_at(4), 1);
        assert!(class_count() > 6000);
    }

    #[test]
    fn hierarchy_queries() {
        assert!(exists("android/view/View"));
        assert_eq!(super_of("android/view/ViewGroup"), Some("android/view/View"));
        assert!(is_subtype("android/widget/FrameLayout", "android/view/View"));
        assert!(is_subtype("java/lang/String", "java/lang/Object"));
        assert!(is_subtype("java/lang/String", "java/lang/CharSequence"));
        assert!(!is_subtype("android/view/View", "java/lang/String"));
        assert!(class_flags("android/view/ViewGroup") & CF_ABSTRACT != 0);
    }

    #[test]
    fn removed_api_families_resolve_as_unknown() {
        assert!(exists("org/apache/http/HttpResponse"));
        assert!(class_flags("org/apache/http/HttpResponse") & CF_UNKNOWN != 0);
    }

    #[test]
    fn member_queries() {
        let mut has_x = false;
        for_each_field("android/view/View", |n, f| {
            if n == "X" && f & MF_STATIC != 0 {
                has_x = true;
            }
        });
        assert!(has_x, "View.X static field");
        let mut ctors = 0;
        for_each_method("java/lang/Throwable", |d, _| {
            if d.starts_with("<init>") {
                ctors += 1;
            }
        });
        assert!(ctors >= 4);
        let mut has_run = false;
        for_each_method("java/lang/Runnable", |d, f| {
            if d == "run()V" && f & MF_ABSTRACT != 0 && f & MF_STATIC == 0 {
                has_run = true;
            }
        });
        assert!(has_run, "Runnable.run abstract instance method");
        assert!(class_flags("java/lang/Runnable") & CF_INTERFACE != 0);
    }
}
