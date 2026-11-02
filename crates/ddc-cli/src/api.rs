//! Typed query cores shared by the CLI's `--format json` branch and the
//! optional xyz HTTP/MCP frontends (`xyz_api.rs`).
//!
//! Each core returns the data its CLI subcommand prints. The text path
//! in `main.rs`/`browse.rs` is deliberately untouched (zero regression
//! risk on the default rendering) — the collection loops here are
//! adapted copies of the printing loops; consolidation is a later,
//! separately verified step.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::inputs::{
    collect_images, expand_inputs, filter_images_by_dex, is_dex_ext, dir_has_dex_files,
};
use crate::browse::{
    for_each_image, for_each_image_named, for_each_member_image, for_each_prefix_image,
    for_each_string_image,
};
use crate::findrefs::{decode_mutf8_lossy, RawDex};

/// Shared signature of `for_each_image` and its prefix variants.
type ImageEachFn = fn(&PathBuf, &[String], &mut dyn FnMut(&str, &[u8])) -> Result<()>;

// ---- responses ---------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
#[derive(xyz_rust::XyzOutput)]
pub struct ImageCounts {
    pub image: String,
    pub dex_version: String,
    pub classes: u64,
    pub methods: u64,
    pub fields: u64,
    pub strings: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
#[derive(xyz_rust::XyzOutput)]
pub struct InfoReport {
    /// App display label (resources.arsc), when a manifest exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub package: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub application: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launcher: Option<String>,
    /// `min–target` when both are known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sdk: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub md5: Option<String>,
    pub images: Vec<ImageCounts>,
    pub total_images: u64,
    pub total_classes: u64,
}

/// One decompiled class (source + the image that defined it).
#[derive(Debug, Clone, serde::Serialize)]
#[derive(xyz_rust::XyzOutput)]
pub struct ClassSource {
    pub class: String,
    /// Defining image label (`apk!classes2.dex`); with several defining
    /// images the first (pool) wins and `images` lists them all.
    pub dex: String,
    /// Every image defining the class (usually one).
    pub images: Vec<String>,
    /// Full decompiled Java source.
    pub source: String,
}

/// One decompiled method slice (all overloads of the name).
#[derive(Debug, Clone, serde::Serialize)]
#[derive(xyz_rust::XyzOutput)]
pub struct MethodSource {
    pub class: String,
    /// The method name requested (overloads carry descriptors in the
    /// slice header lines).
    pub method: String,
    pub dex: String,
    pub source: String,
}

/// A cross-reference hit row (one per method with matches).
#[derive(Debug, Clone, serde::Serialize)]
#[derive(xyz_rust::XyzOutput)]
pub struct RefRow {
    pub dex: String,
    /// First-hit instruction kind (`const-string`, `invoke`...).
    pub kind: String,
    /// Owner class, dotted form.
    pub class: String,
    /// Owner method `name(descriptor)`.
    pub method: String,
    /// Every matched target in this method.
    #[serde(rename = "refs")]
    pub targets: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[derive(xyz_rust::XyzOutput)]
pub struct StringRow {
    pub dex: String,
    /// The string table entry, Java-quoted form.
    pub string: String,
    /// Methods referencing it (only with locations requested).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub used_by: Vec<String>,
}

/// Text-only no-locations scan: emits `(dex entry, quoted string)` for the
/// window without allocating a StringRow (and its dex-name clone) per row.
/// The quoted buffer is reused across rows.
pub fn string_scan_text(
    input: &Path,
    filter: Option<&str>,
    dex_filters: &[String],
    offset: usize,
    limit: usize,
    mut emit: impl FnMut(&str, &str),
) -> Result<usize> {
    use std::fmt::Write as _;
    let needles = filter.map(crate::findrefs::byte_needles);
    let mut base = 0usize;
    let mut quoted = String::new();
    for_each_string_image(
        &input.to_path_buf(),
        dex_filters,
        &mut |label, image| {
            let dex_name = label
                .rsplit_once('!')
                .map(|(_, e)| e)
                .unwrap_or(label);
            let Ok(dex) = RawDex::parse(image) else {
                return;
            };
            let local_lo = offset.saturating_sub(base);
            let local_hi = if limit == 0 {
                usize::MAX
            } else {
                offset.saturating_add(limit).saturating_sub(base)
            };
            let mut window: Vec<u32> = Vec::new();
            let mut count = 0usize;
            for idx in 0..dex.str_n as u32 {
                let Some(bytes) = dex.string_bytes(idx) else {
                    continue;
                };
                let hit = match &needles {
                    Some(f) => crate::findrefs::bytes_contain(bytes, f),
                    None => true,
                };
                if !hit {
                    continue;
                }
                if count >= local_lo && count < local_hi {
                    window.push(idx);
                }
                count += 1;
            }
            for idx in window {
                let s = decode_mutf8_lossy(dex.string_bytes(idx).unwrap_or(&[]));
                quoted.clear();
                let _ = write!(&mut quoted, "{s:?}");
                emit(dex_name, &quoted);
            }
            base += count;
        },
    )?;
    Ok(base)
}

#[derive(Debug, Clone, serde::Serialize)]
#[derive(xyz_rust::XyzOutput)]
pub struct RelationRow {
    pub dex: String,
    /// class | extends | implements | sub | impl
    pub relation: String,
    /// Class descriptor (`Lcom/foo/Bar;` form for extends/implements).
    pub class: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[derive(xyz_rust::XyzOutput)]
pub struct MainActivityReport {
    pub package: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub application: Option<String>,
    pub launcher: String,
    /// Defining image label, `-` when the class lives outside the dex
    /// images (framework class or missing from the APK).
    pub dex: String,
}

// ---- cores -------------------------------------------------------------------

/// Per-image header summaries (`info` needs counts only, not id tables or
/// string payloads). Images are inflated in parallel and only to 0x70 bytes;
/// stored images are read in place.
pub fn image_summaries(input: &Path, dex_filters: &[String]) -> Result<Vec<ImageCounts>> {
    let files = expand_inputs(&[input.to_path_buf()])?;
    let images = filter_images_by_dex(collect_images(&files)?, dex_filters)?;
    let mut handles = Vec::with_capacity(images.len());
    for img in images {
        let label = img.label.clone();
        handles.push((
            label,
            std::thread::spawn(move || crate::inputs::image_header_summary(img)),
        ));
    }
    let mut out = Vec::with_capacity(handles.len());
    for (label, handle) in handles {
        let summary = handle
            .join()
            .map_err(|_| anyhow::anyhow!("header scan thread panicked: {label}"))?
            .map_err(|e| anyhow::anyhow!("header scan {label}: {e}"))?;
        match summary {
            Some(s) => out.push(ImageCounts {
                image: label,
                dex_version: s.dex_version,
                classes: s.classes,
                methods: s.methods,
                fields: s.fields,
                strings: s.strings,
            }),
            None => eprintln!("ddc: skipping {label} (not a DEX header)"),
        }
    }
    if out.is_empty() {
        anyhow::bail!("no parsable DEX image (all images skipped)");
    }
    Ok(out)
}

pub fn info_report(input: &Path) -> Result<InfoReport> {
    let mut report = InfoReport {
        label: None,
        package: String::new(),
        version: None,
        application: None,
        launcher: None,
        sdk: None,
        size_bytes: None,
        md5: None,
        images: Vec::new(),
        total_images: 0,
        total_classes: 0,
    };
    if let Ok(facts) = crate::manifest::facts_for(input) {
        report.label = facts
            .label
            .as_deref()
            .map(|l| crate::arsc::resolve_string_ref(input, l).unwrap_or_else(|| l.to_string()));
        report.package = facts.package.clone();
        report.version = facts.version_name.clone();
        report.application = facts.application.clone();
        report.launcher = facts.launcher.clone();
        report.sdk = match (&facts.min_sdk, &facts.target_sdk) {
            (Some(m), Some(t)) => Some(format!("{m}-{t}")),
            (Some(m), None) => Some(format!("min {m}")),
            (None, Some(t)) => Some(format!("target {t}")),
            (None, None) => None,
        };
    }
    if input.is_file() {
        report.size_bytes = Some(std::fs::metadata(input)?.len());
        report.md5 = Some(crate::md5_hex(input)?);
    }
    let images = image_summaries(input, &[])?;
    report.total_images = images.len() as u64;
    report.total_classes = images.iter().map(|image| image.classes).sum();
    report.images = images;
    Ok(report)
}

/// One row of the class-tree ("prefix frontier") view. `kind` tells a
/// package group from a class leaf — Java allows a class and a package to
/// share a name, in which case both rows appear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeKind {
    /// A terminating item (class name, string, resource file…).
    Leaf,
    /// A prefix group (package / directory) with a subtree count.
    Group,
}

#[derive(Debug, Clone)]
pub struct TreeRow {
    pub path: String,
    pub kind: TreeKind,
    /// Items in this subtree (the item's own weight for a leaf).
    pub count: usize,
    /// Up to N representative leaf names under a group row (opt-in).
    pub samples: Vec<String>,
}

/// A page of class names plus the pre-window match count.
pub struct ClassPage {
    pub names: Vec<String>,
    pub total: usize,
}

/// Normalize a user prefix for a separator: slashes/dots unified to
/// `sep`, trailing separators stripped. Empty means "the root".
pub fn normalize_prefix(p: &str, sep: char) -> String {
    let unified = p.trim().replace(['/', '.'], &sep.to_string());
    unified.trim_end_matches(sep).to_string()
}

/// Normalize a class prefix (dotted form).
pub fn normalize_class_prefix(p: &str) -> String {
    normalize_prefix(p, '.')
}

/// Segment-exact prefix match: with sep='.', `a.b` matches `a.b` and
/// `a.b.*`, never `a.bc`.
pub fn under_prefix(name: &str, prefix: &str, sep: char) -> bool {
    name == prefix
        || (name.len() > prefix.len()
            && name.starts_with(prefix)
            && name.as_bytes()[prefix.len()] == sep as u8)
}

/// Class names, dotted form, deduped in pool order; `pattern` is a
/// case-insensitive substring filter, `prefix` a segment-exact scope,
/// and `(offset, limit)` a window over the filtered set (`limit == 0`
/// means unlimited). The order is the first-appearance order over dex
/// images in input order, so windows are stable across runs.
pub fn class_names(
    input: &Path,
    pattern: Option<&str>,
    prefix: Option<&str>,
    dex_filters: &[String],
    offset: usize,
    limit: usize,
) -> Result<ClassPage> {
    let files = expand_inputs(&[input.to_path_buf()])?;
    let mut images = filter_images_by_dex(collect_images(&files)?, dex_filters)?;
    let mut all: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // 分波解压（每波 ≤8）：此前对每个 dex 无界并发解压，37 个映像同时
    // 驻留会让 serve/MCP 常驻内存冲到 ~500MB；顺序进、顺序出，结果不变。
    const WAVE: usize = 8;
    while !images.is_empty() {
        let take = WAVE.min(images.len());
        let wave: Vec<_> = images.drain(..take).collect();
        let handles: Vec<_> = wave
            .into_iter()
            .map(|img| {
                std::thread::spawn(move || match img.method {
                    crate::ZipMethod::Stored => ddc_dex::DexFile::class_names_from_image(
                        &img.data.bytes()[img.range],
                    ),
                    crate::ZipMethod::Deflate => {
                        let raw = crate::inputs::inflate_until_capacity(
                            img.data.bytes(),
                            img.range.clone(),
                            img.usize_hint,
                            |out| match crate::inputs::class_names_prefix_needed(out) {
                                None => crate::inputs::PrefixStep::Continue(0),
                                Some(needed) => {
                                    if out.len() >= needed {
                                        crate::inputs::PrefixStep::Abort
                                    } else {
                                        crate::inputs::PrefixStep::Continue(needed - out.len())
                                    }
                                }
                            },
                        )
                        .unwrap_or_default();
                        ddc_dex::DexFile::class_names_from_image(&raw)
                    }
                })
            })
            .collect();
        for h in handles {
            if let Ok(names) = h.join() {
                for name in names {
                    if seen.insert(name.clone()) {
                        all.push(name);
                    }
                }
            }
        }
    }
    // pattern 过滤必须作用在点分显示名上：输出是 a.a 形式，在内部
    // 斜杠形式（a/a）上过滤会让任何带点的 pattern 永远匹配 0 结果。
    let dotted: Vec<String> = all.into_iter().map(|n| n.replace('/', ".")).collect();
    let matched: Vec<String> = match pattern {
        Some(p) => {
            let lp = p.to_ascii_lowercase();
            dotted
                .into_iter()
                .filter(|n| n.to_ascii_lowercase().contains(&lp))
                .collect()
        }
        None => dotted,
    };
    let matched: Vec<String> = match prefix {
        Some(p) => matched
            .into_iter()
            .filter(|n| under_prefix(n, p, '.'))
            .collect(),
        None => matched,
    };
    let total = matched.len();
    let names = if limit == 0 {
        matched.into_iter().skip(offset).collect()
    } else {
        matched.into_iter().skip(offset).take(limit).collect()
    };
    Ok(ClassPage { names, total })
}

struct TrieNode {
    children: std::collections::BTreeMap<String, TrieNode>,
    /// Weight of keys terminating exactly here (duplicates accumulate).
    self_count: usize,
    /// Weight of every key in this subtree (self included).
    count: usize,
}

impl TrieNode {
    fn new() -> Self {
        TrieNode {
            children: std::collections::BTreeMap::new(),
            self_count: 0,
            count: 0,
        }
    }
}

/// Build the compressed prefix frontier of `names` (already filtered).
///
/// Adaptive mode (`depth == None`) collapses single-child chains and
/// stops at branching points, so every row is either a class leaf or a
/// package group whose `classes` count is what drilling there would
/// yield. `depth == Some(n)` instead aggregates at exactly `n` segments
/// below the scope (classes that terminate earlier still appear).
/// Rows come out in segment-lexicographic order (deterministic).
#[cfg(test)]
pub fn class_tree(names: &[String], prefix: Option<&str>, depth: Option<usize>) -> Vec<TreeRow> {
    prefix_tree(names.iter().map(|n| (n.as_str(), 1)), prefix, depth, '.')
}

/// Frontend suffix for a group row: `  e.g. a, b, c` (empty when no samples).
pub fn samples_suffix(r: &TreeRow) -> String {
    if r.samples.is_empty() {
        String::new()
    } else {
        format!("  e.g. {}", r.samples.join(", "))
    }
}

/// Collect up to `want` leaf paths under `node` (depth-first, in-order).
fn sample_leaves(node: &TrieNode, path: &str, sep: char, want: usize, out: &mut Vec<String>) {
    if out.len() >= want {
        return;
    }
    if node.self_count > 0 {
        out.push(path.to_string());
    }
    for (seg, child) in &node.children {
        if out.len() >= want {
            return;
        }
        let mut p = path.to_string();
        p.push(sep);
        p.push_str(seg);
        sample_leaves(child, &p, sep, want, out);
    }
}

/// Generic weighted segment tree over `(key, weight)` items. Duplicate
/// keys accumulate their weights, so callers can aggregate rows (hits,
/// members, resources) as well as unique names.
#[cfg(test)]
pub fn prefix_tree<'a, I>(
    items: I,
    prefix: Option<&str>,
    depth: Option<usize>,
    sep: char,
) -> Vec<TreeRow>
where
    I: IntoIterator<Item = (&'a str, usize)>,
{
    prefix_tree_sampled(items, prefix, depth, sep, 0)
}

/// JSON row for a summary view. `samples` is present only when requested.
pub fn tree_row_json(r: &TreeRow, leaf_kind: &str, group_kind: &str) -> serde_json::Value {
    let mut v = serde_json::json!({
        "path": r.path,
        "kind": match r.kind {
            TreeKind::Leaf => leaf_kind,
            TreeKind::Group => group_kind,
        },
        "count": r.count,
    });
    if !r.samples.is_empty() {
        v["samples"] = serde_json::json!(r.samples);
    }
    v
}

/// [`prefix_tree`] with up to `samples` representative leaf names attached
/// to each group row (filled during traversal, bounded by `samples`).
pub fn prefix_tree_sampled<'a, I>(
    items: I,
    prefix: Option<&str>,
    depth: Option<usize>,
    sep: char,
    samples: usize,
) -> Vec<TreeRow>
where
    I: IntoIterator<Item = (&'a str, usize)>,
{
    let mut root = TrieNode::new();
    for (key, weight) in items {
        let mut n = &mut root;
        n.count += weight;
        for seg in key.split(sep) {
            n = n.children.entry(seg.to_string()).or_insert_with(TrieNode::new);
            n.count += weight;
        }
        n.self_count += weight;
    }
    let scope_path = prefix.unwrap_or("").trim_end_matches(sep);
    let mut scope = &root;
    if !scope_path.is_empty() {
        for seg in scope_path.split(sep) {
            match scope.children.get(seg) {
                Some(n) => scope = n,
                None => return Vec::new(),
            }
        }
    }
    let mut out = Vec::new();
    if !scope_path.is_empty() && scope.self_count > 0 {
        out.push(TreeRow {
            path: scope_path.to_string(),
            kind: TreeKind::Leaf,
            count: scope.self_count,
            samples: Vec::new(),
        });
    }
    for (seg, child) in &scope.children {
        let path = if scope_path.is_empty() {
            seg.clone()
        } else {
            format!("{scope_path}{sep}{seg}")
        };
        match depth {
            None => emit_adaptive(child, path, sep, samples, &mut out),
            Some(target) => emit_fixed(child, path, sep, 1, target, samples, &mut out),
        }
    }
    out
}

fn emit_adaptive(
    node: &TrieNode,
    path: String,
    sep: char,
    samples: usize,
    out: &mut Vec<TreeRow>,
) {
    let mut path = path;
    let mut node = node;
    loop {
        if node.self_count > 0 {
            out.push(TreeRow {
                path: path.clone(),
                kind: TreeKind::Leaf,
                count: node.self_count,
                samples: Vec::new(),
            });
        }
        if node.children.is_empty() {
            return;
        }
        if node.children.len() == 1 {
            let (seg, child) = node.children.iter().next().expect("len == 1");
            path.push(sep);
            path.push_str(seg);
            node = child;
            continue;
        }
        let mut s = Vec::new();
        sample_leaves(node, &path, sep, samples, &mut s);
        out.push(TreeRow {
            path,
            kind: TreeKind::Group,
            count: node.count,
            samples: s,
        });
        return;
    }
}

fn emit_fixed(
    node: &TrieNode,
    path: String,
    sep: char,
    level: usize,
    target: usize,
    samples: usize,
    out: &mut Vec<TreeRow>,
) {
    if node.self_count > 0 {
        out.push(TreeRow {
            path: path.clone(),
            kind: TreeKind::Leaf,
            count: node.self_count,
            samples: Vec::new(),
        });
    }
    if node.children.is_empty() {
        return;
    }
    if level == target {
        let mut s = Vec::new();
        sample_leaves(node, &path, sep, samples, &mut s);
        out.push(TreeRow {
            path,
            kind: TreeKind::Group,
            count: node.count,
            samples: s,
        });
        return;
    }
    for (seg, child) in &node.children {
        let mut p = path.clone();
        p.push(sep);
        p.push_str(seg);
        emit_fixed(child, p, sep, level + 1, target, samples, out);
    }
}

/// Window over a row list: the page plus the metadata a client needs to
/// resume. `limit == 0` means unlimited.
pub struct RowPage<T> {
    pub rows: Vec<T>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub next_offset: Option<usize>,
}

pub fn window_rows<T>(rows: Vec<T>, offset: usize, limit: usize) -> RowPage<T> {
    let total = rows.len();
    window_rows_total(rows, offset, limit, total)
}

/// Same window, but with a total that may exceed `rows.len()` because the
/// caller kept only the paged prefix of the sorted result.
pub fn window_rows_total<T>(
    rows: Vec<T>,
    offset: usize,
    limit: usize,
    total: usize,
) -> RowPage<T> {
    let rows: Vec<T> = if limit == 0 {
        rows.into_iter().skip(offset).collect()
    } else {
        rows.into_iter().skip(offset).take(limit).collect()
    };
    let end = offset.saturating_add(rows.len());
    let next_offset = if end < total { Some(end) } else { None };
    RowPage {
        rows,
        total,
        offset,
        limit,
        next_offset,
    }
}

/// Decompile one class (nested classes included). Mirrors
/// `getclass` / `getclass_text`.
pub fn class_source(
    inputs: &[PathBuf],
    fqcn: &str,
    dex_filters: &[String],
) -> Result<ClassSource> {
    let (text, defining) = crate::getclass_text(inputs, fqcn, dex_filters)?;
    Ok(ClassSource {
        class: fqcn.replace('/', "."),
        dex: defining.first().cloned().unwrap_or_default(),
        images: defining.clone(),
        source: format!("{text}\n"),
    })
}

/// Decompile one method (all overloads). Mirrors `getmethod`.
pub fn method_source(
    input: &Path,
    target: &str,
    dex_filters: &[String],
) -> Result<MethodSource> {
    let split = target
        .rsplit_once('.')
        .filter(|(c, m)| !c.is_empty() && !m.is_empty() && !m.contains('('))
        .map(|(c, m)| (c.to_string(), m.to_string()));
    let candidates: Vec<(String, Option<String>)> = match &split {
        Some((c, m)) => vec![(c.clone(), Some(m.clone())), (target.to_string(), None)],
        None => vec![(target.to_string(), None)],
    };
    let mut last: Option<anyhow::Error> = None;
    for (class, method) in candidates {
        match crate::getclass_text(std::slice::from_ref(&input.to_path_buf()), &class, dex_filters) {
            Ok((text, defining)) => {
                let source = match &method {
                    Some(m) => match crate::browse::slice_methods(&text, m) {
                        Some(b) => b,
                        None => {
                            last = Some(anyhow::anyhow!(
                                "method {} not found in {} (methods: {})",
                                m,
                                class,
                                crate::browse::method_names(&text).join(", ")
                            ));
                            continue;
                        }
                    },
                    None => format!("{text}\n"),
                };
                return Ok(MethodSource {
                    class,
                    method: method.unwrap_or_default(),
                    dex: defining.first().cloned().unwrap_or_default(),
                    source,
                });
            }
            Err(e) => {
                last.get_or_insert(e);
            }
        }
    }
    match last {
        Some(e) => Err(e),
        None => bail!("method {target} not found"),
    }
}

/// The manifest as text XML; `component` extracts one group
/// (launcher|activity|service|receiver|provider|activity-alias|application).
pub fn manifest_text(input: &Path, component: Option<&str>) -> Result<String> {
    let (_, xml) = crate::manifest::manifest_xml(input)?;
    Ok(match component {
        Some(c) => crate::manifest::component_xml(&xml, c),
        None => xml,
    })
}

pub fn main_activity(input: &Path) -> Result<MainActivityReport> {
    let facts = crate::manifest::facts_for(input)?;
    if facts.package.is_empty() {
        bail!("manifest has no package attribute");
    }
    let launcher = facts
        .launcher
        .context("manifest declares no MAIN/LAUNCHER activity")?;
    let internal = launcher.replace('.', "/");
    let mut found: Option<String> = None;
    for_each_prefix_image(
        &input.to_path_buf(),
        &[],
        crate::inputs::type_descriptor_prefix_needed,
        &mut |label, image| {
        if found.is_some() {
            return;
        }
        if let Ok(dex) = RawDex::parse(image) {
            if dex.find_class(&internal).is_some() {
                found = Some(label.to_string());
            }
        }
    })?;
    Ok(MainActivityReport {
        package: facts.package,
        application: facts.application,
        launcher,
        dex: found.unwrap_or_else(|| "-".into()),
    })
}

/// String-table rows; `filter` is a substring filter, `with_locations`
/// maps each string to its referencing methods.
/// String-reference counts per owning class, for the summary view.
/// Counts matching const-string sites during one code walk; one class
/// name resolution per class with hits (no per-hit allocation).
pub fn string_owner_counts(
    input: &Path,
    filter: Option<&str>,
    dex_filters: &[String],
) -> Result<Vec<(String, usize)>> {
    let mut by_class: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    let needles = filter.map(crate::findrefs::byte_needles);
    for_each_image(&input.to_path_buf(), dex_filters, &mut |_label, image| {
        let Ok(dex) = RawDex::parse(image) else {
            return;
        };
        // A filtered summary only cares about images whose string table has a
        // match; checking it is far cheaper than decoding all code in a dex
        // that cannot contribute. The common case stops at the first match.
        if let Some(f) = &needles {
            let mut any = false;
            for idx in 0..dex.str_n as u32 {
                if let Some(sb) = dex.string_bytes(idx) {
                    if crate::findrefs::bytes_contain(sb, f) {
                        any = true;
                        break;
                    }
                }
            }
            if !any {
                return;
            }
        }
        for ci in 0..dex.cls_n {
            let Some((ty, _, _, cdo, _)) = dex.class_def_parts(ci) else {
                continue;
            };
            let Some(methods) = dex.methods_of(cdo as usize) else {
                continue;
            };
            let mut local = 0usize;
            for (_midx, _acc, code_off) in methods {
                if code_off == 0 || code_off as usize + 16 > dex.d.len() {
                    continue;
                }
                let insns = u32::from_le_bytes([
                    dex.d[code_off as usize + 12],
                    dex.d[code_off as usize + 13],
                    dex.d[code_off as usize + 14],
                    dex.d[code_off as usize + 15],
                ]) as usize;
                let start = code_off as usize + 16;
                let end = (start + 2 * insns).min(dex.d.len());
                if start >= end {
                    continue;
                }
                ddc_dex::insn::scan_instructions(&dex.d[start..end], &mut |op, pc, bytes| {
                    if op != 0x1a && op != 0x1b {
                        return;
                    }
                    let lo = 2 * (pc + 1);
                    let idx = if op == 0x1b && lo + 4 <= bytes.len() {
                        u32::from_le_bytes([bytes[lo], bytes[lo + 1], bytes[lo + 2], bytes[lo + 3]])
                    } else if lo + 2 <= bytes.len() {
                        u16::from_le_bytes([bytes[lo], bytes[lo + 1]]) as u32
                    } else {
                        return;
                    };
                    let Some(sb) = dex.string_bytes(idx) else {
                        return;
                    };
                    let hit = match &needles {
                        Some(f) => crate::findrefs::bytes_contain(sb, f),
                        None => true,
                    };
                    if hit {
                        local += 1;
                    }
                });
            }
            if local > 0 {
                let name = dex.class_name(ty).replace('/', ".");
                if !name.is_empty() {
                    *by_class.entry(name).or_default() += local;
                }
            }
        }
    })?;
    Ok(by_class.into_iter().collect())
}

/// Windowed string-table scan: emits `[offset, offset+limit)` and returns
/// the total match count. Only the window's rows are decoded/quoted, and
/// `--with-locations` collects users only for window strings and only for
/// images that intersect the window.
pub fn string_scan_window(
    input: &Path,
    filter: Option<&str>,
    with_locations: bool,
    dex_filters: &[String],
    offset: usize,
    limit: usize,
    mut emit: impl FnMut(StringRow),
) -> Result<usize> {
    let mut base = 0usize;
    let needles = filter.map(crate::findrefs::byte_needles);
    let each: ImageEachFn = if with_locations {
            for_each_image
        } else {
            for_each_string_image
        };
    each(&input.to_path_buf(), dex_filters, &mut |label, image| {
        let dex_name = label
            .rsplit_once('!')
            .map(|(_, e)| e)
            .unwrap_or(label)
            .to_string();
        let Ok(dex) = RawDex::parse(image) else {
            return;
        };
        // 单遍扫描字符串表：只保留窗口内的索引（不再为全表建 1.2M 项
        // BTreeMap——窗口查询的成本降到纯 memmem）。
        let local_lo = offset.saturating_sub(base);
        let local_hi = if limit == 0 {
            usize::MAX
        } else {
            offset.saturating_add(limit).saturating_sub(base)
        };
        let mut window_idxs: Vec<u32> = Vec::new();
        let mut count = 0usize;
        for idx in 0..dex.str_n as u32 {
            let Some(bytes) = dex.string_bytes(idx) else {
                continue;
            };
            let hit = match &needles {
                Some(f) => crate::findrefs::bytes_contain(bytes, f),
                None => true,
            };
            if !hit {
                continue;
            }
            if count >= local_lo && (count < local_hi) {
                window_idxs.push(idx);
            }
            // 继续计数：total 需要本映像的全部命中数（不能提前退出）。
            count += 1;
        }
        let in_window = !window_idxs.is_empty();
        let mut users: std::collections::HashMap<u32, Vec<String>> =
            std::collections::HashMap::default();
        if with_locations && in_window {
            let wanted: std::collections::HashSet<u32> = window_idxs.iter().copied().collect();
            for ci in 0..dex.cls_n {
                let Some((ty, _, _, cdo, _)) = dex.class_def_parts(ci) else {
                    continue;
                };
                let class = dex.class_name(ty);
                let Some(methods) = dex.methods_of(cdo as usize) else {
                    continue;
                };
                for (midx, _acc, code_off) in methods {
                    if code_off == 0 || code_off as usize + 16 > dex.d.len() {
                        continue;
                    }
                    let insns = u32::from_le_bytes([
                        dex.d[code_off as usize + 12],
                        dex.d[code_off as usize + 13],
                        dex.d[code_off as usize + 14],
                        dex.d[code_off as usize + 15],
                    ]) as usize;
                    let start = code_off as usize + 16;
                    let end = (start + 2 * insns).min(dex.d.len());
                    if start >= end {
                        continue;
                    }
                    // owner 惰性构造：大多数方法不引用窗口字符串。
                    let mut owner: Option<String> = None;
                    ddc_dex::insn::scan_instructions(&dex.d[start..end], &mut |op, _pc, bytes| {
                        if op != 0x1a && op != 0x1b {
                            return;
                        }
                        let lo = 2 * (_pc + 1);
                        let idx = if op == 0x1b && lo + 4 <= bytes.len() {
                            u32::from_le_bytes([
                                bytes[lo],
                                bytes[lo + 1],
                                bytes[lo + 2],
                                bytes[lo + 3],
                            ])
                        } else if lo + 2 <= bytes.len() {
                            u16::from_le_bytes([bytes[lo], bytes[lo + 1]]) as u32
                        } else {
                            return;
                        };
                        if !wanted.contains(&idx) {
                            return;
                        }
                        let o = owner.get_or_insert_with(|| {
                            dex.method_parts(midx)
                                .map(|(_, p, nb)| {
                                    format!(
                                        "{} {}{}",
                                        class,
                                        decode_mutf8_lossy(nb),
                                        dex.proto_desc(p)
                                    )
                                })
                                .unwrap_or_default()
                        });
                        users.entry(idx).or_default().push(o.clone());
                    });
                }
            }
        }
        if in_window {
            for &idx in &window_idxs {
                let s = decode_mutf8_lossy(dex.string_bytes(idx).unwrap_or(&[]));
                let used_by = match users.get(&idx) {
                    Some(u) if !u.is_empty() => {
                        let mut uniq: Vec<&str> = u.iter().map(|m| m.as_str()).collect();
                        uniq.dedup();
                        uniq.iter().map(|m| m.to_string()).collect()
                    }
                    _ => Vec::new(),
                };
                emit(StringRow {
                    dex: dex_name.clone(),
                    string: format!("{s:?}"),
                    used_by,
                });
            }
        }
        base += count;
    })?;
    Ok(base)
}

#[derive(Debug, Clone, serde::Serialize)]
#[derive(xyz_rust::XyzOutput)]
pub struct MemberRow {
    pub dex: String,
    /// `method` | `field`.
    pub kind: String,
    /// Owner class, dotted form.
    pub class: String,
    /// Rendered member: `name(descriptor)` or `name type`.
    pub member: String,
}

/// Members (`--method`/`--field` filter, optional NAME substring and
/// `--class` scope). Same scan as the CLI text path, dotted owner names.
/// Shared member filters (name substring, owner-class scope, kind).
struct MemberFilter<'a> {
    name: Option<&'a str>,
    class: Option<&'a str>,
    fuzzy_class: bool,
    kind_want: Option<&'a str>,
}

impl MemberFilter<'_> {
    fn name_ok(&self, nb: &[u8]) -> bool {
        match self.name {
            Some(n) => crate::findrefs::bytes_contain_str(nb, n),
            None => true,
        }
    }

