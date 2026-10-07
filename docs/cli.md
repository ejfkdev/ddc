# CLI reference

[English] | [简体中文](zh-CN/cli.md)

Everything `ddc --help` tells you, in more detail: invocation modes,
every option's semantics, every subcommand's output format and behavior.
The binary's help is the quick card; this is the manual.

Four invocation shapes:

```
ddc [OPTIONS] <INPUT>... [OUTPUT]      # full decompile
ddc <SUBCOMMAND> [ARGS...]             # progressive analysis (query)
ddc serve|http [--addr HOST:PORT]      # REST + OpenAPI (+ /mcp on serve)
ddc mcp stdio|http                     # MCP tool server
```

## Language

Messages (help, errors, summaries, table headers) localize automatically.
Resolution: `DDC_LANG` (explicit `zh`/`en`) overrides `LC_ALL` >
`LC_MESSAGES` > `LANG` > `LANGUAGE`; the first variable stating a
language decides — any `zh*` value selects Chinese (`zh`, `zh_CN`,
`zh-Hans`, `zh_TW.UTF-8`, …), any other language English. `LANGUAGE`
is a colon-separated priority list (`zh:en`) — the first entry counts.
`C`, `POSIX` and empty values state no language: the chain keeps
walking. With no locale variable set at all — plain cmd.exe and
PowerShell export none — Windows falls back to the user's UI language
(the one Windows itself displays); set `DDC_LANG` to pin a language.
Git Bash, Cygwin and WSL export `LANG` and are covered by the chain.

```bash
DDC_LANG=zh ddc --help    # 中文帮助
LANG=zh_CN.UTF-8 ddc -V   # also Chinese
LC_ALL=C LANG=zh_CN ddc -V  # C states nothing → LANG decides → Chinese
DDC_LANG=en ddc -V        # forced English even under a zh locale
```

## Full decompile

`ddc [OPTIONS] <INPUT>... [OUTPUT]`

**Inputs** (multiple merge into one class pool; duplicate classes are
deduplicated, first definition wins):

- `.dex` — versions 035–041
- `.apk` / `.jar` / `.zip` — every `classes.dex`, `classes2.dex`, … entry,
  merged in numeric order
- `.xapk` / `.apks` / `.apkm` — a zip of APKs: every inner APK's dexes
  merge, base first; labels are three-level (`container!base.apk!
  classes.dex`)
- a directory — scanned recursively for all of the above

**Output** — the last positional argument or `-o`:

- `<dir>` — output root, package structure preserved; default
  `<input-stem>-out/` next to the input
- `<file.java>` — a single class only (single-class input or `-c`)
- `-` — stdout, classes separated by `// ===== class =====` lines, in pool
  order (forces one thread)

Positional-argument disambiguation: with no `-o` and two or more
positionals, a **last** argument that isn't input-shaped (no dex-bearing
extension, not an existing file, not a directory containing dex files) is
the output — so `ddc app.apk out/` writes into the pre-created `out/`,
while `ddc a.dex dump/` treats a real dex directory as an input.

| Option | Semantics |
|---|---|
| `-o, --output <path>` | output location: dir / `file.java` / `-` |
| `-c, --class FQCN` | decompile only this class (dotted or slashed; nested/anonymous/local classes of it come along) |
| `-l, --list` | list class names and exit |
| `-t, --threads <n>` | worker count (default: CPU count; stdout forces one for pool order) |
| `--no-comments` | omit the provenance header |
| `-v, --verbose` | per-dex stats and slow classes on stderr |
| `-h, --help` / `-V, --version` | help / name+version+homepage |

`--opt=value` and `-o=path` forms both work. Bare `ddc help` / `ddc
version` do the same as `-h` / `-V`; `ddc` with no arguments at all
prints the help (exit 0). `ddc help <SUBCOMMAND>` and
`ddc <SUBCOMMAND> --help` print that command's own arguments and
options instead of the main help; `ddc help serve|http|mcp` (or
`ddc serve -h`, …) document each server mode's flags, endpoints and
worked connection examples.

### Output formats (`--format` on every query command)

`auto` (the default) picks by stdout: a terminal gets the human
rendering, a pipe gets the stable text form scripts have always
parsed.

| Format | Renders |
|---|---|
| `auto` | table on a terminal, text when piped |
| `text` | the fixed-width column format (legacy, stable) |
| `table` | aligned tables with header rules |
| `markdown` / `md` | pipe tables; source-shaped reports (`getclass`, `disasm`, `manifest`) wrap in fenced code blocks |
| `json` | the typed reports the HTTP/MCP frontends serve |
| `jsonl` | the same, one compact line per element |

### Server modes (HTTP REST, OpenAPI, MCP)

