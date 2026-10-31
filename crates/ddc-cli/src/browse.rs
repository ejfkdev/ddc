//! The browse/locate subcommands: strings, members, hierarchy, largest,
//! getmethod, disasm, callers, pkg. All ride the RawDex zero-materialization
//! path (raw images, prefix-friendly), mirroring jadx's quick-lookup tools.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

use crate::findrefs::{decode_mutf8_lossy, RawDex};
use crate::inputs::{
    collect_images, expand_inputs, filter_images_by_dex, inflate_images, inflate_prefix_images,
    parse_images, PrefixNeed,
};
use crate::lang::{bi, bif};

/// Parse every image (parallel), handing each (label, image) to `f`.
/// Bounded like the findrefs pipeline; progressive commands never hold all
/// images at peak — they fold each one and drop it.
pub(crate) fn for_each_image(
    input: &PathBuf,
    dex_filters: &[String],
    f: &mut dyn FnMut(&str, &[u8]),
) -> Result<()> {
    for_each_image_impl(None, input, dex_filters, f)
}

/// Same contract as [`for_each_image`], but deflated images stop at the
/// prefix `need` asks for (see `type_descriptor_prefix_needed`). Stored
/// images are read in full (they are already mapped).
pub(crate) fn for_each_prefix_image(
    input: &PathBuf,
    dex_filters: &[String],
    need: PrefixNeed,
    f: &mut dyn FnMut(&str, &[u8]),
) -> Result<()> {
    for_each_image_impl(Some(need), input, dex_filters, f)
}

fn for_each_image_impl(
    prefix: Option<PrefixNeed>,
    input: &PathBuf,
    dex_filters: &[String],
    f: &mut dyn FnMut(&str, &[u8]),
) -> Result<()> {
    let files = expand_inputs(std::slice::from_ref(input))?;
    let mut images = filter_images_by_dex(collect_images(&files)?, dex_filters)?;
    // 自然顺序输出的分波解压：每波最多 8 个映像，且本波解压后总量
    // 控制在 64MB 左右（单个大映像独占一波）。回调顺序与输入顺序一致
    // ——翻页契约要求可文档化的稳定序。固定 8 个时一波可能同时驻留
    // 上百 MB 的 dex，查询峰值随多 dex 大包线性上升。
    const WAVE_MAX: usize = 8;
    const WAVE_BYTES: usize = 64 << 20;
    while !images.is_empty() {
        let mut take = 0usize;
        let mut bytes = 0usize;
        for img in &images {
            if take >= WAVE_MAX {
                break;
            }
            let hint = img.inflated_hint();
            if take > 0 && bytes.saturating_add(hint) > WAVE_BYTES {
                break;
            }
            bytes = bytes.saturating_add(hint);
            take += 1;
        }
        let take = take.clamp(1, images.len());
        let wave: Vec<_> = images.drain(..take).collect();
        let inflated = match prefix {
            Some(need) => inflate_prefix_images(wave, need)?,
            None => inflate_images(wave)?,
        };
        for (img, raw) in inflated {
            f(&img.label, &raw);
            drop(raw);
        }
    }
    Ok(())
}

/// Shared argument prelude: input + optional --dex filters. Positionals
/// beyond the first (the input) are surfaced as `rest` — several browse
/// commands take a class/method/package argument after the input.
pub(crate) struct Common {
    pub(crate) input: PathBuf,
    pub(crate) dex_filters: Vec<String>,
    pub(crate) rest: Vec<String>,
}

pub(crate) fn parse_common(args: &[String], cmd: &str) -> Result<Common> {
    let mut positionals: Vec<String> = Vec::new();
    let mut dex_filters: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-d" | "--dex" => {
                dex_filters.push(
                    args.get(i + 1)
                        .context(bi!("--dex needs a value", "--dex 需要一个值"))?
                        .to_string(),
                );
                i += 1;
            }
            a if a.starts_with('-') => bail!(
                "{}",
                bif!("{0}: unknown option {1}", "{0}：未知选项 {1}"; cmd, a)
            ),
            a => positionals.push(a.to_string()),
        }
        i += 1;
    }
    let input = positionals
        .first()
        .cloned()
        .context(bif!("{0} needs an input file", "{0} 需要输入文件"; cmd))?;
    Ok(Common {
        input: PathBuf::from(input),
        dex_filters,
        rest: positionals.into_iter().skip(1).collect(),
    })
}

// ---- shared --format plumbing (query commands in this module) ----------------

fn format_value(args: &[String], i: usize) -> anyhow::Result<String> {
    args.get(i + 1)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{}", crate::lang::pick("--format needs a value", "--format 需要一个值")))
}

// ---- strings ---------------------------------------------------------------

pub(crate) fn cmd_strings(args: &[String]) -> Result<()> {
    let mut filter: Option<String> = None;
    let mut with_loc = false;
    let mut rest: Vec<String> = Vec::new();
    let mut format = crate::OutFormat::Auto;
    let mut offset = 0usize;
    let mut limit = 0usize;
    let mut summary_explicit = false;
    let mut full = false;
    let mut prefix: Option<String> = None;
    let mut depth: Option<usize> = None;
    let mut sort_count = false;
    let mut samples = 0usize;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-f" | "--filter" => {
                filter = Some(
                    args.get(i + 1)
                        .context(bi!("--filter needs a value", "--filter 需要一个值"))?
                        .to_string(),
                );
                i += 1;
            }
            "--with-locations" => with_loc = true,
            "--summary" => summary_explicit = true,
            "--full" => full = true,
            "--prefix" => {
                let v = args
                    .get(i + 1)
                    .context(bi!("--prefix needs a value", "--prefix 需要一个值"))?;
                let norm = crate::api::normalize_class_prefix(v);
                prefix = if norm.is_empty() { None } else { Some(norm) };
                i += 1;
            }
            "--depth" => {
                let v = args
                    .get(i + 1)
                    .context(bi!("--depth needs a count", "--depth 需要一个层数"))?;
                let d: usize = v.parse().map_err(|_| {
                    anyhow::anyhow!(
                        "{}",
                        bif!(
                            "--depth expects a positive integer, got {0:?}",
                            "--depth 需要正整数，收到 {0:?}";
                            v
                        )
                    )
                })?;
                if d == 0 {
                    bail!("{}", bi!("--depth must be at least 1", "--depth 至少为 1"));
                }
                depth = Some(d);
                i += 1;
            }
            "--sort" => {
                let v = args
                    .get(i + 1)
                    .context(bi!("--sort needs a value", "--sort 需要一个值"))?;
                match v.as_str() {
                    "name" => sort_count = false,
                    "count" => sort_count = true,
                    other => bail!(
                        "{}",
                        bif!(
                            "--sort expects name|count, got {0:?}",
                            "--sort 需要 name|count，收到 {0:?}";
                            other
                        )
                    ),
                }
                i += 1;
            }
            "--format" => {
                format = crate::parse_format(&format_value(args, i)?)?;
                i += 1;
            }
            "--samples" => {
                samples = crate::parse_count(args.get(i + 1), "--samples")?;
                i += 1;
            }
            "--limit" => {
                limit = crate::parse_count(args.get(i + 1), "--limit")?;
                i += 1;
            }
            "--offset" => {
                offset = crate::parse_count(args.get(i + 1), "--offset")?;
                i += 1;
            }
            "-d" | "--dex" => {
                // parse_common owns -d/--dex, but this loop runs first —
                // forward both tokens so it can see them.
                rest.push(args[i].clone());
                rest.push(
                    args.get(i + 1)
                        .context(bi!("--dex needs a value", "--dex 需要一个值"))?
                        .clone(),
                );
                i += 1;
            }
            a if a.starts_with('-') => bail!(
                "{}",
                bif!("strings: unknown option {0}", "strings：未知选项 {0}"; a)
            ),
            a => rest.push(a.to_string()),
        }
        i += 1;
    }
    // --full 优先；未显式 --summary 时，摘要类参数仍视为误用。
    let summary = summary_explicit && !full;
    if !summary_explicit && !full && (prefix.is_some() || depth.is_some() || sort_count || samples > 0) {
        bail!(
            "{}",
            bi!(
                "--prefix/--depth/--sort/--samples only apply to --summary",
                "--prefix/--depth/--sort/--samples 仅适用于 --summary"
            )
        );
    }
    let common = parse_common(&rest, "strings")?;
    let rf = crate::resolve_format(format);

    if summary {
        // 字符串没有层次——按**引用方类**聚合：哪些包在用这些字符串。
        let mut counts = crate::api::string_owner_counts(
            &common.input,
            filter.as_deref(),
            &common.dex_filters,
        )?;
        if let Some(p) = &prefix {
            counts.retain(|(k, _)| crate::api::under_prefix(k, p, '.'));
        }
        let mut tree = crate::api::prefix_tree_sampled(
            counts.iter().map(|(k, w)| (k.as_str(), *w)),
            prefix.as_deref(),
            depth,
            '.',
            samples,
        );
        if sort_count {
            tree.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.path.cmp(&b.path)));
        }
        let rp = crate::api::window_rows(tree, offset, limit);
        let rows_json: Vec<serde_json::Value> = rp
            .rows
            .iter()
            .map(|r| {
                crate::api::tree_row_json(r, "class", "package")
            })
            .collect();
        match rf {
            crate::ResolvedFormat::Jsonl => {
                for row in &rows_json {
                    println!("{}", serde_json::to_string(row)?);
                }
                return Ok(());
            }
            crate::ResolvedFormat::Json => {
                let v = serde_json::json!({
                    "view": "tree",
                    "filter": filter.unwrap_or_default(),
                    "prefix": prefix.unwrap_or_default(),
                    "depth": depth,
                    "total": rp.total,
                    "offset": rp.offset,
                    "limit": rp.limit,
                    "returned": rp.rows.len(),
                    "next_offset": rp.next_offset,
                    "rows": rows_json,
                });
                return crate::print_report(rf, &v);
            }
            crate::ResolvedFormat::Table | crate::ResolvedFormat::Markdown => {
                return crate::print_report(rf, &rows_json);
            }
            crate::ResolvedFormat::Text => {}
        }
        for r in &rp.rows {
            match r.kind {
                crate::api::TreeKind::Group => {
                    println!("{}/ ({}){}", r.path, r.count, crate::api::samples_suffix(r))
                }
                crate::api::TreeKind::Leaf if r.count > 1 => {
                    println!("{} ({})", r.path, r.count)
                }
                crate::api::TreeKind::Leaf => println!("{}", r.path),
            }
        }
        if limit != 0 || offset != 0 {
            eprintln!(
                "{}",
                bif!(
                    "ddc: showing {0} of {1} rows (offset {2}{3})",
                    "ddc：显示 {0}/{1} 行（偏移 {2}{3}）";
                    rp.rows.len(),
                    rp.total,
                    rp.offset,
                    match rp.next_offset {
                        Some(n) => format!(", next_offset {n}"),
                        None => String::new(),
                    }
                )
            );
        }
        return Ok(());
    }

    if rf == crate::ResolvedFormat::Text {
        println!(
            "{:10}  {}",
            "dex",
            if with_loc {
                bi!("string  used-by", "字符串    使用者")
            } else {
                bi!("string", "字符串")
            }
        );
        let shown = std::cell::Cell::new(0usize);
        let total = crate::api::string_scan_window(
            &common.input,
            filter.as_deref(),
            with_loc,
            &common.dex_filters,
            offset,
            limit,
            |r| {
                if r.used_by.is_empty() {
                    println!("{:10}  {}", r.dex, r.string);
                } else {
                    println!("{:10}  {}  {}", r.dex, r.string, r.used_by.join("; "));
                }
                shown.set(shown.get() + 1);
            },
        )?;
        if limit != 0 || offset != 0 {
            let end = offset.saturating_add(shown.get());
            eprintln!(
                "{}",
                bif!(
                    "ddc: showing {0} of {1} strings (offset {2}{3})",
                    "ddc：显示 {0}/{1} 个字符串（偏移 {2}{3}）";
                    shown.get(),
                    total,
                    offset,
                    if end < total { format!(", next_offset {end}") } else { String::new() }
                )
            );
        }
        return Ok(());
    }

    let mut rows = Vec::new();
    let total = crate::api::string_scan_window(
        &common.input,
        filter.as_deref(),
        with_loc,
        &common.dex_filters,
        offset,
        limit,
        |r| rows.push(r),
    )?;
    if rf == crate::ResolvedFormat::Jsonl {
        for r in &rows {
            println!("{}", serde_json::to_string(r)?);
        }
        return Ok(());
    }
    if rf == crate::ResolvedFormat::Json && (limit != 0 || offset != 0) {
        let end = offset.saturating_add(rows.len());
        let next = if end < total { Some(end) } else { None };
        let v = serde_json::json!({
            "total": total,
            "offset": offset,
            "limit": limit,
            "returned": rows.len(),
            "next_offset": next,
            "data": rows,
        });
        return crate::print_report(rf, &v);
    }
    crate::print_report(rf, &rows)
}

