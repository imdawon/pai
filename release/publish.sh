#!/bin/sh
# Builds and signs a pai release with release/build.sh, then publishes it on the DawonAI site
# (privacy-ai, Cloudflare Pages) at https://<domain>/pai/latest/, and checks what the site serves.
#
#   PAI_SIGNING_KEY=/Volumes/<drive>/pai-release.key release/publish.sh
#
# The API key pins: the current one is read from the box's TLS key over SSH, the spare one from
# release/spare-api-pin (public; its private key stays offline, see pai-gateway/deploy/CERTIFICATES.md).
# Needs ../privacy-ai, node, and wrangler signed in to the Cloudflare account with the Pages project.
set -eu

cd "$(dirname "$0")/.."
root="$(pwd)"
site="$(cd "$root/../privacy-ai" && pwd)"
domain="${PAI_DOMAIN:-ai.imdawon.com}"
project="${PAI_PAGES_PROJECT:-dawonai}"
box="${PAI_BOX:-supercomputer@100.126.243.95}"
box_key="${PAI_BOX_KEY:-$HOME/.ssh/id_supercomputer}"

fail() { echo "publish: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }

: "${PAI_SIGNING_KEY:?set PAI_SIGNING_KEY to the release key on its drive (release/SIGNING.md)}"
command -v node >/dev/null || fail "needs node"
command -v wrangler >/dev/null || fail "needs wrangler (npm install -g wrangler, then wrangler login)"

step "API key pins"
current="$(ssh -i "$box_key" -o IdentitiesOnly=yes "$box" \
	'openssl pkey -in /mnt/re/pai/tls/key.pem -pubout -outform der | openssl dgst -sha256 -binary | base64')"
spare="$(tr -d ' \n' <release/spare-api-pin 2>/dev/null || true)"
pin_ok() { printf '%s' "$1" | grep -Eq '^[A-Za-z0-9+/]{43}=$'; }
pin_ok "$current" || fail "couldn't read the box's key pin over SSH"
pin_ok "$spare" || fail "release/spare-api-pin holds no pin (pai-gateway/deploy/CERTIFICATES.md, step 2)"
[ "$current" != "$spare" ] || fail "the spare pin is the box's current key; it must be a different key"
served="$(echo | openssl s_client -connect "api.$domain:443" -servername "api.$domain" 2>/dev/null |
	openssl x509 -pubkey -noout | openssl pkey -pubin -outform der | openssl dgst -sha256 -binary | base64)"
[ "$served" = "$current" ] || fail "api.$domain serves a key other than the box's ($served); not building"
echo "current $current"
echo "spare   $spare"

PAI_RELEASE_API_PINS="$current,$spare" sh release/build.sh

version="$(awk -F'"' '/^version *=/ { print $2; exit }' Cargo.toml)"
dist="$root/dist"
[ -f "$dist/latest/SHA256SUMS.sig" ] || fail "release/build.sh left no signed release in dist/latest"

step "site for $domain"
(cd "$site" && node tools/build.mjs "$domain")
cp -R "$dist/$version" "$dist/latest" "$site/dist/site/pai/"

step "deploying to Cloudflare Pages ($project)"
wrangler pages deploy "$site/dist/site" --project-name "$project" --branch main --commit-dirty=true

step "checking https://$domain/pai/latest/"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
ok=1
for f in $(awk '{ print $2 }' "$dist/latest/SHA256SUMS") SHA256SUMS SHA256SUMS.sig SHA256SUMS.sshsig; do
	f="${f#\*}"
	if curl -fsS "https://$domain/pai/latest/$f" -o "$tmp/$f" && cmp -s "$tmp/$f" "$dist/latest/$f"; then
		echo "ok    $f"
	else
		echo "FAIL  $f"
		ok=0
	fi
done
[ "$ok" = 1 ] || fail "the site doesn't serve this release yet; Pages can take a minute, then run the check again"
echo
echo "pai $version is live at https://$domain/pai/latest/"
