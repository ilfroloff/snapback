# Installation Extension Brainstorm

**Date:** 2026-10-05  
**Status:** Brainstorming / Architecture Design  
**Context:** Extending snapback's installation options beyond npx/bunx

## Executive Summary

When this document was written, snapback shipped via two channels: npm (prebuilt binaries) and cargo install from git (source). This document architects a phased expansion to additional package managers, prioritized by audience fit, maintenance burden, and integration with the existing release-plz workflow.

**Recommendation:** Start with **Homebrew** (Phase 1), then add **crates.io** (Phase 2), then evaluate **direct GitHub Releases with install script** (Phase 3). Defer Linux distro packages and Windows support unless user demand materializes.

---

## Current State

### Existing Installation Methods

1. **npm/bunx** (`npx snapback-tui install`)
   - Prebuilt binaries for 4 platforms: darwin-arm64, darwin-x64, linux-x64, linux-arm64
   - Installs to `~/.local/bin` by default
   - No Rust toolchain required
   - Uses OIDC trusted publishing (no stored npm token)
   - Package source: `npm/` directory

2. **cargo install from source**
   - `cargo install --path .` (local checkout)
   - `cargo install --git https://github.com/ilfroloff/snapback` (latest main)
   - `cargo install --git ... --tag vX.Y.Z` (pinned release)
   - Requires Rust toolchain

### Release Infrastructure

- **release-plz**: Drives versioning from Conventional Commits, cuts `vX.Y.Z` git tags + GitHub Releases
- **CI/CD workflows**:
  - `ci.yml` - lint/test on PRs
  - `release-plz.yml` - automated releases on push to main
  - `npm-release.yml` - builds 4 platform binaries and publishes to npm on tag push
- **Key characteristics**:
  - musl static linking for Linux (distro-agnostic)
  - Single binary per platform (`snapback`), `sb` is a copy made at install time
  - Version stamped from git tag (package.json has `0.0.0` placeholder)
  - `git_only = true` in release-plz.toml (never publishes to crates.io)

---

## Evaluation Framework

### Criteria for Package Manager Selection

1. **Audience fit** - Does the target audience (developers using Claude Code) use this package manager?
2. **Platform coverage** - Does it cover macOS and/or Linux (the supported platforms)?
3. **Maintenance burden** - Ongoing cost per release (manual steps, automation complexity, testing)
4. **Integration cost** - How well does it fit the existing release-plz workflow?
5. **Community expectations** - Is this standard for Rust CLI tools?
6. **Update story** - How do users get updates? Is it automatic?
7. **Uninstallation** - Can users cleanly remove the tool?

### Platforms in Scope

Based on `npm/package.json` and the build matrix:
- **macOS**: arm64 (Apple Silicon), x64 (Intel)
- **Linux**: x64, arm64

**Not in scope**: Windows (no current support), other Unix variants.

---

## Package Manager Evaluation

### Tier 1: High Priority

#### Homebrew (macOS + Linux)

**What it is**: The standard package manager for macOS, also works on Linux (Linuxbrew).

**Audience fit**: ⭐⭐⭐⭐⭐  
Developers on macOS overwhelmingly use Homebrew. It's the de facto standard for installing CLI tools on macOS. Linux developers also use Linuxbrew.

**Platform coverage**: ⭐⭐⭐⭐⭐  
Both macOS (arm64/x64) and Linux (x64/arm64) - matches snapback's targets exactly.

**Maintenance burden**: ⭐⭐⭐⭐ (Low)  
- One-time setup: Create a Homebrew tap (`ilfroloff/tap`) or submit to homebrew-core
- Per-release: Automated via GitHub Actions (update formula with new version + checksums)
- Testing: Homebrew's CI validates the formula on PR
- Ongoing: Minimal - formula is a Ruby DSL, stable once written

**Integration cost**: ⭐⭐⭐⭐ (Low)  
- Can reuse the same GitHub Release artifacts (or build in a separate workflow)
- Formula can download prebuilt binaries (bottle) or build from source
- release-plz already creates GitHub Releases - Homebrew can consume those

**Community expectations**: ⭐⭐⭐⭐⭐  
Homebrew is the expected distribution channel for Rust CLI tools on macOS. Tools like `bat`, `fd`, `ripgrep`, `exa` all ship via Homebrew.

**Update story**: ⭐⭐⭐⭐⭐  
`brew upgrade snapback` - automatic, familiar, reliable.

**Uninstallation**: ⭐⭐⭐⭐⭐  
`brew uninstall snapback` - clean, standard.

**Implementation approach**:

**Option A: Homebrew Tap (Recommended for Phase 1)**
- Create `github.com/ilfroloff/homebrew-tap` (or `ilfroloff/tap`)
- Maintain `Formula/snapback.rb` in that repo
- Users: `brew tap ilfroloff/tap && brew install snapback`
- Pros: Full control, fast iteration, no homebrew-core review process
- Cons: Users must add the tap first (friction)