// ---- members ---------------------------------------------------------------

pub(crate) fn cmd_members(args: &[String]) -> Result<()> {
    let mut rest: Vec<String> = Vec::new();
    let mut class: Option<String> = None;
    let mut fuzzy_class = false;
    let mut kind_want: Option<&str> = None; // method | field
    let mut format = crate::OutFormat::Auto;
    let mut prefix: Option<String> = None;
    let mut full = false;
    let mut depth: Option<usize> = None;
    let mut sort_count = false;
    let mut samples = 0usize;
    let mut offset = 0usize;
    let mut limit = 0usize;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--class" | "-C" => {
                class = Some(
                    args.get(i + 1)
                        .context(bi!("--class needs a value", "--class 需要一个值"))?
                        .to_string(),
                );
                i += 1;
            }
            "--fuzzy-class" => fuzzy_class = true,
            "--method" | "--field" => kind_want = Some(args[i].trim_start_matches('-')),
            "--format" => {
                format = crate::parse_format(&format_value(args, i)?)?;
                i += 1;
            }
            "--full" => full = true,
            "--prefix" => {
                let v = args
                    .get(i + 1)
                    .context(bi!("--prefix needs a value", "--prefix 需要一个值"))?;
                let norm = crate::api::normalize_class_prefix(v);
                prefix = if norm.is_empty() { None } else { Some(norm) };
                i += 1;
            }
            "--depth" => {
                let v = args
                    .get(i + 1)
                    .context(bi!("--depth needs a count", "--depth 需要一个层数"))?;
                let d: usize = v.parse().map_err(|_| {
                    anyhow::anyhow!(
                        "{}",
                        bif!(
                            "--depth expects a positive integer, got {0:?}",
                            "--depth 需要正整数，收到 {0:?}";
                            v
                        )
                    )
                })?;
                if d == 0 {
                    bail!("{}", bi!("--depth must be at least 1", "--depth 至少为 1"));
                }
                depth = Some(d);
                i += 1;
            }
            "--sort" => {
                let v = args
                    .get(i + 1)
                    .context(bi!("--sort needs a value", "--sort 需要一个值"))?;
                match v.as_str() {
                    "name" => sort_count = false,
                    "count" => sort_count = true,
                    other => bail!(
                        "{}",
                        bif!(
                            "--sort expects name|count, got {0:?}",
                            "--sort 需要 name|count，收到 {0:?}";
                            other
                        )
                    ),
                }
                i += 1;
            }
            "--samples" => {
                samples = crate::parse_count(args.get(i + 1), "--samples")?;
                i += 1;
            }
            "--limit" => {
                limit = crate::parse_count(args.get(i + 1), "--limit")?;
                i += 1;
            }
            "--offset" => {
                offset = crate::parse_count(args.get(i + 1), "--offset")?;
                i += 1;
            }
            "-d" | "--dex" => {
                // parse_common owns -d/--dex, but this loop runs first —
                // forward both tokens so it can see them.
                rest.push(args[i].clone());
                rest.push(
                    args.get(i + 1)
                        .context(bi!("--dex needs a value", "--dex 需要一个值"))?
                        .clone(),
                );
                i += 1;
            }
            a if a.starts_with('-') => bail!(
                "{}",
                bif!("members: unknown option {0}", "members：未知选项 {0}"; a)
            ),
            a => rest.push(a.to_string()),
        }
        i += 1;
    }
    // --full 优先：显式平铺时摘要类参数被忽略。
    let common = parse_common(&rest, "members")?;
    let name = common.rest.first().cloned();
    let rf = crate::resolve_format(format);
    let summary = !full;

    if summary {
        // 摘要只按 owner 类计数：不格式化任何成员字符串（2.5M 行扫描
        // 的耗时/内存与空过滤扫描同级）。
        let mut counts = crate::api::member_class_counts(
            &common.input,
            name.as_deref(),
            class.as_deref(),
            fuzzy_class,
            kind_want,
            &common.dex_filters,
        )?;
        if let Some(p) = &prefix {
            counts.retain(|(k, _)| crate::api::under_prefix(k, p, '.'));
        }
        let mut tree = crate::api::prefix_tree_sampled(
            counts.iter().map(|(k, w)| (k.as_str(), *w)),
            prefix.as_deref(),
            depth,
            '.',
            samples,
        );
        if sort_count {
            tree.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.path.cmp(&b.path)));
        }
        let rp = crate::api::window_rows(tree, offset, limit);
        let rows_json: Vec<serde_json::Value> = rp
            .rows
            .iter()
            .map(|r| {
                crate::api::tree_row_json(r, "class", "package")
            })
            .collect();
        match rf {
            crate::ResolvedFormat::Jsonl => {
                for row in &rows_json {
                    println!("{}", serde_json::to_string(row)?);
                }
                return Ok(());
            }
            crate::ResolvedFormat::Json => {
                let v = serde_json::json!({
                    "view": "tree",
                    "prefix": prefix.unwrap_or_default(),
                    "depth": depth,
                    "total": rp.total,
                    "offset": rp.offset,
                    "limit": rp.limit,
                    "returned": rp.rows.len(),
                    "next_offset": rp.next_offset,
                    "rows": rows_json,
                });
                return crate::print_report(rf, &v);
            }
            crate::ResolvedFormat::Table | crate::ResolvedFormat::Markdown => {
                return crate::print_report(rf, &rows_json);
            }
            crate::ResolvedFormat::Text => {}
        }
        for r in &rp.rows {
            match r.kind {
                crate::api::TreeKind::Group => {
                    println!("{}/ ({}){}", r.path, r.count, crate::api::samples_suffix(r))
                }
                crate::api::TreeKind::Leaf if r.count > 1 => {
                    println!("{} ({})", r.path, r.count)
                }
                crate::api::TreeKind::Leaf => println!("{}", r.path),
            }
        }
        if limit != 0 || offset != 0 {
            eprintln!(
                "{}",
                bif!(
                    "ddc: showing {0} of {1} rows (offset {2}{3})",
                    "ddc：显示 {0}/{1} 行（偏移 {2}{3}）";
                    rp.rows.len(),
                    rp.total,
                    rp.offset,
                    match rp.next_offset {
                        Some(n) => format!(", next_offset {n}"),
                        None => String::new(),
                    }
                )
            );
        }
        return Ok(());
    }

    // 平铺视图（旧行为）：只格式化窗口内的行，total 由扫描计数得出。
    if rf == crate::ResolvedFormat::Text {
        println!(
            "{:10}  {:<6}  {}",
            "dex",
            bi!("kind", "类型"),
            bi!("class member", "类 成员")
        );
        let shown = std::cell::Cell::new(0usize);
        let total = crate::api::member_scan_window(
            &common.input,
            name.as_deref(),
            class.as_deref(),
            fuzzy_class,
            kind_want,
            &common.dex_filters,
            offset,
            limit,
            |r| {
                println!(
                    "{:10}  {:<6}  {} {}",
                    r.dex,
                    r.kind,
                    r.class.replace('.', "/"),
                    r.member
                );
                shown.set(shown.get() + 1);
            },
        )?;
        if limit != 0 || offset != 0 {
            let end = offset.saturating_add(shown.get());
            eprintln!(
                "{}",
                bif!(
                    "ddc: showing {0} of {1} rows (offset {2}{3})",
                    "ddc：显示 {0}/{1} 行（偏移 {2}{3}）";
                    shown.get(),
                    total,
                    offset,
                    if end < total { format!(", next_offset {end}") } else { String::new() }
                )
            );
        }
        return Ok(());
    }
    let mut rows = Vec::new();
    let total = crate::api::member_scan_window(
        &common.input,
        name.as_deref(),
        class.as_deref(),
        fuzzy_class,
        kind_want,
        &common.dex_filters,
        offset,
        limit,
        |r| rows.push(r),
    )?;
    if rf == crate::ResolvedFormat::Json && (limit != 0 || offset != 0) {
        let end = offset.saturating_add(rows.len());
        let next = if end < total { Some(end) } else { None };
        let v = serde_json::json!({
            "total": total,
            "offset": offset,
            "limit": limit,
            "returned": rows.len(),
            "next_offset": next,
            "data": rows,
        });
        return crate::print_report(rf, &v);
    }
    crate::print_report(rf, &rows)
}

