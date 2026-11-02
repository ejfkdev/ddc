//! Shared input plumbing for the ddc CLI: expand inputs (files/dirs) to
//! dex-bearing files, then inflate + parse every image in parallel.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use ddc_dex::DexFile;

use crate::{zip_entries, ZipEntry, ZipMethod};

/// Input file bytes: mmap-backed when possible (zero heap copy — the old
/// `fs::read` copied a 353MB APK into the heap before the first inflate,
/// costing ~80ms and the whole file's RSS), heap for tiny/odd cases.
pub enum Source {
    Map(memmap2::Mmap),
    Heap(Vec<u8>),
}

impl Source {
    pub fn bytes(&self) -> &[u8] {
        match self {
            Source::Map(m) => &m[..],
            Source::Heap(v) => &v[..],
        }
    }
}

/// A raw (possibly still deflated) DEX image with its origin label. The
/// archive bytes are SHARED (Arc) and each image is a range slice — the
/// old per-entry owned copies doubled a 353MB APK's resident footprint
/// before the first inflate.
#[derive(Clone)]
pub struct Image {
    pub label: String,
    pub data: std::sync::Arc<Source>,
    pub range: std::ops::Range<usize>,
    pub method: ZipMethod,
    /// Uncompressed size hint for Deflate inflation (0 = unknown).
    pub usize_hint: usize,
}

impl Image {
    /// Inflated-size estimate for wave budgeting. The central directory
    /// normally carries the exact size; for odd archives fall back to a
    /// conservative 3x compressed range (DEX entries deflate ~2.5-3x).
    pub fn inflated_hint(&self) -> usize {
        if self.usize_hint > 0 {
            self.usize_hint
        } else {
            self.range.len().saturating_mul(3).max(1 << 20)
        }
    }
}

/// Expand input paths: files pass through, directories are scanned
/// recursively (sorted for determinism).
pub fn expand_inputs(inputs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = Vec::new();
    for input in inputs {
        if input.is_dir() {
            let before = files.len();
            walk_dex_files(input, &mut files).with_context(
                || crate::lang::bif!("scanning {0}", "正在扫描 {0}"; input.display()),
            )?;
            if files.len() == before {
                bail!(
                    "{}",
                    crate::lang::bif!("no .dex/.apk/.jar/.zip files under {0}", "{0} 下没有 .dex/.apk/.jar/.zip 文件"; input.display())
                );
            }
        } else if input.is_file() {
            files.push(input.clone());
        } else {
            bail!(
                "{}",
                crate::lang::bif!("input not found: {0}", "输入不存在：{0}"; input.display())
            );
        }
    }
    Ok(files)
}

fn walk_dex_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        if p.is_dir() {
            walk_dex_files(&p, out)?;
        } else if is_dex_ext(&p) {
            out.push(p);
        }
    }
    Ok(())
}

pub fn is_dex_ext(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()),
        Some(ref e) if matches!(
            e.as_str(),
            "dex" | "apk" | "jar" | "zip" | "xapk" | "apks" | "apkm"
        )
    )
}

/// Whether a directory contains (recursively) anything the pool could
/// read. False → an output-directory candidate, not an input.
pub fn dir_has_dex_files(dir: &Path) -> bool {
    let mut v = Vec::new();
    walk_dex_files(dir, &mut v).is_ok() && !v.is_empty()
}

/// Collect raw dex images from every input file: zips contribute all
/// their `*.dex` entries (classes.dex, classes2.dex... numeric order
/// first), raw dex files contribute themselves. Labels read
/// `<input-stem>!<zip-entry>`.
///
/// Expand an XAPK/APKS/APKM container: one image per inner APK's dex
/// entry, labeled `<outer-stem>!<inner-apk>!<dex-entry>`. The BASE apk
/// comes first so duplicate class names resolve to it (pool first-wins);
/// config splits follow in name order.
fn nested_apk_images(
    outer: &std::sync::Arc<Source>,
    entries: &[ZipEntry],
    stem: &str,
) -> Result<Vec<Image>> {
    let mut apks: Vec<&ZipEntry> = entries
        .iter()
        .filter(|e| e.name.ends_with(".apk"))
        .collect();
    apks.sort_by_key(|e| {
        let base = e.name == "base.apk"
            || e.name == format!("{stem}.apk")
            || e.name.starts_with("split_base");
        (!base, e.name.clone())
    });
    let mut images = Vec::new();
    for apk in apks {
        let inner: Vec<u8> = match apk.method {
            ZipMethod::Stored => outer.bytes()[apk.range.clone()].to_vec(),
            ZipMethod::Deflate => {
                crate::inflate_hint(outer.bytes()[apk.range.clone()].as_ref(), apk.usize_hint)?
            }
        };
        if inner.len() < 4 || &inner[..2] != b"PK" {
            continue; // odd entry (renamed obb etc.)
        }
        let inner_entries = zip_entries(&inner)?;
        let src = std::sync::Arc::new(Source::Heap(inner));
        let mut dexes: Vec<(u64, ZipEntry)> = Vec::new();
        let mut extra: Vec<ZipEntry> = Vec::new();
        for e in inner_entries {
            if !e.name.ends_with(".dex") {
                continue;
            }
            let core = e.name.trim_end_matches(".dex");
            if let Some(num) = core.strip_prefix("classes") {
                let key = num.parse::<u64>().unwrap_or(0);
                dexes.push((key, e));
            } else {
                extra.push(e);
            }
        }
        dexes.sort_by_key(|(k, _)| *k);
        let mk = |e: ZipEntry| Image {
            label: format!("{}!{}!{}", stem, apk.name, e.name),
            usize_hint: e.usize_hint,
            data: src.clone(),
            range: e.range,
            method: e.method,
        };
        images.extend(dexes.into_iter().map(|(_, e)| mk(e)));
        images.extend(extra.into_iter().map(mk));
    }
    if images.is_empty() {
        bail!(crate::lang::bi!(
            "XAPK container has no APK entries with *.dex files",
            "XAPK 容器中没有带 *.dex 的 APK 条目"
        ));
    }
    Ok(images)
}

