# CLI reference

[English] | [简体中文](zh-CN/cli.md)

Everything `ddc --help` tells you, in more detail: invocation modes,
every option's semantics, every subcommand's output format and behavior.
The binary's help is the quick card; this is the manual. The complete help output captured from the shipped binary, plus an example run for every subcommand, is at the end of this page.

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
| `-t, --threads <n>` | worker count (default: CPU count minus the file-writer pool; stdout forces one for pool order) |
| `--no-comments` | omit the provenance header |
| `--fast` | skip the cure code-simplification pass, keep formatting (roughly halves large runs) |
| `--symbols <dir>` | rebuild the IntDef symbol table from an SDK platform dir (`android.jar` + `data/annotations.zip`) |
| `-v, --verbose` | per-dex stats and slow classes on stderr |
| `-h, --help` / `-V, --version` | help / name+version+homepage |

`--opt=value` and `-o=path` forms both work. Bare `ddc help` / `ddc
version` do the same as `-h` / `-V`; `ddc` with no arguments at all
prints the help (exit 0). `ddc help <SUBCOMMAND>` and
`ddc <SUBCOMMAND> --help` print that command's own arguments and
options instead of the main help; `ddc help serve|http|mcp` (or
`ddc serve -h`, …) document each server mode's flags, endpoints and
worked connection examples.

### Output formats (`--format` on query commands with a typed report; `res` and `largest` print text only)

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
- **`ddc listclasses <input> [pattern]`** — class structure as a
  compressed prefix frontier (the default): one row per branching package
  (`pkg/ (classes)`) plus class leaves, so a 200k-class app fits on one
  screen. Pattern is a case-insensitive substring filter applied first.
  Drill with `--prefix P` (segment-exact: `a.b` matches `a.b` and
  `a.b.*`) and `--depth N` (aggregate exactly N segments below the
  scope); `--sort count` puts the biggest groups first. `--limit` /
  `--offset` page rows (classes in the flat view): `--format json`
  carries `total`/`offset`/`returned`/`next_offset`, and the summary view
  is always a JSON object with `rows[].path/kind/classes`. Paging is
  deterministic: images are visited in input order and rows within an
  image in table order, so `--offset` resumes without skipping or
  duplicating rows. `--full` restores the pre-summary behavior: flat,
  unwindowed class names.
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
- **`ddc res <apk> [entry] [-o FILE]`** — without an entry: a directory
  summary of the archive with per-prefix entry counts (`res/drawable/
  (1234)`), `--full` for the flat table (method, compressed size; XAPK
  inner APKs flattened into `apk!name` labels), `--prefix` to expand a
  directory, plus `--depth/--sort/--limit/--offset`. With an entry: dumps
  it — binary XML (first chunk `0x0003`) decodes through the AXML
  decoder, text prints as-is, binary content is saved via `-o` (or the
  error tells you to). Entry matching: exact name first, then a unique
  substring.

### Find things

- **`ddc strings <input> [-f TEXT] [--with-locations]`** — the string
  table (one row per string). `-f` filters by substring;
  `--with-locations` walks every method's const-string sites and adds a
  `used-by` column mapping each hit to its owner methods. `--summary`
  groups matching **reference sites** by owning class/package (which
  packages use this URL/key?), drilled with `--prefix`/`--depth` and
  optionally annotated with `--samples N` example classes; `--full`
  prints the raw string table (the default view). `--limit` /
  `--offset` page rows (JSON gains `total`/`next_offset`); the MCP
  tool caps the default at 500 rows and appends a truncation hint.
- **`ddc findrefs <input> <string|type|class|method|field> <query> [--class
  FQCN] [--fuzzy-class] [-o FILE]`** — every reference to a string
  literal / type / method call site / field access (`class` is a plain-
  word alias for `type`; definition-level relations such as
  extends/implements belong to `ddc hierarchy`). The default view is a
  prefix frontier of the **owning classes** with per-package hit counts
  (which packages use this string?), drilling with `--prefix`/`--depth`;
  `--full` prints every hit row: columnar with a header (`dex kind class
  method refs`), **one row per method**, multiple hits aggregate into
  `refs` (`; `-separated, deduped). `--sort`, `--limit` and `--offset`
  apply to either view. Match semantics: queries are case-insensitive
  substrings; `--class` defaults to exact (dots, slashes and `L…;`
  descriptor forms all normalize) and `--fuzzy-class` widens it to
  substring.
- **`ddc callers <input> NAME [FQCN]`** — who invokes method NAME (the
  findrefs method machinery, scoped to one class optionally; accepts the
  same `--full/--prefix/--depth/--sort/--limit/--offset` view flags).
- **`ddc members <input> [NAME] [--class FQCN] [--fuzzy-class]
  [--method|--field]`** — method/field name search over the method and
  field id tables; `--method` / `--field` restrict the kind. The default
  view groups matches by owning class/package with counts; `--full`
  prints one row per member; `--format json` is available; the summary
  accepts `--prefix/--depth/--sort/--limit/--offset/--samples N`.

### Understand structure