// ---- hierarchy ---------------------------------------------------------------

pub(crate) fn cmd_hierarchy(args: &[String]) -> Result<()> {
    let mut rest: Vec<String> = Vec::new();
    let mut format = crate::OutFormat::Auto;
    let mut prefix: Option<String> = None;
    let mut full = false;
    let mut depth: Option<usize> = None;
    let mut sort_count = false;
    let mut samples = 0usize;
    let mut offset = 0usize;
    let mut limit = 0usize;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-d" | "--dex" => {
                // parse_common owns -d/--dex, but this loop runs first —
                // forward both tokens so it can see them.
                rest.push(args[i].clone());
                rest.push(
                    args.get(i + 1)
                        .context(bi!("--dex needs a value", "--dex 需要一个值"))?
                        .clone(),
                );
                i += 1;
            }
            "--format" => {
                format = crate::parse_format(&format_value(args, i)?)?;
                i += 1;
            }
            "--full" => full = true,
            "--prefix" => {
                let v = args
                    .get(i + 1)
                    .context(bi!("--prefix needs a value", "--prefix 需要一个值"))?;
                let norm = crate::api::normalize_class_prefix(v);
                prefix = if norm.is_empty() { None } else { Some(norm) };
                i += 1;
            }
            "--depth" => {
                let v = args
                    .get(i + 1)
                    .context(bi!("--depth needs a count", "--depth 需要一个层数"))?;
                let d: usize = v.parse().map_err(|_| {
                    anyhow::anyhow!(
                        "{}",
                        bif!(
                            "--depth expects a positive integer, got {0:?}",
                            "--depth 需要正整数，收到 {0:?}";
                            v
                        )
                    )
                })?;
                if d == 0 {
                    bail!("{}", bi!("--depth must be at least 1", "--depth 至少为 1"));
                }
                depth = Some(d);
                i += 1;
            }
            "--sort" => {
                let v = args
                    .get(i + 1)
                    .context(bi!("--sort needs a value", "--sort 需要一个值"))?;
                match v.as_str() {
                    "name" => sort_count = false,
                    "count" => sort_count = true,
                    other => bail!(
                        "{}",
                        bif!(
                            "--sort expects name|count, got {0:?}",
                            "--sort 需要 name|count，收到 {0:?}";
                            other
                        )
                    ),
                }
                i += 1;
            }
            "--samples" => {
                samples = crate::parse_count(args.get(i + 1), "--samples")?;
                i += 1;
            }
            "--limit" => {
                limit = crate::parse_count(args.get(i + 1), "--limit")?;
                i += 1;
            }
            "--offset" => {
                offset = crate::parse_count(args.get(i + 1), "--offset")?;
                i += 1;
            }
            a if a.starts_with('-') => bail!(
                "{}",
                bif!("hierarchy: unknown option {0}", "hierarchy：未知选项 {0}"; a)
            ),
            a => rest.push(a.to_string()),
        }
        i += 1;
    }
    // --full 优先：显式平铺时摘要类参数被忽略。
    let common = parse_common(&rest, "hierarchy")?;
    let target = common
        .rest
        .first()
        .context(bi!("hierarchy needs a class name", "hierarchy 需要类名"))?
        .clone();
    let mut rows = crate::api::hierarchy_rows(&common.input, &target, &common.dex_filters)?;
    if let Some(p) = &prefix {
        rows.retain(|r| {
            crate::api::under_prefix(&hierarchy_plain(&r.class).replace('/', "."), p, '.')
        });
    }
    let rf = crate::resolve_format(format);
    let summary = !full;

    if summary {
        // 每个 relation 一棵前缀树；行顺序 = relation 固定序 + 路径字典序。
        const ORDER: [&str; 5] = ["class", "extends", "implements", "sub", "impl"];
        let mut combined: Vec<(usize, String, crate::api::TreeRow)> = Vec::new();
        for (ri, rel) in ORDER.iter().enumerate() {
            let keys: Vec<String> = rows
                .iter()
                .filter(|r| r.relation == *rel)
                .map(|r| hierarchy_plain(&r.class).replace('/', "."))
                .collect();
            if keys.is_empty() {
                continue;
            }
            let tree = crate::api::prefix_tree_sampled(
                keys.iter().map(|k| (k.as_str(), 1usize)),
                prefix.as_deref(),
                depth,
                '.',
                samples,
            );
            for row in tree {
                combined.push((ri, rel.to_string(), row));
            }
        }
        if sort_count {
            combined.sort_by(|a, b| {
                b.2.count
                    .cmp(&a.2.count)
                    .then_with(|| a.2.path.cmp(&b.2.path))
                    .then_with(|| a.0.cmp(&b.0))
            });
        } else {
            combined.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.2.path.cmp(&b.2.path)));
        }
        let rp = crate::api::window_rows(combined, offset, limit);
        let rows_json: Vec<serde_json::Value> = rp
            .rows
            .iter()
            .map(|(_, rel, r)| {
                serde_json::json!({
                    "relation": rel,
                    "path": r.path,
                    "kind": match r.kind {
                        crate::api::TreeKind::Leaf => "class",
                        crate::api::TreeKind::Group => "package",
                    },
                    "count": r.count,
                    "samples": r.samples,
                })
            })
            .collect();
        match rf {
            crate::ResolvedFormat::Jsonl => {
                for row in &rows_json {
                    println!("{}", serde_json::to_string(row)?);
                }
                return Ok(());
            }
            crate::ResolvedFormat::Json => {
                let v = serde_json::json!({
                    "view": "tree",
                    "prefix": prefix.unwrap_or_default(),
                    "depth": depth,
                    "total": rp.total,
                    "offset": rp.offset,
                    "limit": rp.limit,
                    "returned": rp.rows.len(),
                    "next_offset": rp.next_offset,
                    "rows": rows_json,
                });
                return crate::print_report(rf, &v);
            }
            crate::ResolvedFormat::Table | crate::ResolvedFormat::Markdown => {
                return crate::print_report(rf, &rows_json);
            }
            crate::ResolvedFormat::Text => {}
        }
        for (_, rel, row) in &rp.rows {
            match row.kind {
                crate::api::TreeKind::Group => println!(
                    "{:<12} {}/ ({}){}",
                    rel,
                    row.path,
                    row.count,
                    crate::api::samples_suffix(row)
                ),
                crate::api::TreeKind::Leaf => println!("{:<12} {}", rel, row.path),
            }
        }
        if limit != 0 || offset != 0 {
            eprintln!(
                "{}",
                bif!(
                    "ddc: showing {0} of {1} rows (offset {2}{3})",
                    "ddc：显示 {0}/{1} 行（偏移 {2}{3}）";
                    rp.rows.len(),
                    rp.total,
                    rp.offset,
                    match rp.next_offset {
                        Some(n) => format!(", next_offset {n}"),
                        None => String::new(),
                    }
                )
            );
        }
        return Ok(());
    }

    // 平铺视图（旧行为）：窗口作用在行上。
    let rp = crate::api::window_rows(rows, offset, limit);
    if rf != crate::ResolvedFormat::Text {
        if rf == crate::ResolvedFormat::Json && (limit != 0 || offset != 0) {
            let v = serde_json::json!({
                "total": rp.total,
                "offset": rp.offset,
                "limit": rp.limit,
                "returned": rp.rows.len(),
                "next_offset": rp.next_offset,
                "data": rp.rows,
            });
            return crate::print_report(rf, &v);
        }
        return crate::print_report(rf, &rp.rows);
    }
    println!(
        "{:10}  {:<9}  {}",
        "dex",
        bi!("relation", "关系"),
        bi!("class", "类")
    );
    for r in &rp.rows {
        println!("{:10}  {:<9}  {}", r.dex, r.relation, r.class);
    }
    if limit != 0 || offset != 0 {
        eprintln!(
            "{}",
            bif!(
                "ddc: showing {0} of {1} rows (offset {2}{3})",
                "ddc：显示 {0}/{1} 行（偏移 {2}{3}）";
                rp.rows.len(),
                rp.total,
                rp.offset,
                match rp.next_offset {
                    Some(n) => format!(", next_offset {n}"),
                    None => String::new(),
                }
            )
        );
    }
    Ok(())
}

