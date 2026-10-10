# Homebrew Formula for snapback

This directory contains the Homebrew formula for installing snapback via `brew install snapback`.

## Installation

Once the formula is merged into homebrew-core (see status below), you can install snapback with:

```bash
brew install snapback
```

This builds snapback from source and installs both binaries:
- `snapback` — the main TUI
- `sb` — short alias

## Current Status

**Status:** Preparing for submission to homebrew-core

The formula is ready for testing. Once validated, it will be submitted as a pull request to [Homebrew/homebrew-core](https://github.com/Homebrew/homebrew-core).

Track the PR status in the main plan: [`docs/plans/2026-10-06T04:50:25Z-extend-installation-options/PLAN.md`](../docs/plans/2026-10-06T04:50:25Z-extend-installation-options/PLAN.md)

## Local Testing

To test the formula locally before submission:

### 1. Compute the SHA256 checksum

Download the release tarball and compute its SHA256:

```bash
# Download the tarball for the version you're testing
curl -L -o snapback-0.12.0.tar.gz \
  https://github.com/ilfroloff/snapback/archive/refs/tags/v0.12.0.tar.gz

# Compute SHA256
shasum -a 256 snapback-0.12.0.tar.gz
```

Update the `sha256` line in `snapback.rb` with the computed value.

### 2. Install from the local formula

```bash
# Install from the local formula file
brew install --build-from-source ./HomebrewFormula/snapback.rb

# Verify installation
snapback --version
sb --version

# Run the formula's test block
brew test ./HomebrewFormula/snapback.rb
```

### 3. Validate formula style

```bash
# Check formula style (must pass before submission)
brew style ./HomebrewFormula/snapback.rb

# Audit the formula
brew audit --strict ./HomebrewFormula/snapback.rb
```

## Submitting to homebrew-core

### Prerequisites

1. **Fork homebrew-core:**
   ```bash
   # Fork https://github.com/Homebrew/homebrew-core on GitHub
   # Then clone your fork
   git clone https://github.com/YOUR_USERNAME/homebrew-core.git
   cd homebrew-core
   ```

2. **Create a feature branch:**
   ```bash
   git checkout -b snapback
   ```

3. **Add the formula:**
   ```bash
   # Copy the formula to the correct location
   cp /path/to/snapback/HomebrewFormula/snapback.rb Formula/s/snapback.rb
   
   # Verify it passes style and audit
   brew style Formula/s/snapback.rb
   brew audit --strict Formula/s/snapback.rb
   brew audit --online Formula/s/snapback.rb
   ```

4. **Commit and push:**
   ```bash
   git add Formula/s/snapback.rb
   git commit -m "snapback 0.12.0 (new formula)"
   git push origin snapback
   ```

5. **Open a pull request:**
   - Go to https://github.com/Homebrew/homebrew-core/pulls
   - Click "New Pull Request"
   - Select your fork and branch
   - Use the commit message as the PR title
   - Fill in the PR template

### What to expect

- **Review process:** homebrew-core maintainers will review the formula for style, correctness, and notability
- **Timeline:** Review can take days to weeks depending on maintainer availability
- **Iteration:** Be prepared to address feedback and update the formula
- **Bottles:** After merge, Homebrew's CI will build bottles (prebuilt binaries) for supported platforms

### Notability requirements

homebrew-core requires projects to meet notability criteria:
- 100+ GitHub stars (snapback meets this)
- Active development (snapback meets this)
- Clear use case (snapback meets this)

If the submission is rejected for notability, consider maintaining a custom tap instead:
```bash
# Custom tap alternative (if needed)
brew tap ilfroloff/snapback
brew install snapback
```

## Formula details

- **Build method:** Builds from source using `cargo install`
- **Dependencies:** Requires Rust toolchain at build time
- **Binaries:** Installs both `snapback` and `sb`
- **Platforms:** macOS (arm64, x64) and Linux (x64, arm64)
- **Updates:** Uses `livecheck` with GitHub latest release strategy

## Troubleshooting

### Build fails with "Rust not found"

Ensure Rust is installed:
```bash
brew install rust
```

### Test fails

The test runs `snapback --version`. If it fails:
1. Check that the binary was installed: `which snapback`
2. Run manually: `snapback --version`
3. Check Homebrew logs: `brew logs snapback`

### Formula style errors

Run `brew style` and fix any issues:
```bash
brew style ./HomebrewFormula/snapback.rb
```

Common issues:
- Incorrect indentation (use 2 spaces)
- Missing or incorrect metadata fields
- Non-standard method calls

## References

- [Homebrew Formula Cookbook](https://docs.brew.sh/Formula-Cookbook)
- [homebrew-core Contribution Guide](https://github.com/Homebrew/homebrew-core/blob/master/CONTRIBUTING.md)
- [Rust Formula Examples](https://github.com/Homebrew/homebrew-core/search?q=depends_on+%22rust%22+%3A%3E+%3Abuild)
- [snapback Installation Plan](../docs/plans/2026-10-06T04:50:25Z-extend-installation-options/PLAN.md)
