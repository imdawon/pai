#!/usr/bin/env bash
# Fails when CI, a build or a deploy pulls outside code by a name its owner can move: an action
# tag such as @v4, an image tag such as node:26, or a toolchain channel such as "stable". Each one
# must name an exact commit (40 hex characters) or image digest (@sha256:...), so the code that runs
# is the code we reviewed. See privacy-ai CLAUDE.md, "We own the stack", and docs/THREAT-MODEL.md,
# "Pinned CI actions, images and toolchains".
#
# The same file is in privacy-ai, pai, pai-relay and pai-gateway; keep the four copies identical.
# Plain bash, grep and git, so the check itself pulls in nothing.
#
#   tools/check-pins.sh              check every file git tracks in this repo
#   tools/check-pins.sh --self-test  check that the rules catch what they should
set -uo pipefail

registries='docker\.io|ghcr\.io|quay\.io|gcr\.io|mcr\.microsoft\.com|public\.ecr\.aws|registry\.k8s\.io|codeberg\.org|data\.forgejo\.org|code\.forgejo\.org'
sha='@sha256:[0-9a-f]{64}'
official='node|rust|alpine|debian|ubuntu|golang|python|busybox|buildpack-deps'
# An image tag starts with a version number or is a release name; node:fs is a Node.js import.
tags='[0-9][A-Za-z0-9._-]*|latest|lts|current|stable|edge|bookworm|trixie|bullseye|noble|jammy|alpine|slim'