/// extends/implements 行带 `L…;` 描述符；sub/impl/class 行是斜杠内部名。
fn hierarchy_plain(class: &str) -> &str {
    class
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
        .unwrap_or(class)
}

// ---- largest ---------------------------------------------------------------

pub(crate) fn cmd_largest(args: &[String]) -> Result<()> {
    let mut limit = 20usize;
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-n" => {
                let v = args
                    .get(i + 1)
                    .context(bi!("-n needs a count", "-n 需要一个数量"))?;
                limit = v.parse().map_err(|_| {
                    anyhow::anyhow!(
                        "{}",
                        bif!(
                            "largest: -n expects a positive integer, got {0:?}",
                            "largest：-n 需要正整数，收到 {0:?}";
                            v
                        )
                    )
                })?;
                if limit == 0 {
                    bail!("{}", bi!("largest: -n must be at least 1", "largest：-n 至少为 1"));
                }
                i += 1;
            }
            "-d" | "--dex" => {
                // parse_common owns -d/--dex, but this loop runs first —
                // forward both tokens so it can see them.
                rest.push(args[i].clone());
                rest.push(
                    args.get(i + 1)
                        .context(bi!("--dex needs a value", "--dex 需要一个值"))?
                        .clone(),
                );
                i += 1;
            }
            a if a.starts_with('-') => bail!(
                "{}",
                bif!("largest: unknown option {0}", "largest：未知选项 {0}"; a)
            ),
            a => rest.push(a.to_string()),
        }
        i += 1;
    }
    let common = parse_common(&rest, "largest")?;

    struct Row {
        insns: usize,
        dex: String,
        class: String,
        method: String,
    }
    let mut rows: Vec<Row> = Vec::new();
    for_each_image(&common.input, &common.dex_filters, &mut |label, image| {
        let dex_name = label
            .rsplit_once('!')
            .map(|(_, e)| e)
            .unwrap_or(label)
            .to_string();
        let Ok(dex) = RawDex::parse(image) else {
            return;
        };
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
                let method = dex
                    .method_parts(midx)
                    .map(|(_, p, nb)| format!("{}{}", decode_mutf8_lossy(nb), dex.proto_desc(p)))
                    .unwrap_or_default();
                rows.push(Row {
                    insns,
                    dex: dex_name.clone(),
                    class: class.clone(),
                    method,
                });
            }
        }
    })?;
    rows.sort_by_key(|a| std::cmp::Reverse(a.insns));
    println!(
        "{:>7}  {:<10}  {}",
        bi!("insns", "指令数"),
        "dex",
        bi!("class method", "类 方法")
    );
    for r in rows.into_iter().take(limit) {
        println!("{:>7}  {:<10}  {} {}", r.insns, r.dex, r.class, r.method);
    }
    Ok(())
}

// ---- disasm ---------------------------------------------------------------

pub(crate) fn cmd_disasm(args: &[String]) -> Result<()> {
    let mut rest: Vec<String> = Vec::new();
    let mut format = crate::OutFormat::Auto;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-d" | "--dex" => {
                // parse_common owns -d/--dex, but this loop runs first —
                // forward both tokens so it can see them.
                rest.push(args[i].clone());
                rest.push(
                    args.get(i + 1)
                        .context(bi!("--dex needs a value", "--dex 需要一个值"))?
                        .clone(),
                );
                i += 1;
            }
            "--format" => {
                format = crate::parse_format(&format_value(args, i)?)?;
                i += 1;
            }
            a if a.starts_with('-') => bail!(
                "{}",
                bif!("disasm: unknown option {0}", "disasm：未知选项 {0}"; a)
            ),
            a => rest.push(a.to_string()),
        }
        i += 1;
    }
    let common = parse_common(&rest, "disasm")?;
    let target = common.rest.first().context(bi!(
        "disasm needs a class name (optionally Class.method)",
        "disasm 需要类名（可选 类.方法）"
    ))?;
    // 统一走 api::disasm_text：类/方法不存在时返回错误（与 getclass/
    // getmethod 一致），文本输出与旧的逐行打印逐字节相同。
    let text = crate::api::disasm_text(&common.input, target, &common.dex_filters)?;
    let rf = crate::resolve_format(format);
    match rf {
        crate::ResolvedFormat::Text => {
            print!("{text}");
            Ok(())
        }
        crate::ResolvedFormat::Json | crate::ResolvedFormat::Jsonl => crate::print_report(
            rf,
            &serde_json::json!({ "target": target, "disasm": text }),
        ),
        _ => crate::print_source(rf, &text, ""),
    }
}

