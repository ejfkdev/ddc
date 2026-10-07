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
use crate::browse::for_each_image;
use crate::findrefs::{decode_mutf8_lossy, RawDex};

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
    let files = expand_inputs(&[input.to_path_buf()])?;
    let parsed = crate::parse_images(collect_images(&files)?)?;
    let mut total_classes = 0u64;
    for (label, dex) in &parsed {
        let classes = dex.class_defs.len();
        total_classes += classes as u64;
        report.images.push(ImageCounts {
            image: label.clone(),
            dex_version: dex.version.clone(),
            classes: classes as u64,
            methods: dex.method_count() as u64,
            fields: dex.field_count() as u64,
            strings: dex.string_count() as u64,
        });
    }
    report.total_images = parsed.len() as u64;
    report.total_classes = total_classes;
    Ok(report)
}

/// Class names, dotted form, deduped in pool order; `pattern` is a
/// case-insensitive substring filter.
pub fn class_names(
    input: &Path,
    pattern: Option<&str>,
    dex_filters: &[String],
) -> Result<Vec<String>> {
    let files = expand_inputs(&[input.to_path_buf()])?;
    let images = filter_images_by_dex(collect_images(&files)?, dex_filters)?;
    let handles: Vec<_> = images
        .into_iter()
        .map(|img| {
            std::thread::spawn(move || {
                let raw = match img.method {
                    crate::ZipMethod::Stored => img.data.bytes()[img.range].to_vec(),
                    crate::ZipMethod::Deflate => {
                        crate::inputs::inflate_until(img.data.bytes(), img.range.clone(), |out| {
                            match crate::inputs::scan_prefix_needed(out) {
                                None => crate::inputs::PrefixStep::Continue(0),
                                Some(needed) => {
                                    if out.len() >= needed {
                                        crate::inputs::PrefixStep::Abort
                                    } else {
                                        crate::inputs::PrefixStep::Continue(needed - out.len())
                                    }
                                }
                            }
                        })
                        .unwrap_or_default()
                    }
                };
                ddc_dex::DexFile::class_names_from_image(&raw)
            })
        })
        .collect();
    let mut all: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for h in handles {
        if let Ok(names) = h.join() {
            for name in names {
                if seen.insert(name.clone()) {
                    all.push(name);
                }
            }
        }
    }
    let shown: Vec<String> = match pattern {
        Some(p) => {
            let lp = p.to_ascii_lowercase();
            all.into_iter()
                .filter(|n| n.to_ascii_lowercase().contains(&lp))
                .collect()
        }
        None => all,
    };
    Ok(shown.into_iter().map(|n| n.replace('/', ".")).collect())
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
    for_each_image(&input.to_path_buf(), &[], &mut |label, image| {
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
pub fn string_rows(
    input: &Path,
    filter: Option<&str>,
    with_locations: bool,
    dex_filters: &[String],
) -> Result<Vec<StringRow>> {
    let mut rows: Vec<StringRow> = Vec::new();
    for_each_image(&input.to_path_buf(), dex_filters, &mut |label, image| {
        let dex_name = label
            .rsplit_once('!')
            .map(|(_, e)| e)
            .unwrap_or(label)
            .to_string();
        let Ok(dex) = RawDex::parse(image) else {
            return;
        };
        let needle = filter.map(str::as_bytes);
        let matched: std::collections::BTreeMap<u32, ()> = (0..dex.str_n as u32)
            .filter(|&idx| {
                let Some(bytes) = dex.string_bytes(idx) else {
                    return false;
                };
                match needle {
                    Some(f) => bytes.len() >= f.len() && bytes.windows(f.len()).any(|w| w == f),
                    None => true,
                }
            })
            .map(|idx| (idx, ()))
            .collect();
        let mut users: std::collections::HashMap<u32, Vec<String>> =
            std::collections::HashMap::default();
        if with_locations {
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
                    let owner = dex
                        .method_parts(midx)
                        .map(|(_, p, nb)| {
                            format!("{} {}{}", class, decode_mutf8_lossy(nb), dex.proto_desc(p))
                        })
                        .unwrap_or_default();
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
                        if matched.contains_key(&idx) {
                            users.entry(idx).or_default().push(owner.clone());
                        }
                    });
                }
            }
        }
        for &idx in matched.keys() {
            let s = decode_mutf8_lossy(dex.string_bytes(idx).unwrap_or(&[]));
            let used_by = match users.get(&idx) {
                Some(u) if !u.is_empty() => {
                    let mut uniq: Vec<&str> = u.iter().map(|m| m.as_str()).collect();
                    uniq.dedup();
                    uniq.iter().map(|m| m.to_string()).collect()
                }
                _ => Vec::new(),
            };
            rows.push(StringRow {
                dex: dex_name.clone(),
                string: format!("{s:?}"),
                used_by,
            });
        }
    })?;
    Ok(rows)
}

