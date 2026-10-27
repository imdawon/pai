#!/bin/sh
# Builds every pai release file into dist/<version>/, copies it to dist/latest/, and writes
# dist/index.html (release/index.sh). dist/ is what gets hosted at https://<domain>/pai/.
#
# Signs SHA256SUMS with the release key at $PAI_SIGNING_KEY, a PEM Ed25519 private key kept as
# release/SIGNING.md says, and refuses to build without it. While the key changes, it also signs
# with the old key at $PAI_HANDOVER_KEY. Needs OpenSSL 3 for Ed25519.
#
# Runs on a Mac: the macOS binaries build natively, Linux and Windows build in Docker.
set -eu

cd "$(dirname "$0")/.."
root="$(pwd)"
export PATH="$HOME/.cargo/bin:$PATH"

# Pinned so a release can be rebuilt with the same compilers.
RUST_ALPINE="rust:1.95-alpine"
CARGO_XWIN="ghcr.io/rust-cross/cargo-xwin@sha256:9856b895265d4966f212228ba64802cf89337e2a2a537aa2533c1b8784cbc81b"

# The domain this release talks to: pai pairs with relay.<domain> and sends requests to
# api.<domain>, and the installers and skill name https://<domain>/pai/.
: "${PAI_RELEASE_DOMAIN:?set PAI_RELEASE_DOMAIN to the domain this release is for, such as example.com}"
printf '%s' "$PAI_RELEASE_DOMAIN" | grep -Eq '^[a-z0-9]([a-z0-9.-]*[a-z0-9])?\.[a-z]{2,}$' ||
	{ echo "build: PAI_RELEASE_DOMAIN isn't a lowercase domain name: $PAI_RELEASE_DOMAIN" >&2; exit 1; }
[ "$PAI_RELEASE_DOMAIN" != privacyai.example ] || { echo "build: privacyai.example is the placeholder, not a release domain" >&2; exit 1; }
export PAI_RELEASE_RELAY_URL="https://relay.$PAI_RELEASE_DOMAIN"
export PAI_RELEASE_API_URL="https://api.$PAI_RELEASE_DOMAIN"
export PAI_RELEASE_APP_URL="https://app.$PAI_RELEASE_DOMAIN"

# Check the signing key before spending time on the build.
: "${PAI_SIGNING_KEY:?set PAI_SIGNING_KEY to the path of the release signing key (release/SIGNING.md)}"
[ -r "$PAI_SIGNING_KEY" ] || { echo "build: can't read the signing key $PAI_SIGNING_KEY" >&2; exit 1; }
. release/signing.sh
find_openssl
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
trap 'stty echo 2>/dev/null; exit 1' INT TERM

# Asks for key file $1's passphrase, if it has one, into $2. Asked once here, and handed to openssl
# on stdin for each signature.
ask_pass() {
	unset "$2"
	grep -q 'ENCRYPTED' "$1" || return 0
	[ -t 0 ] || { echo "build: $1 has a passphrase; run release/build.sh in a terminal" >&2; exit 1; }
	printf 'Passphrase for %s: ' "$1" >&2
	stty -echo
	IFS= read -r "$2"
	stty echo
	echo >&2
}

# Checks that private key file $1 (passphrase in $2's value) is the private half of the DER public
# key $3.
check_pair() {
	SIGNING_KEY="$1"
	unset SIGNING_PASS
	eval "[ -z \"\${$2+set}\" ] || SIGNING_PASS=\"\$$2\""
	signing_key_public "$work/check.der" || { echo "build: couldn't read the signing key $1" >&2; exit 1; }
	cmp -s "$work/check.der" "$3" || { echo "build: $1 isn't the private half of $4; installers would refuse this release" >&2; exit 1; }
}

# While the key changes, release/handover-key.pub holds the old key: this release is signed with it
# too, under the names pai up to 0.1.9 reads, so installed copies accept it and learn the new key.
# PAI_HANDOVER_KEY is the old private key. release/SIGNING.md, "Changing the key".
handover=false
if [ -f release/handover-key.pub ]; then
	: "${PAI_HANDOVER_KEY:?release/handover-key.pub is there, so this release must also be signed with the old key: set PAI_HANDOVER_KEY to it (release/SIGNING.md)}"
	[ -r "$PAI_HANDOVER_KEY" ] || { echo "build: can't read the old key $PAI_HANDOVER_KEY" >&2; exit 1; }
	mkdir "$work/old"
	key_material release/handover-key.pub "$work/old" || { echo "build: release/handover-key.pub isn't an Ed25519 public key" >&2; exit 1; }
	OLD_SPKI_B64="$SPKI_B64" OLD_SSH_B64="$SSH_B64"
	ask_pass "$PAI_HANDOVER_KEY" OLD_PASS
	check_pair "$PAI_HANDOVER_KEY" OLD_PASS "$work/old/spki.der" release/handover-key.pub
	handover=true
fi

