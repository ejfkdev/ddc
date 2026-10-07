//! The xyz frontends: HTTP REST + MCP for ddc's query commands
//! (github.com/ejfkdev/xyz-rust — one definition, three interfaces).
//!
//! Every ddc command is registered with the CLI channel SKIPPED: the
//! existing hand-rolled CLI keeps handling `ddc getclass ...` exactly as
//! before. What the xyz layer adds:
//!
//! - `ddc serve --addr :8080` — HTTP REST + `GET /openapi.json` +
//!   streamable MCP on the same port
//! - `ddc mcp stdio` / `ddc mcp http` — MCP tool server
//!
//! Enabled with `--features xyz` (xyz-rust is not on crates.io yet; the
//! optional git dependency keeps `cargo publish` working).

use std::path::PathBuf;

use xyz_rust::{errs, CliHints, Ctx, HTTPHints, MCPHints, XyzArgs};

use crate::api;

type XyzResult<T> = std::result::Result<T, errs::Error>;

fn p(input: &str) -> PathBuf {
    PathBuf::from(input)
}

fn map_err(e: anyhow::Error) -> errs::Error {
    // File-not-found and friends surface as 404; the rest as internal.
    let msg = e.to_string();
    let kind = if msg.contains("os error 2") || msg.contains("not found") {
        errs::Kind::NotFound
    } else {
        errs::Kind::Internal
    };
    errs::new(kind, format!("{e:#}"))
}

// ---- ddc.info ----------------------------------------------------------------

#[derive(XyzArgs)]
struct InfoArgs {
    #[xyz(desc = "input: .dex / .apk / .jar / .zip / .xapk / directory", required)]
    input: String,
    #[xyz(desc = "restrict images whose entry name contains this (repeatable)")]
    dex: Vec<String>,
}

fn info(_ctx: &Ctx, a: &InfoArgs) -> XyzResult<api::InfoReport> {
    api::info_report(&p(&a.input)).map_err(map_err)
}

// ---- ddc.listclasses ---------------------------------------------------------

#[derive(XyzArgs)]
struct ListclassesArgs {
    #[xyz(desc = "input: APK / dex / container", required)]
    input: String,
    #[xyz(desc = "case-insensitive substring filter")]
    pattern: String,
    #[xyz(desc = "restrict images whose entry name contains this (repeatable)")]
    dex: Vec<String>,
}

fn listclasses(_ctx: &Ctx, a: &ListclassesArgs) -> XyzResult<Vec<String>> {
    api::class_names(&p(&a.input), empty_or_some(&a.pattern), &a.dex).map_err(map_err)
}

fn empty_or_some(s: &str) -> Option<&str> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

// ---- ddc.getclass ------------------------------------------------------------

#[derive(XyzArgs)]
struct GetClassArgs {
    #[xyz(desc = "input: APK / dex / container", required)]
    input: String,
    #[xyz(desc = "class name, dotted or slashed", required)]
    class: String,
    #[xyz(desc = "restrict images whose entry name contains this (repeatable)")]
    dex: Vec<String>,
}

fn getclass(_ctx: &Ctx, a: &GetClassArgs) -> XyzResult<api::ClassSource> {
    api::class_source(&[p(&a.input)], &a.class, &a.dex).map_err(map_err)
}

// ---- ddc.getmethod -----------------------------------------------------------

#[derive(XyzArgs)]
struct GetMethodArgs {
    #[xyz(desc = "input: APK / dex / container", required)]
    input: String,
    #[xyz(desc = "class.method (dotted), or a bare class for the whole class", required)]
    method: String,
    #[xyz(desc = "restrict images whose entry name contains this (repeatable)")]
    dex: Vec<String>,
}

fn getmethod(_ctx: &Ctx, a: &GetMethodArgs) -> XyzResult<api::MethodSource> {
    api::method_source(&p(&a.input), &a.method, &a.dex).map_err(map_err)
}

// ---- ddc.findrefs ------------------------------------------------------------

#[derive(XyzArgs)]
struct FindRefsArgs {
    #[xyz(desc = "input: APK / dex / container", required)]
    input: String,
    #[xyz(desc = "reference kind", required, enum = "string,type,method,field")]
    kind: String,
    #[xyz(desc = "query: literal substring / type name / member name", required)]
    query: String,
    #[xyz(desc = "owner class for method/field (exact unless --fuzzy-class)")]
    class: String,
    #[xyz(desc = "match the owner class fuzzily")]
    fuzzy_class: bool,
    #[xyz(desc = "restrict images whose entry name contains this (repeatable)")]
    dex: Vec<String>,
}

fn findrefs(_ctx: &Ctx, a: &FindRefsArgs) -> XyzResult<Vec<api::RefRow>> {
    api::ref_rows(
        &p(&a.input),
        &a.kind,
        &a.query,
        empty_or_some(&a.class),
        a.fuzzy_class,
        &a.dex,
    )
    .map_err(map_err)
}

// ---- ddc.strings ---------------------------------------------------------------

#[derive(XyzArgs)]
struct StringsArgs {
    #[xyz(desc = "input: APK / dex / container", required)]
    input: String,
    #[xyz(desc = "substring filter (case-insensitive)")]
    filter: String,
    #[xyz(desc = "map each string to its referencing methods")]
    with_locations: bool,
    #[xyz(desc = "restrict images whose entry name contains this (repeatable)")]
    dex: Vec<String>,
}

