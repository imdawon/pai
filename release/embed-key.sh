#!/bin/sh
# Copies the release signing key in release/signing-key.pub into install.sh, install.ps1 and
# README.md, which each carry it so a download can be checked without trusting the server.
# Run it once after making or changing the key, then commit the four files.
#
# `release/embed-key.sh --check` changes nothing, and fails unless all three carry the key in
# release/signing-key.pub. release/build.sh runs it.
set -eu

cd "$(dirname "$0")/.."
. release/signing.sh

check=false
case "${1:-}" in
--check) check=true ;;
"") ;;
*)
	echo "usage: release/embed-key.sh [--check]" >&2
	exit 2
	;;
esac

find_openssl
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT INT TERM

key_material release/signing-key.pub "$work" || {
	echo "embed-key: release/signing-key.pub holds no Ed25519 public key yet. Make one as release/SIGNING.md says" >&2
	exit 1
}

# Replaces the line starting with $2 in file $1 with $2$3$4.
line() {
	awk -v prefix="$2" -v value="$2$3$4" 'index($0, prefix) == 1 { print value; found = 1; next } { print } END { exit !found }' "$1"
}

line install.sh "SIGNING_KEY_SPKI='" "$SPKI_B64" "'" >"$work/install.sh.1"
line "$work/install.sh.1" "SIGNING_KEY_SSH='" "$SSH_B64" "'" >"$work/install.sh.2"
line "$work/install.sh.2" "SIGNING_KEY_ID='" "$KEY_ID" "'" >"$work/install.sh"
line install.ps1 "\$signingKeySpki = '" "$SPKI_B64" "'" >"$work/install.ps1.1"
line "$work/install.ps1.1" "\$signingKeySsh = '" "$SSH_B64" "'" >"$work/install.ps1.2"
line "$work/install.ps1.2" "\$signingKeyId = '" "$KEY_ID" "'" >"$work/install.ps1"

# README.md: everything between the two markers.
{
	echo '```text'
	echo '-----BEGIN PUBLIC KEY-----'
	echo "$SPKI_B64"
	echo '-----END PUBLIC KEY-----'
	echo '```'
	echo
	echo 'The same key as an OpenSSH allowed-signers line:'
	echo
	echo '```text'
	echo "$SIGNING_NAMESPACE ssh-ed25519 $SSH_B64"
	echo '```'
	echo
	echo "Key ID \`$KEY_ID\`: its signatures are \`SHA256SUMS.$KEY_ID.sig\` and \`SHA256SUMS.$KEY_ID.sshsig\`."
} >"$work/readme-key"
awk -v keyfile="$work/readme-key" '
	/^<!-- release-key -->$/ { print; while ((getline l < keyfile) > 0) print l; skip = 1; found++; next }
	/^<!-- \/release-key -->$/ { skip = 0; found++ }
	!skip { print }
	END { exit found != 2 }
' README.md >"$work/README.md"

status=0
for f in install.sh install.ps1 README.md; do
	if cmp -s "$f" "$work/$f"; then
		continue
	fi
	if $check; then
		echo "embed-key: $f doesn't carry the key in release/signing-key.pub. Run release/embed-key.sh and commit the result" >&2
		status=1
	else
		cat "$work/$f" >"$f"
		echo "Wrote the release key into $f"
	fi
done
exit $status
