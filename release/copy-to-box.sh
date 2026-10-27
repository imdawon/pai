#!/bin/sh
# Copies the pai release the site serves to the box, for `pai update`: the last step of
# release/publish.sh, for when the machine that signed the release can't reach the box.
# Downloads https://<domain>/pai/latest/, checks every file against its SHA256SUMS, and swaps it
# in on the box in one rename. Needs SSH to the box as its admin (PAI_BOX, PAI_BOX_KEY).
#
#   release/copy-to-box.sh
set -eu

domain="${PAI_DOMAIN:-ai.imdawon.com}"
box="${PAI_BOX:-supercomputer@100.126.243.95}"
box_key="${PAI_BOX_KEY:-$HOME/.ssh/id_ed25519}"
releases=/mnt/re/pai/releases
base="https://$domain/pai/latest"

fail() { echo "copy-to-box: $*" >&2; exit 1; }
box_opts=""
[ -f "$box_key" ] && box_opts="-i $box_key -o IdentitiesOnly=yes"
on_box() { ssh $box_opts "$box" "$@"; }

key_id="$(sed -n "s/^SIGNING_KEY_ID='\(.*\)'$/\1/p" "$(dirname "$0")/../install.sh")"
[ -n "$key_id" ] || fail "install.sh has no SIGNING_KEY_ID"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
cd "$tmp"

echo "== downloading $base/"
# This key's signatures (its ID is in install.sh), and the old key's while the key changes.
for f in VERSION SHA256SUMS "SHA256SUMS.$key_id.sig" "SHA256SUMS.$key_id.sshsig"; do
  curl -fsSLo "$f" "$base/$f" || fail "couldn't download $f"
done
for f in SHA256SUMS.sig SHA256SUMS.sshsig; do
  curl -fsSLo "$f" "$base/$f" 2>/dev/null || rm -f "$f"
done
version="$(cat VERSION)"
awk '{ print $2 }' SHA256SUMS | sed 's/^\*//' | while read -r f; do
  case "$f" in */* | .*) fail "SHA256SUMS names $f" ;; esac
  curl -fsSLo "$f" "$base/$f" || fail "couldn't download $f"
done
shasum -a 256 -c SHA256SUMS >/dev/null || fail "a download doesn't match SHA256SUMS"
echo "ok    pai $version, $(wc -l <SHA256SUMS | tr -d ' ') files match SHA256SUMS"

echo "== copying to the box"
on_box test -w "$releases" || fail "$box has no $releases it can write to"
on_box "rm -rf $releases/latest.new && mkdir $releases/latest.new"
scp $box_opts -q ./* "$box:$releases/latest.new/"
# One rename, so pai never sees half of one release and half of another.
on_box "cd $releases && rm -rf latest.old && { [ ! -e latest ] || mv latest latest.old; } && mv latest.new latest && rm -rf latest.old"
api_version="$(curl -fsS "https://api.$domain/pai/latest/VERSION" || true)"
[ "$api_version" = "$version" ] || fail "https://api.$domain/pai/latest/VERSION says '$api_version', not $version"
echo "ok    https://api.$domain/pai/latest/VERSION is $version"