- **`ddc hierarchy <input> FQCN`** — the class's lineage: `class` /
  `extends` / `implements` forward, `sub` / `impl` for every class that
  extends or implements it. Works across images (name-matched when the
  parent lives in another dex). The default view is a per-relation
  prefix frontier (`sub com.example/ (123)`), so a class with 150k
  subtypes fits on one screen; `--full` prints every relation row,
  `--prefix/--depth/--sort/--limit/--offset` shape the view.
- **`ddc largest <input> [-n N]`** — top-N methods by instruction count
  (find the monsters; default 20).
- **`ddc disasm <input> FQCN[.method]`** — raw bytecode of a class or
  one method: one line per instruction (`pc opcode mnemonic`). The
  target resolves as a whole-string class first, then splits at the last
  dot — so both `org.foo.Cells.t1` (a class) and `Greeter.greet` (a
  method) work. A plain method name lists every overload; append the
  full descriptor (`'Greeter.greet()Ljava/lang/String;'`) to select one
  exactly.

### Decompile surgically

- **`ddc getclass <input> FQCN [-o FILE] [--dex NAME] [--fast]
  [--low-memory]`** — one class with its nested/anonymous/local classes,
  through the full pipeline. Ambiguous names (defined in several images)
  get a warning listing the images; the defining image is registered
  first so the pool resolves from it. The default path builds a
  dependency closure: it parses the defining image plus the images of
  the target's supertypes, signatures, code references and enclosing
  classes, then adds images named by unresolved lookups and re-renders
  (up to three rounds). Archives with four or fewer images, or a closure
  that reaches ~90% of the images, use the full path. `--low-memory`
  selects the two-tier mode: start from the defining image and load
  further images only when the renderer asks for a class they define,
  re-rendering until stable — slower on highly connected classes.
  `--fast` skips cure. `--low-memory` is text-only; the default path
  supports every output format.
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

## Full help output

Captured from ddc 0.1.35. Regenerate with `DDC_LANG=en ddc --help` / `DDC_LANG=en ddc help <COMMAND>`.

### Main help

```text
ddc 0.1.35 — DEX → Java decompiler
https://github.com/ejfkdev/ddc  (MIT license)

Usage:
  ddc [OPTIONS] <INPUT>... [OUTPUT]     full decompile
  ddc <SUBCOMMAND> [ARGS...]            metadata query, no full decompile
  ddc serve [--addr HOST:PORT]          HTTP REST + OpenAPI + MCP endpoint
  ddc http  [--addr HOST:PORT]          HTTP REST + OpenAPI only
  ddc mcp stdio|http                    MCP tool server (10 query tools)
  ddc help [SUBCOMMAND]                 this help, or one command's
  ddc version | -h | -V

Language: DDC_LANG=zh|en forces; otherwise auto-detected from the
locale environment (on Windows: the system UI language).

Decompiler for Android DEX images: versions 035-041, multi-dex APKs,
XAPK/APKS/APKM containers, invoke-custom. Fifteen subcommands
answer metadata queries (strings, cross-references, hierarchies,
per-method disassembly) without a full decompile.

INPUT: a .dex file, an .apk/.jar/.zip archive (classes.dex,
classes2.dex, ...), an .xapk/.apks/.apkm container (a zip of APKs;
every inner APK's dexes merge, base first), or a directory (scanned
recursively). Multiple inputs merge into one class pool (duplicate
classes deduplicated, first definition wins).

OUTPUT — last positional argument or -o:
  <dir>        output root, package structure preserved
  <file>.java  one class (single-class input or -c)
  -            stdout, classes separated by `// ===== class =====`
  default      <input-stem>-out/ next to the input

Options:
  -o, --output <path>   output location (dir / file.java / -)
  -c, --class FQCN      decompile only this class (dotted or slashed;
                        nested/anonymous/local classes of it included)
  -l, --list            list class names and exit
  -t, --threads <n>     parallel workers (default: CPU count minus the
                        file-writer pool; stdout forces one thread for pool order)
  --no-comments         omit the provenance header
  --fast                skip the code-simplification pass (keep formatting
                        only) — roughly halves the time on large APKs
  --symbols <dir>       IntDef symbol table (built-in android-37 by
                        default; this rebuilds it from an SDK platform
                        dir: android.jar + data/annotations.zip)
  -v, --verbose         per-dex stats and slow classes on stderr
  -h, --help            print this help
  -V, --version         print name, version and homepage

Subcommands — `ddc help <SUBCOMMAND>` (or `ddc <SUBCOMMAND> --help`)
documents each command's own arguments, options, and examples.
read dex images accept -d/--dex NAME (repeatable, entry-name
substring) to restrict the image set.

  Overview:
    info <input>                    app context + per-image counts
    listclasses <input> [pattern]   prefix tree with counts (--full = raw names)
    manifest <apk> [--component C]  AndroidManifest.xml → text XML
    mainactivity <apk>              package + launcher activity
    res <apk> [entry] [-o FILE]     archive entries; dump one

  Search:
    strings <input> [-f TEXT] [--with-locations]
                                    string table, owning methods
    findrefs <input> string|type|class|method|field NAME [--class FQCN]
                                    cross-references to a query
    callers <input> NAME [FQCN]     methods invoking NAME
    members <input> [NAME] [--class FQCN] [--method|--field]
                                    method/field name search

  Structure:
    hierarchy <input> FQCN          super-/subtypes of a class
    largest <input> [-n N]          top-N methods by instruction count
    disasm <input> FQCN[.method]    raw bytecode (opcode + pc)

  Targeted decompilation:
    getclass <input> FQCN [--fast] [-o f]  one class (+nested)
    getmethod <input> FQCN.method   one method, all overloads
    pkg <input> com.foo [-o DIR]    one package incl. subpackages;
                                    --app uses the manifest package