**Option B: homebrew-core (Long-term goal)**
- Submit to the official homebrew-core repository
- Users: `brew install snapback` (no tap needed)
- Pros: Maximum discoverability, no tap friction
- Cons: Review process, stricter requirements, slower iteration
- Prerequisites: Demonstrated demand, stable API, good test coverage

**Recommendation**: Start with **Option A (tap)**, migrate to homebrew-core once demand justifies it.

**Formula structure** (prebuilt binaries):

```ruby
class Snapback < Formula
  desc "Terminal board for Claude Code: every session and agent, one live board"
  homepage "https://github.com/ilfroloff/snapback"
  version "0.12.0" # Stamped by CI
  license "Apache-2.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/ilfroloff/snapback/releases/download/v#{version}/snapback-aarch64-apple-darwin.tar.gz"
      sha256 "..." # Stamped by CI
    else
      url "https://github.com/ilfroloff/snapback/releases/download/v#{version}/snapback-x86_64-apple-darwin.tar.gz"
      sha256 "..." # Stamped by CI
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/ilfroloff/snapback/releases/download/v#{version}/snapback-aarch64-unknown-linux-musl.tar.gz"
      sha256 "..." # Stamped by CI
    else
      url "https://github.com/ilfroloff/snapback/releases/download/v#{version}/snapback-x86_64-unknown-linux-musl.tar.gz"
      sha256 "..." # Stamped by CI
    end
  end

  def install
    bin.install "snapback"
    bin.install_symlink "snapback" => "sb"
  end

  test do
    system "#{bin}/snapback", "--help"
  end
end
```

**CI/CD integration**:

New workflow `homebrew-release.yml`:
- Triggered on `v*` tag (same as npm-release.yml)
- Downloads the 4 platform binaries from the build job (or rebuilds them)
- Creates tarballs per platform with checksums
- Attaches tarballs to the GitHub Release
- Updates the formula in the tap repo with new version + checksums
- Commits and pushes the formula update

**Alternative**: Build from source in the formula (slower install, but no binary distribution complexity). Given the existing npm binary distribution pattern, prebuilt binaries are preferred.

**Effort estimate**: 2-3 days for initial setup + automation.

---

#### crates.io (Rust ecosystem)

**What it is**: The official Rust package registry. `cargo install snapback` would work.

**Audience fit**: ⭐⭐⭐⭐  
Rust developers expect this. Non-Rust developers may not have cargo installed.

**Platform coverage**: ⭐⭐⭐⭐⭐  
Builds from source on any platform with a Rust toolchain.

**Maintenance burden**: ⭐⭐⭐ (Medium)  
- One-time setup: Prepare crate for publishing (ensure no path dependencies, clean up Cargo.toml)
- Per-release: Automated via release-plz (change `publish = false` to `publish = true`)
- Testing: cargo publish does basic validation
- Ongoing: Low - release-plz handles it

**Integration cost**: ⭐⭐⭐⭐ (Low-Medium)  
- release-plz already supports cargo publish - just flip `publish = false` to `publish = true`
- Must ensure crate is publishable (no git dependencies, no path dependencies)
- Must add `cargo-semver-checks` back (currently disabled in release-plz.toml)

**Community expectations**: ⭐⭐⭐⭐  
Standard for Rust tools, but not required if npm/brew exist.

**Update story**: ⭐⭐⭐  
`cargo install snapback --force` - works but not automatic. Users must know to update.

**Uninstallation**: ⭐⭐⭐⭐  
`cargo uninstall snapback` - clean, standard.

**Current blocker**: The crate is deliberately unpublished (`git_only = true`, `publish = false` in release-plz.toml). The rationale:

> "There is no crates.io entry for this crate, so a registry lookup would never find a 'latest release'. git-only mode makes release-plz read the existing `v{version}` git tags to determine the current version and the next bump."

And:

> "This crate is never published to crates.io, and that guarantee rests on `release-plz.toml`"

**Why this might change**:
- Discoverability: crates.io is where Rust developers look for tools
- Consistency: Most Rust CLI tools are on crates.io
- Ecosystem integration: Tools like `cargo-binstall` can install prebuilt binaries from crates.io

**Why this might NOT change**:
- The current git-only workflow is deliberate and well-reasoned
- npm already provides binary distribution
- crates.io publish adds complexity (semver checks, crate preparation)

**Recommendation**: **Phase 2** - After Homebrew is stable, evaluate whether crates.io publish is worth the workflow change. If the audience is primarily non-Rust developers (using Claude Code), crates.io adds little value. If Rust developers are a key audience, crates.io becomes more important.

**Implementation approach** (if pursued):

1. Remove `publish = false` from release-plz.toml
2. Remove `git_only = true` from release-plz.toml
3. Re-enable `semver_check = true` (or keep it off if the lib is truly internal-only)
4. Ensure Cargo.toml has no path/git dependencies
5. Add `cargo publish` step to release-plz workflow
6. Test with a dry run: `cargo publish --dry-run`

**Effort estimate**: 1 day if the crate is already publishable, 2-3 days if cleanup is needed.

---

