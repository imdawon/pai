# Shared by release/build.sh and release/embed-key.sh: the Ed25519 release key and the two
# signatures made with it over SHA256SUMS. Sourced, not run. See release/SIGNING.md.
#
# Each signature file is named after the key that made it, so a release can carry signatures from
# two keys while the key changes (release/SIGNING.md, "Changing the key"):
# SHA256SUMS.<id>.sig is the raw 64-byte Ed25519 signature of SHA256SUMS, which OpenSSL 3 checks.
# SHA256SUMS.<id>.sshsig is the same key's signature in OpenSSH's format (namespace pai-release),
# which `ssh-keygen -Y verify` checks: macOS ships OpenSSH but no OpenSSL with Ed25519.
# <id> is the key's ID: the first 16 hex digits of the SHA-256 of its DER public key (SPKI).
# Releases up to 0.1.9 named them SHA256SUMS.sig and SHA256SUMS.sshsig, which pai up to 0.1.9 reads.

SIGNING_NAMESPACE=pai-release

# Sets OPENSSL to an OpenSSL 3 or later. macOS's /usr/bin/openssl is LibreSSL, which has no Ed25519.
find_openssl() {
	for o in "${PAI_OPENSSL:-}" openssl /opt/homebrew/opt/openssl@3/bin/openssl /usr/local/opt/openssl@3/bin/openssl; do
		[ -n "$o" ] || continue
		v="$("$o" version 2>/dev/null)" || continue
		case "$v" in
		"OpenSSL "[3-9].*)
			OPENSSL="$o"
			return 0
			;;
		esac
	done
	echo "signing: needs OpenSSL 3 or later for Ed25519 (macOS's openssl is LibreSSL, which has none). Run \`brew install openssl@3\`, or set PAI_OPENSSL to its path" >&2
	return 1
}

# OpenSSH wire format: a big-endian uint32, and a length-prefixed string or file.
u32() {
	printf "$(printf '\\%03o\\%03o\\%03o\\%03o' $(($1 >> 24 & 255)) $(($1 >> 16 & 255)) $(($1 >> 8 & 255)) $(($1 & 255)))"
}
sstr() {
	u32 "$(printf %s "$1" | wc -c | tr -d ' ')"
	printf %s "$1"
}
sfile() {
	u32 "$(wc -c <"$1" | tr -d ' ')"
	cat "$1"
}

# Reads the PEM public key $1 into $2/spki.der and $2/pk.ssh, and sets SPKI_B64 (what OpenSSL
# reads), SSH_B64 (the key in an OpenSSH allowed-signers line) and KEY_ID. Fails unless it's
# Ed25519.
key_material() {
	"$OPENSSL" pkey -pubin -in "$1" -outform der -out "$2/spki.der" 2>/dev/null || return 1
	"$OPENSSL" pkey -pubin -in "$1" -text -noout 2>/dev/null | grep -q '^ED25519 Public-Key' || return 1
	[ "$(wc -c <"$2/spki.der" | tr -d ' ')" = 44 ] || return 1
	tail -c 32 "$2/spki.der" >"$2/pk.raw"
	{
		sstr ssh-ed25519
		sfile "$2/pk.raw"
	} >"$2/pk.ssh"
	SPKI_B64="$(base64 <"$2/spki.der" | tr -d '\n')"
	SSH_B64="$(base64 <"$2/pk.ssh" | tr -d '\n')"
	KEY_ID="$("$OPENSSL" dgst -sha256 -binary "$2/spki.der" | od -An -tx1 | tr -d ' \n' | cut -c1-16)"
}

# Ed25519-signs file $1 into $2 with the private key $SIGNING_KEY. $SIGNING_PASS, when set, is the
# key's passphrase; it goes to openssl on stdin, never on a command line.
ed25519_sign() {
	if [ -n "${SIGNING_PASS+set}" ]; then
		printf '%s\n' "$SIGNING_PASS" | "$OPENSSL" pkeyutl -sign -inkey "$SIGNING_KEY" -passin stdin -rawin -in "$1" -out "$2"
	else
		"$OPENSSL" pkeyutl -sign -inkey "$SIGNING_KEY" -rawin -in "$1" -out "$2"
	fi
}

# The public half of $SIGNING_KEY, DER, into $1.
signing_key_public() {
	if [ -n "${SIGNING_PASS+set}" ]; then
		printf '%s\n' "$SIGNING_PASS" | "$OPENSSL" pkey -in "$SIGNING_KEY" -passin stdin -pubout -outform der -out "$1"
	else
		"$OPENSSL" pkey -in "$SIGNING_KEY" -pubout -outform der -out "$1"
	fi
}

# Writes the OpenSSH signature of file $1 to $2, as `ssh-keygen -Y sign -n pai-release` would.
# $3 is a work directory holding pk.ssh from key_material. The format is PROTOCOL.sshsig in
# OpenSSH's source.
sshsig_sign() {
	"$OPENSSL" dgst -sha512 -binary "$1" >"$3/msg.sha512"
	{
		printf SSHSIG
		sstr "$SIGNING_NAMESPACE"
		sstr ""
		sstr sha512
		sfile "$3/msg.sha512"
	} >"$3/signed-data"
	ed25519_sign "$3/signed-data" "$3/sig.raw"
	{
		sstr ssh-ed25519
		sfile "$3/sig.raw"
	} >"$3/sig.ssh"
	{
		printf SSHSIG
		u32 1
		sfile "$3/pk.ssh"
		sstr "$SIGNING_NAMESPACE"
		sstr ""
		sstr sha512
		sfile "$3/sig.ssh"
	} >"$3/sshsig"
	{
		echo "-----BEGIN SSH SIGNATURE-----"
		base64 <"$3/sshsig" | tr -d '\n' | fold -w 70
		echo
		echo "-----END SSH SIGNATURE-----"
	} >"$2"
}