Output formats — query commands with a typed report take --format (res and largest print text only):
  auto (default)   table when stdout is a terminal, text when piped
  text             the stable column format scripts have always parsed
  table            aligned tables with header rules (xyz-rust renderer)
  markdown | md    pipe tables / fenced source blocks
  json | jsonl     the typed reports the HTTP and MCP frontends serve

HTTP & MCP (built in, via github.com/ejfkdev/xyz-rust):
  ddc serve --addr :8080     one process, three fronts on one port:
                             10 REST routes, GET /openapi.json,
                             GET /healthz, and the streamable MCP
                             endpoint at /mcp — point an MCP client
                             at http://127.0.0.1:8080/mcp
  ddc mcp stdio              MCP over stdin/stdout, no network — the
                             agent's subprocess (10 tools with input
                             and output schemas: ddc.getclass, ...)
  ddc mcp http               MCP-only streamable server (loopback host
                             by default); serve serves the same /mcp
  ddc http --addr :8080      REST + OpenAPI only, no /mcp endpoint
  `ddc help serve` / `ddc help mcp` document every flag and endpoint.

Exit status: 0 success; 1 some classes failed; 2 usage error.

Examples:
  # full decompile
  ddc app.apk                          # → app-out/ next to the apk
  ddc app.apk src/                     # positional output directory
  ddc app.apk -o - | less              # everything to stdout
  ddc base.apk patch.dex -o merged/    # split inputs, one pool

  # one class / one method
  ddc app.apk -c com.example.Foo
  ddc getclass app.apk com.example.Foo -o Foo.java --dex classes3
  ddc getmethod app.apk com.example.Foo.toString

  # search
  ddc findrefs app.apk string api_key
  ddc findrefs app.apk method onCreate --class android/app/Activity
  ddc strings app.apk -f token --with-locations
  ddc callers app.apk sendMessage

  # survey before decompiling
  ddc mainactivity app.apk
  ddc manifest app.apk --component launcher
  ddc hierarchy app.apk androidx.fragment.app.FragmentActivity
  ddc pkg app.apk --app -o own/        # the app's own code only
```

### `info`

```text
ddc info — application context and per-image statistics

Usage: ddc info <INPUT> [options]

Options:
  -d, --dex NAME  restrict to images whose entry name contains NAME (repeatable)
  --format FMT    auto | text | table | markdown | md | json | jsonl (default auto: table on a terminal, text when piped)
  -h, --help      print this help

Examples:
  ddc info app.apk
      app context and the per-image class/method/field table
  ddc info app.apk -d classes2
      restrict the table to matching images
```

### `listclasses`

```text
ddc listclasses — class structure as a prefix frontier with counts (--full: raw names)

Usage: ddc listclasses <INPUT> [PATTERN] [options]

Arguments:
  PATTERN  case-insensitive substring filter

Options:
  -d, --dex NAME     restrict to images whose entry name contains NAME (repeatable)
  --format FMT       auto | text | table | markdown | md | json | jsonl (default auto: table on a terminal, text when piped)
  --full             complete raw class names (default: compressed prefix frontier)
  --prefix P         segment-exact scope (a.b matches a.b and a.b.*); also filters the full view
  --depth N          aggregate exactly N segments below the scope
  --sort name|count  row order (count = biggest groups first)
  --samples N        show up to N example class names per group
  --limit N          keep at most N rows/classes (0 = unlimited); JSON gains total/next_offset
  --offset M         skip M rows/classes — resume where the previous page stopped
  -h, --help         print this help

Examples:
  ddc listclasses app.apk
      one screen of structure instead of 200k lines
  ddc listclasses app.apk --sort count
      biggest packages first
  ddc listclasses app.apk --prefix com.example --depth 1
      children one level down
  ddc listclasses app.apk | grep '/ ('
      just the package rows
  ddc listclasses app.apk --full | wc -l
      full listing: total class count
  ddc listclasses app.apk --limit 20 --offset 40 --format json
      paged rows with next_offset
```

### `manifest`

```text
ddc manifest — decode AndroidManifest.xml into text XML

Usage: ddc manifest <APK> [options]

Options:
  ‑c, --component KIND    one component group only (launcher|activity|service|receiver|provider|permission|activity-alias|application; plurals accepted)
  -o, --output FILE       write the result to FILE instead of stdout
  -h, --help              print this help

Examples:
  ddc manifest app.apk
      the whole manifest as text XML
  ddc manifest app.apk --component launcher
      the launcher activity's intent filters
  ddc manifest app.apk -o manifest.xml
      save to a file