### Tier 2: Medium Priority

#### Direct GitHub Releases + Install Script

**What it is**: Prebuilt binaries attached to GitHub Releases, with a curl-based install script.

**Audience fit**: ⭐⭐⭐  
Developers comfortable with curl pipelines. Less familiar to non-technical users.

**Platform coverage**: ⭐⭐⭐⭐⭐  
Any platform with a shell and curl.

**Maintenance burden**: ⭐⭐⭐⭐ (Low)  
- One-time setup: Write install script
- Per-release: Automated (attach binaries to GitHub Release)
- Testing: Manual or CI-based
- Ongoing: Low

**Integration cost**: ⭐⭐⭐⭐⭐ (Very Low)  
- GitHub Releases already exist (release-plz creates them)
- Just attach the 4 platform binaries to the release
- Install script downloads the right binary for the platform

**Community expectations**: ⭐⭐⭐  
Common for Go tools (e.g., `golangci-lint`), less common for Rust tools.

**Update story**: ⭐⭐  
Manual: re-run the install script. No automatic updates unless the script checks for new versions.

**Uninstallation**: ⭐⭐⭐⭐  
`rm ~/.local/bin/snapback ~/.local/bin/sb` - simple, manual.

**Implementation approach**:

1. Modify `npm-release.yml` to attach binaries to the GitHub Release (currently it only publishes to npm)
2. Write `install.sh`:

```bash
#!/bin/sh
set -e

VERSION="${VERSION:-latest}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

# Detect platform
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)
case "$ARCH" in
  x86_64) ARCH="x64" ;;
  aarch64|arm64) ARCH="arm64" ;;
  *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

# Map to asset name
case "${OS}-${ARCH}" in
  darwin-arm64) ASSET="snapback-aarch64-apple-darwin.tar.gz" ;;
  darwin-x64) ASSET="snapback-x86_64-apple-darwin.tar.gz" ;;
  linux-x64) ASSET="snapback-x86_64-unknown-linux-musl.tar.gz" ;;
  linux-arm64) ASSET="snapback-aarch64-unknown-linux-musl.tar.gz" ;;
  *) echo "Unsupported platform: ${OS}-${ARCH}"; exit 1 ;;
esac

# Download
if [ "$VERSION" = "latest" ]; then
  URL="https://github.com/ilfroloff/snapback/releases/latest/download/$ASSET"
else
  URL="https://github.com/ilfroloff/snapback/releases/download/v$VERSION/$ASSET"
fi

echo "Downloading $URL..."
curl -fsSL "$URL" | tar -xz -C /tmp
mkdir -p "$INSTALL_DIR"
mv /tmp/snapback "$INSTALL_DIR/snapback"
chmod +x "$INSTALL_DIR/snapback"
ln -sf "$INSTALL_DIR/snapback" "$INSTALL_DIR/sb"

echo "Installed snapback to $INSTALL_DIR"
```

3. Document in README:

```bash
curl -fsSL https://raw.githubusercontent.com/ilfroloff/snapback/main/install.sh | sh
```

**Pros**:
- No package manager dependency
- Works everywhere
- Fast to implement
- Reuses existing GitHub Releases

**Cons**:
- No automatic updates
- Manual uninstallation
- Less discoverable than Homebrew/crates.io
- curl | sh is less trusted than package managers

**Effort estimate**: 1 day.

---

### Tier 3: Low Priority (Defer)

#### Linux Distribution Packages (APT/YUM/Pacman)

**What it is**: Native packages for Debian/Ubuntu (APT), Fedora/RHEL (YUM/DNF), Arch (Pacman).

**Audience fit**: ⭐⭐  
Linux power users expect this, but they're a small fraction of the Claude Code audience.

**Platform coverage**: ⭐⭐⭐⭐⭐  
Native Linux support.

**Maintenance burden**: ⭐ (Very High)  
- One-time setup: Create package specs for each distro
- Per-release: Update each package, test on each distro
- Testing: Must test on each distro version
- Ongoing: High - distro-specific quirks, dependency management, review processes

**Integration cost**: ⭐⭐ (High)  
- Need separate workflows for each distro
- Need to maintain package signing keys
- Need to submit to distro repositories (or maintain a PPA/COPR)

**Community expectations**: ⭐⭐  
Expected for system tools, less so for developer tools.

**Update story**: ⭐⭐⭐⭐⭐  
`apt upgrade snapback` - automatic, integrated with system updates.

**Uninstallation**: ⭐⭐⭐⭐⭐  
`apt remove snapback` - clean, standard.

**Why defer**:
- **High maintenance burden**: Each distro has its own packaging format, review process, and quirks
- **Limited audience**: Claude Code users are more likely to use Homebrew or npm than distro packages
- **musl already solves the problem**: The static Linux binaries work on any distro
- **Homebrew covers Linux**: Linuxbrew works on Linux and is simpler to maintain

**When to reconsider**:
- If snapback gains significant Linux adoption
- If users explicitly request distro packages
- If a contributor offers to maintain a specific distro package