    fn class_ok(&self, cb: &[u8]) -> bool {
        match self.class {
            None => true,
            Some(c) => {
                let hay = crate::findrefs::decode_mutf8_lossy(cb);
                let hay = hay.trim_start_matches('L').trim_end_matches(';');
                let needle = c.replace('.', "/");
                if self.fuzzy_class {
                    hay.contains(&needle)
                } else {
                    hay == needle
                }
            }
        }
    }
}

/// Visit matching members in scan order. `wanted(global_index)` decides
/// whether the (potentially expensive) row text is formatted for that
/// match — windowed queries format only their page. Returns the total
/// number of matches (so callers can report `total` without retaining
/// every row).
fn member_scan(
    input: &Path,
    filter: &MemberFilter<'_>,
    dex_filters: &[String],
    mut wanted: impl FnMut(usize) -> bool,
    mut emit: impl FnMut(MemberRow),
) -> Result<usize> {
    let mut index = 0usize;
    for_each_member_image(
        &input.to_path_buf(),
        dex_filters,
        &mut |label, image| {
        let dex_name = label
            .rsplit_once('!')
            .map(|(_, e)| e)
            .unwrap_or(label)
            .to_string();
        let Ok(dex) = RawDex::parse(image) else {
            return;
        };
        if filter.kind_want != Some("field") {
            for mi in 0..dex.method_n as u32 {
                let Some((cidx, proto, nb)) = dex.method_parts(mi) else {
                    continue;
                };
                if !filter.name_ok(nb) {
                    continue;
                }
                let Some(cb) = dex.type_bytes(cidx) else {
                    continue;
                };
                if !filter.class_ok(cb) {
                    continue;
                }
                if wanted(index) {
                    emit(MemberRow {
                        dex: dex_name.clone(),
                        kind: "method".to_string(),
                        class: dex.class_name(cidx).replace('/', "."),
                        member: format!("{}{}", decode_mutf8_lossy(nb), dex.proto_desc(proto)),
                    });
                }
                index += 1;
            }
        }
        if filter.kind_want != Some("method") {
            for fi in 0..dex.field_n as u32 {
                let Some((cidx, nb, tb)) = dex.field_parts(fi) else {
                    continue;
                };
                if !filter.name_ok(nb) {
                    continue;
                }
                let Some(cb) = dex.type_bytes(cidx) else {
                    continue;
                };
                if !filter.class_ok(cb) {
                    continue;
                }
                if wanted(index) {
                    emit(MemberRow {
                        dex: dex_name.clone(),
                        kind: "field".to_string(),
                        class: dex.class_name(cidx).replace('/', "."),
                        member: format!("{} {}", decode_mutf8_lossy(nb), decode_mutf8_lossy(tb)),
                    });
                }
                index += 1;
            }
        }
        },
    )?;
    Ok(index)
}

