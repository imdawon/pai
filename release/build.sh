#!/bin/sh
# Builds every pai release file into dist/<version>/, copies it to dist/latest/, and writes
# dist/index.html (release/index.sh). dist/ is what gets hosted at https://<domain>/pai/.
#
# Runs on a Mac: the macOS binaries build natively, Linux and Windows build in Docker.
set -eu

cd "$(dirname "$0")/.."
root="$(pwd)"
export PATH="$HOME/.cargo/bin:$PATH"

# Pinned so a release can be rebuilt with the same compilers.
RUST_ALPINE="rust:1.95-alpine"
CARGO_XWIN="ghcr.io/rust-cross/cargo-xwin@sha256:9856b895265d4966f212228ba64802cf89337e2a2a537aa2533c1b8784cbc81b"

version="$(awk -F'"' '/^version *=/ { print $2; exit }' Cargo.toml)"
[ -n "$version" ] || { echo "build: no version in Cargo.toml" >&2; exit 1; }
dist="$root/dist"
out="$dist/$version"
rm -rf "$out"
mkdir -p "$out"

# The model API's key pins, compiled into pai: base64 SHA-256 of each public key, comma-separated.
# List the box's current key and a spare one kept offline, so the box can switch keys without
# breaking every installed pai. See pai-gateway/deploy/CERTIFICATES.md.
: "${PAI_RELEASE_API_PINS:?set PAI_RELEASE_API_PINS to the model API key pins (current,spare)}"
export PAI_RELEASE_API_PINS

# Every cargo build below leaves out the `test-mode` feature, so release binaries have no test mode
# (docs/TESTING.md). Never add --features here.

# pai-proto is a path dependency on ../pai-relay, so the containers need both checkouts.
relay="$(cd "$root/../pai-relay" && pwd)"

step() { printf '\n== %s\n' "$*"; }

for target in aarch64-apple-darwin x86_64-apple-darwin; do
	step "$target"
	rustup target add "$target" >/dev/null
	cargo build --release --locked --target "$target"
	cp "target/$target/release/pai" "$out/pai-$target"
done

for arch in aarch64 x86_64; do
	target="$arch-unknown-linux-musl"
	case "$arch" in aarch64) platform=linux/arm64 ;; x86_64) platform=linux/amd64 ;; esac
	step "$target ($platform)"
	docker run --rm --platform "$platform" \
		-v "$root:/src/pai:ro" -v "$relay:/src/pai-relay:ro" -v "$out:/out" \
		-v "pai-cargo-registry-$arch:/usr/local/cargo/registry" -v "pai-target-linux-$arch:/target" \
		-e CARGO_TARGET_DIR=/target -e PAI_RELEASE_API_PINS -w /src/pai "$RUST_ALPINE" \
		sh -euc "apk add --no-cache musl-dev >/dev/null
			cargo build --release --locked --target $target
			cp /target/$target/release/pai /out/pai-$target"
done

for target in x86_64-pc-windows-msvc aarch64-pc-windows-msvc; do
	step "$target"
	# crt-static drops the dependency on the Visual C++ redistributable (vcruntime140.dll).
	# --cross-compiler clang because ring's ARM64 assembly needs clang, not clang-cl.
	docker run --rm \
		-v "$root:/src/pai:ro" -v "$relay:/src/pai-relay:ro" -v "$out:/out" \
		-v pai-xwin-cache:/root/.cache/cargo-xwin -v pai-cargo-registry-xwin:/usr/local/cargo/registry \
		-v pai-target-xwin:/target \
		-e CARGO_TARGET_DIR=/target -e PAI_RELEASE_API_PINS -e RUSTFLAGS="-C target-feature=+crt-static" -w /src/pai "$CARGO_XWIN" \
		sh -euc "cargo xwin build --cross-compiler clang --release --locked --target $target
			cp /target/$target/release/pai.exe /out/pai-$target.exe"
done

step "extras"
(cd skill && rm -f "$out/dawonai-skill.zip" && zip -qrX "$out/dawonai-skill.zip" dawonai)
cp integrations/opencode/src/index.ts "$out/opencode-dawonai.ts"
cp integrations/omp/dawonai.ts "$out/omp-dawonai.ts"

(cd "$out" && shasum -a 256 -- * | grep -v ' SHA256SUMS$' >SHA256SUMS.tmp && mv SHA256SUMS.tmp SHA256SUMS)

# A copy rather than a redirect, so latest/ works on any static host and the installers'
# https-only redirect rule never comes into play.
rm -rf "$dist/latest"
cp -R "$out" "$dist/latest"
cp install.sh install.ps1 "$dist/"

step "index.html"
sh release/index.sh "$version"

step "done"
ls -l "$out"