**Implementation approach** (if pursued):

- **Debian/Ubuntu**: Create a PPA or submit to Debian repositories
- **Fedora/RHEL**: Submit to Fedora COPR or Fedora repositories
- **Arch**: Submit to AUR (Arch User Repository)

Each requires:
- Package spec file (`.spec` for RPM, `debian/` directory for DEB, `PKGBUILD` for Arch)
- Signing keys
- CI/CD workflow to build and publish
- Testing on each distro version

**Effort estimate**: 1-2 weeks for initial setup, ongoing maintenance burden.

---

#### Nix

**What it is**: Declarative package manager for reproducible environments.

**Audience fit**: ⭐⭐  
Nix users are passionate but small in number.

**Platform coverage**: ⭐⭐⭐⭐⭐  
Works on macOS and Linux.

**Maintenance burden**: ⭐⭐⭐ (Medium)  
- One-time setup: Write Nix expression
- Per-release: Update version + checksums
- Testing: Nix CI validates
- Ongoing: Low-Medium

**Integration cost**: ⭐⭐⭐ (Medium)  
- Can reuse GitHub Release artifacts
- Nix expression can download prebuilt binaries or build from source
- Need to maintain a Nix flake or submit to nixpkgs

**Community expectations**: ⭐⭐  
Expected by Nix users, but they're a small audience.

**Update story**: ⭐⭐⭐⭐⭐  
`nix upgrade snapback` - automatic, reproducible.

**Uninstallation**: ⭐⭐⭐⭐⭐  
`nix-env --uninstall snapback` - clean, standard.

**Why defer**:
- **Small audience**: Nix users are a tiny fraction of the target audience
- **Complexity**: Nix has a steep learning curve
- **Maintenance**: Nix packaging is different from other package managers

**When to reconsider**:
- If Nix users explicitly request it
- If a contributor offers to maintain the Nix package
- If snapback is included in a Nix-based tool collection

**Implementation approach** (if pursued):

1. Write a Nix expression (`snapback.nix` or `flake.nix`)
2. Submit to nixpkgs or maintain a Nix overlay
3. CI/CD workflow to update version + checksums on release

**Effort estimate**: 2-3 days for initial setup.

---

#### Windows Support (Scoop/Chocolatey/Winget)

**What it is**: Package managers for Windows.

**Audience fit**: ⭐  
Claude Code is not officially supported on Windows (it requires a Unix-like environment).

**Platform coverage**: ⭐  
Windows only.

**Maintenance burden**: ⭐ (Very High)  
- Need to add Windows to the build matrix
- Need to test on Windows
- Need to maintain Windows-specific code paths (if any)

**Integration cost**: ⭐ (Very High)  
- Need to add Windows targets to the build matrix
- Need to test the TUI on Windows (terminal compatibility)
- Need to handle Windows-specific paths and behaviors

**Community expectations**: ⭐  
Not expected - Claude Code itself is not on Windows.

**Update story**: ⭐⭐⭐⭐  
`winget upgrade snapback` - automatic.

**Uninstallation**: ⭐⭐⭐⭐  
`winget uninstall snapback` - clean.

**Why defer (strongly)**:
- **Claude Code is not on Windows**: The tool is useless without Claude Code
- **High maintenance burden**: Windows terminal compatibility is a separate project
- **Out of scope**: The current platform targets are macOS + Linux only

**When to reconsider**:
- If Claude Code officially supports Windows
- If there is significant user demand
- If Windows terminal compatibility is solved

**Implementation approach** (if pursued):

1. Add Windows targets to the build matrix (x64, arm64)
2. Test TUI on Windows (Windows Terminal, PowerShell, CMD)
3. Handle Windows-specific paths (`%APPDATA%` instead of `~/.config`)
4. Create Scoop/Chocolatey/Winget packages
5. CI/CD workflow to build and publish

**Effort estimate**: 1-2 weeks for Windows support, 2-3 days for package manager integration.

---

#### Docker

**What it is**: Containerized distribution.

**Audience fit**: ⭐⭐  
Useful for CI/CD or isolated environments, but not for interactive TUI use.

**Platform coverage**: ⭐⭐⭐⭐⭐  
Runs anywhere Docker runs.

**Maintenance burden**: ⭐⭐⭐ (Medium)  
- One-time setup: Write Dockerfile
- Per-release: Build and push image
- Testing: Docker CI validates
- Ongoing: Low

**Integration cost**: ⭐⭐⭐⭐ (Low)  
- Can reuse existing build artifacts
- Dockerfile is straightforward

**Community expectations**: ⭐⭐  
Not expected for a TUI tool.

**Update story**: ⭐⭐⭐⭐  
`docker pull ilfroloff/snapback:latest` - manual but simple.

**Uninstallation**: ⭐⭐⭐⭐⭐  
`docker rmi ilfroloff/snapback` - clean.

**Why defer**:
- **TUI in Docker is awkward**: Requires terminal passthrough, not a great UX
- **Not the primary use case**: snapback is an interactive tool, not a service
- **Limited value**: Users can already install via npm/brew