```

### `mainactivity`

```text
ddc mainactivity — print the package and launcher activity, verified against the dex

Usage: ddc mainactivity <APK> [options]

Options:
  -d, --dex NAME  restrict to images whose entry name contains NAME (repeatable)
  --format FMT    auto | text | table | markdown | md | json | jsonl (default auto: table on a terminal, text when piped)
  -h, --help      print this help

Examples:
  ddc mainactivity app.apk
      package + launcher activity, verified in the dex
```

### `res`

```text
ddc res — list archive entries, or dump one entry's content

Usage: ddc res <APK> [ENTRY] [options]

Arguments:
  ENTRY  archive entry to dump; without it, all entries are listed

Options:
  -o, --output FILE  write the entry's raw bytes (binary XML stays binary; stdout decodes it)
  --full             complete raw rows (default: compressed prefix frontier)
  --prefix P         segment-exact scope; expand a frontier row
  --depth N          aggregate exactly N segments below the scope
  --sort name|count  row order (count = biggest groups first)
  --samples N        show up to N example leaf names per group
  --limit N          keep at most N rows (0 = unlimited)
  --offset M         skip M rows — resume where the previous page stopped
  -h, --help         print this help

Examples:
  ddc res app.apk
      directory summary of the archive
  ddc res app.apk --full | head -40
      full entry listing
  ddc res app.apk --prefix res/drawable
      one directory's files
  ddc res app.apk AndroidManifest.xml
      decode one entry (binary XML becomes text)
  ddc res app.apk resources.arsc -o arsc.bin
      save one entry's raw bytes
```

### `strings`

```text
ddc strings — the string table, with the methods referencing each string

Usage: ddc strings <INPUT> [options]

Options:
  -f, --filter TEXT  keep strings containing TEXT (case-insensitive)
  --with-locations   map each string to its referencing methods
  --summary          group matching reference sites by owning class/package (default view is the string table)
  --full             complete raw string table (the default); overrides --summary
  --prefix P         with --summary: segment-exact owner-class scope
  --depth N          with --summary: aggregate exactly N segments below the scope
  --sort name|count  with --summary: row order
  --samples N        with --summary: example owner classes per group
  --limit N          keep at most N rows/strings (0 = unlimited); JSON gains total/next_offset
  --offset M         skip M rows/strings — resume where the previous page stopped
  -d, --dex NAME     restrict to images whose entry name contains NAME (repeatable)
  --format FMT       auto | text | table | markdown | md | json | jsonl (default auto: table on a terminal, text when piped)
  -h, --help         print this help

Examples:
  ddc strings app.apk -f token
      strings containing "token"
  ddc strings app.apk -f api_key --with-locations
      each hit mapped to its referencing methods
  ddc strings app.apk --limit 50 --offset 100 --format json
      paged string table
  ddc strings app.apk -f http --summary --sort count
      which packages use URLs (reference sites)
  ddc strings app.apk -f api_key --summary --samples 2
      groups with example owner classes
```

### `findrefs`

```text
ddc findrefs — cross-references: every method site referencing the query

Usage: ddc findrefs <INPUT> <KIND> <QUERY> [options]

Arguments:
  KIND   string | type | class | method | field (class = type)
  QUERY  the string, type, or member name to resolve

Options:
  -C, --class FQCN   owner class for method/field queries (exact unless --fuzzy-class)
  --fuzzy-class      match the owner class fuzzily
  --full             complete raw rows (default: compressed prefix frontier)
  --prefix P         segment-exact scope; expand a frontier row
  --depth N          aggregate exactly N segments below the scope
  --sort name|count  row order (count = biggest groups first)
  --samples N        with summary: show up to N example leaf names per group
  --limit N          keep at most N rows (0 = unlimited)
  --offset M         skip M rows — resume where the previous page stopped
  -o, --output FILE  write the result to FILE instead of stdout
  -d, --dex NAME     restrict to images whose entry name contains NAME (repeatable)
  --format FMT       auto | text | table | markdown | md | json | jsonl (default auto: table on a terminal, text when piped)
  -h, --help         print this help

Examples:
  ddc findrefs app.apk string api_key
      which packages use "api_key" (frontier with counts)
  ddc findrefs app.apk string api_key --full
      every hit row
  ddc findrefs app.apk string http --sort count
      biggest referencing packages first
  ddc findrefs app.apk class okhttp3.OkHttpClient
      code usage of the class (new/check-cast/instance-of)
  ddc findrefs app.apk method onCreate --class android/app/Activity
      invoke sites of Activity.onCreate
  ddc findrefs app.apk field CREATOR
      reads/writes of fields named CREATOR
```

### `callers`

```text
ddc callers — methods that invoke a method by name

Usage: ddc callers <INPUT> <NAME> [FQCN] [options]

Arguments:
  NAME  method name to search
  FQCN  optional owner class, narrows the search

Options:
  --full             complete raw rows (default: compressed prefix frontier)
  --prefix P         segment-exact scope; expand a frontier row
  --depth N          aggregate exactly N segments below the scope
  --sort name|count  row order (count = biggest groups first)
  --samples N        with summary: show up to N example leaf names per group
  --limit N          keep at most N rows (0 = unlimited)
  --offset M         skip M rows — resume where the previous page stopped
  -d, --dex NAME     restrict to images whose entry name contains NAME (repeatable)
  --format FMT       auto | text | table | markdown | md | json | jsonl (default auto: table on a terminal, text when piped)
  -h, --help         print this help