/// Render one instruction's operands with resolved indices: registers as
/// vN, literals as 0x…, string/type/field/method indices resolved through
/// the raw tables, branch targets as absolute pcs, payloads inline.
pub(crate) fn fmt_operands(
    dex: &RawDex,
    i: &ddc_dex::insn::Insn,
    payloads: &std::collections::HashMap<u32, ddc_dex::insn::Payload>,
) -> String {
    use ddc_dex::insn::InsnKind as K;
    let regs = |r: u16| format!("v{r}");
    let lit = |v: i64| format!("#0x{v:x}");
    let ty = |idx: u32| -> String {
        match dex.type_bytes(idx) {
            Some(b) => format!("type@{idx} {}", crate::findrefs::decode_mutf8_lossy(b)),
            None => format!("type@{idx}"),
        }
    };
    let field = |idx: u32| -> String {
        match dex.field_parts(idx) {
            Some((c, n, t)) => format!(
                "field@{idx} {}->{}:{}",
                dex.class_name(c),
                crate::findrefs::decode_mutf8_lossy(n),
                crate::findrefs::decode_mutf8_lossy(t)
            ),
            None => format!("field@{idx}"),
        }
    };
    let method = |idx: u32| -> String {
        match dex.method_parts(idx) {
            Some((c, p, n)) => format!(
                "method@{idx} {}->{}{}",
                dex.class_name(c),
                crate::findrefs::decode_mutf8_lossy(n),
                dex.proto_desc(p)
            ),
            None => format!("method@{idx}"),
        }
    };
    let strlit = |idx: u32| -> String {
        match dex.string_bytes(idx) {
            Some(b) => format!("string@{idx} {:?}", crate::findrefs::decode_mutf8_lossy(b)),
            None => format!("string@{idx}"),
        }
    };
    let payload = |pc: u32| -> String {
        let mut out = format!("payload@{:04x}", 2 * pc);
        match payloads.get(&pc) {
            Some(ddc_dex::insn::Payload::ArrayData {
                elem_width,
                size,
                data,
            }) => {
                let n = (data.len().min(48)) / (*elem_width as usize).max(1);
                let mut hex = String::new();
                for b in data.iter().take(48) {
                    hex.push_str(&format!("{b:02x} "));
                }
                if data.len() > 48 {
                    hex.push('…');
                }
                out.push_str(&format!(" [elem_width={elem_width} size={size}: {hex}]"));
                let _ = n;
            }
            Some(ddc_dex::insn::Payload::Packed { first_key, targets }) => {
                out.push_str(&format!(
                    " (packed first={first_key} targets={})",
                    targets.len()
                ));
            }
            Some(ddc_dex::insn::Payload::Sparse { pairs }) => {
                out.push_str(&format!(" (sparse {} pairs)", pairs.len()));
            }
            None => {}
        }
        out
    };
    let target = |t: u32| format!("-> {:04x}", 2 * t);
    let _ = lit;
    match &i.kind {
        K::Nop | K::ReturnVoid | K::Unknown => String::new(),
        K::Move { dst, src } => format!("{}, {}", regs(*dst), regs(*src)),
        K::MoveResult { dst } | K::MoveException { dst } => regs(*dst),
        K::Return { src } => regs(*src),
        K::Const { dst, val, .. } => format!("{}, #0x{val:x}", regs(*dst)),
        K::ConstClass { dst, type_idx } => format!("{}, {}", regs(*dst), ty(*type_idx)),
        K::ConstString { dst, str_idx } => format!("{}, {}", regs(*dst), strlit(*str_idx)),
        K::ConstMethodHandle { dst, handle_idx } => {
            format!("{}, method-handle@{handle_idx}", regs(*dst))
        }
        K::ConstMethodType { dst, proto_idx } => {
            let proto = dex.proto_desc(*proto_idx);
            format!("{}, proto@{proto_idx} {}", regs(*dst), proto)
        }
        K::MonitorEnter { reg } | K::MonitorExit { reg } | K::Throw { reg } => regs(*reg),
        K::CheckCast { reg, type_idx } => format!("{}, {}", regs(*reg), ty(*type_idx)),
        K::InstanceOf { dst, src, type_idx } => {
            format!("{}, {}, {}", regs(*dst), regs(*src), ty(*type_idx))
        }
        K::ArrayLength { dst, src } => format!("{}, {}", regs(*dst), regs(*src)),
        K::NewInstance { dst, type_idx } => format!("{}, {}", regs(*dst), ty(*type_idx)),
        K::NewArray {
            dst,
            size,
            type_idx,
        } => {
            format!("{}, {}, {}", regs(*dst), regs(*size), ty(*type_idx))
        }
        K::FilledNewArray { regs: rs, type_idx } => {
            let list = rs.iter().map(|r| regs(*r)).collect::<Vec<_>>().join(", ");
            format!("{{ {list} }}, {}", ty(*type_idx))
        }
        K::FillArrayData { reg, payload_pc } => {
            format!("{}, {}", regs(*reg), payload(*payload_pc))
        }
        K::Goto { target: t } => target(*t),
        K::PackedSwitch { reg, payload_pc } | K::SparseSwitch { reg, payload_pc } => {
            format!("{}, {}", regs(*reg), payload(*payload_pc))
        }
        K::Cmp { dst, a, b, .. } => format!("{}, {}, {}", regs(*dst), regs(*a), regs(*b)),
        K::If {
            a,
            b,
            target: tgt,
            z,
            ..
        } => {
            if *z {
                format!("{}, {}", regs(*a), target(*tgt))
            } else {
                format!("{}, {}, {}", regs(*a), regs(*b), target(*tgt))
            }
        }
        K::AGet {
            dst, array, index, ..
        } => {
            format!("{}, {}, {}", regs(*dst), regs(*array), regs(*index))
        }
        K::APut {
            value,
            array,
            index,
            ..
        } => {
            format!("{}, {}, {}", regs(*value), regs(*array), regs(*index))
        }
        K::IGet {
            dst,
            obj,
            field_idx,
        } => {
            format!("{}, {}, {}", regs(*dst), regs(*obj), field(*field_idx))
        }
        K::IPut {
            value,
            obj,
            field_idx,
        } => {
            format!("{}, {}, {}", regs(*value), regs(*obj), field(*field_idx))
        }
        K::SGet { dst, field_idx } => format!("{}, {}", regs(*dst), field(*field_idx)),
        K::SPut { value, field_idx } => format!("{}, {}", regs(*value), field(*field_idx)),
        K::Invoke {
            regs: rs,
            method_idx,
            ..
        } => {
            let list = rs.iter().map(|r| regs(*r)).collect::<Vec<_>>().join(", ");
            format!("{{ {list} }}, {}", method(*method_idx))
        }
        K::InvokeCustom {
            call_site_idx,
            regs: rs,
        } => {
            let list = rs.iter().map(|r| regs(*r)).collect::<Vec<_>>().join(", ");
            format!("{{ {list} }}, call-site@{call_site_idx}")
        }
        K::Un { dst, src, .. } => format!("{}, {}", regs(*dst), regs(*src)),
        K::Bin { dst, a, b, .. } => format!("{}, {}, {}", regs(*dst), regs(*a), regs(*b)),
        K::BinLit {
            dst, a, lit, rsub, ..
        } => {
            if *rsub {
                format!("{}, #{lit:x}, {}", regs(*dst), regs(*a))
            } else {
                format!("{}, {}, #{lit:x}", regs(*dst), regs(*a))
            }
        }
    }
}

// ---- callers ---------------------------------------------------------------

pub(crate) fn cmd_callers(args: &[String]) -> Result<()> {
    let mut rest: Vec<String> = Vec::new();
    let mut extra: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-d" | "--dex" => {
                extra.push(args[i].clone());
                extra.push(
                    args.get(i + 1)
                        .context(bi!("--dex needs a value", "--dex 需要一个值"))?
                        .clone(),
                );
                i += 1;
            }
            // 透传给 findrefs 的视图/翻页开关。
            "--full" | "--fuzzy-class" => {
                extra.push(args[i].clone())
            }
            "--limit" | "--offset" | "--sort" | "--depth" | "--prefix" | "--format" => {
                extra.push(args[i].clone());
                extra.push(
                    args.get(i + 1)
                        .with_context(|| {
                            bif!("{0} needs a value", "{0} 需要一个值"; args[i])
                        })?
                        .clone(),
                );
                i += 1;
            }
            a if a.starts_with('-') => bail!(
                "{}",
                bif!("callers: unknown option {0}", "callers：未知选项 {0}"; a)
            ),
            a => rest.push(a.to_string()),
        }
        i += 1;
    }
    let common = parse_common(&rest, "callers")?;
    // Reuse findrefs method machinery: callers of M = findrefs --kind method
    // name M (optionally scoped to one class).
    let target = common.rest.first().context(bi!(
        "callers needs a method name [class]",
        "callers 需要方法名 [类名]"
    ))?;
    let (name, class) = match common.rest.get(1) {
        Some(c) => (target.clone(), Some(c.clone())),
        None => (target.clone(), None),
    };
    let mut fwd: Vec<String> = vec![common.input.display().to_string(), "method".into()];
    fwd.extend(extra);
    if let Some(c) = class {
        fwd.push("--class".into());
        fwd.push(c);
    }
    fwd.push(name);
    crate::cmd_findrefs(&fwd, std::time::Instant::now())
}

// ---- pkg (package subtree decompile) ----------------------------------------