# check_file <path> <name shown in messages>: prints one line per unpinned reference.
check_file() {
  local f="$1" name="$2" base n line ref
  base="$(basename "$name")"
  n=0
  while IFS= read -r line || [ -n "$line" ]; do
    n=$((n + 1))
    # Comments don't run. A pinned line may keep its version as a trailing comment.
    case "$line" in [[:space:]]\#* | \#*) continue ;; esac

    # Workflow steps: uses: owner/repo@<40 hex> or ./local-action; docker:// needs a digest.
    if [[ "$line" =~ ^[[:space:]-]*uses:[[:space:]]*[\"\']?([^[:space:]\"\'#]+) ]]; then
      ref="${BASH_REMATCH[1]}"
      if [[ "$ref" == docker://* ]]; then
        [[ "$ref" =~ $sha$ ]] || echo "$name:$n: action image without a digest: $ref"
      elif [[ "$ref" != ./* ]] && ! [[ "$ref" =~ @[0-9a-f]{40}$ ]]; then
        echo "$name:$n: action not pinned to a full commit SHA: $ref"
      fi
    fi

    # Images named in a Containerfile, a Quadlet unit or a runner label.
    if [[ "$base" =~ ^(Containerfile|Dockerfile) ]] && [[ "$line" =~ ^[[:space:]]*FROM[[:space:]]+(--[^[:space:]]+[[:space:]]+)*([^[:space:]]+) ]]; then
      ref="${BASH_REMATCH[2]}"
      # scratch is empty, and a lower-case word with no / : or . names an earlier build stage.
      if [[ "$ref" != scratch && "$ref" != \$* ]] && ! [[ "$ref" =~ ^[a-z0-9_-]+$ ]] && ! [[ "$ref" =~ $sha$ ]]; then
        echo "$name:$n: FROM image without a digest: $ref"
      fi
    fi
    if [[ "$line" =~ ^[[:space:]]*Image=([^[:space:]]+) ]]; then
      ref="${BASH_REMATCH[1]}"
      # localhost/ is an image we built from this repo; $image is checked where it's set.
      if [[ "$ref" != localhost/* && "$ref" != \$* ]] && ! [[ "$ref" =~ $sha$ ]]; then
        echo "$name:$n: Image= without a digest: $ref"
      fi
    fi
    rest="$line"
    while [[ "$rest" =~ docker://([^[:space:],\"\']+)(.*) ]]; do
      ref="${BASH_REMATCH[1]}"; rest="${BASH_REMATCH[2]}"
      [[ "$ref" =~ $sha$ ]] || echo "$name:$n: docker:// image without a digest: $ref"
    done

    # Any image written with its registry's name, in a script or anywhere else, needs a digest.
    rest="$line"
    while [[ "$rest" =~ (^|[^/A-Za-z0-9.-])(($registries)/[a-z0-9._/-]+(:[A-Za-z0-9._-]+)?(@sha256:[0-9a-f]*)?)(.*) ]]; do
      ref="${BASH_REMATCH[2]}"; rest="${BASH_REMATCH[6]}"
      # A bare repository path with no tag and no digest is a web link or a mirror name, not a pull.
      if [[ -n "${BASH_REMATCH[4]}" || -n "${BASH_REMATCH[5]}" ]] && ! [[ "$ref" =~ $sha$ ]]; then
        echo "$name:$n: image without a digest: $ref"
      fi
    done

    # Docker Hub's own images written by their short name, as in docker run ... rust:1-alpine.
    rest="$line"
    while [[ "$rest" =~ (^|[[:space:]\"\'=])((library/)?($official):($tags)(@sha256:[0-9a-f]*)?)([^A-Za-z0-9._-].*|$) ]]; do
      ref="${BASH_REMATCH[2]}"; rest="${BASH_REMATCH[7]}"
      [[ "$ref" =~ $sha$ ]] || echo "$name:$n: image without a digest: $ref"
    done

    # Toolchains: an exact version, never a channel that moves.
    if [[ "$line" =~ node-version:[[:space:]]*[\"\']?([^[:space:]\"\'#]+) ]]; then
      ref="${BASH_REMATCH[1]}"
      [[ "$ref" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || echo "$name:$n: node-version isn't an exact version: $ref"
    fi
    if [[ "$line" =~ rustup ]] && [[ "$line" =~ (toolchain[[:space:]]+install|default|--default-toolchain[\",[:space:]=]+)[[:space:]]*[\"\']?(stable|beta|nightly|lts|latest)([^-A-Za-z0-9]|$) ]]; then
      echo "$name:$n: rustup installs the moving \"${BASH_REMATCH[2]}\" channel"
    fi
  done <"$f"
}

self_test() {
  local dir bad good out fails=0
  dir="$(mktemp -d)"; trap 'rm -rf "$dir"' RETURN
  mkdir -p "$dir/bad" "$dir/good"
  cat >"$dir/bad/ci.yml" <<'EOF'
      - uses: dorny/paths-filter@v3
      - uses: actions/checkout@v4.2.2
      - uses: "peaceiris/actions-gh-pages@main"
      - uses: docker://docker.io/library/alpine:3.22
          node-version: 22
        run: rustup toolchain install stable --profile minimal
EOF
  cat >"$dir/bad/Containerfile" <<'EOF'
FROM docker.io/library/alpine:3.22
FROM --platform=linux/amd64 rust:1-alpine
EOF
  cat >"$dir/bad/x.container" <<'EOF'
Image=docker.io/victoriametrics/vmalert:v1.0
EOF
  cat >"$dir/bad/setup.sh" <<'EOF'
image="codeberg.org/forgejo/forgejo:17-rootless"
labels="ubuntu-latest:docker://node:20-bookworm,rust:docker://rust:1-bookworm@sha256:abc"
  - [runuser, -u, build, --, /tmp/rustup-init, -y, --default-toolchain, stable, --target, x]
  docker.io/library/rust:1-alpine \
RUST_ALPINE="rust:1.95-alpine"
  node:20-bookworm \
EOF
  cat >"$dir/good/ci.yml" <<'EOF'
      - uses: actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683 # v4.2.2
      - uses: ./local/action
          node-version: 26.13.0
        run: rustup toolchain install 1.99.1 --profile minimal && rustup default 1.99.1
      # - uses: dorny/paths-filter@v3
EOF
  cat >"$dir/good/Containerfile" <<'EOF'
FROM docker.io/library/alpine:3.22.8@sha256:e1011cfb1c3f0d0a3a06d5371ef866b5713e035a866d84a419e0e3d73ca0dfd8 AS build
FROM build
FROM scratch
EOF
  cat >"$dir/good/x.container" <<'EOF'
Image=localhost/pai-gateway:latest
Image=$image
Image=docker.io/victoriametrics/vmalert@sha256:0c1c7718b54b03e8559d0f257f31f1bd79ef7fa32ac6e9ed90d7b92a74a4fe76
EOF
  cat >"$dir/good/setup.sh" <<'EOF'
image="codeberg.org/forgejo/forgejo:17.0.1-rootless@sha256:5ef02fbcf22beef56b8c844952ffad58cfe177b6f98e46e6abb5df90b88dc8d3"
labels="ubuntu-latest:docker://docker.io/library/node:26.13.0-bookworm@sha256:a546d3f8acd2af4382668a7e6acd3bc4418e6a27478373e3246f8162f5181509"
curl -fsS https://codeberg.org/forgejo/forgejo/releases https://data.forgejo.org/v2/
# Forgejo pull-mirrors github.com/imdawon/pai; ALLOWED_DOMAINS=github.com
  - [runuser, -u, build, --, /tmp/rustup-init, -y, --default-toolchain, 1.99.1, --target, x]
RUST_ALPINE="docker.io/library/rust:1.95-alpine@sha256:606fd313a0f49743ee2a7bd49a0914bab7deedb12791f3a846a34a4711db7ed2"
echo "node: $(node --version)"; cargo build --target x86_64-unknown-linux-musl
EOF
  for f in "$dir"/bad/*; do
    want="$(grep -cvE '^(# |$)' "$f")"
    got="$(check_file "$f" "bad/$(basename "$f")" | awk -F: '!seen[$1 FS $2]++' | wc -l)"
    [ "$got" = "$want" ] || { echo "self-test: bad/$(basename "$f"): caught $got of $want" >&2; check_file "$f" "bad/$(basename "$f")" >&2; fails=1; }
  done
  for f in "$dir"/good/*; do
    out="$(check_file "$f" "good/$(basename "$f")")"
    [ -z "$out" ] || { echo "self-test: flagged a pinned line:" >&2; echo "$out" >&2; fails=1; }
  done
  [ "$fails" = 0 ] && echo "check-pins self-test passed"
  return "$fails"
}

if [ "${1:-}" = --self-test ]; then self_test; exit; fi

root="$(git rev-parse --show-toplevel)" || exit 2
cd "$root" || exit 2
found="$(
  git ls-files -z | while IFS= read -r -d '' f; do
    # Prose and data can mention a moving name; what runs is code, units, workflows and images.
    case "$f" in
      *.md | *.html | *.txt | *.json | *.jsonl | *.csv | *.svg | *.lock | *.sum | */vectors/* | tools/check-pins.sh) continue ;;
    esac
    [ -f "$f" ] && grep -Iq . "$f" 2>/dev/null || continue
    check_file "$f" "$f" | awk -F: '!seen[$1 FS $2]++'
  done
)"
if [ -n "$found" ]; then
  printf '%s\n' "$found"
  echo "check-pins: pin each line above to a full commit SHA or an @sha256: digest (tools/check-pins.sh)" >&2
  exit 1
fi
echo "check-pins: every action, image and toolchain is pinned"