pub fn map_source(f: &Path) -> Result<Source> {
    let file = std::fs::File::open(f)
        .with_context(|| crate::lang::bif!("open {0}", "打开 {0}"; f.display()))?;
    let len = file
        .metadata()
        .with_context(|| crate::lang::bif!("stat {0}", "取属性 {0}"; f.display()))?
        .len();
    if len >= 1 << 20 {
        // Mmap faults pages in as touched (the CD scan touches the tail,
        // inflates touch entry ranges); the heap copy touched everything.
        let map = unsafe { memmap2::Mmap::map(&file) }
            .with_context(|| crate::lang::bif!("map {0}", "映射 {0}"; f.display()))?;
        Ok(Source::Map(map))
    } else {
        Ok(Source::Heap(std::fs::read(f).with_context(
            || crate::lang::bif!("read {0}", "读取 {0}"; f.display()),
        )?))
    }
}

pub fn collect_images(files: &[PathBuf]) -> Result<Vec<Image>> {
    let mut images: Vec<Image> = Vec::new();
    for f in files {
        let src = std::sync::Arc::new(map_source(f)?);
        let stem = f
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("input")
            .to_string();
        let bytes: &[u8] = src.bytes();
        if bytes.len() >= 4 && &bytes[..2] == b"PK" {
            let entries = zip_entries(bytes)?;
            // XAPK / APKS / APKM: a zip of APKs (base + config splits, plus
            // a manifest.json/info.json the pool ignores). Detected by
            // CONTENT — .apk entries and no .dex entries — so renamed or
            // mislabeled containers work too.
            let has_apk = entries.iter().any(|e| e.name.ends_with(".apk"));
            let has_dex = entries.iter().any(|e| e.name.ends_with(".dex"));
            if has_apk && !has_dex {
                images.extend(nested_apk_images(&src, &entries, &stem)?);
                continue;
            }
            let mut dexes: Vec<(u64, ZipEntry)> = Vec::new();
            let mut extra: Vec<ZipEntry> = Vec::new();
            for e in entries {
                if !e.name.ends_with(".dex") {
                    continue;
                }
                let core = e.name.trim_end_matches(".dex");
                if let Some(num) = core.strip_prefix("classes") {
                    let key = num.parse::<u64>().unwrap_or(0);
                    dexes.push((key, e));
                } else {
                    extra.push(e);
                }
            }
            if dexes.is_empty() && extra.is_empty() {
                bail!(
                    "{}: no *.dex entries in archive (is it an Android APK/dx jar?)",
                    f.display()
                );
            }
            dexes.sort_by_key(|(k, _)| *k);
            let mk = |e: ZipEntry| Image {
                label: format!("{}!{}", stem, e.name),
                usize_hint: e.usize_hint,
                data: src.clone(),
                range: e.range,
                method: e.method,
            };
            images.extend(dexes.into_iter().map(|(_, e)| mk(e)));
            images.extend(extra.into_iter().map(mk));
        } else if bytes.len() >= 4 && &bytes[..3] == b"dex" {
            let n = bytes.len();
            images.push(Image {
                label: stem,
                data: src,
                range: 0..n,
                method: ZipMethod::Stored,
                usize_hint: 0,
            });
        } else {
            bail!(
                "{}: not a DEX image or ZIP/APK archive (bad magic)",
                f.display()
            );
        }
    }
    Ok(images)
}

/// Filter images by dex entry name (`--dex classes20` matches the entry
/// part of `<stem>!classes20.dex`; a bare raw dex matches its whole
/// label). Substring, case-insensitive; multiple patterns OR. Filtering
/// BEFORE the parse keeps unwanted images off the decode entirely.
pub fn filter_images_by_dex(images: Vec<Image>, patterns: &[String]) -> Result<Vec<Image>> {
    if patterns.is_empty() {
        return Ok(images);
    }
    // Match against the WHOLE label: plain APK labels are
    // `<stem>!classes.dex` (an entry-name match behaves exactly as
    // before), nested XAPK labels are `<stem>!<apk>!<dex>` where either
    // segment may be the thing the user names (`--dex base`,
    // `--dex config.arm64`, `--dex classes2`).
    let pats: Vec<String> = patterns.iter().map(|p| p.to_ascii_lowercase()).collect();
    let (kept, dropped): (Vec<Image>, Vec<Image>) = images.into_iter().partition(|img| {
        let e = img.label.to_ascii_lowercase();
        pats.iter().any(|p| e.contains(p.as_str()))
    });
    if kept.is_empty() {
        let mut names: Vec<String> = dropped
            .iter()
            .map(|img| {
                img.label
                    .rsplit_once('!')
                    .map(|(_, e)| e)
                    .unwrap_or(&img.label)
                    .to_string()
            })
            .collect();
        names.sort();
        names.dedup();
        bail!(
            "{}",
            crate::lang::bif!(
                "--dex {0}: no matching dex images (available: {1})",
                "--dex {0}：没有匹配的 dex 镜像（可用：{1}）";
                patterns.join(", "),
                names.join(", ")
            )
        );
    }
    Ok(kept)
}

/// Streaming prefix inflation with a decision callback: `decide` sees the
/// bytes produced so far and either keeps going, commits to a needed byte
/// count, or aborts (the bytes so far stay exact). zlib-ng under the hood.
pub fn inflate_until(
    data: &[u8],
    range: std::ops::Range<usize>,
    decide: impl FnMut(&[u8]) -> PrefixStep,
) -> Result<Vec<u8>> {
    inflate_until_capacity(data, range, 0, decide)
}