One process, three fronts — every query command defined once via
[xyz-rust](https://github.com/ejfkdev/xyz-rust):

| Command | Serves | `/mcp` |
|---|---|---|
| `ddc serve [--addr :8080]` | 10 REST routes + `GET /openapi.json` + `GET /healthz` | streamable MCP endpoint |
| `ddc http [--addr :8080]` | REST + OpenAPI only | not mounted (404) |
| `ddc mcp stdio` | MCP over stdin/stdout (agent subprocess) | — |
| `ddc mcp http` | MCP-only streamable server | — |

Shared flags: `--addr`, `--bearer tok1,tok2` (Authorization: Bearer
on REST and `/mcp`), `--timeout`, `--cors`, `--tls-cert` +
`--tls-key` (HTTPS), `--default k=v` (channel defaults). MCP modes
add `--versions` (spec revisions), `--stateless`, `--session-timeout`,
`--name`, `--server-version`.

An MCP client connects to `http://<addr>/mcp` when serve is running
(the endpoint answers plain curl GETs with 406 — it speaks the MCP
streamable protocol). Each REST route accepts both GET (query
params) and POST (JSON body); REST examples:

```bash
ddc serve --addr 127.0.0.1:8080
curl '127.0.0.1:8080/class?input=app.apk&class=com.example.Foo'
curl -X POST '127.0.0.1:8080/class' -H 'Content-Type: application/json' \
    -d '{"input":"app.apk","class":"com.example.Foo"}'
curl '127.0.0.1:8080/findrefs?input=app.apk&kind=string&query=token'
# Claude Desktop: mcpServers → { ddc: { command: ddc, args: [mcp, stdio] } }
```

**Provenance header** (unless `--no-comments`):

```java
// Decompiled by https://github.com/ejfkdev/ddc 1.2.3
// From: app!classes3.dex (DEX 038)
// Source file: Foo.java
```

`From:` names the input file, the dex image, and the DEX version — the
fastest clue for which image a class lives in (jadx's `loaded from:`
pattern). Headers carry no timestamps, so two runs diff cleanly; a few
variable IDs may still differ between runs (std HashMap seed), with
identical semantics.

**Exit codes**: `0` success; `1` some classes failed (e.g. a
pathological-CFG timeout); `2` usage error (the error message only —
run `ddc --help` for the reference). A one-line summary goes to stderr when a run finishes: `ddc:
wrote 98348 file(s) to out/, 1 failed in 6.13s`.

## Progressive-analysis subcommands

Every dex-reading subcommand accepts `-d/--dex NAME` (repeatable;
entry-name substring — the filter runs before parsing, so
`getclass --dex classes20` parses exactly one image); several accept
`-o FILE` to write the result. `ddc help <SUBCOMMAND>` (or
`ddc <SUBCOMMAND> --help`) documents each command's own signature.
Queries never enter the lift/structure/render pipeline; stdout stays
clean (timing prints only with `-o`).

### Get oriented

- **`ddc info <input>`** — one command, the whole picture. A context
  header first (when a manifest exists): app label — an `@0x…` ref is
  resolved through a minimal resources.arsc walk, literals print
  as-is — package, `versionName (versionCode)`, custom Application
  class, launcher activity, `uses-sdk` bounds, file size and MD5 (the
  label lookup mirrors the manifest's base-first container rule). Then
  the per-dex table: one row per image with version, class, method,
  field, string counts, plus a total row. Bare `.dex` inputs skip the
  header (no manifest) and print the table only.
- **`ddc listclasses <input> [pattern]`** — class names (internal
  `com/foo/Bar` form); pattern is a case-insensitive substring.
- **`ddc manifest <apk> [--component C] [-o FILE]`** — decodes the
  binary AndroidManifest.xml to text XML. `--component` filters to one
  element kind: `launcher` (the MAIN/LAUNCHER activity), `activity`,
  `service`, `receiver`, `provider`, `permission`, `activity-alias`,
  `application`. Raw `.axml` files are accepted directly. In XAPK/APKS
  containers the base APK's manifest is used.
- **`ddc mainactivity <apk>`** — package, custom Application class (if
  any), and the launcher activity, resolved through relative-name rules
  (`.MainActivity` → package-prefixed; bare word → package + word) and
  activity-alias `targetActivity`; then verified against the dex images
  (the defining image is reported).
- **`ddc res <apk> [entry] [-o FILE]`** — without an entry: every archive
  entry (method, compressed size; XAPK inner APKs flattened into
  `apk!name` labels). With an entry: dumps it — binary XML (first chunk
  `0x0003`) decodes through the AXML decoder, text prints as-is, binary
  content is saved via `-o` (or the error tells you to). Entry matching:
  exact name first, then a unique substring.

### Find things

- **`ddc strings <input> [-f TEXT] [--with-locations]`** — the string
  table (one row per string). `-f` filters by substring;
  `--with-locations` walks every method's const-string sites and adds a
  `used-by` column mapping each hit to its owner methods.
- **`ddc findrefs <input> <string|type|class|method|field> <query> [--class
  FQCN] [--fuzzy-class] [-o FILE]`** — every reference to a string
  literal / type / method call site / field access (`class` is a plain-
  word alias for `type`; definition-level relations such as
  extends/implements belong to `ddc hierarchy`). Output is columnar
  with a header (`dex kind class method refs`), **one row per method**:
  multiple hits aggregate into `refs` (`; `-separated, deduped); `kind`
  is the first hit's instruction. Match semantics: queries are
  case-insensitive substrings; `--class` defaults to exact (dots,
  slashes and `L…;` descriptor forms all normalize) and `--fuzzy-class`
  widens it to substring.
- **`ddc callers <input> NAME [FQCN]`** — who invokes method NAME (the
  findrefs method machinery, scoped to one class optionally).
- **`ddc members <input> [NAME] [--class FQCN] [--fuzzy-class]
  [--method|--field]`** — method/field name search over the method and
  field id tables; `--method` / `--field` restrict the kind.

### Understand structure

- **`ddc hierarchy <input> FQCN`** — the class's lineage: `class` /
  `extends` / `implements` forward, `sub` / `impl` for every class that
  extends or implements it. Works across images (name-matched when the
  parent lives in another dex).
- **`ddc largest <input> [-n N]`** — top-N methods by instruction count
  (find the monsters; default 20).
- **`ddc disasm <input> FQCN[.method]`** — raw bytecode of a class or
  one method: one line per instruction (`pc opcode mnemonic`). The
  target resolves as a whole-string class first, then splits at the last
  dot — so both `org.foo.Cells.t1` (a class) and `Greeter.greet` (a
  method) work.

### Decompile surgically

- **`ddc getclass <input> FQCN [-o FILE] [--dex NAME]`** — one class
  with its nested/anonymous/local classes, through the full pipeline.
  Ambiguous names (defined in several images) get a warning listing the
  images; the defining image is registered first so the pool resolves
  from it.
- **`ddc getmethod <input> FQCN[.method] [-o FILE]`** — one method,
  sliced out of the decompiled class: provenance header + package line +
  every matching overload, dedented. A miss lists the class's method
  names. A bare class name falls back to the whole class.
- **`ddc pkg <input> PACKAGE [-o DIR] [-t N] [--app]`** — decompile a
  whole package subtree through the full pipeline: the package name is
  a segment-boundary prefix, so `com.example.app` matches every class
  under it, subpackages like `com.example.app.ui` included (default output:
  `<package-with-underscores>-pkg/` next to the input). `""` or `.`
  means the root (default package included). `--app` takes the package
  from the manifest — and when that package has no classes (Telegram:
  manifest says `org.telegram.messenger.web`, code lives in
  `org.telegram.messenger`) it retries with the launcher class's
  package, which is where an app's own code clusters.

## Output quality passes

Four passes clean the decompiled body, all on by default:

- **jadx-style local names** — a synthetic `v12`/`p3` never survives
  when a better name exists: a Kotlin
  `Intrinsics.checkNotNullParameter(x, "name")` names `x` from the
  message string (the compiler wrote the real parameter name into the
  check); the single consistent defining call (`getFoo() → foo`,
  `new File(…) → file`); jadx's type-alias table (`str/cls/it/…`)
  with the lowercased class simple name as fallback. Collisions take
  `2, 3, …`; debug-info names are never touched.
- **Kotlin null-check elision** — statement-position
  `Intrinsics.checkNotNull…` calls are runtime assertions; they are
  dropped after the naming pass harvested their strings (lark:
  59,106 → 9).
- **Synthetic-accessor inlining** — `access$NNN` static bridges inline
  at call sites when the body is an identity, a field getter, or a
  method forwarder (the d8 APM trace wrappers around the core are
  tolerated). Only STATIC+SYNTHETIC callees qualify. lark: 43% of
  9,214 call sites.
- **IntDef constant rendering** — see below.

## Platform symbols

ddc renders IntDef/LongDef literal arguments as their constant names
out of the box (`setVisibility(8)` → `android.view.View.GONE`). The
domain table (android-37) lives in the repo as a READABLE,
diffable text file — `crates/ddc-cli/src/platform_symbols.txt`, one
domain per line — which build.rs raw-DEFLATEs into the binary (72KB).
Regenerate with `scripts/gen-platform-symbols.sh [platform-dir]` and
commit the .txt. The table is exact-match — combined flag values stay
numeric — and version-independent in effect: methods missing from the
baked API level simply stay numeric. Startup cost is under 5ms.

`--symbols <sdk-platform-dir>` (e.g.
`~/Library/Android/sdk/platforms/android-37.0`, needs `android.jar`
and `data/annotations.zip`) rebuilds the table from that platform for
this invocation, overriding the built-in — full decompile and
subcommands alike.

## Exit codes for subcommands

Usage errors (missing arguments, unknown options, bad `--dex`) print the
error message only and exit `2`. Query misses (class/method/entry
not found) are also `2` but carry the specific miss in the error —
`getmethod` lists the available methods, `--dex` the available images.