**When to reconsider**:
- If users want to run snapback in CI/CD
- If there's a use case for isolated snapback environments
- If snapback gains a non-interactive mode

**Implementation approach** (if pursued):

1. Write `Dockerfile`:

```dockerfile
FROM rust:1.75 as builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates
COPY --from=builder /app/target/release/snapback /usr/local/bin/snapback
RUN ln -s /usr/local/bin/snapback /usr/local/bin/sb
ENTRYPOINT ["snapback"]
```

2. CI/CD workflow to build and push to Docker Hub / GitHub Container Registry
3. Document usage: `docker run -it -v ~/.claude:/root/.claude ilfroloff/snapback`

**Effort estimate**: 1 day.

---

## Architecture Overview

### Recommended Distribution System

```
┌─────────────────────────────────────────────────────────────┐
│                    release-plz (existing)                    │
│  - Drives versioning from Conventional Commits             │
│  - Cuts vX.Y.Z git tags + GitHub Releases                  │
└────────────────────────┬────────────────────────────────────┘
                         │ triggers on v* tag
                         ▼
         ┌──────────────────────────────────┐
         │   Build Job (existing, extended) │
         │  - Builds 4 platform binaries    │
         │  - Verifies sb == snapback       │
         │  - Uploads artifacts             │
         └────────┬─────────────────────────┘
                  │
                  ├──────────────────┬──────────────────┐
                  ▼                  ▼                  ▼
    ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐
    │   npm publish   │  │  Homebrew tap   │  │ GitHub Releases │
    │   (existing)    │  │   (Phase 1)     │  │   (Phase 3)     │
    │                 │  │                 │  │                 │
    │ - OIDC trusted  │  │ - Update formula│  │ - Attach bins   │
    │ - 4 platforms   │  │ - Commit + push │  │ - Install script│
    └─────────────────┘  └─────────────────┘  └─────────────────┘
```

### Key Design Decisions

1. **Single source of truth**: The git tag remains the source of truth for versioning. All package managers derive their version from the tag.

2. **Reuse build artifacts**: The 4 platform binaries are built once and distributed to all package managers. No redundant builds.

3. **Automated publishing**: Each package manager has its own workflow triggered by the `v*` tag. All workflows run in parallel after the build completes.

4. **Prebuilt binaries preferred**: All package managers should distribute prebuilt binaries, not build from source. This ensures consistency with the npm package and reduces install time.

5. **Fail-fast validation**: Each workflow validates its artifacts before publishing (like the npm preflight script).

---

## CI/CD Integration Approach

### Workflow Architecture

**Current workflows**:
- `ci.yml` - lint/test on PRs
- `release-plz.yml` - automated releases on push to main
- `npm-release.yml` - builds and publishes to npm on tag push

**Proposed workflows**:

1. **`homebrew-release.yml`** (Phase 1)
   - Trigger: `v*` tag
   - Jobs:
     - `build`: Reuse the build job from `npm-release.yml` (or refactor into a shared workflow)
     - `publish`: Update the Homebrew formula with new version + checksums
   - Secrets: `HOMEBREW_TAP_TOKEN` (PAT with repo scope for the tap repo)

2. **`github-release-assets.yml`** (Phase 3)
   - Trigger: `v*` tag
   - Jobs:
     - `build`: Reuse the build job
     - `attach`: Attach binaries to the GitHub Release
   - Permissions: `contents: write` (to attach assets)

3. **Shared build workflow** (refactor)
   - Extract the build job from `npm-release.yml` into a reusable workflow
   - All publishing workflows call this shared workflow
   - Reduces duplication, ensures consistency

### Workflow Refactoring

**Current duplication**: The build job in `npm-release.yml` is specific to npm. If we add Homebrew and GitHub Releases, we'd have 3 workflows with the same build job.

**Solution**: Extract into a shared workflow.

**`.github/workflows/build-binaries.yml`** (reusable):

```yaml
name: "🔨 Build binaries"

on:
  workflow_call:
    outputs:
      version:
        description: "The version being built"
        value: ${{ jobs.build.outputs.version }}

jobs:
  build:
    name: "🔨 Build ${{ matrix.key }}"
    runs-on: ${{ matrix.runner }}
    strategy:
      matrix:
        include:
          - key: darwin-arm64
            target: aarch64-apple-darwin
            runner: macos-15
          - key: darwin-x64
            target: x86_64-apple-darwin
            runner: macos-15-intel
          - key: linux-x64
            target: x86_64-unknown-linux-musl
            runner: ubuntu-latest
          - key: linux-arm64
            target: aarch64-unknown-linux-musl
            runner: ubuntu-24.04-arm
    steps:
      # ... (same as npm-release.yml)
    
  upload:
    name: "⬆️ Upload artifacts"
    needs: build
    runs-on: ubuntu-latest
    steps:
      - uses: actions/download-artifact@v4
        with:
          pattern: bin-*
          merge-multiple: true
          path: dist
      - uses: actions/upload-artifact@v4
        with:
          name: all-binaries
          path: dist
```