/// Hierarchy rows for a class (dotted or slashed `target`): its own
/// `class`/`extends`/`implements` rows plus every direct `sub`/`impl`.
pub fn hierarchy_rows(input: &Path, target: &str, dex_filters: &[String]) -> Result<Vec<RelationRow>> {
    let target = target.replace('.', "/");
    let mut rows: Vec<RelationRow> = Vec::new();
    for_each_image(&input.to_path_buf(), dex_filters, &mut |label, image| {
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
                            push("sub", self_name.clone());
                        }
                    }
                }
                for it in dex.interface_types(iface_off) {
                    if let Some(ib) = dex.type_bytes(it) {
                        if plain_of(&decode_mutf8_lossy(ib)) == target {
                            push("impl", self_name.clone());
                        }
                    }
                }
                if plain_of(&self_name) == target {
                    push("class", self_name.clone());
                }
            }
        }
    })?;
    Ok(rows)
}

/// Raw bytecode listing of a class (or one method's overloads).
pub fn disasm_text(input: &Path, target: &str, dex_filters: &[String]) -> Result<String> {
    let class_full = target.replace('.', "/");
    let split = target
        .rsplit_once('.')
        .filter(|(c, m)| !c.is_empty() && !m.is_empty() && !m.contains('('))
        .map(|(c, m)| (c.replace('.', "/"), m.to_string()));
    let mut out = String::new();
    for_each_image(&input.to_path_buf(), dex_filters, &mut |label, image| {
        let Ok(dex) = RawDex::parse(image) else {
            return;
        };
        let (ci, method_want) = match dex.find_class(&class_full) {
            Some(ci) => (ci, None),
            None => {
                let Some((c, m)) = &split else { return };
                match dex.find_class(c) {
                    Some(ci) => (ci, Some(m.clone())),
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
                if !owner.starts_with(w.as_str()) {
                    continue;
                }
            }
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
    })?;
    Ok(out)
}

// ---- findrefs ----------------------------------------------------------------

/// Cross-references to a query (string/type/method/field; `class` is
/// accepted as an alias of `type`). Mirrors the
/// `findrefs` command: the pipelined scan drops each image after
/// scanning, so resident memory stays bounded.
pub fn ref_rows(
    input: &Path,
    kind: &str,
    query: &str,
    class: Option<&str>,
    fuzzy_class: bool,
    dex_filters: &[String],
) -> Result<Vec<RefRow>> {
    let q = match kind {
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
    };
    let (hits, _t) = crate::findrefs_scan(input, &q, dex_filters)?;
    let mut rows: Vec<RefRow> = hits
        .iter()
        .map(|h| RefRow {
            dex: h.dex.clone(),
            kind: h.insn.to_string(),
            class: h.class.replace('/', "."),
            method: h.method.clone(),
            targets: h.targets.clone(),
        })
        .collect();
    rows.sort_by(|a, b| a.class.cmp(&b.class).then(a.method.cmp(&b.method)));
    Ok(rows)
}

// Keep the positional-output probe helpers referenced (they mirror the
// CLI's positional-output logic; unused in this module but part of the
// shared contract documentation).
#[allow(dead_code)]
fn _positional_output_probe(a: &Path, b: &Path) -> bool {
    is_dex_ext(a) || (a.exists() && !a.is_dir()) || (a.is_dir() && dir_has_dex_files(b))
}
