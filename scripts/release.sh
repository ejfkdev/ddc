#!/bin/zsh
# ddc one-command release: bump → tests → crates.io → tag → release CI
# → homebrew-tap + scoop-bucket refresh.
#
# Usage:
#   scripts/release.sh v0.1.3                # ddc-cli/ddc-dec version bump
#   scripts/release.sh v0.1.4 0.2.1          # …and ddc-dex (independent line)
#   scripts/release.sh v0.1.3 -m notes.md    # tag message from a file
#
# Prereqs: clean worktree, local gh auth (tag push + bucket dispatch),
# cargo login token for crates.io.
#
# The crates.io publishes are ordered (ddc-dex → ddc-dec → ddc-cli) with a
# sparse-index poll between them: ddc-dec's `cargo publish` resolves its
# dependency off the registry, so the just-published ddc-dex must be
# visible first. Re-running after a partial failure is safe — an
# already-published version is detected and skipped.
set -euo pipefail

REPO=ejfkdev/ddc
TAP_REPO=ejfkdev/homebrew-tap
BUCKET_REPO=ejfkdev/scoop-bucket

ver=${1:-}
dexver=${2:-}
msgfile=""
if [[ ${2:-} == "-m" || ${3:-} == "-m" ]]; then
  msgfile=${@[-1]}
  if [[ ${2:-} == "-m" ]]; then dexver=""; fi
fi
if [[ -z $ver ]]; then
  echo "usage: scripts/release.sh v<version> [ddc-dex-version] [-m <tag-message-file>]" >&2
  exit 2