/// Same as [`inflate_until`], with an initial capacity. Prefix callers pass
/// the image's uncompressed-size hint so a near-full prefix does not pay for
/// repeated `Vec` growth/copying; only the written pages become resident.
pub fn inflate_until_capacity(
    data: &[u8],
    range: std::ops::Range<usize>,
    capacity: usize,
    mut decide: impl FnMut(&[u8]) -> PrefixStep,
) -> Result<Vec<u8>> {
    use std::io::Read;
    let compressed = &data[range];
    // `decide` runs at checkpoints (immediately, then after each
    // Continue(n) has produced n more bytes) and may run repeatedly — a
    // checkpoint is not a stop, it is "ask me again here". Abort returns
    // the exact prefix produced so far; Continue(usize::MAX) drains the
    // rest of the stream without further callbacks.
    const CHUNK: usize = 1 << 18;
    let mut reader = flate2::read::DeflateDecoder::new(compressed);
    let mut out: Vec<u8> = Vec::with_capacity(capacity);
    let mut checkpoint = 0usize;
    let mut drain = false;
    let mut chunk = vec![0u8; CHUNK];
    loop {
        if !drain && out.len() >= checkpoint {
            match decide(&out) {
                PrefixStep::Continue(n) => {
                    if n == usize::MAX {
                        drain = true;
                    } else {
                        let n = n.max(1);
                        out.reserve_exact(n);
                        checkpoint = out.len() + n;
                    }
                }
                PrefixStep::Abort => return Ok(out),
            }
        }
        let read = reader.read(&mut chunk)?;
        if read > 0 {
            out.extend_from_slice(&chunk[..read]);
        } else {
            return Ok(out);
        }
    }
}

/// What the prefix decider wants next.
pub enum PrefixStep {
    /// Keep going; the number is how many bytes are needed before the next
    /// decision (0 = ask again after the next chunk).
    Continue(usize),
    /// Stop: the prefix is all that is needed.
    Abort,
}

/// A prefix decider: returns the required byte count, or `None` while the
/// header/tables themselves are still incomplete.
pub type PrefixNeed = fn(&[u8]) -> Option<usize>;

/// How far into a DEX image the id tables + string data reach (the
/// reference-search working set): max(class_defs end, string-data end).
/// `None` while the prefix is too short to answer.
pub fn scan_prefix_needed(image: &[u8]) -> Option<usize> {
    if image.len() < 0x70 || !image.starts_with(b"dex\n") {
        return None;
    }
    let u4 = |o: usize| -> usize {
        u32::from_le_bytes([image[o], image[o + 1], image[o + 2], image[o + 3]]) as usize
    };
    let str_n = u4(0x38);
    let str_off = u4(0x3c);
    let cls_n = u4(0x60);
    let cls_off = u4(0x64);
    let ids_end = str_off + 4 * str_n;
    let defs_end = cls_off + 32 * cls_n;
    let tables_end = ids_end.max(defs_end);
    if tables_end > image.len() {
        return None; // id/class tables not fully in the prefix yet
    }
    // String data end: max data offset + a walkable margin; offsets point
    // into the data section which may extend beyond the current prefix, so
    // this is a LOWER bound request — the decider re-runs as it grows.
    let mut max_off = 0usize;
    for i in 0..str_n {
        let off = u4(str_off + 4 * i);
        if off > max_off {
            max_off = off;
        }
    }
    if max_off >= image.len() {
        return Some(max_off + 4096); // need at least up to this string
    }
    Some(tables_end.max(max_off + 64))
}

/// How far into a DEX image the class descriptor strings reach.
///
/// `class_names_from_image` needs only the header, the string/type/class_def
/// tables and the descriptors those class_defs point at — not the full string
/// pool or any code item. Returning the exact descriptor end lets the
/// two-tier getclass path inflate a fraction of a large image instead of the
/// whole thing.
///
/// `None` means the current prefix is too short to answer yet; `Some(n)` means
/// `n` bytes are enough. Invalid/non-DEX input returns `Some(0)` so the caller
/// stops immediately and yields an empty name list.
pub fn class_names_prefix_needed(image: &[u8]) -> Option<usize> {
    if image.len() < 0x70 {
        return Some(0x70);
    }
    if !image.starts_with(b"dex\n") {
        return Some(0);
    }
    let u4 = |o: usize| -> usize {
        u32::from_le_bytes([image[o], image[o + 1], image[o + 2], image[o + 3]]) as usize
    };
    let str_n = u4(0x38);
    let str_off = u4(0x3c);
    let type_n = u4(0x40);
    let type_off = u4(0x44);
    let cls_n = u4(0x60);
    let cls_off = u4(0x64);

    let tables_end = str_off
        .saturating_add(4usize.saturating_mul(str_n))
        .max(type_off.saturating_add(4usize.saturating_mul(type_n)))
        .max(cls_off.saturating_add(32usize.saturating_mul(cls_n)));
    if tables_end > image.len() {
        return Some(tables_end);
    }

    let mut need = tables_end;
    for i in 0..cls_n {
        let cd = cls_off + 32 * i;
        let class_idx = u4(cd);
        if class_idx >= type_n {
            continue;
        }
        let string_idx = u4(type_off + 4 * class_idx);
        if string_idx >= str_n {
            continue;
        }
        let Some(id_off) = str_off
            .checked_add(4 * string_idx)
            .filter(|off| *off + 4 <= image.len())
        else {
            continue;
        };
        let data_off = u4(id_off);
        if data_off >= image.len() {
            return Some(data_off.saturating_add(4096));
        }
        // Skip the uleb128 UTF-16 length and find this item's NUL terminator.
        // Descriptors are short; the 4 KiB request is re-evaluated by the
        // inflate decider, so a longer item simply grows the prefix again.
        let mut p = data_off;
        loop {
            let Some(&b) = image.get(p) else {
                return Some(data_off.saturating_add(4096));
            };
            p += 1;
            if b & 0x80 == 0 {
                break;
            }
        }
        let Some(nul) = image
            .get(p..)
            .and_then(|tail| tail.iter().position(|&b| b == 0))
        else {
            return Some(
                data_off
                    .saturating_add(4096)
                    .max(image.len().saturating_add(4096)),
            );
        };
        need = need.max(p + nul + 1);
    }
    Some(need)
}