**`.github/workflows/npm-release.yml`** (refactored):

```yaml
name: "📦 Publish to npm"

on:
  push:
    tags: ["v*"]

jobs:
  build:
    uses: ./.github/workflows/build-binaries.yml

  publish:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - uses: actions/download-artifact@v4
        with:
          name: all-binaries
          path: npm/bin
      # ... (rest of the publish job)
```

**`.github/workflows/homebrew-release.yml`** (new):

```yaml
name: "🍺 Publish to Homebrew"

on:
  push:
    tags: ["v*"]

jobs:
  build:
    uses: ./.github/workflows/build-binaries.yml

  publish:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - uses: actions/download-artifact@v4
        with:
          name: all-binaries
          path: dist
      
      - name: "📦 Create tarballs"
        run: |
          for platform in darwin-arm64 darwin-x64 linux-x64 linux-arm64; do
            tar -czf "snapback-${platform}.tar.gz" -C "dist/${platform}" snapback
          done
      
      - name: "🔐 Compute checksums"
        run: |
          sha256sum snapback-*.tar.gz > checksums.txt
      
      - name: "📝 Update formula"
        run: |
          # Clone the tap repo
          git clone https://github.com/ilfroloff/homebrew-tap.git
          cd homebrew-tap
          
          # Update the formula
          VERSION="${GITHUB_REF_NAME#v}"
          # ... (update Formula/snapback.rb with new version + checksums)
          
          # Commit and push
          git config user.name "github-actions[bot]"
          git config user.email "github-actions[bot]@users.noreply.github.com"
          git add Formula/snapback.rb
          git commit -m "Update snapback to v${VERSION}"
          git push
        env:
          GITHUB_TOKEN: ${{ secrets.HOMEBREW_TAP_TOKEN }}
```

### Secret Management

**Current secrets**:
- `RELEASE_PLZ_TOKEN` - PAT for release-plz (required)

**New secrets**:
- `HOMEBREW_TAP_TOKEN` - PAT with `repo` scope for the Homebrew tap repo (Phase 1)
- `CARGO_REGISTRY_TOKEN` - crates.io API token (Phase 2, if pursued)

**Best practices**:
- Use OIDC trusted publishing where possible (npm already does this)
- Use fine-grained PATs with minimal scope
- Document each secret's purpose and scope in the workflow file
- Rotate secrets periodically

### Release Orchestration

**Current flow**:
1. Push to main → release-plz creates/updates release PR
2. Merge release PR → release-plz cuts tag + GitHub Release
3. Tag push → npm-release.yml builds and publishes to npm

**Proposed flow**:
1. Push to main → release-plz creates/updates release PR
2. Merge release PR → release-plz cuts tag + GitHub Release
3. Tag push → **parallel**:
   - npm-release.yml builds and publishes to npm
   - homebrew-release.yml builds and publishes to Homebrew
   - github-release-assets.yml attaches binaries to GitHub Release

