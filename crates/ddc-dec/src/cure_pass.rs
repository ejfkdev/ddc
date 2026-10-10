//! cure 源码级后处理：反编译产物 → 人写形态的语义保持简化。
//!
//! ddc 的质量 pass 工作在 IR 层（Stmt/Expr + VarTable + DexPool——那里
//! 有源码层没有的寄存器类型与池信息）；cure 补的是 IR 层刻意保守留下
//! 的源码层残余（R8 寄存器拷贝链、`x = e; return x` 尾巴、三元化、
//! while→for-each、死存储…），容错解析 + 成本单调不动点保证语义不变
//! （reqable 5,579 文件实测：0 解析错误、行数 −18.9%、javac 门 386→383）。
//!
//! 接线位：`decompile_class_impl` 组装完文件文本之后。三重守卫：
//! - `$DDC` 失败块标记是承重诊断（cure 的 AST 无注释节点会丢弃）→
//!   这类文件原样返回；
//! - 解析错误非空 → 原样返回（容错兜底；实测语料为 0）；
//! - `ClassOptions::cure == false`（CLI `--no-cure`）时整个 pass 跳过。

/// 头部注释行（provenance/Source file/synthetic）的长度：逐行扫描，
/// 直到首个非注释非空行（package 或类型声明）。cure 的打印器不回放
/// 注释，头部由 ddc 自己拼回。
fn header_len(src: &str) -> usize {
    let mut off = 0usize;
    for line in src.split_inclusive('\n') {
        let t = line.trim_end();
        if t.is_empty() || t.starts_with("//") {
            off += line.len();
        } else {
            break;
        }
    }
    off
}

/// 跑一遍 cure（parse → simplify_unit → print_unit）。守卫命中或解析
/// 失败 → 原文返回（fail-open：净化是增益，不是正确性前提）。
pub fn simplify_source(src: String) -> String {
    // 失败块标记（`$DDC: … blocks failed to decompile` / 不可约 CFG
    // 提示）必须逐字保留，且这些文件本身带敏感结构——整体跳过。
    if src.contains("$DDC") {
        return src;
    }
    let cut = header_len(&src);
    let (header, body) = src.split_at(cut);
    let mut outcome = cure_java_parser::parse(body);
    if !outcome.errors.is_empty() {
        return src;
    }
    cure_java_simplify::simplify_unit(
        &mut outcome.ast,
        &mut outcome.unit,
        &cure_engine::Config::default(),
    );
    let printed = cure_java_print::print_unit(&outcome.ast, &outcome.unit);
    let mut out = String::with_capacity(header.len() + printed.len() + 1);
    out.push_str(header);
    if !header.is_empty() && !header.ends_with("\n\n") {
        out.push('\n');
    }
    out.push_str(&printed);
    out
}