fn string_item_end(image: &[u8], data_off: usize) -> std::result::Result<usize, usize> {
    if data_off >= image.len() {
        return Err(data_off.saturating_add(4096));
    }
    let mut p = data_off;
    loop {
        let Some(&b) = image.get(p) else {
            return Err(data_off.saturating_add(4096));
        };
        p += 1;
        if b & 0x80 == 0 {
            break;
        }
    }
    image
        .get(p..)
        .and_then(|tail| tail.iter().position(|&b| b == 0))
        .map(|nul| p + nul + 1)
        .ok_or_else(|| {
            data_off
                .saturating_add(4096)
                .max(image.len().saturating_add(4096))
        })
}

/// How far into a DEX image the type descriptor strings reach.
///
/// `hierarchy`/`mainactivity` resolve class names, superclasses and interfaces
/// from `type_ids`; they never need code items or non-type strings. Returning
/// the exact end of those descriptors lets the caller inflate only that
/// prefix. `None` means the prefix is still too short; `Some(0)` is invalid
/// input.
pub fn type_descriptor_prefix_needed(image: &[u8]) -> Option<usize> {
    if image.len() < 0x70 {
        return Some(0x70);
    }
    if !image.starts_with(b"dex\n") {
        return Some(0);
    }
    let u4 = |o: usize| -> usize {
        u32::from_le_bytes([image[o], image[o + 1], image[o + 2], image[o + 3]]) as usize
    };
    let str_n = u4(0x38);
    let str_off = u4(0x3c);
    let type_n = u4(0x40);
    let type_off = u4(0x44);
    let cls_n = u4(0x60);
    let cls_off = u4(0x64);

    let tables_end = str_off
        .saturating_add(4usize.saturating_mul(str_n))
        .max(type_off.saturating_add(4usize.saturating_mul(type_n)))
        .max(cls_off.saturating_add(32usize.saturating_mul(cls_n)));
    if tables_end > image.len() {
        return Some(tables_end);
    }

    let mut need = tables_end;
    for i in 0..type_n {
        let string_idx = u4(type_off + 4 * i);
        if string_idx >= str_n {
            continue;
        }
        let id_off = str_off + 4 * string_idx;
        if id_off + 4 > image.len() {
            continue;
        }
        let data_off = u4(id_off);
        match string_item_end(image, data_off) {
            Ok(end) => need = need.max(end),
            Err(request) => return Some(request),
        }
    }
    // `hierarchy` reads each class_def's interfaces_off -> type_list, which
    // lives in the data section independently of the type_ids table.
    for i in 0..cls_n {
        let interfaces_off = u4(cls_off + 32 * i + 12);
        if interfaces_off == 0 {
            continue;
        }
        if interfaces_off >= image.len() {
            return Some(interfaces_off.saturating_add(4096));
        }
        let count = u4(interfaces_off);
        let end = interfaces_off
            .saturating_add(4)
            .saturating_add(2usize.saturating_mul(count));
        if end > image.len() {
            return Some(end.saturating_add(4096));
        }
        need = need.max(end);
    }
    Some(need)
}

/// The three id tables `RawDex::parse` needs, plus where they end.
fn string_table_bounds(image: &[u8]) -> Option<(usize, usize, usize)> {
    if image.len() < 0x70 || !image.starts_with(b"dex\n") {
        return None;
    }
    let u4 = |o: usize| -> usize {
        u32::from_le_bytes([image[o], image[o + 1], image[o + 2], image[o + 3]]) as usize
    };
    let str_n = u4(0x38);
    let str_off = u4(0x3c);
    let type_n = u4(0x40);
    let type_off = u4(0x44);
    let cls_n = u4(0x60);
    let cls_off = u4(0x64);
    let tables_end = str_off
        .saturating_add(4usize.saturating_mul(str_n))
        .max(type_off.saturating_add(4usize.saturating_mul(type_n)))
        .max(cls_off.saturating_add(32usize.saturating_mul(cls_n)));
    Some((str_n, str_off, tables_end))
}

/// How far into a DEX image the whole string table reaches. Used by the
/// no-locations `strings` path, which needs every string payload but no code
/// items or class_data.
pub fn string_table_prefix_needed(image: &[u8]) -> Option<usize> {
    if image.len() < 0x70 {
        return Some(0x70);
    }
    let Some((str_n, str_off, tables_end)) = string_table_bounds(image) else {
        return Some(0);
    };
    if tables_end > image.len() {
        return Some(tables_end);
    }
    let u4 = |o: usize| -> usize {
        u32::from_le_bytes([image[o], image[o + 1], image[o + 2], image[o + 3]]) as usize
    };
    let mut need = tables_end;
    for i in 0..str_n {
        let data_off = u4(str_off + 4 * i);
        match string_item_end(image, data_off) {
            Ok(end) => need = need.max(end),
            Err(request) => need = need.max(request),
        }
    }
    Some(need)
}

/// Tables-only prefix: stops once every string_id is readable, which is
/// enough to learn the largest string offset before inflating any payload.
fn string_tables_end(image: &[u8]) -> Option<usize> {
    let (_, _, tables_end) = string_table_bounds(image)?;
    (tables_end <= image.len()).then_some(tables_end)
}

