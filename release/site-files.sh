#!/bin/sh
# Copies the release in dist/ into a site build at SITE/pai/: this version's folder, latest/, and
# the release page as latest/index.html. Cloudflare Pages replaces the whole site on each deploy,
# so every site deploy needs these, not only release/publish.sh's.
#
#   release/site-files.sh <privacy-ai>/dist/site
set -eu

cd "$(dirname "$0")/.."
site="${1:?usage: release/site-files.sh SITE_DIST}"
dist="$(pwd)/dist"
key_id="$(sed -n "s/^SIGNING_KEY_ID='\(.*\)'$/\1/p" install.sh)"
[ -f "$dist/latest/SHA256SUMS.$key_id.sig" ] || { echo "site-files: no release in $dist/latest signed with key $key_id" >&2; exit 1; }
version="$(cat "$dist/latest/VERSION")"
[ -d "$dist/$version" ] || { echo "site-files: no $dist/$version" >&2; exit 1; }
[ -d "$site/pai" ] || { echo "site-files: $site/pai is missing; build the site first" >&2; exit 1; }

rm -rf "$site/pai/latest" "$site/pai/$version"
cp -R "$dist/$version" "$dist/latest" "$site/pai/"
# The release page (release/index.sh) goes in latest/, since the site has its own page at /pai/.
# Its links are written for /pai/, so they're rewritten for one folder down.
sed -e 's#href="latest/#href="#g' -e 's#href="\([0-9][^"/]*\)/#href="../\1/#g' -e 's#href="install\.#href="../install.#g' \
	"$dist/index.html" >"$site/pai/latest/index.html"
