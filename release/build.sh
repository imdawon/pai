#!/bin/sh
# Builds every pai release file into dist/<version>/, copies it to dist/latest/, and writes
# dist/index.html (release/index.sh). dist/ is what gets hosted at https://<domain>/pai/.
#
# Signs SHA256SUMS with the release key at $PAI_SIGNING_KEY, a PEM Ed25519 private key that lives
# offline (release/SIGNING.md), and refuses to build without it. Needs OpenSSL 3 for Ed25519.
#
# Runs on a Mac: the macOS binaries build natively, Linux and Windows build in Docker.
set -eu

cd "$(dirname "$0")/.."
root="$(pwd)"
export PATH="$HOME/.cargo/bin:$PATH"

# Pinned so a release can be rebuilt with the same compilers.
RUST_ALPINE="rust:1.95-alpine"
CARGO_XWIN="ghcr.io/rust-cross/cargo-xwin@sha256:9856b895265d4966f212228ba64802cf89337e2a2a537aa2533c1b8784cbc81b"

# Check the signing key before spending time on the build.
: "${PAI_SIGNING_KEY:?set PAI_SIGNING_KEY to the path of the release signing key (release/SIGNING.md)}"
[ -r "$PAI_SIGNING_KEY" ] || { echo "build: can't read the signing key $PAI_SIGNING_KEY" >&2; exit 1; }
. release/signing.sh
find_openssl
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
trap 'stty echo 2>/dev/null; exit 1' INT TERM
key_material release/signing-key.pub "$work" || {
	echo "build: release/signing-key.pub holds no Ed25519 public key yet. Make the key as release/SIGNING.md says" >&2
	exit 1
}
sh release/embed-key.sh --check
SIGNING_KEY="$PAI_SIGNING_KEY"
unset SIGNING_PASS
if grep -q 'ENCRYPTED' "$SIGNING_KEY"; then
	# Asked once here, and handed to openssl on stdin for each signature.
	[ -t 0 ] || { echo "build: $SIGNING_KEY has a passphrase; run release/build.sh in a terminal" >&2; exit 1; }
	printf 'Passphrase for %s: ' "$SIGNING_KEY" >&2
	stty -echo
	IFS= read -r SIGNING_PASS
	stty echo
	echo >&2
fi
signing_key_public "$work/signing.der" || { echo "build: couldn't read the signing key $SIGNING_KEY" >&2; exit 1; }
cmp -s "$work/signing.der" "$work/spki.der" || {
	echo "build: $SIGNING_KEY isn't the private half of release/signing-key.pub; installers would refuse this release" >&2
	exit 1
}

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

step "signing SHA256SUMS"
ed25519_sign "$out/SHA256SUMS" "$out/SHA256SUMS.sig"
sshsig_sign "$out/SHA256SUMS" "$out/SHA256SUMS.sshsig" "$work"
unset SIGNING_PASS
# Check both the way the installers will, before anything is copied to latest/.
printf -- '-----BEGIN PUBLIC KEY-----\n%s\n-----END PUBLIC KEY-----\n' "$SPKI_B64" >"$work/release-key.pem"
"$OPENSSL" pkeyutl -verify -pubin -inkey "$work/release-key.pem" -rawin -in "$out/SHA256SUMS" -sigfile "$out/SHA256SUMS.sig"
echo "$SIGNING_NAMESPACE ssh-ed25519 $SSH_B64" >"$work/allowed_signers"
ssh-keygen -Y verify -f "$work/allowed_signers" -I "$SIGNING_NAMESPACE" -n "$SIGNING_NAMESPACE" \
	-s "$out/SHA256SUMS.sshsig" <"$out/SHA256SUMS"

# A copy rather than a redirect, so latest/ works on any static host and the installers'
# https-only redirect rule never comes into play.
rm -rf "$dist/latest"
cp -R "$out" "$dist/latest"
cp install.sh install.ps1 "$dist/"

step "index.html"
sh release/index.sh "$version"

step "done"
ls -l "$out"
