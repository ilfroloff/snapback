#!/usr/bin/env bash
# Re-record website/public/demo.gif from SYNTHETIC data. Needs vhs, ttyd and
# ffmpeg on PATH. Never reads the real ~/.claude, ~/.config/snapback or `claude`:
# HOME points at a throwaway copy of website/demo/home and the stub in
# website/demo/bin shadows `claude`.
set -euo pipefail

repo=$(git rev-parse --show-toplevel)
demo="$repo/website/demo"
# Fixed: every fixture record's `cwd` spells this path literally, so the home
# cannot move; the guard below is what makes deleting it safe instead.
readonly demo_parent=/tmp/snapback-demo
readonly demo_home=$demo_parent/home

# The cargo target directory as cargo itself resolves it, so CARGO_TARGET_DIR,
# CARGO_BUILD_TARGET_DIR and `build.target-dir` are all honored and the binary
# recorded is the one just built.
target_dir=$(cargo metadata --format-version 1 --no-deps --manifest-path "$repo/Cargo.toml" |
  python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
[ -n "$target_dir" ] || { echo "record.sh: cargo metadata gave no target_directory" >&2; exit 1; }

cargo build --release --manifest-path "$repo/Cargo.toml"

# rm -rf only <parent>/home, and only inside a parent that is a real directory
# owned by us: a symlinked or foreign-owned /tmp/snapback-demo could point the
# delete anywhere.
[ -e "$demo_parent" ] || [ -L "$demo_parent" ] || mkdir -m 700 "$demo_parent"
remove_demo_home() {
  [ "$1" = "$demo_parent/home" ] || { echo "record.sh: refusing to rm -rf $1 (not $demo_parent/home)" >&2; exit 1; }
  [ ! -L "$demo_parent" ] || { echo "record.sh: refusing: $demo_parent is a symlink" >&2; exit 1; }
  [ -d "$demo_parent" ] && [ -O "$demo_parent" ] || { echo "record.sh: refusing: $demo_parent is not a directory owned by you" >&2; exit 1; }
  rm -rf "$1"
}
remove_demo_home "$demo_home"
mkdir -p "$demo_home"
cp -R "$demo/home/." "$demo_home/"
cp "$demo/agents.json" "$demo_home/agents.json"
# A quick reply refuses a cwd that does not exist (src/send.rs plan_send).
grep -rhoE '"cwd":"[^"]+"' "$demo_home/.claude/projects" | sort -u |
  sed -e 's/^"cwd":"//' -e 's/"$//' | while IFS= read -r dir; do mkdir -p "$dir"; done

export HOME="$demo_home"
unset CLAUDE_PROJECTS_DIR CLAUDE_CONFIG_DIR SNAPBACK_CONFIG_DIR ANTHROPIC_MODEL
# The compose box's model label must not vary by machine.
for var in $(compgen -e | grep -E '^ANTHROPIC_DEFAULT_.*_MODEL$' || true); do unset "$var"; done
export PATH="$demo/bin:$target_dir/release:$PATH"

# Privacy gate: the stub must shadow the real `claude` before anything runs.
[ "$(command -v claude)" = "$demo/bin/claude" ] || { echo "record.sh: claude does not resolve to the stub" >&2; exit 1; }

# A stale installed `snapback` must not be the one recorded.
[ "$(command -v snapback)" = "$target_dir/release/snapback" ] || { echo "record.sh: snapback does not resolve to $target_dir/release" >&2; exit 1; }

mkdir -p "$demo/out"
cd "$demo"
vhs snapback.tape