/// Windowed member rows: emits `[offset, offset+limit)` (limit 0 =
/// unlimited) and returns the total match count. Only the window's rows
/// are formatted, so a `--limit 20` scan stays close to scan cost.
#[allow(clippy::too_many_arguments)]
pub fn member_scan_window(
    input: &Path,
    name: Option<&str>,
    class: Option<&str>,
    fuzzy_class: bool,
    kind_want: Option<&str>,
    dex_filters: &[String],
    offset: usize,
    limit: usize,
    emit: impl FnMut(MemberRow),
) -> Result<usize> {
    let filter = MemberFilter {
        name,
        class,
        fuzzy_class,
        kind_want,
    };
    member_scan(input, &filter, dex_filters, |idx| {
        idx >= offset && (limit == 0 || idx < offset + limit)
    }, emit)
}

/// Owner-class counts for the member summary. Counts by type index per
/// image (no per-row allocation), then resolves one name per class with
/// hits — a 2.5M-member scan never formats a member string.
pub fn member_class_counts(
    input: &Path,
    name: Option<&str>,
    class: Option<&str>,
    fuzzy_class: bool,
    kind_want: Option<&str>,
    dex_filters: &[String],
) -> Result<Vec<(String, usize)>> {
    let filter = MemberFilter {
        name,
        class,
        fuzzy_class,
        kind_want,
    };
    let mut by_name: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for_each_member_image(
        &input.to_path_buf(),
        dex_filters,
        &mut |_label, image| {
        let Ok(dex) = RawDex::parse(image) else {
            return;
        };
        let mut counts = vec![0usize; dex.type_n];
        let mut any = false;
        if filter.kind_want != Some("field") {
            for mi in 0..dex.method_n as u32 {
                let Some((cidx, _proto, nb)) = dex.method_parts(mi) else {
                    continue;
                };
                if !filter.name_ok(nb) {
                    continue;
                }
                let Some(cb) = dex.type_bytes(cidx) else {
                    continue;
                };
                if !filter.class_ok(cb) {
                    continue;
                }
                counts[cidx as usize] += 1;
                any = true;
            }
        }
        if filter.kind_want != Some("method") {
            for fi in 0..dex.field_n as u32 {
                let Some((cidx, nb, _tb)) = dex.field_parts(fi) else {
                    continue;
                };
                if !filter.name_ok(nb) {
                    continue;
                }
                let Some(cb) = dex.type_bytes(cidx) else {
                    continue;
                };
                if !filter.class_ok(cb) {
                    continue;
                }
                counts[cidx as usize] += 1;
                any = true;
            }
        }
        if any {
            for (idx, &c) in counts.iter().enumerate() {
                if c > 0 {
                    let name = dex.class_name(idx as u32).replace('/', ".");
                    if !name.is_empty() {
                        *by_name.entry(name).or_default() += c;
                    }
                }
            }
        }
        },
    )?;
    Ok(by_name.into_iter().collect())
}