Examples:
  ddc callers app.apk sendMessage
      which packages invoke sendMessage (frontier)
  ddc callers app.apk sendMessage --full
      every call site row
  ddc callers app.apk onCreate android/app/Activity
      scope the callee to one owner class
```

### `members`

```text
ddc members — method and field name search

Usage: ddc members <INPUT> [NAME] [options]

Arguments:
  NAME  name substring to search

Options:
  -C, --class FQCN   restrict to one class (exact unless --fuzzy-class)
  --fuzzy-class      match the class fuzzily
  --method           methods only
  --field            fields only
  --full             complete raw rows (default: compressed prefix frontier)
  --prefix P         segment-exact scope; expand a frontier row
  --depth N          aggregate exactly N segments below the scope
  --sort name|count  row order (count = biggest groups first)
  --samples N        with summary: show up to N example leaf names per group
  --limit N          keep at most N rows (0 = unlimited)
  --offset M         skip M rows — resume where the previous page stopped
  -d, --dex NAME     restrict to images whose entry name contains NAME (repeatable)
  --format FMT       auto | text | table | markdown | md | json | jsonl (default auto: table on a terminal, text when piped)
  -h, --help         print this help

Examples:
  ddc members app.apk
      member counts per class/package (frontier)
  ddc members app.apk token
      methods/fields with "token" in the name
  ddc members app.apk --class com.example.Foo
      every member of one class, summarized
  ddc members app.apk --class com.example.Foo --full
      every member row
```

### `hierarchy`

```text
ddc hierarchy — supertypes (extends/implements) and subtypes of a class

Usage: ddc hierarchy <INPUT> <FQCN> [options]

Arguments:
  FQCN  class name, dotted or slashed

Options:
  --full             complete raw rows (default: compressed prefix frontier)
  --prefix P         segment-exact scope; expand a frontier row
  --depth N          aggregate exactly N segments below the scope
  --sort name|count  row order (count = biggest groups first)
  --samples N        with summary: show up to N example leaf names per group
  --limit N          keep at most N rows (0 = unlimited)
  --offset M         skip M rows — resume where the previous page stopped
  -d, --dex NAME     restrict to images whose entry name contains NAME (repeatable)
  --format FMT       auto | text | table | markdown | md | json | jsonl (default auto: table on a terminal, text when piped)
  -h, --help         print this help

Examples:
  ddc hierarchy app.apk java.lang.Object
      subclasses by package (frontier with counts)
  ddc hierarchy app.apk java.lang.Object --sort count
      biggest packages first
  ddc hierarchy app.apk java.lang.Object --full
      every relation row
  ddc hierarchy app.apk androidx.fragment.app.FragmentActivity
      its supertypes and every subclass
```

### `largest`

```text
ddc largest — largest methods by instruction count

Usage: ddc largest <INPUT> [options]

Options:
  -n N            report the top N methods (default 20)
  -d, --dex NAME  restrict to images whose entry name contains NAME (repeatable)
  -h, --help      print this help

Examples:
  ddc largest app.apk
      top 20 methods by instruction count (default)
  ddc largest app.apk -n 50
      top 50
```

### `disasm`

```text
ddc disasm — raw bytecode listing of a class or one method

Usage: ddc disasm <INPUT> <FQCN>[.<method>] [options]

Arguments:
  FQCN[.<method>[(params)ret]]  class; a method name lists all overloads, a full descriptor picks one

Options:
  -d, --dex NAME  restrict to images whose entry name contains NAME (repeatable)
  --format FMT    auto | text | table | markdown | md | json | jsonl (default auto: table on a terminal, text when piped)
  -h, --help      print this help

Examples:
  ddc disasm app.apk com.example.Foo
      bytecode of every method in the class
  ddc disasm app.apk com.example.Foo.toString
      one method name, all overloads
  ddc disasm app.apk 'com.example.Foo.parse(Ljava/lang/String;)V'
      one exact overload
```

### `getclass`

```text
ddc getclass — decompile one class, nested classes included

Usage: ddc getclass <INPUT>... <FQCN> [options]

Arguments:
  <INPUT>...  one or more inputs, merged into one class pool
  FQCN        class name, dotted or slashed

Options:
  -o, --output FILE  write the result to FILE instead of stdout
  -d, --dex NAME     restrict to images whose entry name contains NAME (repeatable)
  --format FMT       auto | text | table | markdown | md | json | jsonl (default auto: table on a terminal, text when piped)
  --fast             skip the code-simplification pass (formatting only)
  --low-memory       two-tier loading: start from the defining image, load on demand (default: dependency closure)
  -h, --help         print this help

Examples:
  ddc getclass app.apk com.example.Foo
      one class with its nested classes, to stdout
  ddc getclass app.apk com.example.Foo -o Foo.java --dex classes3
      pick the image, write a file
  ddc getclass base.apk patch.dex com.example.Foo
      several inputs merge into one pool
  ddc getclass app.apk com.example.Foo --low-memory
      two-tier on-demand loading (multi-dex memory saver)