fn strings(_ctx: &Ctx, a: &StringsArgs) -> XyzResult<Vec<api::StringRow>> {
    api::string_rows(&p(&a.input), empty_or_some(&a.filter), a.with_locations, &a.dex)
        .map_err(map_err)
}

// ---- ddc.hierarchy -------------------------------------------------------------

#[derive(XyzArgs)]
struct HierarchyArgs {
    #[xyz(desc = "input: APK / dex / container", required)]
    input: String,
    #[xyz(desc = "class name, dotted or slashed", required)]
    class: String,
    #[xyz(desc = "restrict images whose entry name contains this (repeatable)")]
    dex: Vec<String>,
}

fn hierarchy(_ctx: &Ctx, a: &HierarchyArgs) -> XyzResult<Vec<api::RelationRow>> {
    api::hierarchy_rows(&p(&a.input), &a.class, &a.dex).map_err(map_err)
}

// ---- ddc.disasm ----------------------------------------------------------------

#[derive(XyzArgs)]
struct DisasmArgs {
    #[xyz(desc = "input: APK / dex / container", required)]
    input: String,
    #[xyz(desc = "class name, optionally Class.method", required)]
    target: String,
    #[xyz(desc = "restrict images whose entry name contains this (repeatable)")]
    dex: Vec<String>,
}

fn disasm(_ctx: &Ctx, a: &DisasmArgs) -> XyzResult<String> {
    api::disasm_text(&p(&a.input), &a.target, &a.dex).map_err(map_err)
}

// ---- ddc.manifest --------------------------------------------------------------

#[derive(XyzArgs)]
struct ManifestArgs {
    #[xyz(desc = "input: APK / container", required)]
    input: String,
    #[xyz(desc = "extract one component group", enum = "launcher,activity,service,receiver,provider,activity-alias,application,permission")]
    component: String,
}

fn manifest(_ctx: &Ctx, a: &ManifestArgs) -> XyzResult<String> {
    api::manifest_text(&p(&a.input), empty_or_some(&a.component)).map_err(map_err)
}

// ---- ddc.mainactivity ----------------------------------------------------------

#[derive(XyzArgs)]
struct MainActivityArgs {
    #[xyz(desc = "input: APK / container", required)]
    input: String,
}

fn mainactivity(_ctx: &Ctx, a: &MainActivityArgs) -> XyzResult<api::MainActivityReport> {
    api::main_activity(&p(&a.input)).map_err(map_err)
}

// ---- registration ---------------------------------------------------------------

/// The CLI channel is skipped on every command: the existing ddc CLI
/// surface is untouched, and `serve`/`mcp` (xyz mode words) are the only
/// new top-level words. Flip a command's CliHints to enable the xyz CLI
/// rendering for it later.
fn skip_cli() -> CliHints {
    CliHints { skip: true, ..Default::default() }
}

/// 空 method = xyz §11.1 默认双路由：GET 绑 query、POST 绑 JSON body
/// （query 同样生效，body 缺席键由 query 补齐）。
fn http(path: &str) -> HTTPHints {
    HTTPHints {
        method: String::new(),
        path: path.into(),
        ..Default::default()
    }
}

fn mcp_read() -> MCPHints {
    MCPHints {
        annotations: vec!["read".into()],
        ..Default::default()
    }
}

/// Register every ddc query command and dispatch `args` through the xyz
/// pipeline. Only reached with `serve` / `mcp` as the first argument
/// (see main); returns the process exit code.
pub fn run(args: Vec<String>) -> i32 {
    xyz_rust::define("ddc.info", info)
        .summary("App context and per-image statistics")
        .cli(skip_cli())
        .http(http("/info"))
        .mcp(mcp_read())
        .also(&[
            &xyz_rust::define("ddc.listclasses", listclasses)
                .summary("List class names, optionally filtered")
                .cli(skip_cli())
                .http(http("/classes"))
                .mcp(mcp_read()),
            &xyz_rust::define("ddc.getclass", getclass)
                .summary("Decompile one class (nested classes included)")
                .cli(skip_cli())
                .http(http("/class"))
                .mcp(mcp_read()),
            &xyz_rust::define("ddc.getmethod", getmethod)
                .summary("Decompile one method, all overloads")
                .cli(skip_cli())
                .http(http("/method"))
                .mcp(mcp_read()),
            &xyz_rust::define("ddc.findrefs", findrefs)
                .summary("Cross-references to a string / type / method / field")
                .cli(skip_cli())
                .http(http("/findrefs"))
                .mcp(mcp_read()),
            &xyz_rust::define("ddc.strings", strings)
                .summary("String table, optionally with referencing methods")
                .cli(skip_cli())
                .http(http("/strings"))
                .mcp(mcp_read()),
            &xyz_rust::define("ddc.hierarchy", hierarchy)
                .summary("Supertypes and subtypes of a class")
                .cli(skip_cli())
                .http(http("/hierarchy"))
                .mcp(mcp_read()),
            &xyz_rust::define("ddc.disasm", disasm)
                .summary("Raw bytecode listing of a class or method")
                .cli(skip_cli())
                .http(http("/disasm"))
                .mcp(mcp_read()),
            &xyz_rust::define("ddc.manifest", manifest)
                .summary("AndroidManifest.xml as text XML")
                .cli(skip_cli())
                .http(http("/manifest"))
                .mcp(mcp_read()),
            &xyz_rust::define("ddc.mainactivity", mainactivity)
                .summary("Package and launcher activity, verified in the dex")
                .cli(skip_cli())
                .http(http("/mainactivity"))
                .mcp(mcp_read()),
        ])
        .run_args(args)
}
