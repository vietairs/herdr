#!/usr/bin/env bash
# Local-only Linux toolchain for scripts/local-ci.mjs legs (a rust:*-bookworm container). The workflow gets
# the same tools from setup actions on the self-hosted runner; keep the pinned versions in sync with
# .github/workflows/ci.yml (RUST_TOOLCHAIN_VERSION, Bun, Zig).
set -euo pipefail

RUST_VERSION=1.96.1
BUN_VERSION=1.3.14
ZIG_VERSION=0.16.0

apt-get update -qq
apt-get install -y -qq --no-install-recommends python3 xz-utils unzip curl ca-certificates git >/dev/null

git config --global --add safe.directory '*'

rustup toolchain install "$RUST_VERSION" --profile minimal -c rustfmt -c clippy
rustup default "$RUST_VERSION"

curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-x86_64-linux-${ZIG_VERSION}.tar.xz" | tar -xJ -C /opt
ln -sf "/opt/zig-x86_64-linux-${ZIG_VERSION}/zig" /usr/local/bin/zig

curl -fsSL "https://github.com/oven-sh/bun/releases/download/bun-v${BUN_VERSION}/bun-linux-x64.zip" -o /tmp/bun.zip
unzip -q -o /tmp/bun.zip -d /tmp/bun-dist
install -m 0755 /tmp/bun-dist/bun-linux-x64/bun /usr/local/bin/bun

curl --proto '=https' --tlsv1.2 -fsSL https://just.systems/install.sh | bash -s -- --to /usr/local/bin
curl --proto '=https' --tlsv1.2 -fsSL https://get.nexte.st/latest/linux | tar -xzf - -C /usr/local/bin