```

### `getmethod`

```text
ddc getmethod — decompile one method, all of its overloads

Usage: ddc getmethod <INPUT> <FQCN>.<method> [options]

Arguments:
  FQCN.<method>  dotted class name and method name

Options:
  -o, --output FILE  write the result to FILE instead of stdout
  -d, --dex NAME     restrict to images whose entry name contains NAME (repeatable)
  --fast             skip the code-simplification pass (formatting only)
  --format FMT       auto | text | table | markdown | md | json | jsonl (default auto: table on a terminal, text when piped)
  -h, --help         print this help

Examples:
  ddc getmethod app.apk com.example.Foo.toString
      one method, all overloads
  ddc getmethod app.apk com.example.Foo.toString -o toString.java
      save to a file
```

### `pkg`

```text
ddc pkg — decompile every class of one package, subpackages included

Usage: ddc pkg <INPUT> <PACKAGE> [options]

Arguments:
  PACKAGE  package name (omit with --app)

Options:
  --app             take the package from AndroidManifest.xml instead
  -o, --output DIR  output root (default: <package>-out/)
  -t, --threads N   parallel workers (default: CPU count)
  --fast            skip the code-simplification pass (formatting only)
  -d, --dex NAME    restrict to images whose entry name contains NAME (repeatable)
  -h, --help        print this help

Examples:
  ddc pkg app.apk com.example.app
      that package AND every subpackage under it
  ddc pkg app.apk com.example.app -o src/
      choose the output directory
  ddc pkg app.apk --app -o own/
      the app's own package, skipping libraries
```

### `serve`

```text
ddc serve — HTTP REST + OpenAPI + MCP on one port

Usage: ddc serve [FLAGS]

Starts one network process serving three fronts on the same
address:
  REST              the 10 query commands as routes
                    (GET/POST /info, /classes, /class, /method,
                    /findrefs, /strings, /hierarchy, /disasm,
                    /manifest, /mainactivity; GET binds query
                    params, POST also accepts a JSON body;
                    unknown query params are rejected with 400)
  GET /openapi.json the OpenAPI 3 document for those routes
  GET /healthz      liveness probe
  /mcp              streamable MCP endpoint — point an MCP client
                    at http://<addr>/mcp (10 tools, input and
                    output schemas, e.g. ddc.getclass)

Flags:
  --addr HOST:PORT      listen address (default :8080)
  --bearer TOK[,TOK]    require Authorization: Bearer <tok> on REST
                        and /mcp (empty = no auth)
  --timeout 45s         per-request timeout (0 = none)
  --cors ORIGIN[,ORIG]  allowed CORS origins
  --tls-cert F --tls-key F  switch to HTTPS (both required)
  --xyz.allow-unknown-query  accept unknown query params (default 400)
  --default k=v         channel default filled into absent request
                        keys (repeatable; unknown --key=value is
                        shorthand for it)

Examples:
  ddc serve --addr 127.0.0.1:8080
  curl '127.0.0.1:8080/class?input=app.apk&class=com.example.Foo'
  curl -X POST '127.0.0.1:8080/class' -H 'Content-Type: application/json' \
      -d '{"input":"app.apk","class":"com.example.Foo"}'

  # bearer auth + TLS + CORS, one process:
  ddc serve --addr 0.0.0.0:8443 --bearer tok1,tok2 \
      --tls-cert cert.pem --tls-key key.pem --cors 'https://app.example.com'
  curl -H 'Authorization: Bearer tok1' \
      'https://host:8443/findrefs?input=app.apk&kind=string&query=token'
  curl -H 'Authorization: Bearer tok2' 'https://host:8443/openapi.json'
  # (no header → HTTP 401 with a JSON error body)

  # MCP client (Claude Desktop / any MCP client):
  #   url: http://127.0.0.1:8080/mcp
  #   (with --bearer: same URL + 'Authorization: Bearer <tok>' header)
```

### `http`

```text
ddc http — HTTP REST + OpenAPI only (no MCP endpoint)

Usage: ddc http [FLAGS]

The REST-only form of serve: the 10 query routes (GET+POST, GET
binds query params, POST also accepts a JSON body) and
GET /openapi.json on one port, /mcp NOT mounted (use serve
when MCP clients need the endpoint, mcp when REST is not
wanted at all).

Flags: same as serve — --addr HOST:PORT (default :8080),
  --bearer TOK[,TOK], --timeout 45s, --cors ORIGIN[,ORIG],
  --tls-cert F --tls-key F, --default k=v,
  --xyz.allow-unknown-query (default: unknown query params are 400).

Examples:
  ddc http --addr 127.0.0.1:8080
  curl '127.0.0.1:8080/class?input=app.apk&class=com.example.Foo'
  curl -X POST '127.0.0.1:8080/class' -H 'Content-Type: application/json' \
      -d '{"input":"app.apk","class":"com.example.Foo"}'
  curl '127.0.0.1:8080/openapi.json' | python3 -m json.tool | head

  # bearer auth + TLS:
  ddc http --addr 0.0.0.0:8443 --bearer tok1 \
      --tls-cert cert.pem --tls-key key.pem
  curl -H 'Authorization: Bearer tok1' \
      'https://host:8443/strings?input=app.apk&filter=token'