/// Hierarchy rows for a class (dotted or slashed `target`): its own
/// `class`/`extends`/`implements` rows plus every direct `sub`/`impl`.
pub fn hierarchy_rows(input: &Path, target: &str, dex_filters: &[String]) -> Result<Vec<RelationRow>> {
    let display = target.to_string();
    let target = target.replace('.', "/");
    let mut rows: Vec<RelationRow> = Vec::new();
    let mut found = false;
    for_each_prefix_image(
        &input.to_path_buf(),
        dex_filters,
        crate::inputs::type_descriptor_prefix_needed,
        &mut |label, image| {
        let dex_name = label
            .rsplit_once('!')
            .map(|(_, e)| e)
            .unwrap_or(label)
            .to_string();
        let Ok(dex) = RawDex::parse(image) else {
            return;
        };
        let mut target_idx: Option<u32> = None;
        for ti in 0..dex.type_n as u32 {
            if let Some(b) = dex.type_bytes(ti) {
                let n = decode_mutf8_lossy(b);
                let plain = n.trim_start_matches('L').trim_end_matches(';');
                if plain == target {
                    target_idx = Some(ti);
                    found = true;
                    break;
                }
            }
        }
        fn plain_of(desc: &str) -> &str {
            desc.trim_start_matches('L').trim_end_matches(';')
        }
        for ci in 0..dex.cls_n {
            let Some((ty, sup, iface_off, _, _)) = dex.class_def_parts(ci) else {
                continue;
            };
            let self_name = dex.class_name(ty);
            let tidx = target_idx.unwrap_or(u32::MAX);
            let mut push = |rel: &str, cls: String| {
                rows.push(RelationRow {
                    dex: dex_name.clone(),
                    relation: rel.to_string(),
                    class: cls,
                });
            };
            if target_idx.is_some() && ty == tidx {
                push("class", self_name.clone());
                if sup != u32::MAX {
                    if let Some(sb) = dex.type_bytes(sup) {
                        push("extends", decode_mutf8_lossy(sb));
                    }
                }
                for it in dex.interface_types(iface_off) {
                    if let Some(ib) = dex.type_bytes(it) {
                        push("implements", decode_mutf8_lossy(ib));
                    }
                }
            }
            if sup != u32::MAX && sup == tidx {
                push("sub", self_name.clone());
            }
            for it in dex.interface_types(iface_off) {
                if it == tidx {
                    push("impl", self_name.clone());
                }
            }
            if target_idx.is_none() {
                if sup != u32::MAX {
                    if let Some(sb) = dex.type_bytes(sup) {
                        if plain_of(&decode_mutf8_lossy(sb)) == target {
                            found = true;
                            push("sub", self_name.clone());
                        }
                    }
                }
                for it in dex.interface_types(iface_off) {
                    if let Some(ib) = dex.type_bytes(it) {
                        if plain_of(&decode_mutf8_lossy(ib)) == target {
                            found = true;
                            push("impl", self_name.clone());
                        }
                    }
                }
                if plain_of(&self_name) == target {
                    found = true;
                    push("class", self_name.clone());
                }
            }
        }
    })?;
    if !found && rows.is_empty() {
        bail!(
            crate::lang::bif!(
                "class {0} not found in the selected image(s) (try `ddc listclasses <input> <pattern>`)",
                "类 {0} 在所选镜像中不存在（可试 `ddc listclasses <输入> <模式>`）";
                display
            )
        );
    }
    Ok(rows)
}