pub(crate) fn cmd_pkg(args: &[String]) -> Result<()> {
    let mut rest: Vec<String> = Vec::new();
    let mut out_dir: Option<PathBuf> = None;
    let mut from_manifest = false;
    let mut cure = ddc_dec::classdec::CureMode::Full;
    let mut threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" | "--output" => {
                out_dir = Some(PathBuf::from(
                    args.get(i + 1)
                        .context(bi!("-o needs a value", "-o 需要一个值"))?,
                ));
                i += 1;
            }
            "-t" | "--threads" => {
                threads = args
                    .get(i + 1)
                    .context(bi!("-t needs a count", "-t 需要一个数量"))?
                    .parse()
                    .unwrap_or(4);
                i += 1;
            }
            "-d" | "--dex" => {
                // parse_common owns -d/--dex, but this loop runs first —
                // forward both tokens so it can see them.
                rest.push(args[i].clone());
                rest.push(
                    args.get(i + 1)
                        .context(bi!("--dex needs a value", "--dex 需要一个值"))?
                        .clone(),
                );
                i += 1;
            }
            "--app" => from_manifest = true,
            "--fast" => cure = ddc_dec::classdec::CureMode::Fast,
            a if a.starts_with('-') => bail!(
                "{}",
                bif!("pkg: unknown option {0}", "pkg：未知选项 {0}"; a)
            ),
            a => rest.push(a.to_string()),
        }
        i += 1;
    }
    let common = parse_common(&rest, "pkg")?;
    let package = if from_manifest {
        // --app: the package from the manifest — decompile the app's own
        // code, skipping androidx/library noise.
        let facts = crate::manifest::facts_for(&common.input)?;
        if facts.package.is_empty() {
            bail!(
                "{}",
                bif!(
                    "{0}: manifest has no package attribute",
                    "{0}：manifest 没有 package 属性";
                    common.input.display()
                )
            );
        }
        eprintln!(
            "{}",
            bif!("ddc: app package is {0}", "ddc：应用包名为 {0}"; facts.package)
        );
        facts.package
    } else {
        common
            .rest
            .first()
            .context(bi!(
                "pkg needs a package name (com.example.foo), or --app",
                "pkg 需要包名（com.example.foo），或 --app"
            ))?
            .clone()
    };
    let out = out_dir.unwrap_or_else(|| {
        common
            .input
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join(format!("{}-pkg", package.replace('.', "_")))
    });
    std::fs::create_dir_all(&out)?;

    // Names from every image (class_defs only, prefix-friendly).
    // "" / "." = the root: every class (default package included).
    // --app fallback: the manifest package is not always the code root
    // (Telegram: manifest says org.telegram.messenger.web, code lives in
    // org.telegram.messenger) — retry with the LAUNCHER class's package,
    // which is where the app's own code clusters.
    let collect = |pkg: &str| -> Result<Vec<String>> {
        let mut names: Vec<String> = Vec::new();
        let pkg_prefix = if pkg.is_empty() || pkg == "." {
            String::new()
        } else {
            format!("{}/", pkg.replace('.', "/"))
        };
        for_each_image(&common.input, &common.dex_filters, &mut |_label, image| {
            let Ok(dex) = RawDex::parse(image) else {
                return;
            };
            for ci in 0..dex.cls_n {
                let Some((ty, _, _, _, _)) = dex.class_def_parts(ci) else {
                    continue;
                };
                let name = dex.class_name(ty);
                if name.starts_with(&pkg_prefix) {
                    names.push(name);
                }
            }
        })?;
        Ok(names)
    };
    let mut package = package;
    let mut names = collect(&package)?;
    if names.is_empty() && from_manifest {
        if let Some(launcher) = crate::manifest::facts_for(&common.input)?.launcher {
            if let Some((lp, _)) = launcher.rsplit_once('.') {
                eprintln!(
                    "{}",
                    bif!("ddc: no classes under {0}; retrying with launcher package {1}", "ddc：包 {0} 下没有类；改用 launcher 所在包 {1}"; package, lp)
                );
                package = lp.to_string();
                names = collect(&package)?;
            }
        }
    }
    if names.is_empty() {
        bail!(
            "{}",
            bif!("no classes under package {0}", "包 {0} 下没有类"; package)
        );
    }

    // Decompile those names through the full pipeline: build the pool with
    // ONLY those classes selected (reuse -c machinery via target list).
    let files = expand_inputs(std::slice::from_ref(&common.input))?;
    let parsed = parse_images(filter_images_by_dex(
        collect_images(&files)?,
        &common.dex_filters,
    )?)?;
    let mut pool = crate::DexPool::new();
    for (label, dex) in parsed {
        let idx = pool.add_dex_lazy(dex);
        pool.set_dex_label(idx, label);
    }
    let pool = std::sync::Arc::new(pool);
    // Materialize the selection FIRST: member renames (field/method
    // collisions) need the pooled fields of every class about to be
    // emitted.
    let selected: Vec<String> = names
        .iter()
        .filter(|n| pool.get(n).is_some())
        .cloned()
        .collect();
    ddc_dec::install_case_renames(&pool);
    eprintln!("ddc: {} class(es) under {package}", selected.len());

    let dirs: std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>> =
        std::sync::Mutex::new(std::collections::HashSet::new());
    let written = std::sync::atomic::AtomicUsize::new(0);
    let queue: Vec<Vec<String>> = selected.chunks(32).map(|c| c.to_vec()).collect();
    let cursor = crate::AtomicUsize::new(0);
    let pending: std::sync::Mutex<Vec<ddc_dec::classdec::PendingMonitor>> =
        std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        let queue_ref = &queue;
        let cursor_ref = &cursor;
        let pool_ref = &pool;
        let out_ref = &out;
        let dirs_ref = &dirs;
        let written_ref = &written;
        let pending_ref = &pending;
        let n = threads.min(queue.len()).max(1);
        for _ in 0..n {
            handles.push(
                std::thread::Builder::new()
                    .stack_size(64 * 1024 * 1024)
                    .spawn_scoped(scope, move || loop {
                        let qi = cursor_ref.fetch_add(1, crate::Ordering::Relaxed);
                        let Some(chunk) = queue_ref.get(qi) else {
                            break;
                        };
                        for name in chunk {
                            let Some(pc) = pool_ref.get(name) else {
                                continue;
                            };
                            let res =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    ddc_dec::classdec::decompile_class(
                                        pool_ref,
                                        pc,
                                        &crate::ClassOptions {
                                            provenance: true,
                                            cure,
                                        },
                                        pending_ref,
                                    )
                                }));
                            match res {
                                Ok(Ok(text)) => {
                                    let path = crate::source_path(out_ref, name);
                                    if let Some(parent) = path.parent() {
                                        if dirs_ref.lock().unwrap().insert(parent.to_path_buf()) {
                                            let _ = std::fs::create_dir_all(parent);
                                        }
                                    }
                                    let _ = std::fs::write(&path, text);
                                    written_ref.fetch_add(1, crate::Ordering::Relaxed);
                                }
                                Ok(Err(e)) => eprintln!("[!] {}: {e:#}", name.replace('/', ".")),
                                Err(_) => eprintln!("[!] {}: panic", name.replace('/', ".")),
                            }
                        }
                    }),
            );
        }
        for h in handles.into_iter().flatten() {
            let _ = h.join();
        }
    });
    eprintln!(
        "ddc: wrote {} file(s) to {}",
        written.load(crate::Ordering::Relaxed),
        out.display()
    );
    Ok(())
}

// ---- getmethod ----------------------------------------------------------------

pub(crate) fn cmd_getmethod(args: &[String]) -> Result<()> {
    let mut rest: Vec<String> = Vec::new();
    let mut format = crate::OutFormat::Auto;
    let mut cure = ddc_dec::classdec::CureMode::Full;
    let mut out: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--fast" => cure = ddc_dec::classdec::CureMode::Fast,
            "-d" | "--dex" => {
                // parse_common owns -d/--dex, but this loop runs first —
                // forward both tokens so it can see them.
                rest.push(args[i].clone());
                rest.push(
                    args.get(i + 1)
                        .context(bi!("--dex needs a value", "--dex 需要一个值"))?
                        .clone(),
                );
                i += 1;
            }
            "-o" | "--output" => {
                out = Some(PathBuf::from(
                    args.get(i + 1)
                        .context(bi!("-o needs a value", "-o 需要一个值"))?,
                ));
                i += 1;
            }
            "--format" => {
                format = crate::parse_format(&format_value(args, i)?)?;
                i += 1;
            }
            a if a.starts_with('-') => bail!(
                "{}",
                bif!("getmethod: unknown option {0}", "getmethod：未知选项 {0}"; a)
            ),
            a => rest.push(a.to_string()),
        }
        i += 1;
    }
    let common = parse_common(&rest, "getmethod")?;
    let target = common.rest.first().context(bi!(
        "getmethod needs a Class.method target",
        "getmethod 需要类.方法 目标"
    ))?;
    // `Class.method` is the documented form; a bare class name whose last
    // segment looks like a method (`Cells.t1`) must still resolve — try the
    // split class first, then the whole string (no method filter then).
    let split = target
        .rsplit_once('.')
        .filter(|(c, m)| !c.is_empty() && !m.is_empty() && !m.contains('('))
        .map(|(c, m)| (c.to_string(), m.to_string()));
    if format != crate::OutFormat::Text && out.is_none() {
        let rf = crate::resolve_format(format);
        match rf {
            crate::ResolvedFormat::Text => {}
            crate::ResolvedFormat::Json | crate::ResolvedFormat::Jsonl => {
                let src = crate::api::method_source(&common.input, target, &common.dex_filters)?;
                return crate::print_report(rf, &src);
            }
            _ => {
                let src = crate::api::method_source(&common.input, target, &common.dex_filters)?;
                return crate::print_source(rf, &src.source, "java");
            }
        }
    }
    let candidates: Vec<(String, Option<String>)> = match &split {
        Some((c, m)) => vec![(c.clone(), Some(m.clone())), (target.to_string(), None)],
        None => vec![(target.to_string(), None)],
    };
    let mut last_err: Option<anyhow::Error> = None;
    for (class, method) in candidates {
        match crate::getclass_text_mode(
            std::slice::from_ref(&common.input),
            &class,
            &common.dex_filters,
            cure,
        ) {
            Ok((text, _defining)) => {
                let body = match &method {
                    Some(m) => match slice_methods(&text, m) {
                        Some(b) => b,
                        None => {
                            let avail = method_names(&text).join(", ");
                            bail!(
                                "{}",
                                bif!("method {0} not found in {1} (methods: {2})", "方法 {0} 不在 {1} 中（可用方法：{2}）"; m, class, avail)
                            )
                        }
                    },
                    None => format!("{text}\n"),
                };
                match out {
                    Some(f) => {
                        if let Some(parent) = f.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        std::fs::write(&f, &body)?;
                    }
                    None => print!("{body}"),
                }
                return Ok(());
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        anyhow::anyhow!(
            "{}",
            bi!("getmethod: class not found", "getmethod：找不到类")
        )
    }))
}