```

### `mcp`

```text
ddc mcp — MCP tool server (no REST routes)

Usage: ddc mcp stdio|http [FLAGS]

  stdio   MCP over stdin/stdout — run as the agent's command, no
          network (how Claude Desktop and local agents connect)
  http    streamable-HTTP MCP server on its own address (SDK
          default: loopback Host headers only; serve's /mcp is
          the same protocol on the shared port)

Flags (http; stdio ignores address flags):
  --addr HOST:PORT      listen address (default :8080)
  --bearer TOK[,TOK]    required Authorization header
  --versions A,B        pin MCP spec revisions (2024-11-05,
                        2025-03-26, 2025-06-18, 2025-11-25,
                        2026-07-28)
  --stateless           streamable HTTP in stateless mode (the
                        2026-07-28 handshake-free form, SEP-2567)
  --json-response       prefer JSON responses (requires --stateless;
                        legacy session mode always answers SSE)
  --session-timeout 30m session keep-alive window
  --default k=v         channel default for absent tool keys
  --name N              server name reported in the handshake
  --server-version V    server version reported in the handshake

Every command becomes a tool: ddc.getclass, ddc.findrefs, ... —
tools/list carries inputSchema and outputSchema.

Examples:
  ddc mcp stdio                # agent config: command = ddc,
                               #                args = ['mcp', 'stdio']

  # Claude Desktop — claude_desktop_config.json:
  #   'mcpServers': { 'ddc': { 'command': 'ddc',
  #                    'args': ['mcp', 'stdio'] } }

  # authenticated streamable HTTP + pinned spec revisions:
  ddc mcp http --addr 127.0.0.1:9300 --bearer tok1 \
      --versions 2025-06-18,2026-07-28

  # session timeout + server identity in the handshake:
  ddc mcp http --addr 127.0.0.1:9300 --session-timeout 30m \
      --name ddc-tools --server-version 0.1.25
```

## Example outputs

Examples use the repository fixture `crates/ddc-dec/tests/fixtures/hello.dex` (shown as `hello.dex`) and reqable 3.2.23 (shown as `reqable.apk`). Outputs longer than 24 lines are truncated with `… (truncated)`; the `pkg` example writes to a temporary output directory.

### info — app context and per-image counts

```bash
$ ddc info hello.dex
```

```text
size        1.3 KB (1284 bytes)
md5         15babc33e49d5a458d83530f843d18f0

     image       dex     classes     methods     fields     strings
     hello       038           2          10          3          27
total: 1 image(s), 2 classes
```

### listclasses — class prefix tree

```bash
$ ddc listclasses hello.dex
```

```text
Greeter
Hello
```

### manifest — AndroidManifest.xml

```bash
$ ddc manifest reqable.apk
```

```text
<manifest versionCode="221" versionName="3.2.23" compileSdkVersion="34" compileSdkVersionCodename="14" package="com.reqable.android" platformBuildVersionCode="34" platformBuildVersionName="14">
    <uses-sdk minSdkVersion="21" targetSdkVersion="35">
    </uses-sdk>
    <uses-permission name="android.permission.INTERNET">
    </uses-permission>
    <uses-permission name="android.permission.ACCESS_NETWORK_STATE">
    </uses-permission>
    <uses-permission name="android.permission.ACCESS_WIFI_STATE">
    </uses-permission>
    <uses-permission name="android.permission.READ_EXTERNAL_STORAGE">
    </uses-permission>
    <uses-permission name="android.permission.WRITE_EXTERNAL_STORAGE">
    </uses-permission>
    <uses-permission name="android.permission.FOREGROUND_SERVICE">
    </uses-permission>
    <uses-permission name="android.permission.FOREGROUND_SERVICE_SPECIAL_USE">
    </uses-permission>
    <uses-permission name="android.permission.QUERY_ALL_PACKAGES">
    </uses-permission>
    <uses-permission name="com.android.vending.CHECK_LICENSE">
    </uses-permission>
    <uses-permission name="android.permission.POST_NOTIFICATIONS">
    </uses-permission>
    <queries>