/// Raw bytecode listing of a class (or one method's overloads).
pub fn disasm_text(input: &Path, target: &str, dex_filters: &[String]) -> Result<String> {
    let class_full = target.replace('.', "/");
    // A method part may carry a full descriptor (`parse(Ljava/lang/String;)V`)
    // for exact-overload selection; descriptors never contain '.', so the
    // last dot still separates class from method.
    let split = target
        .rsplit_once('.')
        .filter(|(c, m)| !c.is_empty() && !m.is_empty())
        .map(|(c, m)| (c.replace('.', "/"), m.to_string()));
    let mut out = String::new();
    let mut found_class = false;
    let mut wanted_method: Option<String> = None;
    let mut printed_method = false;
    let mut scan = |label: &str, image: &[u8]| {
        let Ok(dex) = RawDex::parse(image) else {
            return;
        };
        let (ci, method_want) = match dex.find_class(&class_full) {
            Some(ci) => {
                found_class = true;
                (ci, None)
            }
            None => {
                let Some((c, m)) = &split else { return };
                match dex.find_class(c) {
                    Some(ci) => {
                        found_class = true;
                        wanted_method = Some(m.clone());
                        (ci, Some(m.clone()))
                    }
                    None => return,
                }
            }
        };
        let (ty, _, _, cdo, _) = dex.class_def_parts(ci).unwrap();
        out.push_str(&format!("// {} {}\n", label, dex.class_name(ty)));
        let Some(methods) = dex.methods_of(cdo as usize) else {
            return;
        };
        for (midx, _acc, code_off) in methods {
            if code_off == 0 {
                continue;
            }
            let owner = dex
                .method_parts(midx)
                .map(|(_, p, nb)| format!("{}{}", decode_mutf8_lossy(nb), dex.proto_desc(p)))
                .unwrap_or_default();
            if let Some(w) = &method_want {
                let matched = if w.contains('(') {
                    owner == *w
                } else {
                    owner.starts_with(w.as_str())
                };
                if !matched {
                    continue;
                }
            }
            printed_method = true;
            out.push_str(&format!("  {}:\n", owner));
            if code_off as usize + 16 > dex.d.len() {
                continue;
            }
            let insns = u32::from_le_bytes([
                dex.d[code_off as usize + 12],
                dex.d[code_off as usize + 13],
                dex.d[code_off as usize + 14],
                dex.d[code_off as usize + 15],
            ]) as usize;
            let start = code_off as usize + 16;
            let end = (start + 2 * insns).min(dex.d.len());
            let (insns, payloads) = ddc_dex::insn::decode_all(&dex.d[start..end]);
            for i in &insns {
                out.push_str(&format!(
                    "    {:04x}: {} {}\n",
                    2 * i.pc,
                    ddc_dex::insn::op_name(i.op),
                    crate::browse::fmt_operands(&dex, i, &payloads)
                ));
            }
        }
    };

    // Which images define the class? A class-descriptor prefix is enough to
    // answer that; only those images are then inflated in full. On a 50-dex
    // APK this skips tens of unrelated full decompressions.
    let mut wanted_classes = vec![class_full.as_str()];
    if let Some((c, _)) = &split {
        wanted_classes.push(c.as_str());
    }
    let mut labels: std::collections::HashSet<String> = std::collections::HashSet::new();
    for_each_prefix_image(
        &input.to_path_buf(),
        dex_filters,
        crate::inputs::class_names_prefix_needed,
        &mut |label, image| {
            let Ok(dex) = RawDex::parse(image) else {
                return;
            };
            if wanted_classes
                .iter()
                .any(|c| dex.find_class(c).is_some())
            {
                labels.insert(label.to_string());
            }
        },
    )?;
    if labels.is_empty() {
        // Keep the full scan (and its exact not-found error) when the prefix
        // pass saw no match.
        for_each_image(&input.to_path_buf(), dex_filters, &mut scan)?;
    } else {
        for_each_image_named(&input.to_path_buf(), dex_filters, &labels, &mut scan)?;
    }
    if !found_class {
        bail!(
            crate::lang::bif!(
                "class {0} not found in the selected image(s) (try `ddc listclasses <input> <pattern>`)",
                "类 {0} 在所选镜像中不存在（可试 `ddc listclasses <输入> <模式>`）";
                target
            )
        );
    }
    if let Some(m) = &wanted_method {
        if !printed_method {
            bail!(
                crate::lang::bif!(
                    "method {0} not found in {1}",
                    "方法 {0} 在 {1} 中不存在";
                    m,
                    split.as_ref().map(|(c, _)| c.as_str()).unwrap_or(&class_full)
                )
            );
        }
    }
    Ok(out)
}