/// Slice one method's block out of a decompiled class: keeps the
/// provenance header + package line, then every signature whose
/// pre-paren token equals `method` (all overloads), dedented.
pub(crate) fn slice_methods(text: &str, method: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    // Header: leading // lines (provenance), then the package line.
    let mut header: Vec<&str> = Vec::new();
    let mut package: Option<&str> = None;
    for l in &lines {
        if l.starts_with("//") {
            header.push(l);
        } else if l.trim_start().starts_with("package ") {
            package = Some(l);
            break;
        } else if !l.trim().is_empty() {
            break;
        }
    }
    let mut out = String::new();
    for h in &header {
        out.push_str(h);
        out.push('\n');
    }
    if let Some(p) = package {
        out.push('\n');
        out.push_str(p);
        out.push('\n');
    }

    let mut blocks = 0usize;
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        let t = line.trim_start();
        let indent = line.len() - t.len();
        if indent == 0 || !t.ends_with('{') || !t.contains('(') {
            i += 1;
            continue;
        }
        // The token before the first '(' names the method
        // (`java.lang.String greet() {` → greet; `Greeter(...) {` → ctor).
        let paren = t.find('(').unwrap();
        let name = t[..paren].split_whitespace().next_back().unwrap_or("");
        if name != method {
            i += 1;
            continue;
        }
        // Block ends at the matching-indent closing brace line.
        let close = " ".repeat(indent) + "}";
        let mut j = i;
        let mut block: Vec<&str> = Vec::new();
        while j < lines.len() {
            block.push(lines[j]);
            if lines[j] == close {
                break;
            }
            j += 1;
        }
        out.push('\n');
        for b in &block {
            // Dedent by the signature indent (nested-class methods too).
            out.push_str(b.get(indent..).unwrap_or(b));
            out.push('\n');
        }
        blocks += 1;
        i = j + 1;
    }
    (blocks > 0).then_some(out)
}

/// Every method name in a decompiled class (for getmethod's error hint).
pub(crate) fn method_names(text: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in text.lines() {
        let t = line.trim_start();
        let indent = line.len() - t.len();
        if indent == 0 || !t.ends_with('{') || !t.contains('(') {
            continue;
        }
        let paren = t.find('(').unwrap();
        if let Some(name) = t[..paren].split_whitespace().next_back() {
            if !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
    }
    names
}

// ---- mainactivity -------------------------------------------------------------

pub(crate) fn cmd_mainactivity(args: &[String]) -> Result<()> {
    let mut rest: Vec<String> = Vec::new();
    let mut format = crate::OutFormat::Auto;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-d" | "--dex" => {
                // parse_common owns -d/--dex, but this loop runs first —
                // forward both tokens so it can see them.
                rest.push(args[i].clone());
                rest.push(
                    args.get(i + 1)
                        .context(bi!("--dex needs a value", "--dex 需要一个值"))?
                        .clone(),
                );
                i += 1;
            }
            "--format" => {
                format = crate::parse_format(&format_value(args, i)?)?;
                i += 1;
            }
            a if a.starts_with('-') => bail!(
                "{}",
                bif!("mainactivity: unknown option {0}", "mainactivity：未知选项 {0}"; a)
            ),
            a => rest.push(a.to_string()),
        }
        i += 1;
    }
    let common = parse_common(&rest, "mainactivity")?;

    if format != crate::OutFormat::Text {
        let report = crate::api::main_activity(&common.input)?;
        let rf = crate::resolve_format(format);
        if rf != crate::ResolvedFormat::Text {
            return crate::print_report(rf, &report);
        }
    }

    let facts = crate::manifest::facts_for(&common.input)?;
    if facts.package.is_empty() {
        bail!(bi!(
            "manifest has no package attribute",
            "manifest 没有 package 属性"
        ));
    }
    println!("{:<11} {}", bi!("package", "包名"), facts.package);
    if let Some(app) = &facts.application {
        println!(
            "{:<11} {} {}",
            bi!("class", "类"),
            app,
            bi!("(application)", "（Application 类）")
        );
    }
    let Some(launcher) = &facts.launcher else {
        bail!(
            "{}",
            bif!(
                "manifest declares no MAIN/LAUNCHER activity (headless app? try `ddc manifest {0} --component activity-alias`)",
                "manifest 未声明 MAIN/LAUNCHER 入口 Activity（无界面应用？可试 `ddc manifest {0} --component activity-alias`）";
                common.input.display()
            )
        );
    };
    println!("{:<11} {}", bi!("launcher", "启动入口"), launcher);

    // Verify the launcher against the dex images: which one defines it?
    // (A name the manifest inherited from a library still resolves; a
    // framework stub like android.app.Application won't — that's fine.)
    let internal = launcher.replace('.', "/");
    let mut found: Option<String> = None;
    for_each_prefix_image(
        &common.input,
        &common.dex_filters,
        crate::inputs::type_descriptor_prefix_needed,
        &mut |label, image| {
        if found.is_some() {
            return;
        }
        if let Ok(dex) = RawDex::parse(image) {
            if let Some(label_short) = label.rsplit_once('!').map(|(_, e)| e) {
                if dex.find_class(&internal).is_some() {
                    let _ = label_short;
                    found = Some(label.to_string());
                }
            }
        }
    })?;
    match &found {
        Some(image) => println!("{:<11} {}", "dex", image),
        None => println!(
            "{:<11} - (not defined in the dex images: framework or missing)",
            "dex"
        ),
    }
    Ok(())
}

// ---- res ------------------------------------------------------------------------

/// One archive entry, flattened across nested containers: `name` is the
/// user-visible path (`res/values/strings.xml`, or `base.apk!res/...`);
/// metadata only — nothing is inflated for listing.
struct FlatEntry {
    name: String,
    /// "" = top-level container; otherwise the inner APK the entry lives in.
    container: String,
    method: &'static str,
    /// Compressed size on disk.
    size: usize,
}

fn flatten_entries(input: &std::path::Path) -> Result<Vec<FlatEntry>> {
    let src = crate::inputs::map_source(input)?;
    let bytes: &[u8] = src.bytes();
    if bytes.len() < 4 || &bytes[..2] != b"PK" {
        bail!(
            "{}",
            bif!("{0}: not a zip container", "{0}：不是 zip 容器"; input.display())
        );
    }
    let entries = crate::zip_entries(bytes)?;
    let mut out: Vec<FlatEntry> = Vec::new();
    for e in &entries {
        // Nested APK (XAPK/APKS/APKM): recurse one level; resources live
        // in the inner APKs, not the container.
        if e.name.ends_with(".apk") {
            if let Ok(inner) = crate::manifest::entry_bytes(bytes, e) {
                if inner.len() > 4 && &inner[..2] == b"PK" {
                    if let Ok(inner_entries) = crate::zip_entries(&inner) {
                        for ie in &inner_entries {
                            out.push(FlatEntry {
                                name: format!("{}!{}", e.name, ie.name),
                                container: e.name.clone(),
                                method: match ie.method {
                                    crate::ZipMethod::Stored => "stored",
                                    crate::ZipMethod::Deflate => "deflate",
                                },
                                size: ie.range.len(),
                            });
                        }
                    }
                }
            }
            continue;
        }
        out.push(FlatEntry {
            name: e.name.clone(),
            container: String::new(),
            method: match e.method {
                crate::ZipMethod::Stored => "stored",
                crate::ZipMethod::Deflate => "deflate",
            },
            size: e.range.len(),
        });
    }
    Ok(out)
}

/// Inflate exactly one entry: (pretty name, bytes). `container` empty =
/// top-level archive.
fn dump_entry(input: &std::path::Path, name: &str, container: &str) -> Result<Vec<u8>> {
    let src = crate::inputs::map_source(input)?;
    let bytes: &[u8] = src.bytes();
    if bytes.len() < 4 || &bytes[..2] != b"PK" {
        bail!(
            "{}",
            bif!("{0}: not a zip container", "{0}：不是 zip 容器"; input.display())
        );
    }
    let entries = crate::zip_entries(bytes)?;
    if container.is_empty() {
        let e = entries
            .iter()
            .find(|e| e.name == name)
            .with_context(|| bif!("res: no entry {0:?}", "res：没有条目 {0:?}"; name))?;
        return crate::manifest::entry_bytes(bytes, e);
    }
    let apk = entries
        .iter()
        .find(|e| e.name == container)
        .with_context(|| bif!("res: no inner APK {0:?}", "res：没有内层 APK {0:?}"; container))?;
    let inner = crate::manifest::entry_bytes(bytes, apk)?;
    let inner_entries = crate::zip_entries(&inner)?;
    let e = inner_entries.iter().find(|e| e.name == name).with_context(
        || bif!("res: no entry {0:?} in {1}", "res：{1} 中没有条目 {0:?}"; name, container),
    )?;
    crate::manifest::entry_bytes(&inner, e)
}