… (truncated)
```

### mainactivity — package and launcher

```bash
$ ddc mainactivity reqable.apk
```

```text
package     com.reqable.android
class       android.app.Application (application)
launcher    com.reqable.android.MainActivity
dex         reqable-app-android-arm64!classes.dex
```

### res — archive entry summary

```bash
$ ddc res reqable.apk
```

```text
AndroidManifest.xml
DebugProbesKt.bin
META-INF/ (38)
assets/ (125)
billing.properties
classes.dex
firebase-encoders-json.properties
firebase-encoders-proto.properties
firebase-encoders.properties
kotlin/ (7)
kotlin-tooling-metadata.json
lib/arm64-v8a/ (12)
okhttp3/internal/publicsuffix/ (2)
play-services-ads-identifier.properties
play-services-auth-api-phone.properties
play-services-auth-base.properties
play-services-auth-blockstore.properties
play-services-auth.properties
play-services-base.properties
play-services-basement.properties
play-services-fido.properties
play-services-location.properties
play-services-places-placereport.properties
play-services-tasks.properties
… (truncated)
```

### strings — string table and referencing methods

```bash
$ ddc strings hello.dex -f Hello --with-locations
```

```text
dex         string  used-by
hello       "Hello.java"
hello       "LHello;"
```

### findrefs — cross-references

```bash
$ ddc findrefs hello.dex string hi --full
```

```text
dex         kind          class method refs
hello       const-string  Greeter greet()Ljava/lang/String;  "hi "
```

### callers — method callers

```bash
$ ddc callers hello.dex greet --full
```

```text
dex         kind          class method refs
hello       invoke        Hello main([Ljava/lang/String;)V  LGreeter;->greet()Ljava/lang/String;
```

### members — member search

```bash
$ ddc members hello.dex --full
```

```text
dex         kind    class member
hello       method  Greeter <init>(Ljava/lang/String;)V
hello       method  Greeter greet()Ljava/lang/String;
hello       method  Hello <clinit>()V
hello       method  Hello <init>()V
hello       method  Hello main([Ljava/lang/String;)V
hello       method  java/io/PrintStream println(Ljava/lang/String;)V
hello       method  java/lang/Object <init>()V
hello       method  java/lang/StringBuilder <init>(Ljava/lang/String;)V
hello       method  java/lang/StringBuilder append(Ljava/lang/String;)Ljava/lang/StringBuilder;
hello       method  java/lang/StringBuilder toString()Ljava/lang/String;
hello       field   Greeter name Ljava/lang/String;
hello       field   Hello counter I
hello       field   java/lang/System out Ljava/io/PrintStream;
```

### hierarchy — lineage

```bash
$ ddc hierarchy hello.dex Hello
```

```text
class        Hello
extends      java.lang.Object
```

### largest — largest methods

```bash
$ ddc largest hello.dex -n 3
```

```text
  insns  dex         class method
     23  hello       Hello main([Ljava/lang/String;)V
     18  hello       Greeter greet()Ljava/lang/String;
      6  hello       Greeter <init>(Ljava/lang/String;)V
```

### disasm — bytecode disassembly

```bash
$ ddc disasm hello.dex Greeter.greet
```

```text
// hello Greeter
  greet()Ljava/lang/String;:
    0000: iget v2, v2, field@0 Greeter->name:Ljava/lang/String;
    0004: new-instance v0, type@6 Ljava/lang/StringBuilder;
    0008: const-string v1, string@19 "hi "
    000c: invoke { v0, v1 }, method@7 java/lang/StringBuilder-><init>(Ljava/lang/String;)V
    0012: invoke { v0, v2 }, method@8 java/lang/StringBuilder->append(Ljava/lang/String;)Ljava/lang/StringBuilder;
    0018: move-result-object v2
    001a: invoke { v2 }, method@9 java/lang/StringBuilder->toString()Ljava/lang/String;
    0020: move-result-object v2
    0022: return-object v2
```

### getclass — one class

```bash
$ ddc getclass hello.dex Greeter
```

```text
// Decompiled by https://github.com/ejfkdev/ddc 0.1.35
// From: hello (DEX 038)
// Source file: Hello.java

class Greeter {
    private final java.lang.String name;
    Greeter(java.lang.String str) {
        super();
        this.name = str;
    }
    java.lang.String greet() {
        return "hi " + this.name;
    }
}
```

### getmethod — one method

```bash
$ ddc getmethod hello.dex Greeter.greet
```

```text
// Decompiled by https://github.com/ejfkdev/ddc 0.1.35
// From: hello (DEX 038)
// Source file: Hello.java

java.lang.String greet() {
    return "hi " + this.name;
}
```

### pkg — whole package

```bash
$ ddc pkg hello.dex . -o pkg-out
```

```text
ddc: 2 class(es) under .
ddc: wrote 2 file(s) to pkg-out
```

### serve — HTTP REST + OpenAPI + /mcp

```bash
$ ddc serve --addr 127.0.0.1:18095
```

```text
xyz[info]: listening on http://127.0.0.1:18095 (REST + /openapi.json + /mcp)
```

```bash
$ curl 'http://127.0.0.1:18095/info?input=hello.dex'
```

```json
{
  "package": "",
  "size_bytes": 1284,
  "md5": "15babc33e49d5a458d83530f843d18f0",
  "images": [
    {
      "image": "hello",
      "dex_version": "038",
      "classes": 2,
      "methods": 10,
      "fields": 3,
      "strings": 27
    }
  ],
  "total_images": 1,
  "total_classes": 2
}
```

### http — REST + OpenAPI only

```bash
$ ddc http --addr 127.0.0.1:18096
```

```text
xyz[info]: listening on http://127.0.0.1:18096 (REST + /openapi.json)
```

### mcp stdio — MCP subprocess

```bash
$ echo '{"jsonrpc":"2.0","id":1,"method":"initialize",...}' | ddc mcp stdio
```

```json
{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"ddc","version":"0.1.35"}}}
```
