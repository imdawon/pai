#!/bin/sh
# Builds and signs a pai release with release/build.sh, then publishes it on the DawonAI site
# (privacy-ai, Cloudflare Pages) at https://<domain>/pai/latest/, and checks what the site serves.
#
#   PAI_SIGNING_KEY=/Volumes/<drive>/pai-release.key release/publish.sh
#
# The API key pins: the current one is the key api.<domain> serves, which must match
# release/current-api-pin (the box's TLS key belongs to its service users, so nothing here can read
# it); the spare one is release/spare-api-pin (its private key stays offline, see
# pai-gateway/deploy/CERTIFICATES.md). Both files are public.
# Then copies it to the box, which serves it at https://api.<domain>/pai/latest/ for `pai update`.
# Needs ../privacy-ai, node, wrangler signed in to the Cloudflare account with the Pages project, and
# SSH to the box as its admin (PAI_BOX, PAI_BOX_KEY, as pai-gateway/deploy/deploy.sh uses).
set -eu

cd "$(dirname "$0")/.."
root="$(pwd)"
site="$(cd "$root/../privacy-ai" && pwd)"
domain="${PAI_DOMAIN:-ai.imdawon.com}"
project="${PAI_PAGES_PROJECT:-dawonai}"

export PAI_RELEASE_DOMAIN="$domain"

fail() { echo "publish: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }

: "${PAI_SIGNING_KEY:?set PAI_SIGNING_KEY to the release key on its drive (release/SIGNING.md)}"
command -v node >/dev/null || fail "needs node"
command -v wrangler >/dev/null || fail "needs wrangler (npm install -g wrangler, then wrangler login)"

step "API key pins"
current="$(tr -d ' \n' <release/current-api-pin 2>/dev/null || true)"
spare="$(tr -d ' \n' <release/spare-api-pin 2>/dev/null || true)"
pin_ok() { printf '%s' "$1" | grep -Eq '^[A-Za-z0-9+/]{43}=$'; }
pin_ok "$current" || fail "release/current-api-pin holds no pin"
pin_ok "$spare" || fail "release/spare-api-pin holds no pin (pai-gateway/deploy/CERTIFICATES.md, step 2)"
[ "$current" != "$spare" ] || fail "the spare pin is the box's current key; it must be a different key"
served="$(echo | openssl s_client -connect "api.$domain:443" -servername "api.$domain" 2>/dev/null |
	openssl x509 -pubkey -noout | openssl pkey -pubin -outform der | openssl dgst -sha256 -binary | base64)"
[ "$served" = "$current" ] || fail "api.$domain serves $served, not release/current-api-pin ($current). If the box's key changed on purpose, put the new pin in release/current-api-pin; otherwise something else is answering for api.$domain. Not building."
echo "current $current"
echo "spare   $spare"

PAI_RELEASE_API_PINS="$current,$spare" sh release/build.sh

version="$(awk -F'"' '/^version *=/ { print $2; exit }' Cargo.toml)"
dist="$root/dist"
key_id="$(sed -n "s/^SIGNING_KEY_ID='\(.*\)'$/\1/p" install.sh)"
[ -f "$dist/latest/SHA256SUMS.$key_id.sig" ] || fail "release/build.sh left no release in dist/latest signed with key $key_id"
# Every signature file: this key's, and while the key changes, the old key's.
sigs="$(cd "$dist/latest" && ls SHA256SUMS*.sig SHA256SUMS*.sshsig)"

step "site for $domain"
(cd "$site" && node tools/build.mjs "$domain")
sh release/site-files.sh "$site/dist/site"

step "deploying to Cloudflare Pages ($project)"
wrangler pages deploy "$site/dist/site" --project-name "$project" --branch main --commit-dirty=true

step "checking https://$domain/pai/latest/"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
# Pages can take a minute to serve a new deployment, so this tries for a few minutes before failing.
# Failing here stops the script before the box gets the release, so no installed pai updates.
for attempt in 1 2 3 4 5 6 7 8; do
	ok=1
	for f in $(awk '{ print $2 }' "$dist/latest/SHA256SUMS") SHA256SUMS $sigs; do
		f="${f#\*}"
		if curl -fsS "https://$domain/pai/latest/$f" -o "$tmp/$f" 2>/dev/null && cmp -s "$tmp/$f" "$dist/latest/$f"; then
			[ "$attempt" -gt 1 ] || echo "ok    $f"
		else
			[ "$attempt" -gt 1 ] || echo "wait  $f"
			ok=0
		fi
	done
	[ "$ok" = 1 ] && break
	echo "the site doesn't serve all of this release yet; checking again in 20 seconds ($attempt of 8)"
	sleep 20
done
[ "$ok" = 1 ] || fail "after about 3 minutes the site still doesn't serve this release. Check the Pages deployment, then run release/publish.sh again"
step "copying to the box, for pai update"
box="${PAI_BOX:-supercomputer@100.126.243.95}"
box_key="${PAI_BOX_KEY:-$HOME/.ssh/id_ed25519}"
releases=/mnt/re/pai/releases
# Without that key file, ssh uses your SSH config and agent.
box_opts=""
[ -f "$box_key" ] && box_opts="-i $box_key -o IdentitiesOnly=yes"
on_box() { ssh $box_opts "$box" "$@"; }
on_box test -w "$releases" || fail "$box has no $releases it can write to. Deploy the gateway first (pai-gateway/deploy/deploy.sh), which makes it"
on_box "rm -rf $releases/latest.new && mkdir $releases/latest.new"
scp $box_opts -q "$dist/latest"/* "$box:$releases/latest.new/"
on_box "rm -f $releases/latest.new/index.html"
# One rename, so pai never sees half of one release and half of another.
on_box "cd $releases && rm -rf latest.old && { [ ! -e latest ] || mv latest latest.old; } && mv latest.new latest && rm -rf latest.old"
api_version="$(curl -fsS "https://api.$domain/pai/latest/VERSION" || true)"
[ "$api_version" = "$version" ] || fail "https://api.$domain/pai/latest/VERSION says '$api_version', not $version. Is the gateway deployed with PAI_RELEASES_DIR?"
echo "ok    https://api.$domain/pai/latest/VERSION"

echo
echo "pai $version is live at https://$domain/pai/latest/, and installed copies update to it within a day"
