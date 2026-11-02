#!/usr/bin/env python3
"""Generate binary-package manifests for every program in programs.toml.

The catalog is the only hand-maintained input. Release assets are addressed by
their GitHub release tag; `--refresh` reads the asset SHA-256 digests from the
release API (falling back to downloading an asset when GitHub has no digest)
and writes checksums.toml. Rendering is deterministic, so `--check` can verify
committed manifests without network access.

Usage:
    python3 packaging/sync.py                 # render from checksums.toml
    python3 packaging/sync.py --refresh       # refresh hashes, then render
    python3 packaging/sync.py --check         # verify committed output
    python3 packaging/sync.py --program ddc --refresh
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import tomllib
import urllib.error
import urllib.request
from pathlib import Path
from typing import NoReturn

ROOT = Path(__file__).resolve().parent.parent
PKG = ROOT / "packaging"
CATALOG = PKG / "programs.toml"
CHECKSUMS = PKG / "checksums.toml"
SCHEMA = 1
WINGET_MANIFEST_VERSION = "1.6.0"


def die(message: str) -> NoReturn:
    print(f"packaging: error: {message}", file=sys.stderr)
    raise SystemExit(1)


def note(message: str) -> None:
    print(f"packaging: {message}")


def load_catalog() -> list[dict]:
    if not CATALOG.is_file():
        die(f"missing catalog: {CATALOG}")
    data = tomllib.loads(CATALOG.read_text(encoding="utf-8"))
    if data.get("schema") != SCHEMA:
        die(f"{CATALOG} has schema {data.get('schema')!r}, expected {SCHEMA}")
    programs = data.get("program") or []
    if not isinstance(programs, list) or not programs:
        die(f"{CATALOG} contains no [[program]] entries")
    seen: set[str] = set()
    for program in programs:
        name = program.get("name")
        if not isinstance(name, str) or not name:
            die("every program needs a name")
        if name in seen:
            die(f"duplicate program name: {name}")
        seen.add(name)
        if not program.get("repo"):
            die(f"{name}: missing repo")
        if not program.get("asset"):
            die(f"{name}: missing asset template")
        targets = program.get("targets")
        if not isinstance(targets, list) or not targets:
            die(f"{name}: missing targets")
        if len(set(targets)) != len(targets):
            die(f"{name}: duplicate targets")
    return programs


def resolve_version(program: dict) -> str:
    name = program["name"]
    source = program.get("version_from")
    if source:
        path = ROOT / source
        if not path.is_file():
            die(f"{name}: version_from does not exist: {source}")
        manifest = tomllib.loads(path.read_text(encoding="utf-8"))
        package = manifest.get("package") or {}
        version = package.get("version")
        if isinstance(version, str):
            return version
        if isinstance(version, dict) and version.get("workspace") is True:
            root = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
            try:
                return root["workspace"]["package"]["version"]
            except KeyError:
                die(f"{name}: workspace.package.version is missing from Cargo.toml")
        die(f"{name}: cannot resolve version from {source}")
    version = program.get("version")
    if not isinstance(version, str) or not version:
        die(f"{name}: set version_from or version")
    return version


def asset_name(program: dict, version: str, target: str) -> str:
    exe = ".exe" if "windows" in target else ""
    try:
        return program["asset"].format(version=version, target=target, exe=exe)
    except KeyError as exc:
        die(f"{program['name']}: unknown placeholder {exc} in asset template")


def asset_url(program: dict, version: str, target: str) -> str:
    return (
        f"https://github.com/{program['repo']}/releases/download/v{version}/"
        f"{asset_name(program, version, target)}"
    )


def license_url(program: dict, version: str) -> str:
    return (
        f"https://raw.githubusercontent.com/{program['repo']}/v{version}/"
        f"{program.get('license_file', 'LICENSE')}"
    )


def api_json(path: str) -> object:
    url = f"https://api.github.com/{path}"
    headers = {
        "Accept": "application/vnd.github+json",
        "User-Agent": "ddc-packaging",
    }
    token = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(url, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return json.load(response)
    except urllib.error.HTTPError as exc:
        if exc.code in (403, 429):
            gh = shutil.which("gh")
            if gh:
                result = subprocess.run(
                    [gh, "api", path], capture_output=True, text=True
                )
                if result.returncode == 0:
                    return json.loads(result.stdout)
        if exc.code == 404:
            die(f"GitHub API returned 404 for {url} (is the release published?)")
        die(f"GitHub API error {exc.code} for {url}: {exc.reason}")
    except urllib.error.URLError as exc:
        die(f"cannot reach {url}: {exc.reason}")


def download_digest(url: str) -> tuple[str, int]:
    request = urllib.request.Request(
        url, headers={"User-Agent": "ddc-packaging"}
    )
    digest = hashlib.sha256()
    size = 0
    try:
        with urllib.request.urlopen(request, timeout=300) as response:
            while True:
                chunk = response.read(1024 * 1024)
                if not chunk:
                    break
                digest.update(chunk)
                size += len(chunk)
    except (urllib.error.HTTPError, urllib.error.URLError) as exc:
        die(f"cannot download {url}: {exc}")
    return digest.hexdigest(), size


def fetch_license(program: dict, version: str) -> bytes:
    path = (
        f"repos/{program['repo']}/contents/"
        f"{program.get('license_file', 'LICENSE')}?ref=v{version}"
    )
    payload = api_json(path)
    if not isinstance(payload, dict) or "content" not in payload:
        die(f"{program['name']}: unexpected contents response for {path}")
    raw = base64.b64decode(payload["content"])
    if not raw:
        die(f"{program['name']}: license file is empty")
    return raw


def refresh_program(program: dict, version: str) -> dict:
    name = program["name"]
    release = api_json(f"repos/{program['repo']}/releases/tags/v{version}")
    if not isinstance(release, dict):
        die(f"{name}: unexpected release response")
    assets = {asset["name"]: asset for asset in release.get("assets", [])}
    record: dict = {
        "version": version,
        "license_sha256": hashlib.sha256(fetch_license(program, version)).hexdigest(),
        "targets": {},
    }
    missing: list[str] = []
    for target in program["targets"]:
        wanted = asset_name(program, version, target)
        asset = assets.get(wanted)
        if asset is None:
            missing.append(wanted)
            continue
        digest = asset.get("digest")
        if isinstance(digest, str) and digest.startswith("sha256:"):
            sha256 = digest.removeprefix("sha256:").lower()
            size = int(asset.get("size", 0))
        else:
            sha256, size = download_digest(asset["browser_download_url"])
        if not re.fullmatch(r"[0-9a-f]{64}", sha256):
            die(f"{name}: invalid sha256 for {wanted}: {sha256!r}")
        record["targets"][target] = {"sha256": sha256, "size": size}
    if missing:
        available = ", ".join(sorted(assets)) or "(none)"
        die(
            f"{name}: release v{version} is missing assets: {', '.join(missing)}\n"
            f"  available: {available}"
        )
    return record


def load_checksums() -> dict:
    if not CHECKSUMS.is_file():
        return {}
    data = tomllib.loads(CHECKSUMS.read_text(encoding="utf-8"))
    if data.get("schema") != SCHEMA:
        die(f"{CHECKSUMS} has schema {data.get('schema')!r}, expected {SCHEMA}")
    return {key: value for key, value in data.items() if key != "schema"}


def dump_checksums(records: dict) -> str:
    lines = [
        "# Generated by packaging/sync.py — do not edit by hand.",
        f"schema = {SCHEMA}",
        "",
    ]
    for name in sorted(records):
        record = records[name]
        lines.append(f"[{name}]")
        lines.append(f"version = {json.dumps(record['version'])}")
        lines.append(
            f"license_sha256 = {json.dumps(record['license_sha256'])}"
        )
        for target in sorted(record["targets"]):
            entry = record["targets"][target]
            lines.append("")
            lines.append(f'[{name}.targets.{json.dumps(target)}]')
            lines.append(f"sha256 = {json.dumps(entry['sha256'])}")
            lines.append(f"size = {entry['size']}")
        lines.append("")
    return "\n".join(lines) + "\n"


def check_checksums(program: dict, version: str, record: dict | None) -> dict:
    name = program["name"]
    if record is None:
        die(f"{name}: no checksums — run `python3 packaging/sync.py --refresh`")
    if record.get("version") != version:
        die(
            f"{name}: checksums are for {record.get('version')!r}, catalog says "
            f"{version!r} — run `python3 packaging/sync.py --refresh`"
        )
    license_sha = record.get("license_sha256")
    if not isinstance(license_sha, str) or not re.fullmatch(r"[0-9a-f]{64}", license_sha):
        die(f"{name}: checksums.toml has an invalid license_sha256")
    for target in program["targets"]:
        entry = (record.get("targets") or {}).get(target)
        if not isinstance(entry, dict):
            die(f"{name}: checksums.toml is missing target {target}")
        sha = entry.get("sha256")
        if not isinstance(sha, str) or not re.fullmatch(r"[0-9a-f]{64}", sha):
            die(f"{name}: invalid sha256 for {target}")
        if not isinstance(entry.get("size"), int) or entry["size"] <= 0:
            die(f"{name}: invalid size for {target}")
    return record


def one_line(text: str) -> str:
    return " ".join(text.split())


def shell_array(values: list[str]) -> str:
    return "(" + " ".join(shlex.quote(value) for value in values) + ")"


def render_aur(program: dict, version: str, record: dict) -> dict[Path, bytes]:
    name = program["name"]
    config = program.get("aur")
    if not config:
        return {}
    pkgname = config["pkgname"]
    pkgrel = int(config.get("pkgrel", 1))
    arch_map = config.get("arch") or {}
    if not arch_map:
        die(f"{name}: aur.arch is empty")
    arches = sorted(arch_map)
    missing = [arch for arch in arches if arch_map[arch] not in record["targets"]]
    if missing:
        die(f"{name}: aur.arch references unknown targets: {missing}")

    description = one_line(program["summary"])
    repo = program["repo"]
    page = f"https://github.com/{repo}"
    lic = program.get("license", "MIT")
    license_source = f"LICENSE::{license_url(program, version)}"

    lines = [
        "# Maintainer: ejfkdev (https://github.com/ejfkdev)",
        f"pkgname={pkgname}",
        f"pkgver={version}",
        f"pkgrel={pkgrel}",
        f"pkgdesc={shlex.quote(description)}",
        f"arch={shell_array(arches)}",
        f"url={shlex.quote(page)}",
        f"license={shell_array([lic])}",
    ]
    depends = config.get("depends") or []
    provides = config.get("provides") or []
    conflicts = config.get("conflicts") or []
    if depends:
        lines.append(f"depends={shell_array(depends)}")
    if provides:
        lines.append(f"provides={shell_array(provides)}")
    if conflicts:
        lines.append(f"conflicts={shell_array(conflicts)}")
    lines += [
        "options=('!strip' '!debug')",
        f"source=({shlex.quote(license_source)})",
    ]
    for arch in arches:
        target = arch_map[arch]
        url = asset_url(program, version, target)
        lines.append(
            f'source_{arch}=("ddc-${{pkgver}}-{arch}::{url}")'
        )
    lines.append(f"sha256sums=({shlex.quote(record['license_sha256'])})")
    for arch in arches:
        target = arch_map[arch]
        lines.append(
            f"sha256sums_{arch}=({shlex.quote(record['targets'][target]['sha256'])})"
        )
    lines += [
        "",
        "package() {",
        '  install -Dm755 "${srcdir}/ddc-${pkgver}-${CARCH}" "${pkgdir}/usr/bin/ddc"',
        '  install -Dm644 "${srcdir}/LICENSE" "${pkgdir}/usr/share/licenses/${pkgname}/LICENSE"',
        "}",
        "",
    ]
    pkgbuild = "\n".join(lines)

    srcinfo = [
        f"pkgbase = {pkgname}",
        f"\tpkgdesc = {description}",
        f"\tpkgver = {version}",
        f"\tpkgrel = {pkgrel}",
        f"\turl = {page}",
    ]
    for arch in arches:
        srcinfo.append(f"\tarch = {arch}")
    srcinfo.append(f"\tlicense = {lic}")
    srcinfo.append("\toptions = !strip")
    srcinfo.append("\toptions = !debug")
    for value in depends:
        srcinfo.append(f"\tdepends = {value}")
    for value in provides:
        srcinfo.append(f"\tprovides = {value}")
    for value in conflicts:
        srcinfo.append(f"\tconflicts = {value}")
    srcinfo.append(f"\tsource = {license_source}")
    srcinfo.append(f"\tsha256sums = {record['license_sha256']}")
    for arch in arches:
        target = arch_map[arch]
        url = asset_url(program, version, target)
        srcinfo.append(f"\tsource_{arch} = ddc-{version}-{arch}::{url}")
        srcinfo.append(
            f"\tsha256sums_{arch} = {record['targets'][target]['sha256']}"
        )
    srcinfo += [f"pkgname = {pkgname}", ""]

    base = PKG / "aur" / pkgname
    return {
        base / "PKGBUILD": pkgbuild.encode(),
        base / ".SRCINFO": "\n".join(srcinfo).encode(),
    }


def yq(value: str) -> str:
    """Quote a YAML scalar only when it would not survive as a plain scalar."""
    if (
        value
        and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9 ._+()/,'-]*", value)
        and ": " not in value
        and not value.endswith(":")
    ):
        return value
    return json.dumps(value, ensure_ascii=False)


def yaml_list(key: str, values: list[str]) -> list[str]:
    lines = [f"{key}:"]
    for value in values:
        lines.append(f"  - {yq(value)}")
    return lines


def winget_dir(program: dict, version: str) -> Path:
    identifier = program["winget"]["id"]
    parts = identifier.split(".")
    if len(parts) < 2:
        die(f"{program['name']}: winget.id must contain at least one dot")
    return (
        PKG
        / "winget"
        / "manifests"
        / identifier[0].lower()
        / parts[0]
        / ".".join(parts[1:])
        / version
    )


def render_winget(program: dict, version: str, record: dict) -> dict[Path, bytes]:
    name = program["name"]
    config = program.get("winget")
    if not config:
        return {}
    identifier = config["id"]
    arch_map = config.get("arch") or {}
    if not arch_map:
        die(f"{name}: winget.arch is empty")
    for arch, target in arch_map.items():
        if target not in record["targets"]:
            die(f"{name}: winget.arch.{arch} references unknown target {target}")
    base = winget_dir(program, version)
    repo = program["repo"]
    page = f"https://github.com/{repo}"
    publisher = program.get("publisher", repo.split("/")[0])
    summary = one_line(program["summary"])
    description_lines = [line.rstrip() for line in program["description"].strip().splitlines()]
    tags = config.get("tags") or program.get("keywords") or []
    license_file = program.get("license_file", "LICENSE")

    version_yaml = "\n".join(
        [
            "# yaml-language-server: $schema=https://aka.ms/winget-manifest.version.1.6.0.schema.json",
            f"PackageIdentifier: {yq(identifier)}",
            f"PackageVersion: {yq(version)}",
            "DefaultLocale: en-US",
            "ManifestType: version",
            f"ManifestVersion: {WINGET_MANIFEST_VERSION}",
            "",
        ]
    )

    locale_lines = [
        "# yaml-language-server: $schema=https://aka.ms/winget-manifest.defaultLocale.1.6.0.schema.json",
        f"PackageIdentifier: {yq(identifier)}",
        f"PackageVersion: {yq(version)}",
        "PackageLocale: en-US",
        f"Publisher: {yq(publisher)}",
        f"PublisherUrl: {yq(f'https://github.com/{publisher}')}",
        f"PublisherSupportUrl: {yq(f'{page}/issues')}",
        f"PackageName: {yq(program['name'])}",
        f"PackageUrl: {yq(page)}",
        f"License: {yq(program.get('license', 'MIT'))}",
        f"LicenseUrl: {yq(f'{page}/blob/v{version}/{license_file}')}",
        f"ShortDescription: {yq(summary)}",
        "Description: |-",
    ]
    locale_lines += [f"  {line}" if line else "" for line in description_lines]
    locale_lines += yaml_list("Tags", tags)
    locale_lines += [
        f"ReleaseNotesUrl: {yq(f'{page}/releases/tag/v{version}')}",
        "ManifestType: defaultLocale",
        f"ManifestVersion: {WINGET_MANIFEST_VERSION}",
        "",
    ]
    locale_yaml = "\n".join(locale_lines)

    installer_lines = [
        "# yaml-language-server: $schema=https://aka.ms/winget-manifest.installer.1.6.0.schema.json",
        f"PackageIdentifier: {yq(identifier)}",
        f"PackageVersion: {yq(version)}",
        "InstallerType: portable",
        "Commands:",
        f"  - {program['name']}",
        "UpgradeBehavior: install",
        "Installers:",
    ]
    for arch in sorted(arch_map):
        target = arch_map[arch]
        entry = record["targets"][target]
        installer_lines += [
            f"  - Architecture: {arch}",
            f"    InstallerUrl: {yq(asset_url(program, version, target))}",
            f"    InstallerSha256: {entry['sha256'].upper()}",
        ]
    installer_lines += [
        "ManifestType: installer",
        f"ManifestVersion: {WINGET_MANIFEST_VERSION}",
        "",
    ]
    installer_yaml = "\n".join(installer_lines)

    return {
        base / f"{identifier}.yaml": version_yaml.encode(),
        base / f"{identifier}.locale.en-US.yaml": locale_yaml.encode(),
        base / f"{identifier}.installer.yaml": installer_yaml.encode(),
    }


def node_key(target: str) -> str | None:
    musl = False
    if target.endswith("-apple-darwin"):
        platform = "darwin"
    elif target.endswith("-pc-windows-msvc"):
        platform = "win32"
    elif target.endswith("-unknown-linux-gnu"):
        platform = "linux"
    elif target.endswith("-unknown-linux-musl"):
        platform = "linux"
        musl = True
    else:
        return None
    if target.startswith("aarch64"):
        arch = "arm64"
    elif target.startswith("x86_64"):
        arch = "x64"
    else:
        return None
    return f"{platform}-{arch}" + ("-musl" if musl else "")


NPM_INSTALL_JS = r'''#!/usr/bin/env node
"use strict";

// Generated by packaging/sync.py. Downloads the matching GitHub release
// binary and verifies its SHA-256 before installing it.
const fs = require("node:fs");
const path = require("node:path");
const https = require("node:https");
const crypto = require("node:crypto");
const { version } = require("./package.json");

const NAME = "__NAME__";
const ENV_PREFIX = "__ENV_PREFIX__";
const ASSETS = __ASSETS__;

function fail(message) {
  console.error(`${NAME}: ${message}`);
  process.exit(1);
}

function isMusl() {
  if (process.platform !== "linux") return false;
  try {
    return !process.report.getReport().header.glibcVersionRuntime;
  } catch (_) {
    return (
      fs.existsSync("/lib/ld-musl-x86_64.so.1") ||
      fs.existsSync("/lib/ld-musl-aarch64.so.1")
    );
  }
}

function download(url, dest, redirects) {
  if (redirects === undefined) redirects = 0;
  return new Promise((resolve, reject) => {
    const request = https.get(
      url,
      {
        headers: { "User-Agent": `${NAME}-npm-installer/${version}` },
        timeout: 120000,
      },
      (response) => {
        const status = response.statusCode || 0;
        if (status >= 300 && status < 400 && response.headers.location) {
          response.resume();
          if (redirects >= 10) {
            reject(new Error(`too many redirects for ${url}`));
            return;
          }
          const next = new URL(response.headers.location, url).toString();
          download(next, dest, redirects + 1).then(resolve, reject);
          return;
        }
        if (status !== 200) {
          response.resume();
          reject(new Error(`HTTP ${status} for ${url}`));
          return;
        }
        const hash = crypto.createHash("sha256");
        const out = fs.createWriteStream(dest, { mode: 0o755 });
        response.on("data", (chunk) => hash.update(chunk));
        response.on("error", reject);
        out.on("error", reject);
        out.on("finish", () => resolve(hash.digest("hex")));
        response.pipe(out);
      }
    );
    request.on("error", reject);
    request.on("timeout", () => {
      request.destroy(new Error(`download timed out for ${url}`));
    });
  });
}

async function downloadWithRetry(url, dest) {
  let last;
  for (let attempt = 1; attempt <= 3; attempt += 1) {
    try {
      return await download(url, dest, 0);
    } catch (error) {
      last = error;
      fs.rmSync(dest, { force: true });
      if (attempt < 3) {
        console.error(
          `${NAME}: download failed (${error.message}); retrying (${attempt}/3)...`
        );
        await new Promise((resolve) => setTimeout(resolve, attempt * 2000));
      }
    }
  }
  throw last;
}

async function main() {
  const platform = process.platform;
  const arch = process.arch;
  const key = isMusl() ? `${platform}-${arch}-musl` : `${platform}-${arch}`;

  const binDir = path.join(__dirname, "bin");
  const fileName = platform === "win32" ? `${NAME}.exe` : NAME;
  const dest = path.join(binDir, fileName);
  const tmp = path.join(binDir, `.${NAME}-download-${process.pid}`);
  fs.mkdirSync(binDir, { recursive: true });

  const local = process.env[`${ENV_PREFIX}_BINARY_PATH`];
  if (local) {
    try {
      fs.copyFileSync(local, dest);
      if (platform !== "win32") fs.chmodSync(dest, 0o755);
      console.log(`${NAME}: installed from ${local}`);
    } catch (error) {
      fail(`cannot copy ${ENV_PREFIX}_BINARY_PATH (${local}): ${error.message}`);
    }
    return;
  }

  const meta = ASSETS[key];
  if (!meta) {
    fail(
      `no prebuilt binary for ${key}; supported: ${Object.keys(ASSETS).join(", ")}. ` +
        `Set ${ENV_PREFIX}_BINARY_PATH to a local ${NAME} binary instead.`
    );
  }

  try {
    const actual = await downloadWithRetry(meta.url, tmp);
    if (actual !== meta.sha256) {
      throw new Error(
        `sha256 mismatch for ${meta.url}: expected ${meta.sha256}, got ${actual}`
      );
    }
    fs.rmSync(dest, { force: true });
    fs.renameSync(tmp, dest);
    if (platform !== "win32") fs.chmodSync(dest, 0o755);
    console.log(`${NAME} ${version} installed (${key})`);
  } catch (error) {
    fs.rmSync(tmp, { force: true });
    fail(
      `${error.message}\n  Download it manually from ${meta.url} and set ` +
        `${ENV_PREFIX}_BINARY_PATH.`
    );
  }
}

main().catch((error) => fail(error && error.stack ? error.stack : String(error)));
'''


NPM_LAUNCHER_JS = r'''#!/usr/bin/env node
"use strict";

// Generated by packaging/sync.py. Keeps the npm shim portable while the real
// binary stays a native executable.
const fs = require("node:fs");
const path = require("node:path");
const os = require("node:os");
const { spawn, spawnSync } = require("node:child_process");

const NAME = "__NAME__";
const ENV_PREFIX = "__ENV_PREFIX__";
const exe = process.platform === "win32" ? `${NAME}.exe` : NAME;
let bin = process.env[`${ENV_PREFIX}_BINARY_PATH`] || path.join(__dirname, exe);

if (!process.env[`${ENV_PREFIX}_BINARY_PATH`] && !fs.existsSync(bin)) {
  console.error(`${NAME}: binary not installed; fetching it now...`);
  const installer = spawnSync(
    process.execPath,
    [path.join(__dirname, "..", "install.js")],
    { stdio: "inherit" }
  );
  if (installer.status !== 0 || !fs.existsSync(bin)) {
    console.error(
      `${NAME}: automatic install failed. Reinstall without --ignore-scripts, ` +
        `or set ${ENV_PREFIX}_BINARY_PATH to a ${NAME} binary.`
    );
    process.exit(127);
  }
}

if (!fs.existsSync(bin)) {
  console.error(
    `${NAME}: binary not found at ${bin}. ` +
      `Check ${ENV_PREFIX}_BINARY_PATH or reinstall the package.`
  );
  process.exit(127);
}

const child = spawn(bin, process.argv.slice(2), { stdio: "inherit" });

child.on("error", (error) => {
  console.error(`${NAME}: failed to start ${bin}: ${error.message}`);
  process.exit(127);
});

child.on("exit", (code, signal) => {
  if (signal) {
    const number = os.constants.signals[signal] || 0;
    process.exit(128 + number);
  }
  process.exit(code === null ? 1 : code);
});
'''


def render_npm(program: dict, version: str, record: dict) -> dict[Path, bytes]:
    name = program["name"]
    config = program.get("npm")
    if not config:
        return {}
    package_name = config["package"]
    command = config.get("bin", name)
    env_prefix = re.sub(r"[^A-Za-z0-9]", "_", command).upper()
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", command):
        die(f"{name}: npm bin name is not a safe file name: {command}")

    assets: dict[str, dict] = {}
    for target in program["targets"]:
        key = node_key(target)
        if key is None:
            continue
        if key in assets:
            die(f"{name}: two targets map to npm {key}")
        assets[key] = {
            "url": asset_url(program, version, target),
            "sha256": record["targets"][target]["sha256"],
        }
    if not assets:
        die(f"{name}: no npm-compatible targets")

    assets_js = "{\n" + "".join(
        f"  {json.dumps(key)}: {{\n"
        f"    url: {json.dumps(value['url'])},\n"
        f"    sha256: {json.dumps(value['sha256'])},\n"
        f"  }},\n"
        for key, value in sorted(assets.items())
    ) + "}"

    install_js = (
        NPM_INSTALL_JS.replace("__ASSETS__", assets_js)
        .replace("__NAME__", command)
        .replace("__ENV_PREFIX__", env_prefix)
    )
    launcher_js = (
        NPM_LAUNCHER_JS.replace("__NAME__", command)
        .replace("__ENV_PREFIX__", env_prefix)
    )

    package = {
        "name": package_name,
        "version": version,
        "description": program["summary"],
        "license": program.get("license", "MIT"),
        "author": program.get("publisher", ""),
        "homepage": program.get("homepage", f"https://github.com/{program['repo']}"),
        "repository": {
            "type": "git",
            "url": f"git+https://github.com/{program['repo']}.git",
        },
        "bugs": {"url": f"https://github.com/{program['repo']}/issues"},
        "bin": {command: f"bin/{command}.js"},
        "files": [f"bin/{command}.js", "install.js"],
        "scripts": {"postinstall": "node install.js"},
        "engines": {"node": ">=18"},
        "os": sorted({key.split("-")[0] for key in assets}),
        "cpu": sorted({key.split("-")[1] for key in assets}),
        "publishConfig": {"access": "public"},
        "keywords": program.get("keywords", []),
    }
    package_json = json.dumps(package, indent=2, ensure_ascii=False) + "\n"

    if any(key.endswith("-musl") for key in assets):
        linux_note = (
            "Linux builds include glibc and fully static musl variants; the "
            "installer selects musl automatically on Alpine."
        )
    else:
        linux_note = (
            "Linux builds target glibc (built on ubuntu-latest); Alpine/musl "
            "users should install from source instead."
        )

    readme = f"""# {package_name}