/// Does the string-table prefix pay off for this image set? Probe the largest
/// deflated image's id tables once; a pool that reaches most of the image
/// gains too little to justify the extra checkpoints.
pub fn string_prefix_wins(images: &[Image]) -> bool {
    let Some(img) = images
        .iter()
        .filter(|i| matches!(i.method, ZipMethod::Deflate))
        .max_by_key(|i| i.inflated_hint())
    else {
        return false;
    };
    let hint = img.usize_hint;
    if hint == 0 {
        return false;
    }
    let data = &img.data.bytes()[img.range.clone()];
    let Ok(probe) = inflate_until_capacity(data, 0..data.len(), hint, |out| {
        match string_tables_end(out) {
            Some(end) if out.len() >= end => PrefixStep::Abort,
            Some(end) => PrefixStep::Continue(end - out.len()),
            None => PrefixStep::Continue(0),
        }
    }) else {
        return false;
    };
    let needed = string_table_prefix_needed(&probe).unwrap_or(hint);
    needed.saturating_mul(4) <= hint.saturating_mul(3)
}

fn note_string_end(
    image: &[u8],
    str_off: usize,
    str_n: usize,
    idx: usize,
    need: &mut usize,
) {
    if idx >= str_n {
        return;
    }
    let id_off = str_off + 4 * idx;
    if id_off + 4 > image.len() {
        return;
    }
    let data_off = u32::from_le_bytes([
        image[id_off],
        image[id_off + 1],
        image[id_off + 2],
        image[id_off + 3],
    ]) as usize;
    match string_item_end(image, data_off) {
        Ok(end) => *need = (*need).max(end),
        Err(request) => *need = (*need).max(request),
    }
}

/// Where the id tables end (string/type/proto/field/method/class_defs),
/// once every one of them is fully inside `image`.
fn member_tables_end(image: &[u8]) -> Option<usize> {
    if image.len() < 0x70 || !image.starts_with(b"dex\n") {
        return None;
    }
    let u4 = |o: usize| -> usize {
        u32::from_le_bytes([image[o], image[o + 1], image[o + 2], image[o + 3]]) as usize
    };
    let end = u4(0x3c)
        .saturating_add(4usize.saturating_mul(u4(0x38)))
        .max(u4(0x44).saturating_add(4usize.saturating_mul(u4(0x40))))
        .max(u4(0x4c).saturating_add(12usize.saturating_mul(u4(0x48))))
        .max(u4(0x54).saturating_add(8usize.saturating_mul(u4(0x50))))
        .max(u4(0x5c).saturating_add(8usize.saturating_mul(u4(0x58))))
        .max(u4(0x64).saturating_add(32usize.saturating_mul(u4(0x60))));
    (end <= image.len()).then_some(end)
}

/// Does the identifier prefix pay off for this image set? Probe the largest
/// deflated image's id tables once; if its names reach most of the image,
/// the prefix saves too little to be worth the extra checkpoints.
pub fn member_prefix_wins(images: &[Image]) -> bool {
    let Some(img) = images
        .iter()
        .filter(|i| matches!(i.method, ZipMethod::Deflate))
        .max_by_key(|i| i.inflated_hint())
    else {
        return false;
    };
    let hint = img.usize_hint;
    if hint == 0 {
        return false;
    }
    let data = &img.data.bytes()[img.range.clone()];
    let Ok(probe) = inflate_until_capacity(data, 0..data.len(), hint, |out| {
        match member_tables_end(out) {
            Some(end) if out.len() >= end => PrefixStep::Abort,
            Some(end) => PrefixStep::Continue(end - out.len()),
            None => PrefixStep::Continue(0),
        }
    }) else {
        return false;
    };
    let needed = member_ids_prefix_needed(&probe).unwrap_or(hint);
    needed.saturating_mul(4) <= hint.saturating_mul(3)
}

/// How far into a DEX image every id-table name/descriptor reaches.
///
/// `members` walks method_ids/field_ids and renders owner class names,
/// member names and proto descriptors — it never reads class_data or code
/// items. This prefix ends at the last such string payload, which on
/// code-heavy APKs is well before the instruction stream.
pub fn member_ids_prefix_needed(image: &[u8]) -> Option<usize> {
    if image.len() < 0x70 {
        return Some(0x70);
    }
    if !image.starts_with(b"dex\n") {
        return Some(0);
    }
    let u4 = |o: usize| -> usize {
        u32::from_le_bytes([image[o], image[o + 1], image[o + 2], image[o + 3]]) as usize
    };
    let u2 = |o: usize| -> usize { u16::from_le_bytes([image[o], image[o + 1]]) as usize };
    let str_n = u4(0x38);
    let str_off = u4(0x3c);
    let type_n = u4(0x40);
    let type_off = u4(0x44);
    let proto_n = u4(0x48);
    let proto_off = u4(0x4c);
    let field_n = u4(0x50);
    let field_off = u4(0x54);
    let method_n = u4(0x58);
    let method_off = u4(0x5c);
    let cls_n = u4(0x60);
    let cls_off = u4(0x64);

    let tables_end = str_off
        .saturating_add(4usize.saturating_mul(str_n))
        .max(type_off.saturating_add(4usize.saturating_mul(type_n)))
        .max(proto_off.saturating_add(12usize.saturating_mul(proto_n)))
        .max(field_off.saturating_add(8usize.saturating_mul(field_n)))
        .max(method_off.saturating_add(8usize.saturating_mul(method_n)))
        .max(cls_off.saturating_add(32usize.saturating_mul(cls_n)));
    if tables_end > image.len() {
        return Some(tables_end);
    }

    let mut need = tables_end;
    // Type descriptors: every owner class, field type and proto type is a
    // type_id whose name comes from here.
    for i in 0..type_n {
        note_string_end(image, str_off, str_n, u4(type_off + 4 * i), &mut need);
    }
    for i in 0..field_n {
        let p = field_off + 8 * i;
        note_string_end(image, str_off, str_n, u4(p + 4), &mut need);
    }
    for i in 0..method_n {
        let p = method_off + 8 * i;
        note_string_end(image, str_off, str_n, u4(p + 4), &mut need);
        let proto_idx = u2(p + 2);
        if proto_idx >= proto_n {
            continue;
        }
        let q = proto_off + 12 * proto_idx;
        note_string_end(image, str_off, str_n, u4(q + 4), &mut need);
        let params_off = u4(q + 8);
        if params_off == 0 {
            continue;
        }
        if params_off + 4 > image.len() {
            need = need.max(params_off.saturating_add(4));
            continue;
        }
        let count = u4(params_off);
        let end = params_off
            .saturating_add(4)
            .saturating_add(2usize.saturating_mul(count));
        if end > image.len() {
            need = need.max(end);
            continue;
        }
        for k in 0..count {
            note_string_end(
                image,
                str_off,
                str_n,
                u2(params_off + 4 + 2 * k),
                &mut need,
            );
        }
    }
    Some(need)
}

