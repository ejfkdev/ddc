# Progressive analysis — subcommand reference

[English] | [简体中文](zh-CN/subcommands.md)

Query the artifact as a database before paying for a full decompile:
metadata loads only inflate and parse the dex header tables (weibo's 20
images, in parallel, ~0.35s); scans never enter the lift/structure/render
pipeline. Times are from the author's machine against weibo (226MB,
20 dex) unless noted.

All subcommands accept `-d/--dex NAME` (repeatable, entry-name substring)
to restrict the image set; most take `-o` to write results to a file.
stdout output is clean (stderr silent); timing prints only with `-o`.

| Command | What it does | Time |
|---|---|---|
| `ddc manifest <apk> [--component C]` | AndroidManifest.xml → text XML; `--component launcher\|activity\|service\|receiver\|provider\|permission\|activity-alias\|application` filters | **0.06s** |
| `ddc info <input>` | app context (label via arsc, package, version, launcher, sdk, size, md5) + per-image dex counts | **0.11s** (md5 dominates on huge files) |
| `ddc listclasses <input> [pattern]` | class names, optional fuzzy filter | **0.10s** |
| `ddc getclass <input> FQCN [-o f]` | one class (+nested), targeted decompile | **0.03s** (typical class) |
| `ddc findrefs <input> string TEXT` | every string-literal reference (const-string scan) | **0.14s** |
| `ddc findrefs <input> type com.example.Foo` | type references (new-instance/check-cast/…) | ~0.5s |
| `ddc findrefs <input> method onCreate --class android/app/Activity` | call sites | ~0.5s |
| `ddc findrefs <input> field CREATOR --class com.example --fuzzy-class` | field read/write sites | ~0.5s |
| `ddc strings <input> [-f TEXT] [--with-locations]` | string table; `--with-locations` maps const-string hits to owner methods | **0.04s** |
| `ddc members <input> [NAME] [--class FQCN] [--fuzzy-class] [--method\|--field]` | method/field name search | **0.04s** |
| `ddc hierarchy <input> FQCN` | lineage: extends/implements + subclasses/implementors | **0.04s** |
| `ddc largest <input> [-n N]` | top-N methods by instruction count | **0.06s** |
| `ddc disasm <input> FQCN[.method]` | raw bytecode of a class/method (opcode + pc) | **0.04s** |
| `ddc callers <input> NAME [FQCN]` | who invokes method NAME (rides the findrefs machinery) | ~0.5s |
| `ddc getmethod <input> FQCN.method` | decompile ONE method — all overloads, sliced out of the class, provenance header kept | **0.03s** |
| `ddc pkg <input> com.example.foo [-o DIR] [-t N]` | whole-package decompile through the full pipeline (segment-boundary prefix: subpackages included); `--app` takes the package from the manifest | 0.165s (Telegram tgnet, 1561 classes) |
| `ddc mainactivity <apk>` | package + MAIN/LAUNCHER activity from the manifest, verified against the dex images | **0.02s** |
| `ddc res <apk> [entry] [-o FILE]` | list archive entries (XAPK inner APKs flattened); dump one: binary XML decoded through the AXML decoder, text as-is, binary saved via `-o` | **0.01s** |
| (full run, for contrast) `ddc app.apk -o out/` | all 98,348 classes | 5.45s / 1.28GB |

## Finding which dex holds a class

`--dex NAME` narrows the image set before parsing — `getclass --dex
classes20` parses exactly one image (0.09s on weibo). Ambiguous class
names get a warning listing the images and the `--dex` hint; a bad
`--dex` lists all valid entry names. For nested containers the labels are
three-level (`container!base.apk!classes.dex`) and either inner segment
matches (`--dex base`, `--dex config.arm64`).

## findrefs output format

Columnar (header first, `info`-style; the class is its own column, no
boundary guessing). **One row per method**: multiple hits in the same
method aggregate into the refs column (`; `-separated, deduped, ordered by
first hit); the kind column is the first hit's instruction:

```
dex         kind          class method refs
hello       const-string  Greeter greet()Ljava/lang/String;  "hi "
classes50.dex  const-string  bz7/c d(...)Ljava/lang/String;  "both_feishu_doubao"; "only_feishu"
```

The aggregation matches ASC's semantics (reqable token: 82 instruction
rows → 69 method rows — identical to ASC's method count); ddc additionally
keeps the method descriptor.

Match semantics: string/type/names are case-insensitive substrings;
`--class` defaults to exact (`com.poc.Main`, `com/poc/Main`,
`Lcom/poc/Main;` all normalize), `--fuzzy-class` widens it to substring.
One thread per image; zero-hit queries return instantly.

## The recommended workflow

```
info → listclasses → findrefs → getclass          # drill down
strings / members / hierarchy / largest / disasm / callers   # browse
mainactivity                                      # entry point
res                                               # resources
pkg --app                                         # bulk, app code only
full decompile                                    # last resort
```

Notes:

- The AXML decoder (`ddc-cli/src/axml.rs`) handles UTF-16/UTF-8 string
  pools and typedValue rendering; `manifest` also accepts bare `.axml`
  files.
- Modern APKs collapse resource paths (AAPT2 optimized: `res/-G.png`,
  no `values/` directory) — the jadx-style `res/values/strings.xml`
  lookup only works on non-collapsed APKs; use `res <apk> res/X.xml` (a
  layout file) to exercise XML decoding there.
- `pkg --app` falls back to the launcher class's package when the
  manifest package has no classes (Telegram: manifest says
  `org.telegram.messenger.web`, code lives in `org.telegram.messenger`).
