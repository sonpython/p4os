#!/usr/bin/env bash
# Build the user-mode apps in userapps/ and pack each into an app image:
# out/userapps/<name>.p4app. Prints the image paths.
#
# RUSTFLAGS is set explicitly so the result does not depend on which
# .cargo/config.toml files happen to be merged (git worktrees nest them).
set -euo pipefail
cd "$(dirname "$0")/.."
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
TARGET=riscv32imafc-unknown-none-elf
OUT=out/userapps
APPS=(hello crash ticker)

RUSTFLAGS="-C link-arg=-Tlink.x -C relocation-model=pie -C link-arg=-pie -C link-arg=--no-dynamic-linker" \
  cargo build --release --manifest-path userapps/Cargo.toml --target "$TARGET" --target-dir "$OUT/cargo" >&2

for app in "${APPS[@]}"; do
  python3 scripts/mkapp.py "$OUT/cargo/$TARGET/release/$app" "$OUT/$app.p4app" >&2
  echo "$OUT/$app.p4app"
done
