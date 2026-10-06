//! The browse/locate subcommands: strings, members, hierarchy, largest,
//! getmethod, disasm, callers, pkg. All ride the RawDex zero-materialization
//! path (raw images, prefix-friendly), mirroring jadx's quick-lookup tools.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

use crate::findrefs::{decode_mutf8_lossy, RawDex};
use crate::inputs::{
    collect_images, expand_inputs, filter_images_by_dex, inflate_images, parse_images,
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
    let files = expand_inputs(std::slice::from_ref(input))?;
    let images = filter_images_by_dex(collect_images(&files)?, dex_filters)?;
    const WAVE: usize = 8;
    let mut rest = images;
    while !rest.is_empty() {
        let wave = rest.split_off(rest.len().saturating_sub(WAVE));
        for (label, raw) in inflate_images(wave)? {
            f(&label, &raw);
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
            "--format" => {
                format = crate::parse_format(&format_value(args, i)?)?;
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
    let common = parse_common(&rest, "strings")?;

    if format != crate::OutFormat::Text {
        let rows =
            crate::api::string_rows(&common.input, filter.as_deref(), with_loc, &common.dex_filters)?;
        let rf = crate::resolve_format(format);
        if rf != crate::ResolvedFormat::Text {
            return crate::print_report(rf, &rows);
        }
    }

    println!(
        "{:10}  {}",
        "dex",
        if with_loc {
            bi!("string  used-by", "字符串    使用者")
        } else {
            bi!("string", "字符串")
        }
    );
    for_each_image(&common.input, &common.dex_filters, &mut |label, image| {
        let dex_name = label
            .rsplit_once('!')
            .map(|(_, e)| e)
            .unwrap_or(label)
            .to_string();
        let Ok(dex) = RawDex::parse(image) else {
            return;
        };
        // Matching strings (SIMD memmem over raw bytes).
        let needle = filter.as_deref().map(str::as_bytes);
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
        // Exact usage locations: walk every method's const-string sites.
        let mut users: std::collections::HashMap<u32, Vec<String>> =
            std::collections::HashMap::new();
        if with_loc {
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
            match users.get(&idx) {
                Some(u) if !u.is_empty() => {
                    let mut uniq: Vec<&str> = u.iter().map(|m| m.as_str()).collect();
                    uniq.dedup();
                    println!("{:10}  {:?}  {}", dex_name, s, uniq.join("; "))
                }
                _ => println!("{:10}  {:?}", dex_name, s),
            }
        }
    })
}

// ---- members ---------------------------------------------------------------

pub(crate) fn cmd_members(args: &[String]) -> Result<()> {
    let mut rest: Vec<String> = Vec::new();
    let mut class: Option<String> = None;
    let mut fuzzy_class = false;
    let mut kind_want: Option<&str> = None; // method | field
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
    let common = parse_common(&rest, "members")?;
    let name = common.rest.first().cloned();

    println!(
        "{:10}  {:<6}  {}",
        "dex",
        bi!("kind", "类型"),
        bi!("class member", "类 成员")
    );
    for_each_image(&common.input, &common.dex_filters, &mut |label, image| {
        let dex_name = label
            .rsplit_once('!')
            .map(|(_, e)| e)
            .unwrap_or(label)
            .to_string();
        let Ok(dex) = RawDex::parse(image) else {
            return;
        };
        let class_ok = |cb: &[u8]| -> bool {
            match &class {
                None => true,
                Some(c) => {
                    let hay = String::from_utf8_lossy(cb);
                    let hay = hay.trim_start_matches('L').trim_end_matches(';');
                    let needle = c.replace('.', "/");
                    if fuzzy_class {
                        hay.contains(&needle)
                    } else {
                        hay == needle
                    }
                }
            }
        };
        if kind_want != Some("field") {
            for mi in 0..dex.method_n as u32 {
                let Some((cidx, proto, nb)) = dex.method_parts(mi) else {
                    continue;
                };
                if let Some(n) = name.as_deref() {
                    if !dex_match(nb, n.as_bytes()) {
                        continue;
                    }
                }
                if let Some(cb) = dex.type_bytes(cidx) {
                    if !class_ok(cb) {
                        continue;
                    }
                    println!(
                        "{:10}  {:<6}  {} {}{}",
                        dex_name,
                        "method",
                        dex.class_name(cidx),
                        decode_mutf8_lossy(nb),
                        dex.proto_desc(proto)
                    );
                }
            }
        }
        if kind_want != Some("method") {
            for fi in 0..dex.field_n as u32 {
                let Some((cidx, nb, tb)) = dex.field_parts(fi) else {
                    continue;
                };
                if let Some(n) = name.as_deref() {
                    if !dex_match(nb, n.as_bytes()) {
                        continue;
                    }
                }
                if let Some(cb) = dex.type_bytes(cidx) {
                    if !class_ok(cb) {
                        continue;
                    }
                    println!(
                        "{:10}  {:<6}  {} {} {}",
                        dex_name,
                        "field",
                        dex.class_name(cidx),
                        decode_mutf8_lossy(nb),
                        decode_mutf8_lossy(tb)
                    );
                }
            }
        }
    })
}

/// Case-sensitive substring match on raw MUTF-8 bytes (None needle = all).
fn dex_match(nb: &[u8], needle: &[u8]) -> bool {
    nb.len() >= needle.len() && nb.windows(needle.len()).any(|w| w == needle)
}

// ---- hierarchy ---------------------------------------------------------------

pub(crate) fn cmd_hierarchy(args: &[String]) -> Result<()> {
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
                bif!("hierarchy: unknown option {0}", "hierarchy：未知选项 {0}"; a)
            ),
            a => rest.push(a.to_string()),
        }
        i += 1;
    }
    let common = parse_common(&rest, "hierarchy")?;
    let target = common
        .rest
        .first()
        .context(bi!("hierarchy needs a class name", "hierarchy 需要类名"))?
        .replace('.', "/");

    if format != crate::OutFormat::Text {
        let rows = crate::api::hierarchy_rows(&common.input, &target, &common.dex_filters)?;
        let rf = crate::resolve_format(format);
        if rf != crate::ResolvedFormat::Text {
            return crate::print_report(rf, &rows);
        }
    }

    println!(
        "{:10}  {:<9}  {}",
        "dex",
        bi!("relation", "关系"),
        bi!("class", "类")
    );
    // Map: super/interface type idx -> child classes (per image).
    for_each_image(&common.input, &common.dex_filters, &mut |label, image| {
        let dex_name = label
            .rsplit_once('!')
            .map(|(_, e)| e)
            .unwrap_or(label)
            .to_string();
        let Ok(dex) = RawDex::parse(image) else {
            return;
        };
        // Resolve the target's type idx in THIS image (name → idx).
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
        // Build the child map for whichever relation names we hit.
        for ci in 0..dex.cls_n {
            let Some((ty, sup, iface_off, _, _)) = dex.class_def_parts(ci) else {
                continue;
            };
            let self_name = dex.class_name(ty);
            if target_idx.is_some() && ty == target_idx.unwrap() {
                // print self + lineage up
                println!("{:10}  {:<9}  {}", dex_name, "class", self_name);
                if sup != u32::MAX {
                    if let Some(sb) = dex.type_bytes(sup) {
                        println!(
                            "{:10}  {:<9}  {}",
                            dex_name,
                            "extends",
                            decode_mutf8_lossy(sb)
                        );
                    }
                }
                for it in dex.interface_types(iface_off) {
                    if let Some(ib) = dex.type_bytes(it) {
                        println!(
                            "{:10}  {:<9}  {}",
                            dex_name,
                            "implements",
                            decode_mutf8_lossy(ib)
                        );
                    }
                }
            }
            if sup != u32::MAX && sup == target_idx.unwrap_or(u32::MAX) {
                println!("{:10}  {:<9}  {}", dex_name, "sub", self_name);
            }
            for it in dex.interface_types(iface_off) {
                if it == target_idx.unwrap_or(u32::MAX) {
                    println!("{:10}  {:<9}  {}", dex_name, "impl", self_name);
                }
            }
            // target not present as a type in this image: match by name
            // (hierarchy across images where the parent lives elsewhere).
            if target_idx.is_none() {
                if sup != u32::MAX {
                    if let Some(sb) = dex.type_bytes(sup) {
                        if plain_of(&decode_mutf8_lossy(sb)) == target {
                            println!("{:10}  {:<9}  {}", dex_name, "sub", self_name);
                        }
                    }
                }
                for it in dex.interface_types(iface_off) {
                    if let Some(ib) = dex.type_bytes(it) {
                        if plain_of(&decode_mutf8_lossy(ib)) == target {
                            println!("{:10}  {:<9}  {}", dex_name, "impl", self_name);
                        }
                    }
                }
                if plain_of(&self_name) == target {
                    println!("{:10}  {:<9}  {}", dex_name, "class", self_name);
                }
            }
        }
    })
}