Prebuilt [{command}]({package.get('homepage')}) binaries for macOS, Linux and
Windows. The postinstall script downloads the matching release asset and
verifies its SHA-256. If install scripts are disabled, the launcher fetches
the binary on first use instead.

```bash
npm install -g {package_name}
{command} --version
```

Supported platforms: {", ".join(sorted(assets))}. {linux_note}

Set `{env_prefix}_BINARY_PATH` to a local binary to skip the download.
With `npm install --ignore-scripts`, the launcher fetches the binary on first
use instead.
"""

    gitignore = (
        f"/bin/{command}\n"
        f"/bin/{command}.exe\n"
        f"/bin/.{command}-download-*\n"
        "*.tgz\n"
        "node_modules/\n"
    )

    base = PKG / "npm" / package_name.split("/")[-1]
    return {
        base / "package.json": package_json.encode(),
        base / "install.js": install_js.encode(),
        base / "bin" / f"{command}.js": launcher_js.encode(),
        base / "README.md": readme.encode(),
        base / ".gitignore": gitignore.encode(),
    }


def render_all(
    programs: list[dict], versions: dict[str, str], checksums: dict
) -> tuple[dict[Path, bytes], dict[str, Path]]:
    files: dict[Path, bytes] = {}
    winget_roots: dict[str, Path] = {}
    for program in programs:
        name = program["name"]
        version = versions[name]
        record = check_checksums(program, version, checksums.get(name))
        for render in (render_aur, render_winget, render_npm):
            rendered = render(program, version, record)
            for path, content in rendered.items():
                if path in files:
                    die(f"two programs render the same path: {path}")
                files[path] = content
        if program.get("winget"):
            winget_roots[name] = winget_dir(program, version).parent
    return files, winget_roots


def stale_winget_versions(program: dict, version: str) -> list[Path]:
    root = winget_dir(program, version).parent
    if not root.is_dir():
        return []
    return sorted(
        child for child in root.iterdir() if child.is_dir() and child.name != version
    )


def validate_outputs(files: dict[Path, bytes]) -> None:
    for path, content in files.items():
        text = content.decode("utf-8")
        leftover = re.search(r"__[A-Z][A-Z0-9_]*__", text)
        if leftover:
            die(f"{path}: unreplaced template placeholder {leftover.group(0)}")

    # Validate rendered bytes, not the on-disk copies: --check may run before
    # a manifest is committed, and a first render has no output files yet.
    with tempfile.TemporaryDirectory(prefix="ddc-packaging-") as tmp:
        tmpdir = Path(tmp)
        materialized: dict[Path, Path] = {}
        for index, (path, content) in enumerate(sorted(files.items())):
            suffix = path.suffix or ""
            target = tmpdir / f"{index:03d}-{path.name}"
            if suffix and not target.name.endswith(suffix):
                target = tmpdir / f"{index:03d}-{path.stem}{suffix}"
            target.write_bytes(content)
            materialized[path] = target

        pkgbuild = next((path for path in files if path.name == "PKGBUILD"), None)
        if pkgbuild and shutil.which("bash"):
            result = subprocess.run(
                ["bash", "-n", str(materialized[pkgbuild])],
                capture_output=True,
                text=True,
            )
            if result.returncode != 0:
                die(f"PKGBUILD syntax error: {result.stderr.strip()}")

        yaml_files = sorted(
            path for path in files if path.suffix in (".yaml", ".yml")
        )
        if yaml_files and shutil.which("ruby"):
            result = subprocess.run(
                [
                    "ruby",
                    "-ryaml",
                    "-e",
                    "ARGV.each { |f| YAML.load_file(f) }",
                    *[str(materialized[path]) for path in yaml_files],
                ],
                capture_output=True,
                text=True,
            )
            if result.returncode != 0:
                die(f"WinGet YAML validation failed: {result.stderr.strip()}")

        for path in sorted(files):
            if path.name == "package.json":
                try:
                    json.loads(files[path])
                except json.JSONDecodeError as exc:
                    die(f"{path}: invalid JSON: {exc}")

        js_files = sorted(path for path in files if path.suffix == ".js")
        if js_files and shutil.which("node"):
            for path in js_files:
                result = subprocess.run(
                    ["node", "--check", str(materialized[path])],
                    capture_output=True,
                    text=True,
                )
                if result.returncode != 0:
                    die(f"{path}: JavaScript syntax error: {result.stderr.strip()}")


def compare_outputs(files: dict[Path, bytes]) -> list[str]:
    problems: list[str] = []
    for path in sorted(files):
        if not path.is_file():
            problems.append(f"missing: {path.relative_to(ROOT)}")
            continue
        if path.read_bytes() != files[path]:
            problems.append(f"out of date: {path.relative_to(ROOT)}")
    return problems


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="verify without writing")
    parser.add_argument(
        "--refresh", action="store_true", help="fetch release digests and licenses"
    )
    parser.add_argument(
        "--program",
        action="append",
        default=[],
        help="limit to a program name (repeatable)",
    )
    parser.add_argument(
        "--version",
        help="version override for a single selected program",
    )
    args = parser.parse_args()

    catalog = load_catalog()
    selected = catalog
    if args.program:
        wanted = set(args.program)
        known = {program["name"] for program in catalog}
        unknown = wanted - known
        if unknown:
            die(f"unknown program(s): {', '.join(sorted(unknown))}")
        selected = [program for program in catalog if program["name"] in wanted]
    if args.version and len(selected) != 1:
        die("--version requires exactly one --program")

    checksums = load_checksums()
    versions: dict[str, str] = {}
    for program in selected:
        versions[program["name"]] = args.version or resolve_version(program)

    if args.refresh:
        for program in selected:
            name = program["name"]
            version = versions[name]
            note(f"fetching release v{version} for {name}")
            checksums[name] = refresh_program(program, version)
        CHECKSUMS.write_bytes(dump_checksums(checksums).encode())
        note(f"wrote {CHECKSUMS.relative_to(ROOT)}")

    files, _ = render_all(selected, versions, checksums)
    validate_outputs(files)

    if args.check:
        problems = compare_outputs(files)
        for program in selected:
            if program.get("winget"):
                version = versions[program["name"]]
                for stale in stale_winget_versions(program, version):
                    problems.append(
                        f"stale WinGet version: {stale.relative_to(ROOT)}"
                    )
        if problems:
            for problem in problems:
                print(f"packaging: {problem}", file=sys.stderr)
            die("generated manifests do not match the catalog")
        note(f"all {len(files)} files match ({', '.join(versions.values())})")
        return

    changed = 0
    for path, content in sorted(files.items()):
        path.parent.mkdir(parents=True, exist_ok=True)
        if path.is_file() and path.read_bytes() == content:
            continue
        path.write_bytes(content)
        changed += 1
    for program in selected:
        if program.get("winget"):
            version = versions[program["name"]]
            for stale in stale_winget_versions(program, version):
                shutil.rmtree(stale)
                note(f"removed stale {stale.relative_to(ROOT)}")
    note(
        f"{'wrote' if changed else 'unchanged'}: {changed} file(s), "
        f"{len(files)} generated ({', '.join(versions.values())})"
    )


if __name__ == "__main__":
    main()