// ---- findrefs ----------------------------------------------------------------

/// Cross-references to a query (string/type/method/field; `class` is
/// accepted as an alias of `type`). Mirrors the
/// `findrefs` command: the pipelined scan drops each image after
/// scanning, so resident memory stays bounded.
pub(crate) fn find_query(
    kind: &str,
    query: &str,
    class: Option<&str>,
    fuzzy_class: bool,
) -> Result<crate::findrefs::FindQuery> {
    Ok(match kind {
        "string" => crate::findrefs::FindQuery::String(query.to_string()),
        // `class` is the plain-word alias for `type` — same search.
        "type" | "class" => crate::findrefs::FindQuery::Type(query.to_string()),
        "method" => crate::findrefs::FindQuery::Method {
            class: class.map(|c| c.to_string()),
            name: query.to_string(),
            fuzzy_class,
        },
        "field" => crate::findrefs::FindQuery::Field {
            class: class.map(|c| c.to_string()),
            name: query.to_string(),
            fuzzy_class,
        },
        other => bail!("findrefs: unknown kind {other:?} (string|type|class|method|field)"),
    })
}

fn ref_row_from_hit(h: &crate::findrefs::Hit) -> RefRow {
    RefRow {
        dex: h.dex.clone(),
        kind: h.insn.to_string(),
        class: h.class.replace('/', "."),
        method: h.method.clone(),
        targets: h.targets.clone(),
    }
}