fn plain_of(desc: &str) -> &str {
    desc.trim_start_matches('L').trim_end_matches(';')
}

// ---- largest ---------------------------------------------------------------

pub(crate) fn cmd_largest(args: &[String]) -> Result<()> {
    let mut limit = 20usize;
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-n" => {
                limit = args
                    .get(i + 1)
                    .context(bi!("-n needs a count", "-n 需要一个数量"))?
                    .parse()
                    .unwrap_or(20);
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
    if format != crate::OutFormat::Text {
        let text = crate::api::disasm_text(&common.input, target, &common.dex_filters)?;
        let rf = crate::resolve_format(format);
        match rf {
            crate::ResolvedFormat::Text => {}
            crate::ResolvedFormat::Json | crate::ResolvedFormat::Jsonl => {
                return crate::print_report(
                    rf,
                    &serde_json::json!({ "target": target, "disasm": text }),
                );
            }
            _ => return crate::print_source(rf, &text, ""),
        }
    }
    let class_full = target.replace('.', "/");
    // `Cells.t1` (whole thing is a class) vs `Greeter.greet` (class + method):
    // try the whole string as a class first, then fall back to splitting at
    // the last dot.
    let split = target
        .rsplit_once('.')
        .filter(|(c, m)| !c.is_empty() && !m.is_empty() && !m.contains('('))
        .map(|(c, m)| (c.replace('.', "/"), m.to_string()));

    for_each_image(&common.input, &common.dex_filters, &mut |label, image| {
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
        println!("// {} {}", label, dex.class_name(ty));
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
            println!("  {}:", owner);
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
                println!(
                    "    {:04x}: {} {}",
                    2 * i.pc,
                    ddc_dex::insn::op_name(i.op),
                    fmt_operands(&dex, i, &payloads)
                );
            }
        }
    })
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
                                        &crate::ClassOptions::default(),
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
    let mut out: Option<PathBuf> = None;
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
            _ => {}
        }
        i += 1;
    }
    // `Class.method` is the documented form; a bare class name whose last
    // segment looks like a method (`Cells.t1`) must still resolve — try the
    // split class first, then the whole string (no method filter then).
    let split = target
        .rsplit_once('.')
        .filter(|(c, m)| !c.is_empty() && !m.is_empty() && !m.contains('('))
        .map(|(c, m)| (c.to_string(), m.to_string()));
    if format != crate::OutFormat::Text
        && !args.iter().any(|a| a == "-o" || a == "--output")
    {
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
        match crate::getclass_text(
            std::slice::from_ref(&common.input),
            &class,
            &common.dex_filters,
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
    for_each_image(&common.input, &common.dex_filters, &mut |label, image| {
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
            a if a.starts_with('-') => bail!(
                "{}",
                bif!("res: unknown option {0}", "res：未知选项 {0}"; a)
            ),
            a => rest.push(a.to_string()),
        }
        i += 1;
    }
    let common = parse_common(&rest, "res")?;
    let entries = flatten_entries(&common.input)?;
    let Some(want) = common.rest.first() else {
        // List mode: every entry, method + compressed size.
        println!(
            "{:<8}  {:>9}  {}",
            bi!("method", "压缩方式"),
            bi!("size", "大小"),
            bi!("entry", "条目")
        );
        for e in &entries {
            println!("{:<8}  {:>9}  {}", e.method, e.size, e.name);
        }
        println!(
            "{}",
            bif!("total: {0} entries", "合计：{0} 个条目"; entries.len())
        );
        return Ok(());
    };

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
