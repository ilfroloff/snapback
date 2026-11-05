class Snapback < Formula
  desc "Every Claude Code session and agent — one live board"
  homepage "https://github.com/ilfroloff/snapback"
  url "https://github.com/ilfroloff/snapback/archive/refs/tags/v0.12.0.tar.gz"
  sha256 "PLACEHOLDER_COMPUTE_FROM_RELEASE_TARBALL"
  license "Apache-2.0"
  head "https://github.com/ilfroloff/snapback.git", branch: "main"

  # Minimum Rust version required (matches rust-toolchain.toml)
  depends_on "rust" => :build

  livecheck do
    url :stable
    strategy :github_latest
  end

  # Homebrew CI generates bottles after acceptance; this block is populated automatically
  # bottle do
  #   sha256 cellar: :any_skip_relocation, arm64_sonoma:   "..."
  #   sha256 cellar: :any_skip_relocation, arm64_ventura:  "..."
  #   sha256 cellar: :any_skip_relocation, sonoma:         "..."
  #   sha256 cellar: :any_skip_relocation, ventura:        "..."
  #   sha256 cellar: :any_skip_relocation, x86_64_linux:   "..."
  # end

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    # Verify the binary runs and reports its version
    # snapback is a TUI, so we use --version which exits immediately
    assert_match version.to_s, shell_output("#{bin}/snapback --version")
  end
end