pub fn ref_rows(
    input: &Path,
    kind: &str,
    query: &str,
    class: Option<&str>,
    fuzzy_class: bool,
    dex_filters: &[String],
) -> Result<Vec<RefRow>> {
    let q = find_query(kind, query, class, fuzzy_class)?;
    let (hits, _t) = crate::findrefs_scan(input, &q, dex_filters)?;
    // findrefs_scan already orders by class/method/dex; re-sorting here would
    // drop the dex tiebreaker and make duplicate definitions nondeterministic.
    Ok(hits.iter().map(ref_row_from_hit).collect())
}

/// Summary counts for the frontends: one owner-class count per hit, without
/// materializing a row per hit.
pub fn ref_class_counts(
    input: &Path,
    kind: &str,
    query: &str,
    class: Option<&str>,
    fuzzy_class: bool,
    dex_filters: &[String],
) -> Result<std::collections::BTreeMap<String, usize>> {
    let q = find_query(kind, query, class, fuzzy_class)?;
    let (counts, _t) = crate::findrefs_scan_counts(input, &q, dex_filters)?;
    Ok(counts)
}

/// Paged full rows: the first `want` rows in output order plus the total hit
/// count, without building every row.
pub fn ref_rows_window(
    input: &Path,
    kind: &str,
    query: &str,
    class: Option<&str>,
    fuzzy_class: bool,
    dex_filters: &[String],
    want: usize,
) -> Result<(Vec<RefRow>, usize)> {
    let q = find_query(kind, query, class, fuzzy_class)?;
    let (hits, total, _t) = crate::findrefs_scan_top(input, &q, dex_filters, want)?;
    Ok((hits.iter().map(ref_row_from_hit).collect(), total))
}

