#!/usr/bin/env bash
# Controlled release helper for redfish-exporter.
#
# Every GitHub Actions workflow in this repository is manual-dispatch only.
# This script is the local entry point: it checks preconditions, dispatches the
# CI gate, creates the tag, dispatches release.yml and verifies the published
# assets. It never builds production source on a production host and never
# deploys anything.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

die() { echo "ERROR: $*" >&2; exit 1; }

py() {
  if command -v python3 >/dev/null 2>&1; then python3 "$@"; else python "$@"; fi
}

cargo_version() {
  py -c 'import tomllib; print(tomllib.load(open("Cargo.toml","rb"))["package"]["version"])'
}

# Dispatch a workflow on a ref, then follow the newly created run to completion.
# Record the previous run id first so a concurrent/older run cannot be watched.
dispatch_and_wait() {
  local workflow="$1" ref="$2"
  shift 2
  local before run_id
  before="$(gh run list --workflow="$workflow" --limit 1 --json databaseId --jq '.[0].databaseId // 0')"
  gh workflow run "$workflow" --ref "$ref" "$@"
  run_id=""
  for _ in $(seq 1 30); do
    run_id="$(gh run list --workflow="$workflow" --limit 1 --json databaseId --jq '.[0].databaseId // 0')"
    if [[ "$run_id" != "$before" && "$run_id" != "0" ]]; then break; fi
    run_id=""
    sleep 2
  done
  [[ -n "$run_id" ]] || die "could not locate the new run for $workflow"
  echo "==> watching $workflow run $run_id"
  gh run watch "$run_id" --exit-status --interval 15
  gh run view "$run_id" --json conclusion,headSha,url --jq '{conclusion, headSha, url}'
}

require_gh() {
  command -v gh >/dev/null 2>&1 || die "gh CLI is required (https://cli.github.com)"
  gh auth status >/dev/null 2>&1 || die "gh is not authenticated"
}

cmd_preflight() {
  local version="${1:?usage: release.sh preflight <version>}"
  local tag="v$version" declared sha
  require_gh
  declared="$(cargo_version)"
  [[ "$declared" == "$version" ]] || die "Cargo.toml version is $declared, expected $version"
  [[ -z "$(git status --porcelain)" ]] || die "working tree is dirty; commit or stash first"
  [[ "$(git branch --show-current)" == "main" ]] || die "must be on main"
  git fetch origin main --quiet
  sha="$(git rev-parse HEAD)"
  [[ "$sha" == "$(git rev-parse origin/main)" ]] || die "local main differs from origin/main"
  ! git rev-parse -q --verify "refs/tags/$tag" >/dev/null || die "tag $tag already exists locally"
  ! git ls-remote --exit-code origin "refs/tags/$tag" >/dev/null 2>&1 || die "tag $tag already exists on the remote"
  echo "preflight OK: $tag from $sha"
}

cmd_ci() {
  require_gh
  dispatch_and_wait ci.yml main
}

cmd_publish() {
  local version="${1:?usage: release.sh publish <version>}"
  local tag="v$version" sha
  cmd_preflight "$version"
  cmd_ci
  sha="$(git rev-parse HEAD)"
  git tag -a "$tag" "$sha" -m "redfish-exporter $tag"
  git push origin "refs/tags/$tag"
  echo "==> tag $tag pushed; dispatching release.yml"
  dispatch_and_wait release.yml "$tag" -f "tag=$tag" -f dry_run=false
  cmd_verify "$version"
}

# Build and verify assets from an existing tag without touching its Release.
# The workflow definition is read from main; release.yml checks out the tag.
cmd_dry_run() {
  local tag="${1:?usage: release.sh dry-run <tag>}"
  require_gh
  dispatch_and_wait release.yml main -f "tag=$tag" -f dry_run=true
}

cmd_verify() {
  local version="${1:?usage: release.sh verify <version>}"
  local tag="v$version" tmp
  require_gh
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  gh release download "$tag" -D "$tmp" --clobber
  ( cd "$tmp" && sha256sum -c SHA256SUMS )
  gh release view "$tag" --json tagName,targetCommitish,assets \
    --jq '{tag: .tagName, commit: .targetCommitish, assets: [.assets[].name]}'
}

# Optional local build, useful on Windows where no Rust toolchain is installed.
# Produces dist/redfish-exporter (gitignored).
cmd_build_linux_x64() {
  local sha actual cid
  sha="$(git rev-parse HEAD)"
  mkdir -p dist
  docker build --build-arg "GIT_SHA=$sha" -t redfish-exporter:ci .
  actual="$(docker run --rm --network none --read-only --cap-drop ALL --security-opt no-new-privileges redfish-exporter:ci --version)"
  [[ "$actual" == "redfish-exporter v$(cargo_version) ($sha)" ]] || die "version mismatch: $actual"
  cid="$(docker create redfish-exporter:ci)"
  docker cp "$cid:/usr/local/bin/redfish-exporter" dist/redfish-exporter
  docker rm "$cid" >/dev/null
  echo "$actual"
  ls -l dist/redfish-exporter
}

usage() {
  cat <<'EOF'
redfish-exporter controlled release helper

  version                      Print the version in Cargo.toml
  preflight <version>          Check version/clean tree/tag/remote before releasing
  ci                           Dispatch the CI quality gate on main and wait
  publish <version>            preflight -> CI -> tag -> push -> release.yml -> verify
  dry-run <tag>                Build and verify assets only, no Release mutation
  verify <version>             Re-download Release assets and check SHA256SUMS
  build-linux-x64              Build/extract the binary locally via Docker

Examples:
  scripts/release.sh publish 0.2.9
  scripts/release.sh dry-run v0.2.8
  scripts/release.sh verify 0.2.8

Prefer the justfile recipes (just release 0.2.9, just dry-run 0.2.8).
EOF
}

case "${1:-help}" in
  version)          cargo_version ;;
  preflight)        shift; cmd_preflight "$@" ;;
  ci)               cmd_ci ;;
  publish)          shift; cmd_publish "$@" ;;
  dry-run)          shift; cmd_dry_run "$@" ;;
  verify)           shift; cmd_verify "$@" ;;
  build-linux-x64)  cmd_build_linux_x64 ;;
  help|--help|-h)   usage ;;
  *)                usage; exit 1 ;;
esac
