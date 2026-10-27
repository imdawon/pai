#!/bin/sh
# Installs pai on macOS or Linux: downloads the release binary for this OS and CPU, checks the
# Ed25519 signature on SHA256SUMS with the pai release key below, checks the binary against
# SHA256SUMS, and puts it in ~/.local/bin (or $PAI_INSTALL_DIR).
#
#   sh install.sh                                  install only
#   sh install.sh pair <code> --setup <harness>    then run `pai pair <code> --setup <harness>`:
#                                                  the one command the DawonAI web app shows
set -eu

case "${1:-}" in
"" | pair) ;;
*) echo "pai install: the only command this installer runs after installing is \`pai pair\`, not \`$1\`" >&2; exit 1 ;;
esac

# The pai release key (release/signing-key.pub), written here by release/embed-key.sh. The same
# key is in pai's README on GitHub and on https://privacyai.example/pai/.
SIGNING_KEY_SPKI='MCowBQYDK2VwAyEAAaxr74KfLKGdOR5jPlOo1ngZVAIOOafRkstl+2po5Cs='
SIGNING_KEY_SSH='AAAAC3NzaC1lZDI1NTE5AAAAIAGsa++CnyyhnTkeYz5TqNZ4GVQCDjmn0ZLLZftqaOQr'
# Names this key's signature files, SHA256SUMS.<id>.sig and .sshsig.
SIGNING_KEY_ID='f0a52c411efb5295'

BASE="${PAI_DOWNLOAD_BASE:-https://privacyai.example/pai/latest}"
BASE="${BASE%/}"
BIN_DIR="${PAI_INSTALL_DIR:-$HOME/.local/bin}"

fail() {
	echo "pai install: $*" >&2
	exit 1
}

[ -n "$SIGNING_KEY_SPKI" ] && [ -n "$SIGNING_KEY_SSH" ] && [ -n "$SIGNING_KEY_ID" ] ||
	fail "this copy of install.sh has no pai release key, so it can't check the download. Use https://privacyai.example/pai/install.sh"

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

# OpenSSL 3 checks Ed25519 signatures. LibreSSL (macOS's openssl) and OpenSSL 1.1 can't.
openssl_can_verify() {
	command -v openssl >/dev/null 2>&1 || return 1
	case "$(openssl version 2>/dev/null)" in
	"OpenSSL "[3-9].*) return 0 ;;
	esac
	return 1
}

# `ssh-keygen -Y verify` arrived in OpenSSH 8.1. macOS ships OpenSSH.
ssh_keygen_can_verify() {
	command -v ssh-keygen >/dev/null 2>&1 && command -v ssh >/dev/null 2>&1 || return 1
	v="$(ssh -V 2>&1 | sed -n 's/^OpenSSH_\([0-9][0-9]*\)\.\([0-9][0-9]*\).*/\1 \2/p')"
	[ -n "$v" ] || return 1
	set -- $v
	[ "$1" -gt 8 ] || { [ "$1" -eq 8 ] && [ "$2" -ge 1 ]; }
}

not_signed() {
	fail "SHA256SUMS from $BASE isn't signed by the pai release key, so the download may have been changed; not installing"
}

check_signature() {
	if openssl_can_verify; then
		download "$BASE/SHA256SUMS.$SIGNING_KEY_ID.sig" "$tmp/SHA256SUMS.sig" || fail "couldn't download $BASE/SHA256SUMS.$SIGNING_KEY_ID.sig"
		printf -- '-----BEGIN PUBLIC KEY-----\n%s\n-----END PUBLIC KEY-----\n' "$SIGNING_KEY_SPKI" >"$tmp/release-key.pem"
		openssl pkeyutl -verify -pubin -inkey "$tmp/release-key.pem" -rawin \
			-in "$tmp/SHA256SUMS" -sigfile "$tmp/SHA256SUMS.sig" >/dev/null 2>&1 || not_signed
		echo "Checked the signature on SHA256SUMS with OpenSSL."
	elif ssh_keygen_can_verify; then
		download "$BASE/SHA256SUMS.$SIGNING_KEY_ID.sshsig" "$tmp/SHA256SUMS.sshsig" || fail "couldn't download $BASE/SHA256SUMS.$SIGNING_KEY_ID.sshsig"
		echo "pai-release ssh-ed25519 $SIGNING_KEY_SSH" >"$tmp/allowed_signers"
		ssh-keygen -Y verify -f "$tmp/allowed_signers" -I pai-release -n pai-release \
			-s "$tmp/SHA256SUMS.sshsig" <"$tmp/SHA256SUMS" >/dev/null 2>&1 || not_signed
		echo "Checked the signature on SHA256SUMS with ssh-keygen."
	else
		echo "pai install: warning: this computer has neither OpenSSL 3 nor OpenSSH 8.1 or later, so the signature on SHA256SUMS isn't checked. Only the checksum, from the same server as the binary, protects this install." >&2
	fi
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "Downloading $asset from $BASE"
download "$BASE/$asset" "$tmp/pai" || fail "couldn't download $BASE/$asset"
download "$BASE/SHA256SUMS" "$tmp/SHA256SUMS" || fail "couldn't download $BASE/SHA256SUMS"
check_signature

expected="$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1; exit }' "$tmp/SHA256SUMS")"
[ -n "$expected" ] || fail "SHA256SUMS has no entry for $asset"
actual="$(sha256 "$tmp/pai")"
[ "$actual" = "$expected" ] || fail "checksum mismatch for $asset (expected $expected, got $actual); not installing"

mkdir -p "$BIN_DIR"
chmod 0755 "$tmp/pai"
# Move into place last, so a failed install never leaves a broken pai behind.
mv -f "$tmp/pai" "$BIN_DIR/pai"
echo "Installed $("$BIN_DIR/pai" --version) to $BIN_DIR/pai"

# Put BIN_DIR on the PATH of new shells, in the login shell's startup file. A terminal or coding
# harness that's already open keeps its PATH, so the note names the full path too.
# PAI_NO_MODIFY_PATH=1 leaves the startup files alone.
case ":$PATH:" in
*":$BIN_DIR:"*) ;;
*)
	if [ "$BIN_DIR" = "$HOME/.local/bin" ]; then dir_expr='$HOME/.local/bin'; else dir_expr="$BIN_DIR"; fi
	case "${SHELL##*/}" in
	zsh) rc="$HOME/.zshrc" ;;
	bash) if [ "$(uname -s)" = Darwin ]; then rc="$HOME/.bash_profile"; else rc="$HOME/.bashrc"; fi ;;
	*) rc="$HOME/.profile" ;;
	esac
	line="export PATH=\"$dir_expr:\$PATH\""
	if [ "${PAI_NO_MODIFY_PATH:-0}" = 1 ]; then
		echo "Note: $BIN_DIR isn't on your PATH. Add it, or run $BIN_DIR/pai directly."
	elif grep -qsF "$line" "$rc"; then
		echo "Note: open a new terminal to use pai, or run $BIN_DIR/pai directly."
	else
		printf '\n# Added by the pai installer\n%s\n' "$line" >>"$rc"
		echo "Added $BIN_DIR to your PATH in $rc. Open a new terminal to use pai, or run $BIN_DIR/pai directly."
	fi
	;;
esac

if [ "${1:-}" = pair ]; then
	echo
	exec "$BIN_DIR/pai" "$@"
fi