// Keep the positional-output probe helpers referenced (they mirror the
// CLI's positional-output logic; unused in this module but part of the
// shared contract documentation).
#[allow(dead_code)]
fn _positional_output_probe(a: &Path, b: &Path) -> bool {
    is_dex_ext(a) || (a.exists() && !a.is_dir()) || (a.is_dir() && dir_has_dex_files(b))
}

#[cfg(test)]
mod tree_tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn render(rows: &[TreeRow]) -> Vec<String> {
        rows.iter()
            .map(|r| match r.kind {
                TreeKind::Group => format!("{}/ ({})", r.path, r.count),
                TreeKind::Leaf => r.path.clone(),
            })
            .collect()
    }

    #[test]
    fn adaptive_frontier_groups_common_prefixes() {
        // The example from the design discussion: the branching nodes a.b
        // and q become package rows (transitive counts), the rest are leaves.
        let n = names(&[
            "a.b.c1", "a.b.c2", "a.b.c3", "q.m.a", "q.n.a", "x", "y", "z",
        ]);
        let rows = class_tree(&n, None, None);
        assert_eq!(
            render(&rows),
            vec!["a.b/ (3)", "q/ (2)", "x", "y", "z"]
        );
    }

    #[test]
    fn fixed_depth_aggregates_at_the_requested_level() {
        let n = names(&[
            "a.b.c1", "a.b.c2", "a.b.c3", "q.m.a", "q.n.a", "x", "y", "z",
        ]);
        // depth 1 = one segment below the scope: a and q become groups;
        // root leaves stay leaves (they cannot be descended).
        let rows = class_tree(&n, None, Some(1));
        assert_eq!(render(&rows), vec!["a/ (3)", "q/ (2)", "x", "y", "z"]);
    }

    #[test]
    fn prefix_scopes_the_view() {
        let n = names(&["a.b.c1", "a.b.c2", "a.b.c3", "q.m.a"]);
        let rows = class_tree(&n, Some("a.b"), None);
        assert_eq!(render(&rows), vec!["a.b.c1", "a.b.c2", "a.b.c3"]);
        // A prefix with no matches yields nothing.
        assert!(class_tree(&n, Some("nope"), None).is_empty());
    }

    #[test]
    fn class_and_package_sharing_a_name_both_appear() {
        let n = names(&["a.b", "a.b.c"]);
        let rows = class_tree(&n, None, None);
        assert_eq!(render(&rows), vec!["a.b", "a.b.c"]);
        // Within the a.b scope the class itself is a row plus its child.
        let rows = class_tree(&n, Some("a.b"), None);
        assert_eq!(render(&rows), vec!["a.b", "a.b.c"]);
    }

    #[test]
    fn normalize_prefix_accepts_both_separators() {
        assert_eq!(normalize_class_prefix("com/example/"), "com.example");
        assert_eq!(normalize_class_prefix(" com.example. "), "com.example");
        assert_eq!(normalize_class_prefix(""), "");
    }

    #[test]
    fn window_rows_reports_resume_metadata() {
        let rows: Vec<u32> = (0..10).collect();
        let p = window_rows(rows, 4, 3);
        assert_eq!(p.rows, vec![4, 5, 6]);
        assert_eq!(p.total, 10);
        assert_eq!(p.next_offset, Some(7));
        let p = window_rows((0..10).collect(), 8, 3);
        assert_eq!(p.rows, vec![8, 9]);
        assert_eq!(p.next_offset, None);
        // Past the end: empty page, no next.
        let p = window_rows((0..10).collect(), 20, 3);
        assert!(p.rows.is_empty());
        assert_eq!(p.next_offset, None);
        // limit 0 = unlimited.
        let p = window_rows((0..10).collect(), 7, 0);
        assert_eq!(p.rows, vec![7, 8, 9]);
        assert_eq!(p.next_offset, None);
    }
}
