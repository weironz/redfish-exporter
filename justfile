# redfish-exporter release entry point.
# All workflows are manual-dispatch only; nothing is published automatically.
# The implementation lives in scripts/release.sh and can also be run directly.
set shell := ["bash", "-uc"]
set windows-shell := ["bash.exe", "-uc"]

# Show all recipes
default:
    @just --list

# Print the version declared in Cargo.toml
version:
    @bash scripts/release.sh version

# Dispatch the CI quality gate (rust + image + image-ready) on main and wait
ci:
    bash scripts/release.sh ci

# Check version, clean tree, main sync and tag collisions before a release
preflight version:
    bash scripts/release.sh preflight {{version}}

# Full controlled release: preflight -> CI -> tag -> push -> release.yml -> verify
release version:
    bash scripts/release.sh publish {{version}}

# Build/verify Linux x86_64 assets for an existing tag without touching its Release
dry-run version:
    bash scripts/release.sh dry-run v{{version}}

# Re-download published assets and verify SHA256SUMS
verify-release version:
    bash scripts/release.sh verify {{version}}

# Build and extract a Linux x86_64 binary locally via Docker (no Rust toolchain needed)
build-linux-x64:
    bash scripts/release.sh build-linux-x64
