#!/bin/sh
# Installs pai on macOS or Linux: downloads the release binary for this OS and CPU, checks it
# against SHA256SUMS, and puts it in ~/.local/bin (or $PAI_INSTALL_DIR).
set -eu

BASE="${PAI_DOWNLOAD_BASE:-https://privacyai.example/pai/latest}"
BASE="${BASE%/}"
BIN_DIR="${PAI_INSTALL_DIR:-$HOME/.local/bin}"

fail() {
	echo "pai install: $*" >&2
	exit 1
}

case "$(uname -s)" in
Linux) os=unknown-linux-musl ;;
Darwin) os=apple-darwin ;;
*) fail "unsupported OS $(uname -s); on Windows use install.ps1" ;;
esac

case "$(uname -m)" in
x86_64 | amd64) arch=x86_64 ;;
arm64 | aarch64) arch=aarch64 ;;
*) fail "unsupported CPU $(uname -m)" ;;
esac

# A shell running under Rosetta reports x86_64 on Apple silicon; the native build is better.
if [ "$os" = apple-darwin ] && [ "$arch" = x86_64 ] &&
	[ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
	arch=aarch64
fi

asset="pai-$arch-$os"

download() {
	if command -v curl >/dev/null 2>&1; then
		curl -fsSL --proto-redir '=https' -o "$2" "$1"
	elif command -v wget >/dev/null 2>&1; then
		wget -q -O "$2" "$1"
	else
		fail "needs curl or wget"
	fi
}

sha256() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | cut -d' ' -f1
	elif command -v shasum >/dev/null 2>&1; then
		shasum -a 256 "$1" | cut -d' ' -f1
	else
		fail "needs sha256sum or shasum to check the download"
	fi
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "Downloading $asset from $BASE"
download "$BASE/$asset" "$tmp/pai" || fail "couldn't download $BASE/$asset"
download "$BASE/SHA256SUMS" "$tmp/SHA256SUMS" || fail "couldn't download $BASE/SHA256SUMS"

expected="$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1; exit }' "$tmp/SHA256SUMS")"
[ -n "$expected" ] || fail "SHA256SUMS has no entry for $asset"
actual="$(sha256 "$tmp/pai")"
[ "$actual" = "$expected" ] || fail "checksum mismatch for $asset (expected $expected, got $actual); not installing"

mkdir -p "$BIN_DIR"
chmod 0755 "$tmp/pai"
# Move into place last, so a failed install never leaves a broken pai behind.
mv -f "$tmp/pai" "$BIN_DIR/pai"
echo "Installed $("$BIN_DIR/pai" --version) to $BIN_DIR/pai"

case ":$PATH:" in
*":$BIN_DIR:"*) ;;
*) echo "Note: $BIN_DIR isn't on your PATH. Add it, or run $BIN_DIR/pai directly." ;;
esac