/// Header-only DEX summary: the counts `info` prints, without decoding any id
/// table or string payload.
#[derive(Debug, Clone)]
pub struct DexHeaderSummary {
    pub dex_version: String,
    pub classes: u64,
    pub methods: u64,
    pub fields: u64,
    pub strings: u64,
}

/// Read one image's 0x70-byte header. Stored images are read in place; a
/// deflated image inflates only the first chunk needed for the header.
pub fn image_header_summary(img: Image) -> std::result::Result<Option<DexHeaderSummary>, String> {
    let raw = match img.method {
        ZipMethod::Stored => {
            return Ok(parse_dex_header_summary(&img.data.bytes()[img.range]));
        }
        ZipMethod::Deflate => inflate_until(
            img.data.bytes(),
            img.range.clone(),
            |out| {
                if out.len() >= 0x70 {
                    PrefixStep::Abort
                } else {
                    PrefixStep::Continue(0x70 - out.len())
                }
            },
        )
        .map_err(|e| e.to_string())?,
    };
    Ok(parse_dex_header_summary(&raw))
}

fn parse_dex_header_summary(image: &[u8]) -> Option<DexHeaderSummary> {
    if image.len() < 0x70 || !image.starts_with(b"dex\n") {
        return None;
    }
    let version = std::str::from_utf8(&image[4..7]).ok()?;
    if !matches!(
        version,
        "035" | "037" | "038" | "039" | "040" | "041"
    ) {
        return None;
    }
    let endian = u32::from_le_bytes(image[40..44].try_into().ok()?);
    if endian != 0x1234_5678 {
        return None;
    }
    let u4 = |o: usize| -> u64 {
        u32::from_le_bytes(image[o..o + 4].try_into().unwrap()) as u64
    };
    Some(DexHeaderSummary {
        dex_version: version.to_string(),
        strings: u4(0x38),
        fields: u4(0x50),
        methods: u4(0x58),
        classes: u4(0x60),
    })
}

/// Inflate each image on its own thread WITHOUT DexFile::parse — the raw
/// bytes go straight to the caller (the browse commands run their own
/// zero-materialization queries on them).
pub fn inflate_images(images: Vec<Image>) -> Result<Vec<(Image, Vec<u8>)>> {
    inflate_images_with(images, "inflate", inflate_full_image)
}

fn inflate_full_image(img: &Image) -> std::result::Result<Vec<u8>, String> {
    match img.method {
        ZipMethod::Stored => Ok(img.data.bytes()[img.range.clone()].to_vec()),
        ZipMethod::Deflate => crate::inflate_hint(
            img.data.bytes()[img.range.clone()].as_ref(),
            img.usize_hint,
        )
        .map_err(|e| e.to_string()),
    }
}

fn inflate_images_with(
    images: Vec<Image>,
    what: &'static str,
    inflate: impl Fn(&Image) -> std::result::Result<Vec<u8>, String>
        + Clone
        + Send
        + Sync
        + 'static,
) -> Result<Vec<(Image, Vec<u8>)>> {
    let mut handles = Vec::with_capacity(images.len());
    for img in images {
        let label = img.label.clone();
        let inflate = inflate.clone();
        handles.push((
            label,
            std::thread::spawn(move || {
                let raw = inflate(&img)?;
                Ok::<_, String>((img, raw))
            }),
        ));
    }
    let mut out = Vec::with_capacity(handles.len());
    for (name, h) in handles {
        let (img, raw) = h
            .join()
            .map_err(|_| anyhow::anyhow!("inflate thread panicked: {}", name))?
            .map_err(|e| anyhow::anyhow!("{} {}: {}", what, name, e))?;
        out.push((img, raw));
    }
    Ok(out)
}

/// Inflate each image only as far as `need` asks (see
/// `type_descriptor_prefix_needed`), in parallel and in input order.
pub fn inflate_prefix_images(
    images: Vec<Image>,
    need: PrefixNeed,
) -> Result<Vec<(Image, Vec<u8>)>> {
    inflate_images_with(images, "inflate prefix", move |img| match img.method {
        ZipMethod::Stored => Ok(img.data.bytes()[img.range.clone()].to_vec()),
        ZipMethod::Deflate => inflate_until_capacity(
            img.data.bytes(),
            img.range.clone(),
            img.usize_hint,
            |out| match need(out) {
                None => PrefixStep::Continue(0),
                Some(needed) => {
                    if out.len() >= needed {
                        PrefixStep::Abort
                    } else {
                        PrefixStep::Continue(needed - out.len())
                    }
                }
            },
        )
        .map_err(|e| e.to_string()),
    })
}