fi
ver=${ver#v}
if [[ -n ${dexver:-} ]]; then dexver=${dexver#v}; fi

echo "==> release ddc $ver${dexver:+ (ddc-dex $dexver)}"
cd "$(dirname "$0")/.."

# ---- 1. sanity ---------------------------------------------------------------
if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "worktree is dirty — commit first" >&2
  exit 1
fi
# PRE-TEST lock guard: tests+clippy compile against the CURRENT lock
# (before stamping regenerates it) — a stale jdc-core pin fails there
# with trait/method-missing errors instead of this clean message
# (v0.1.22 first run: the pin was 0.2.13-registry with the trait hooks
# in 0.2.14). The post-stamp check below covers the stamping step.
jdc_req0=$(grep -m1 '^jdc-core = ' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')
jdc_got0=$(grep -A1 'name = "jdc-core"' Cargo.lock | grep -m1 version | sed 's/.*"\(.*\)".*/\1/')
if [[ $(printf '%s\n' "$jdc_req0" "$jdc_got0" | sort -V | head -1) != "$jdc_req0" ]]; then
  echo "Cargo.lock resolves jdc-core $jdc_got0 < required $jdc_req0 — run cargo update -p jdc-core BEFORE releasing" >&2
  exit 1
fi
echo "==> jdc-core lock pre-check: $jdc_got0 >= $jdc_req0"
echo "==> tests + clippy"
# The xyz frontends (HTTP REST + MCP, crates/dad... see xyz_api.rs) are an
# opt-in cargo feature (xyz-rust is a git dep — not on crates.io yet);
# tests, clippy and the release build must exercise it.
cargo test --release --quiet
cargo test --release --features xyz --quiet
cargo clippy --release --all-targets --quiet
cargo clippy --release --all-targets --features xyz --quiet

# ---- 2. version stamping -----------------------------------------------------
python3 - "$ver" "${dexver:-}" <<'PY'
import re, sys
ver, dexver = sys.argv[1], sys.argv[2]
root = open('Cargo.toml').read()
root = re.sub(r'^version = "[^"]+"', f'version = "{ver}"', root, count=1, flags=re.M)
root = re.sub(r'(ddc-dex = \{ path = "crates/ddc-dex", version = )"0\.\d+\.\d+"',
              rf'\g<1>"{dexver or "0.2.0"}"', root)
root = re.sub(r'(ddc-dec = \{ path = "crates/ddc-dec", version = )"0\.\d+\.\d+"',
              rf'\g<1>"{ver}"', root)
open('Cargo.toml', 'w').write(root)
if dexver:
    p = 'crates/ddc-dex/Cargo.toml'
    s = open(p).read()
    s = re.sub(r'^version = "[^"]+"', f'version = "{dexver}"', s, count=1, flags=re.M)
    open(p, 'w').write(s)
print("stamped")
PY
cargo build --release --features xyz --quiet
# The lockfile must satisfy the workspace's jdc-core requirement: a stale
# lock silently ships an older jdc-core (v0.1.20 linked 0.2.12 with the
# fixes in 0.2.13 — `cargo build` does not re-resolve a satisfied pin).
# The version bump in the stamping step must have regenerated the lock.
jdc_req=$(grep -m1 '^jdc-core = ' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')
jdc_got=$(grep -A1 'name = "jdc-core"' Cargo.lock | grep -m1 version | sed 's/.*"\(.*\)".*/\1/')
if [[ $(printf '%s\n' "$jdc_req" "$jdc_got" | sort -V | head -1) != "$jdc_req" ]]; then
  echo "Cargo.lock resolves jdc-core $jdc_got < required $jdc_req — run cargo update -p jdc-core" >&2
  exit 1
fi
echo "==> jdc-core lock check: $jdc_got >= $jdc_req"
git add -A
if git diff --cached --quiet; then
  echo "==> nothing to commit (version already stamped)"
else
  git commit -q -m "$ver"
  git push -q origin main
  echo "==> committed and pushed"
fi

# ---- 3. crates.io (ordered, idempotent) --------------------------------------
published() {  # crate version → 0/1
  # The version-scoped REST endpoint (404 until the version is live).
  # NOT the sparse index: its paths 404 wholesale from some networks
  # (serde included), and the REST endpoint 403s a bare curl UA —
  # crates.io's policy wants a descriptive User-Agent.
  curl -fsS -A "ddc-release-script (https://github.com/ejfkdev/ddc)" \
    -o /dev/null "https://crates.io/api/v1/crates/$1/$2" 2>/dev/null
}
pub() {  # crate version
  if published "$1" "$2"; then
    echo "==> crates.io: $1 $2 already published (skip)"
    return
  fi
  echo "==> publish $1 $2"
  if ! out=$(cargo publish -p "$1" --quiet 2>&1); then
    # Idempotent re-run: a version that raced out earlier is a success.
    if [[ $out == *"already exists"* ]]; then
      echo "==> crates.io: $1 $2 already published (absorbed)"
    else
      print -r -- "$out" >&2
      exit 1
    fi
  fi
  typeset -i i=0
  until published "$1" "$2"; do
    (( i += 1 )); (( i > 40 )) && { echo "index wait timeout for $1" >&2; exit 1; }
    sleep 5
  done
}
if [[ -n $dexver ]]; then pub ddc-dex "$dexver"; fi
pub ddc-dec "$ver"
pub ddc-cli "$ver"

# ---- 4. tag ------------------------------------------------------------------
if git rev-parse -q --verify "v$ver" >/dev/null; then
  echo "==> tag v$ver exists (skip)"
else
  echo "==> tag v$ver"
  if [[ -n $msgfile ]]; then
    git tag -a "v$ver" -F "$msgfile"
  else
    printf 'v%s\n\nReleased via scripts/release.sh.\n' "$ver" | git tag -a "v$ver" -F -
  fi
  git push -q origin "v$ver"
fi

# ---- 5. release CI -----------------------------------------------------------
echo "==> waiting for the release workflow"
sleep 15
run=""
typeset -i i=0
while [[ -z $run ]]; do
  run=$(gh run list --repo "$REPO" --workflow release.yml --limit 1 \
    --json databaseId,status --jq '.[0].databaseId' 2>/dev/null)
  (( i += 1 )); (( i > 12 )) && { echo "no release run appeared" >&2; exit 1; }
  sleep 10
done
gh run watch "$run" --repo "$REPO" --exit-status >/dev/null
echo "==> release CI green: https://github.com/$REPO/actions/runs/$run"

# ---- 6. tap + scoop ----------------------------------------------------------
echo "==> dispatching homebrew-tap + scoop-bucket updates"
gh workflow run auto-update.yml --repo "$TAP_REPO"
gh workflow run auto-update.yml --repo "$BUCKET_REPO"
echo "    $TAP_REPO/actions (Auto-update formulae)"
echo "    $BUCKET_REPO/actions (Auto-update manifests)"
echo "==> done: brew install ejfkdev/tap/ddc / scoop bucket add ejfkdev https://github.com/ejfkdev/scoop-bucket && scoop install ddc"