**Failure handling**:
- Each workflow is independent - one failure doesn't block others
- Each workflow validates its artifacts before publishing (fail-fast)
- If npm publish fails, the version is burned (npm doesn't allow re-publish) - same risk as today
- If Homebrew publish fails, the formula can be manually updated

**Rollback story**:
- npm: Cannot rollback (version is burned) - same as today
- Homebrew: Can revert the formula commit
- GitHub Releases: Can delete and re-create the release

---

## Maintenance Model

### Ongoing Costs Per Package Manager

| Package Manager | Per-Release Cost | Testing Cost | Documentation Cost | Community Support |
|----------------|------------------|--------------|---------------------|-------------------|
| npm (existing) | 0 (automated) | 0 (automated) | 0 (existing) | Low |
| Homebrew | 0 (automated) | Low (formula validation) | Low (update README) | Low-Medium |
| crates.io | 0 (automated) | Low (cargo publish validation) | Low (update README) | Medium |
| GitHub Releases | 0 (automated) | 0 (automated) | Low (update README) | Low |
| APT/YUM | High (manual per distro) | High (test on each distro) | Medium (distro-specific docs) | High |
| Nix | Low (update expression) | Low (Nix CI) | Low (update README) | Low |
| Windows | High (new platform) | High (Windows testing) | High (Windows docs) | High |

### Mitigation Strategies

1. **Automate everything**: All package managers should have automated publishing workflows. Manual steps are a maintenance burden.

2. **Reuse build artifacts**: Build once, distribute everywhere. Don't rebuild for each package manager.

3. **Fail-fast validation**: Each workflow validates its artifacts before publishing. Catch errors early.

4. **Community contribution model**: Allow contributors to maintain specific package managers. Document the process in `CONTRIBUTING.md`.

5. **Deprecation policy**: If a package manager becomes too burdensome, deprecate it gracefully. Announce in README, give users 6 months to migrate.

6. **Testing strategy**:
   - **CI**: Each workflow validates its artifacts (like npm's preflight script)
   - **Manual**: Periodically test each installation method on a clean machine
   - **Community**: Encourage users to report installation issues

### Documentation Updates

**README.md**:
- Add Homebrew installation method (Phase 1)
- Add crates.io installation method (Phase 2)
- Add direct download + install script (Phase 3)
- Keep npm as the primary recommendation (it's the most tested)

**docs/agents/OPERATIONS.md**:
- Document each new workflow
- Document the shared build workflow
- Document secret management
- Document failure handling

**docs/GUIDE.md**:
- Update installation instructions
- Add troubleshooting for each installation method

---

## Implementation Phases

### Phase 1: Homebrew (2-3 days)

**Goal**: Add Homebrew as an installation method for macOS and Linux users.

**Tasks**:
1. Create the Homebrew tap repo (`github.com/ilfroloff/homebrew-tap`)
2. Write the initial formula (`Formula/snapback.rb`)
3. Create `.github/workflows/homebrew-release.yml`
4. Refactor the build job into a shared workflow (`.github/workflows/build-binaries.yml`)
5. Update `npm-release.yml` to use the shared workflow
6. Test the workflow end-to-end with a dry run
7. Update README.md with Homebrew installation instructions
8. Update docs/agents/OPERATIONS.md with workflow documentation

**Success criteria**:
- `brew tap ilfroloff/tap && brew install snapback` works on macOS (arm64/x64) and Linux (x64/arm64)
- `brew upgrade snapback` works
- Automated publishing on tag push
- No manual steps per release

**Risk mitigation**:
- Start with a tap (not homebrew-core) for faster iteration
- Test on a clean machine before announcing
- Document the tap in README, but keep npm as the primary recommendation

---

### Phase 2: crates.io (1-3 days, optional)

**Goal**: Publish snapback to crates.io for Rust ecosystem discoverability.

**Decision gate**: After Phase 1 is stable, evaluate whether crates.io is worth the workflow change. If the audience is primarily non-Rust developers, skip this phase.

**Tasks**:
1. Ensure Cargo.toml has no path/git dependencies
2. Remove `publish = false` from release-plz.toml
3. Remove `git_only = true` from release-plz.toml
4. Re-enable `semver_check = true` (or keep it off if the lib is truly internal-only)
5. Test with `cargo publish --dry-run`
6. Add `cargo publish` step to release-plz workflow (or create a separate workflow)
7. Update README.md with `cargo install snapback` instructions
8. Update docs/agents/OPERATIONS.md with workflow documentation

**Success criteria**:
- `cargo install snapback` works
- Automated publishing on tag push
- No manual steps per release

**Risk mitigation**:
- Test with a dry run before the real publish
- Keep the npm package as the primary recommendation
- Document that `cargo install` requires a Rust toolchain

---

### Phase 3: Direct GitHub Releases + Install Script (1 day)

**Goal**: Provide a curl-based install script for users who don't use package managers.

**Tasks**:
1. Create `.github/workflows/github-release-assets.yml`
2. Modify the workflow to attach binaries to the GitHub Release
3. Write `install.sh` (curl-based install script)
4. Test the install script on macOS and Linux
5. Update README.md with install script instructions
6. Update docs/agents/OPERATIONS.md with workflow documentation

**Success criteria**:
- `curl -fsSL https://raw.githubusercontent.com/ilfroloff/snapback/main/install.sh | sh` works
- Binaries are attached to every GitHub Release
- No manual steps per release

**Risk mitigation**:
- Document that the install script is a fallback, not the primary recommendation
- Test on a clean machine before announcing
- Provide checksums for verification

---

### Phase 4: Evaluate and Expand (ongoing)

**Goal**: Evaluate user demand and expand to additional package managers if justified.

**Decision gates**:
- **crates.io**: After Phase 1, evaluate if Rust ecosystem discoverability is worth the workflow change
- **Linux distro packages**: After 6 months, evaluate if there's significant Linux demand
- **Windows support**: Only if Claude Code officially supports Windows
- **Nix**: Only if Nix users explicitly request it

**Tasks**:
- Monitor GitHub issues for installation requests
- Track installation method usage (if possible)
- Survey users on preferred installation method
- Evaluate maintenance burden of existing package managers

---

## Open Questions

### 1. Homebrew: Tap vs homebrew-core?

**Decision**: Start with a tap (faster iteration, less friction), migrate to homebrew-core later if demand justifies it.

**Trade-offs**:
- **Tap**: Full control, fast iteration, but users must add the tap first
- **homebrew-core**: Maximum discoverability, no tap friction, but review process and slower iteration

**Recommendation**: Start with a tap. Migrate to homebrew-core after 6 months if there are 100+ Homebrew installs.

### 2. crates.io: To publish or not to publish?

**Decision**: Defer to Phase 2. Evaluate after Homebrew is stable.

**Trade-offs**:
- **Publish**: Rust ecosystem discoverability, consistency with other Rust tools, but workflow change
- **Don't publish**: Simpler workflow, but less discoverable for Rust developers

**Recommendation**: If the audience is primarily non-Rust developers (using Claude Code), skip crates.io. If Rust developers are a key audience, publish.

### 3. Build from source vs prebuilt binaries in Homebrew?

**Decision**: Prebuilt binaries.

**Trade-offs**:
- **Prebuilt**: Faster install, consistent with npm, but more complex formula
- **From source**: Simpler formula, but slower install and requires Rust toolchain

**Recommendation**: Prebuilt binaries. Users who want to build from source can use `cargo install`.

### 4. Should we attach binaries to GitHub Releases?

**Decision**: Yes, in Phase 3.

**Trade-offs**:
- **Attach**: Provides a direct download link, useful for the install script, but widens the workflow's permissions
- **Don't attach**: Simpler workflow, but no direct download link

**Recommendation**: Attach binaries. It's a small change and provides value for the install script.

### 5. Should we support Windows?

**Decision**: No, not until Claude Code officially supports Windows.

**Trade-offs**:
- **Support**: Larger audience, but high maintenance burden and Claude Code isn't on Windows
- **Don't support**: Smaller audience, but focused on the platforms where Claude Code works

**Recommendation**: Don't support Windows until Claude Code does.

### 6. How to handle version synchronization across registries?

**Decision**: All registries derive their version from the git tag. No separate version sources.

**Trade-offs**:
- **Tag as source of truth**: Consistent, but requires automated publishing
- **Separate versions**: More flexible, but drift is inevitable

**Recommendation**: Tag as source of truth. All publishing is automated and triggered by the tag.

### 7. What's the rollback story if one registry fails?

**Decision**: Each registry is independent. Failure in one doesn't block others.

**Trade-offs**:
- **Independent**: One failure doesn't block others, but users on different registries may have different versions
- **Atomic**: All registries succeed or fail together, but one failure blocks all

**Recommendation**: Independent. It's simpler and more resilient. Users on different registries may have different versions temporarily, but that's acceptable.

### 8. Should installation methods be feature-flagged or all ship together?

**Decision**: All ship together, but npm remains the primary recommendation.

**Trade-offs**:
- **All together**: Simpler release process, but all methods must work
- **Feature-flagged**: Can ship methods independently, but more complex release process

**Recommendation**: All together. The workflows are automated and independent. If one fails, the others still work.

---

## Final Recommendations

### Priority Order

1. **Homebrew** (Phase 1) - Highest priority. macOS standard, low maintenance, high audience fit.
2. **crates.io** (Phase 2, optional) - Medium priority. Rust ecosystem discoverability, but workflow change.
3. **Direct GitHub Releases + install script** (Phase 3) - Low priority. Fallback for users who don't use package managers.
4. **Linux distro packages** (defer) - Low priority. High maintenance, limited audience.
5. **Nix** (defer) - Low priority. Small audience.
6. **Windows** (defer) - Very low priority. Claude Code isn't on Windows.
7. **Docker** (defer) - Very low priority. Awkward for TUI, limited value.

### Architecture Decisions

1. **Single source of truth**: Git tag is the source of truth for versioning.
2. **Reuse build artifacts**: Build once, distribute everywhere.
3. **Automated publishing**: All package managers have automated workflows.
4. **Prebuilt binaries preferred**: All package managers distribute prebuilt binaries.
5. **Fail-fast validation**: Each workflow validates its artifacts before publishing.

### Maintenance Model

1. **Automate everything**: No manual steps per release.
2. **Community contribution model**: Allow contributors to maintain specific package managers.
3. **Deprecation policy**: If a package manager becomes too burdensome, deprecate it gracefully.
4. **Testing strategy**: CI validation + periodic manual testing.

### Success Metrics

- **Homebrew**: 100+ installs in the first 6 months
- **crates.io**: 50+ installs in the first 6 months (if pursued)
- **Install script**: 20+ uses in the first 6 months

If these metrics are not met, reconsider the investment in those package managers.

---

## Conclusion

The recommended path is:

1. **Phase 1**: Add Homebrew (2-3 days)
2. **Phase 2**: Evaluate crates.io (1-3 days, optional)
3. **Phase 3**: Add direct GitHub Releases + install script (1 day)
4. **Phase 4**: Evaluate and expand based on user demand

This approach prioritizes the highest-value package managers first, minimizes maintenance burden through automation, and leaves room for future expansion based on user demand.

The key insight is that **Homebrew is the natural next step** for a Rust CLI tool targeting macOS developers. It's the expected distribution channel, has low maintenance burden, and integrates well with the existing release-plz workflow.

After Homebrew is stable, evaluate whether crates.io is worth the workflow change. If the audience is primarily non-Rust developers (using Claude Code), crates.io adds little value. If Rust developers are a key audience, crates.io becomes more important.

Direct GitHub Releases + install script is a nice-to-have fallback, but not a priority. It's useful for users who don't use package managers, but it's less discoverable and has no automatic updates.

Linux distro packages, Nix, Windows, and Docker are all deferred due to high maintenance burden, limited audience, or platform mismatch. They can be reconsidered if user demand materializes.