/// Inflate AND parse each image on its own thread. Returns
/// `(label, DexFile)` pairs in input order.
pub fn parse_images(images: Vec<Image>) -> Result<Vec<(String, DexFile)>> {
    let mut handles = Vec::new();
    for img in images {
        let label = img.label.clone();
        handles.push((
            label,
            std::thread::spawn(move || {
                let raw = match img.method {
                    ZipMethod::Stored => img.data.bytes()[img.range].to_vec(),
                    ZipMethod::Deflate => crate::inflate_hint(
                        img.data.bytes()[img.range].as_ref(),
                        img.usize_hint,
                    )
                    .map_err(|e| e.to_string())?,
                };
                DexFile::parse(raw)
                    .map_err(|e| e.to_string())
                    .map(|dex| (img.label, dex))
            }),
        ));
    }
    let mut out = Vec::with_capacity(handles.len());
    let mut skipped: Vec<String> = Vec::new();
    for (name, h) in handles {
        let parsed = h
            .join()
            .map_err(|_| anyhow::anyhow!("parse thread panicked: {}", name))?
            .map_err(|e| anyhow::anyhow!("parse {}: {}", name, e));
        match parsed {
            Ok(p) => out.push(p),
            Err(e) => {
                // Packed apps ship dex-LIKE entries (uuyc's
                // assets/39285EFA.dex is an encrypted blob with a .dex
                // name): skip with a visible notice instead of failing
                // the whole run over the real images.
                eprintln!("ddc: skipping {} (not parsable: {:#})", name, e);
                skipped.push(name);
            }
        }
    }
    if out.is_empty() {
        anyhow::bail!(
            "no parsable DEX image ({} image(s) skipped, e.g. {})",
            skipped.len(),
            skipped.first().map(|s| s.as_str()).unwrap_or("?")
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{
        class_names_prefix_needed, member_ids_prefix_needed, parse_dex_header_summary,
        string_table_prefix_needed, type_descriptor_prefix_needed,
    };

    fn put_u4(data: &mut [u8], off: usize, value: u32) {
        data[off..off + 4].copy_from_slice(&value.to_le_bytes());
    }

    /// Minimal DEX with one class descriptor (`Lfoo/Bar;`) and an unrelated
    /// string far behind it. A correct decider stops after the descriptor,
    /// not after the whole string pool.
    #[test]
    fn class_names_prefix_stops_after_descriptor() {
        let mut data = vec![0u8; 0x100];
        data[..8].copy_from_slice(b"dex\n035\0");
        put_u4(&mut data, 0x20, 0x100);
        put_u4(&mut data, 0x38, 2); // string_ids_size
        put_u4(&mut data, 0x3c, 0x70); // string_ids_off
        put_u4(&mut data, 0x40, 1); // type_ids_size
        put_u4(&mut data, 0x44, 0x78); // type_ids_off
        put_u4(&mut data, 0x60, 1); // class_defs_size
        put_u4(&mut data, 0x64, 0x7c); // class_defs_off
        put_u4(&mut data, 0x70, 0x9c); // string_ids[0] -> descriptor data
        put_u4(&mut data, 0x74, 0xc0); // string_ids[1] -> unrelated string
        put_u4(&mut data, 0x78, 0); // type_ids[0] -> string 0
        put_u4(&mut data, 0x7c, 0); // class_defs[0].class_idx = 0

        let payload = b"Lfoo/Bar;";
        data[0x9c] = payload.len() as u8;
        data[0x9d..0x9d + payload.len()].copy_from_slice(payload);
        data[0x9d + payload.len()] = 0;
        let end = 0x9e + payload.len();

        let unrelated = b"zzz";
        data[0xc0] = unrelated.len() as u8;
        data[0xc1..0xc1 + unrelated.len()].copy_from_slice(unrelated);
        data[0xc1 + unrelated.len()] = 0;

        assert_eq!(class_names_prefix_needed(&data), Some(end));
        assert_eq!(class_names_prefix_needed(&data[..0xc0]), Some(end));
        assert_eq!(
            ddc_dex::DexFile::class_names_from_image(&data[..end]),
            vec!["foo/Bar".to_string()]
        );
        assert!(ddc_dex::DexFile::class_names_from_image(&data[..end - 1]).is_empty());
        assert_eq!(class_names_prefix_needed(&data[..end + 8]), Some(end));
    }

    #[test]
    fn header_summary_reads_counts_without_tables() {
        let mut data = vec![0u8; 0x70];
        data[..8].copy_from_slice(b"dex\n035\0");
        put_u4(&mut data, 0x28, 0x1234_5678); // endian_tag
        put_u4(&mut data, 0x38, 11); // string_ids_size
        put_u4(&mut data, 0x50, 22); // field_ids_size
        put_u4(&mut data, 0x58, 33); // method_ids_size
        put_u4(&mut data, 0x60, 44); // class_defs_size

        let summary = parse_dex_header_summary(&data).expect("valid header");
        assert_eq!(summary.dex_version, "035");
        assert_eq!(summary.strings, 11);
        assert_eq!(summary.fields, 22);
        assert_eq!(summary.methods, 33);
        assert_eq!(summary.classes, 44);
        assert!(parse_dex_header_summary(&data[..0x60]).is_none());

        data[4..7].copy_from_slice(b"036");
        assert!(parse_dex_header_summary(&data).is_none());
    }

    #[test]
    fn type_prefix_covers_descriptors_but_not_unrelated_strings() {
        let mut data = vec![0u8; 0x300];
        data[..8].copy_from_slice(b"dex\n035\0");
        put_u4(&mut data, 0x38, 3); // string_ids_size
        put_u4(&mut data, 0x3c, 0x70); // string_ids_off
        put_u4(&mut data, 0x40, 2); // type_ids_size
        put_u4(&mut data, 0x44, 0x7c); // type_ids_off
        put_u4(&mut data, 0x60, 1); // class_defs_size
        put_u4(&mut data, 0x64, 0x84); // class_defs_off

        put_u4(&mut data, 0x70, 0xa8); // string 0: Lfoo/Bar;
        put_u4(&mut data, 0x74, 0xc0); // string 1: long descriptor
        put_u4(&mut data, 0x78, 0x200); // string 2: unrelated
        put_u4(&mut data, 0x7c, 0); // type 0 -> string 0
        put_u4(&mut data, 0x80, 1); // type 1 -> string 1
        put_u4(&mut data, 0x84, 0); // class_defs[0].class_idx = 0
        put_u4(&mut data, 0x90, 0x1c0); // class_defs[0].interfaces_off

        let first = b"Lfoo/Bar;";
        data[0xa8] = first.len() as u8;
        data[0xa9..0xa9 + first.len()].copy_from_slice(first);
        data[0xa9 + first.len()] = 0;

        let long = [b'z'; 100];
        data[0xc0] = long.len() as u8;
        data[0xc1..0xc1 + long.len()].copy_from_slice(&long);
        data[0xc1 + long.len()] = 0;

        put_u4(&mut data, 0x1c0, 1); // type_list count
        data[0x1c4..0x1c6].copy_from_slice(&0u16.to_le_bytes()); // interface type 0
        let interface_end = 0x1c6;

        let unrelated = b"not a type";
        data[0x200] = unrelated.len() as u8;
        data[0x201..0x201 + unrelated.len()].copy_from_slice(unrelated);
        data[0x201 + unrelated.len()] = 0;

        assert_eq!(
            type_descriptor_prefix_needed(&data),
            Some(interface_end)
        );
        assert!(interface_end < 0x200);
    }

    #[test]
    fn member_prefix_covers_names_and_proto_types() {
        let mut data = vec![0u8; 0x300];
        data[..8].copy_from_slice(b"dex\n035\0");
        put_u4(&mut data, 0x38, 3); // string_ids_size
        put_u4(&mut data, 0x3c, 0x70); // string_ids_off
        put_u4(&mut data, 0x40, 1); // type_ids_size
        put_u4(&mut data, 0x44, 0x7c); // type_ids_off
        put_u4(&mut data, 0x48, 1); // proto_ids_size
        put_u4(&mut data, 0x4c, 0x80); // proto_ids_off
        put_u4(&mut data, 0x50, 0); // field_ids_size
        put_u4(&mut data, 0x54, 0x8c); // field_ids_off
        put_u4(&mut data, 0x58, 1); // method_ids_size
        put_u4(&mut data, 0x5c, 0x8c); // method_ids_off
        put_u4(&mut data, 0x60, 0); // class_defs_size
        put_u4(&mut data, 0x64, 0x94); // class_defs_off

        put_u4(&mut data, 0x70, 0x100); // string 0: class descriptor
        put_u4(&mut data, 0x74, 0x180); // string 1: method name
        put_u4(&mut data, 0x78, 0x200); // string 2: unrelated
        put_u4(&mut data, 0x7c, 0); // type 0 -> string 0
        put_u4(&mut data, 0x80, 0); // proto shorty
        put_u4(&mut data, 0x84, 0); // proto return type 0
        put_u4(&mut data, 0x88, 0); // proto params_off
        data[0x8c..0x8e].copy_from_slice(&0u16.to_le_bytes()); // method class_idx
        data[0x8e..0x90].copy_from_slice(&0u16.to_le_bytes()); // method proto_idx
        put_u4(&mut data, 0x90, 1); // method name_idx

        let class = b"Lfoo/Bar;";
        data[0x100] = class.len() as u8;
        data[0x101..0x101 + class.len()].copy_from_slice(class);
        data[0x101 + class.len()] = 0;

        let name = b"run";
        data[0x180] = name.len() as u8;
        data[0x181..0x181 + name.len()].copy_from_slice(name);
        let name_end = 0x181 + name.len() + 1;
        data[0x181 + name.len()] = 0;

        let unrelated = b"unrelated";
        data[0x200] = unrelated.len() as u8;
        data[0x201..0x201 + unrelated.len()].copy_from_slice(unrelated);
        data[0x201 + unrelated.len()] = 0;

        assert_eq!(member_ids_prefix_needed(&data), Some(name_end));
        assert!(name_end < 0x200);
    }

    #[test]
    fn string_prefix_covers_unrelated_strings_too() {
        let mut data = vec![0u8; 0x300];
        data[..8].copy_from_slice(b"dex\n035\0");
        put_u4(&mut data, 0x38, 2); // string_ids_size
        put_u4(&mut data, 0x3c, 0x70); // string_ids_off
        put_u4(&mut data, 0x40, 0); // type_ids_size
        put_u4(&mut data, 0x44, 0x78); // type_ids_off
        put_u4(&mut data, 0x60, 0); // class_defs_size
        put_u4(&mut data, 0x64, 0x78); // class_defs_off

        put_u4(&mut data, 0x70, 0x100); // string 0
        put_u4(&mut data, 0x74, 0x200); // string 1 at the tail

        let first = b"Lfoo/Bar;";
        data[0x100] = first.len() as u8;
        data[0x101..0x101 + first.len()].copy_from_slice(first);
        data[0x101 + first.len()] = 0;

        let tail = b"plain-text";
        data[0x200] = tail.len() as u8;
        data[0x201..0x201 + tail.len()].copy_from_slice(tail);
        let tail_end = 0x201 + tail.len() + 1;
        data[0x201 + tail.len()] = 0;

        assert_eq!(string_table_prefix_needed(&data), Some(tail_end));
        assert!(tail_end > 0x100 + first.len());
    }
}