pub(crate) fn cmd_res(args: &[String]) -> Result<()> {
    let mut rest: Vec<String> = Vec::new();
    let mut out: Option<PathBuf> = None;
    let mut prefix: Option<String> = None;
    let mut full = false;
    let mut depth: Option<usize> = None;
    let mut sort_count = false;
    let mut samples = 0usize;
    let mut offset = 0usize;
    let mut limit = 0usize;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" | "--output" => {
                out = Some(PathBuf::from(
                    args.get(i + 1)
                        .context(bi!("-o needs a value", "-o 需要一个值"))?,
                ));
                i += 1;
            }
            "--full" => full = true,
            "--prefix" => {
                let v = args
                    .get(i + 1)
                    .context(bi!("--prefix needs a value", "--prefix 需要一个值"))?;
                let norm = crate::api::normalize_prefix(v, '/');
                prefix = if norm.is_empty() { None } else { Some(norm) };
                i += 1;
            }
            "--depth" => {
                let v = args
                    .get(i + 1)
                    .context(bi!("--depth needs a count", "--depth 需要一个层数"))?;
                let d: usize = v.parse().map_err(|_| {
                    anyhow::anyhow!(
                        "{}",
                        bif!(
                            "--depth expects a positive integer, got {0:?}",
                            "--depth 需要正整数，收到 {0:?}";
                            v
                        )
                    )
                })?;
                if d == 0 {
                    bail!("{}", bi!("--depth must be at least 1", "--depth 至少为 1"));
                }
                depth = Some(d);
                i += 1;
            }
            "--sort" => {
                let v = args
                    .get(i + 1)
                    .context(bi!("--sort needs a value", "--sort 需要一个值"))?;
                match v.as_str() {
                    "name" => sort_count = false,
                    "count" => sort_count = true,
                    other => bail!(
                        "{}",
                        bif!(
                            "--sort expects name|count, got {0:?}",
                            "--sort 需要 name|count，收到 {0:?}";
                            other
                        )
                    ),
                }
                i += 1;
            }
            "--samples" => {
                samples = crate::parse_count(args.get(i + 1), "--samples")?;
                i += 1;
            }
            "--limit" => {
                limit = crate::parse_count(args.get(i + 1), "--limit")?;
                i += 1;
            }
            "--offset" => {
                offset = crate::parse_count(args.get(i + 1), "--offset")?;
                i += 1;
            }
            a if a.starts_with('-') => bail!(
                "{}",
                bif!("res: unknown option {0}", "res：未知选项 {0}"; a)
            ),
            a => rest.push(a.to_string()),
        }
        i += 1;
    }
    // --full 优先：显式平铺时摘要类参数被忽略。
    let common = parse_common(&rest, "res")?;
    let mut entries = flatten_entries(&common.input)?;
    if let Some(p) = &prefix {
        entries.retain(|e| crate::api::under_prefix(&e.name, p, '/'));
    }
    let Some(want) = common.rest.first() else {
        // 列表模式：默认目录摘要；--full 为逐条表格（可翻页）。
        if full {
            let rp = crate::api::window_rows(entries, offset, limit);
            println!(
                "{:<8}  {:>9}  {}",
                bi!("method", "压缩方式"),
                bi!("size", "大小"),
                bi!("entry", "条目")
            );
            for e in &rp.rows {
                println!("{:<8}  {:>9}  {}", e.method, e.size, e.name);
            }
            if limit != 0 || offset != 0 {
                eprintln!(
                    "{}",
                    bif!(
                        "ddc: showing {0} of {1} entries (offset {2}{3})",
                        "ddc：显示 {0}/{1} 个条目（偏移 {2}{3}）";
                        rp.rows.len(),
                        rp.total,
                        rp.offset,
                        match rp.next_offset {
                            Some(n) => format!(", next_offset {n}"),
                            None => String::new(),
                        }
                    )
                );
            } else {
                println!(
                    "{}",
                    bif!("total: {0} entries", "合计：{0} 个条目"; rp.total)
                );
            }
            return Ok(());
        }
        let keys: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
        let mut tree = crate::api::prefix_tree_sampled(
            keys.iter().map(|k| (k.as_str(), 1usize)),
            prefix.as_deref(),
            depth,
            '/',
            samples,
        );
        if sort_count {
            tree.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.path.cmp(&b.path)));
        }
        let rp = crate::api::window_rows(tree, offset, limit);
        let mut text = String::new();
        for r in &rp.rows {
            match r.kind {
                crate::api::TreeKind::Group => text.push_str(&format!(
                    "{}/ ({}){}\n",
                    r.path,
                    r.count,
                    crate::api::samples_suffix(r)
                )),
                crate::api::TreeKind::Leaf if r.count > 1 => {
                    text.push_str(&format!("{} ({})\n", r.path, r.count))
                }
                crate::api::TreeKind::Leaf => text.push_str(&format!("{}\n", r.path)),
            }
        }
        match out {
            Some(f) => {
                if let Some(parent) = f.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(&f, text)?;
                eprintln!(
                    "{}",
                    bif!(
                        "ddc: wrote {0} row(s) to {1}",
                        "ddc：已写出 {0} 行到 {1}";
                        rp.rows.len(),
                        f.display()
                    )
                );
            }
            None => print!("{text}"),
        }
        if limit != 0 || offset != 0 {
            eprintln!(
                "{}",
                bif!(
                    "ddc: showing {0} of {1} rows (offset {2}{3})",
                    "ddc：显示 {0}/{1} 行（偏移 {2}{3}）";
                    rp.rows.len(),
                    rp.total,
                    rp.offset,
                    match rp.next_offset {
                        Some(n) => format!(", next_offset {n}"),
                        None => String::new(),
                    }
                )
            );
        }
        return Ok(());
    };
    if full
        || prefix.is_some()
        || depth.is_some()
        || sort_count
        || limit != 0
        || offset != 0
    {
        bail!(
            "{}",
            bi!(
                "res: --full/--prefix/--depth/--sort/--limit/--offset apply to the list form only",
                "res：--full/--prefix/--depth/--sort/--limit/--offset 仅适用于列表形式"
            )
        );
    }

    // Dump mode: exact match first, then a unique substring match.
    let hit = entries
        .iter()
        .find(|e| e.name == *want)
        .or_else(|| {
            let sub: Vec<&FlatEntry> = entries
                .iter()
                .filter(|e| e.name.contains(want.as_str()))
                .collect();
            (sub.len() == 1).then(|| sub[0])
        })
        .with_context(|| {
            let matches: Vec<&str> = entries
                .iter()
                .filter(|e| e.name.contains(want.as_str()))
                .map(|e| e.name.as_str())
                .take(5)
                .collect();
            if matches.is_empty() {
                bif!("res: no entry matches {0:?}", "res：没有条目匹配 {0:?}"; want)
            } else {
                bif!("res: {0:?} is ambiguous: {1}", "res：{0:?} 有歧义：{1}"; want, matches.join(", "))
            }
        })?;
    let plain = hit
        .name
        .rsplit_once('!')
        .map(|(_, n)| n)
        .unwrap_or(&hit.name);
    let bytes = dump_entry(&common.input, plain, &hit.container)?;

    // Binary XML? (first chunk 0x0003 = RES_XML_TYPE) — res/**.xml and
    // AndroidManifest.xml decode through the existing AXML decoder.
    let is_axml = bytes.len() >= 8 && u16::from_le_bytes([bytes[0], bytes[1]]) == 0x0003;
    if is_axml {
        let text =
            crate::axml::axml_to_xml(&bytes).map_err(|e| anyhow::anyhow!("{}: {e}", hit.name))?;
        match out {
            Some(f) => std::fs::write(&f, &text)?,
            None => print!("{text}"),
        }
        return Ok(());
    }
    match String::from_utf8(bytes.clone()) {
        Ok(text) if !text.contains('\0') => match out {
            Some(f) => std::fs::write(&f, text)?,
            None => print!("{text}"),
        },
        _ => match out {
            Some(f) => {
                std::fs::write(&f, &bytes)?;
                eprintln!(
                    "{}",
                    bif!(
                        "ddc: wrote {0} ({1} bytes) from {2}",
                        "ddc：已写出 {0}（{1} 字节），来自 {2}";
                        f.display(),
                        bytes.len(),
                        hit.name
                    )
                );
            }
            None => bail!(
                "{}",
                bif!(
                    "{0}: {1} binary bytes — pass -o FILE to save",
                    "{0}：{1} 字节二进制内容 —— 加 -o 文件 保存";
                    hit.name,
                    bytes.len()
                )
            ),
        },
    }
    Ok(())
}