key_material release/signing-key.pub "$work" || {
	echo "build: release/signing-key.pub holds no Ed25519 public key yet. Make the key as release/SIGNING.md says" >&2
	exit 1
}
[ "$handover" = false ] || [ "$SPKI_B64" != "$OLD_SPKI_B64" ] || { echo "build: release/handover-key.pub is the same key as release/signing-key.pub" >&2; exit 1; }
sh release/embed-key.sh --check
ask_pass "$PAI_SIGNING_KEY" NEW_PASS
check_pair "$PAI_SIGNING_KEY" NEW_PASS "$work/spki.der" release/signing-key.pub

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
		-e CARGO_TARGET_DIR=/target -e PAI_RELEASE_API_PINS \
		-e PAI_RELEASE_RELAY_URL -e PAI_RELEASE_API_URL -e PAI_RELEASE_APP_URL -w /src/pai "$RUST_ALPINE" \
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
		-e CARGO_TARGET_DIR=/target -e PAI_RELEASE_API_PINS \
		-e PAI_RELEASE_RELAY_URL -e PAI_RELEASE_API_URL -e PAI_RELEASE_APP_URL -e RUSTFLAGS="-C target-feature=+crt-static" -w /src/pai "$CARGO_XWIN" \
		sh -euc "cargo xwin build --cross-compiler clang --release --locked --target $target
			cp /target/$target/release/pai.exe /out/pai-$target.exe"
done

step "extras"
# The skill tells a coding agent where to download pai, so it names the release domain.
cp -R skill "$work/skill"
find "$work/skill" -type f -exec sed -i '' "s/privacyai\.example/$PAI_RELEASE_DOMAIN/g" {} +
(cd "$work/skill" && rm -f "$out/dawonai-skill.zip" && zip -qrX "$out/dawonai-skill.zip" dawonai)
cp integrations/opencode/src/index.ts "$out/opencode-dawonai.ts"
cp integrations/omp/dawonai.ts "$out/omp-dawonai.ts"

# Every binary must name this release's relay, not the placeholder.
for bin in "$out"/pai-*; do
	grep -aq "https://relay.$PAI_RELEASE_DOMAIN" "$bin" && ! grep -aq 'relay\.privacyai\.example' "$bin" ||
		{ echo "build: $(basename "$bin") doesn't point at relay.$PAI_RELEASE_DOMAIN" >&2; exit 1; }
done

# What `pai update` reads first, to tell whether there's anything newer. Signed like every file.
printf '%s\n' "$version" >"$out/VERSION"

(cd "$out" && shasum -a 256 -- * | grep -v ' SHA256SUMS$' >SHA256SUMS.tmp && mv SHA256SUMS.tmp SHA256SUMS)

step "signing SHA256SUMS with key $KEY_ID"
SIGNING_KEY="$PAI_SIGNING_KEY"
unset SIGNING_PASS
[ -z "${NEW_PASS+set}" ] || SIGNING_PASS="$NEW_PASS"
ed25519_sign "$out/SHA256SUMS" "$out/SHA256SUMS.$KEY_ID.sig"
sshsig_sign "$out/SHA256SUMS" "$out/SHA256SUMS.$KEY_ID.sshsig" "$work"
unset SIGNING_PASS NEW_PASS
# Check both the way the installers will, before anything is copied to latest/.
printf -- '-----BEGIN PUBLIC KEY-----\n%s\n-----END PUBLIC KEY-----\n' "$SPKI_B64" >"$work/release-key.pem"
"$OPENSSL" pkeyutl -verify -pubin -inkey "$work/release-key.pem" -rawin -in "$out/SHA256SUMS" -sigfile "$out/SHA256SUMS.$KEY_ID.sig"
echo "$SIGNING_NAMESPACE ssh-ed25519 $SSH_B64" >"$work/allowed_signers"
ssh-keygen -Y verify -f "$work/allowed_signers" -I "$SIGNING_NAMESPACE" -n "$SIGNING_NAMESPACE" \
	-s "$out/SHA256SUMS.$KEY_ID.sshsig" <"$out/SHA256SUMS"

if $handover; then
	step "signing SHA256SUMS with the old key too, for pai up to 0.1.9"
	SIGNING_KEY="$PAI_HANDOVER_KEY"
	unset SIGNING_PASS
	[ -z "${OLD_PASS+set}" ] || SIGNING_PASS="$OLD_PASS"
	ed25519_sign "$out/SHA256SUMS" "$out/SHA256SUMS.sig"
	sshsig_sign "$out/SHA256SUMS" "$out/SHA256SUMS.sshsig" "$work/old"
	unset SIGNING_PASS OLD_PASS
	printf -- '-----BEGIN PUBLIC KEY-----\n%s\n-----END PUBLIC KEY-----\n' "$OLD_SPKI_B64" >"$work/old-key.pem"
	"$OPENSSL" pkeyutl -verify -pubin -inkey "$work/old-key.pem" -rawin -in "$out/SHA256SUMS" -sigfile "$out/SHA256SUMS.sig"
	echo "$SIGNING_NAMESPACE ssh-ed25519 $OLD_SSH_B64" >"$work/old_signers"
	ssh-keygen -Y verify -f "$work/old_signers" -I "$SIGNING_NAMESPACE" -n "$SIGNING_NAMESPACE" \
		-s "$out/SHA256SUMS.sshsig" <"$out/SHA256SUMS"
fi

# A copy rather than a redirect, so latest/ works on any static host and the installers'
# https-only redirect rule never comes into play.
rm -rf "$dist/latest"
cp -R "$out" "$dist/latest"
cp install.sh install.ps1 "$dist/"

step "index.html"
PAI_SITE="https://$PAI_RELEASE_DOMAIN" sh release/index.sh "$version"

step "done"
ls -l "$out"
