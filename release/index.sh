#!/bin/sh
# Writes dist/index.html for the release in dist/<version>/. build.sh runs it; run it alone to
# redo the page without rebuilding.
set -eu

cd "$(dirname "$0")/.."
version="${1:?usage: release/index.sh <version>}"
SITE="${PAI_SITE:-https://privacyai.example}"
SITE="${SITE%/}"
dist="$(pwd)/dist"
out="$dist/$version"
[ -f "$out/SHA256SUMS" ] || { echo "index: no $out/SHA256SUMS; run release/build.sh" >&2; exit 1; }

size() {
	n="$(stat -f %z "$1")"
	awk -v n="$n" 'BEGIN { if (n >= 1048576) printf "%.1f MB", n / 1048576; else printf "%.0f KB", n / 1024 }'
}
describe() {
	case "$1" in
	pai-aarch64-apple-darwin) echo "macOS, Apple silicon" ;;
	pai-x86_64-apple-darwin) echo "macOS, Intel" ;;
	pai-aarch64-unknown-linux-musl) echo "Linux, ARM64 (static)" ;;
	pai-x86_64-unknown-linux-musl) echo "Linux, x86-64 (static)" ;;
	pai-x86_64-pc-windows-msvc.exe) echo "Windows, x86-64" ;;
	pai-aarch64-pc-windows-msvc.exe) echo "Windows, ARM64" ;;
	privacy-ai-skill.zip) echo "Agent skill: unzip into your tool's skills folder" ;;
	opencode-privacy-ai.ts) echo "OpenCode plugin: copy to ~/.config/opencode/plugins/privacy-ai.ts" ;;
	omp-privacy-ai.ts) echo "OMP extension: copy to ~/.omp/agent/extensions/privacy-ai.ts" ;;
	SHA256SUMS) echo "Checksums for every file above" ;;
	*) echo "" ;;
	esac
}
rows=""
for f in pai-aarch64-apple-darwin pai-x86_64-apple-darwin pai-x86_64-unknown-linux-musl \
	pai-aarch64-unknown-linux-musl pai-x86_64-pc-windows-msvc.exe pai-aarch64-pc-windows-msvc.exe \
	privacy-ai-skill.zip opencode-privacy-ai.ts omp-privacy-ai.ts SHA256SUMS; do
	hash="$(awk -v f="$f" '$2 == f { print $1 }' "$out/SHA256SUMS")"
	rows="$rows
      <tr><td><a href=\"latest/$f\">$f</a><span>$(describe "$f")</span></td><td>$(size "$out/$f")</td><td><code>${hash:-}</code></td></tr>"
done
older=""
for d in $(cd "$dist" && ls -d */ 2>/dev/null | tr -d / | grep -v '^latest$' | sort -r); do
	older="$older <a href=\"$d/SHA256SUMS\">$d</a>"
done

cat >"$dist/index.html" <<EOF
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>pai $version · Privacy AI</title>
<style>
  :root { --brand: #1f5fa6; --text: #1f2033; --dim: #5d5f76; --faint: #8a8ca3; --line: #e3e3ee; --band: #f5f5fa; --page: #fff;
    --sans: -apple-system, BlinkMacSystemFont, "SF Pro Display", "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;
    --mono: ui-monospace, "SF Mono", Menlo, Consolas, monospace; color-scheme: light; }
  * { box-sizing: border-box; }
  body { margin: 0; background: var(--page); color: var(--text); font-family: var(--sans); font-size: 17px; line-height: 1.6; }
  .wrap { max-width: 60rem; margin: 0 auto; padding: 64px 24px 80px; display: grid; gap: 40px; }
  .wrap > * { min-width: 0; }
  h1 { margin: 0; font-size: clamp(32px, 4.4vw, 46px); font-weight: 300; letter-spacing: -0.025em; line-height: 1.1; }
  h2 { margin: 0 0 12px; font-size: 24px; font-weight: 300; letter-spacing: -0.015em; }
  p { margin: 0; color: var(--dim); }
  a { color: var(--brand); }
  pre { margin: 6px 0 16px; background: var(--band); border: 1px solid var(--line); border-radius: 10px; padding: 12px 14px; overflow-x: auto; }
  code { font-family: var(--mono); font-size: 13px; }
  table { width: 100%; border-collapse: collapse; table-layout: fixed; }
  td:first-child { width: 40%; overflow-wrap: anywhere; }
  td:nth-child(2) { width: 6.5rem; }
  td { border-top: 1px solid var(--line); padding: 12px 12px 12px 0; vertical-align: top; }
  td:nth-child(2) { white-space: nowrap; color: var(--dim); font-size: 15px; }
  td:nth-child(3) code { font-size: 11.5px; color: var(--dim); word-break: break-all; }
  td span { display: block; color: var(--faint); font-size: 14px; }
  .note { font-size: 15px; }
  @media (max-width: 640px) { .wrap { padding: 40px 16px 64px; } td:first-child { width: auto; } td:nth-child(3) { display: none; } }
</style>
</head>
<body>
<main class="wrap">
  <header>
    <h1>pai $version</h1>
    <p>The Privacy AI command-line helper. Coding tools run <code>pai token</code> for a day credential instead of storing an API key.</p>
  </header>

  <section>
    <h2>Install</h2>
    <p>macOS and Linux:</p>
    <pre><code>curl -fsSL $SITE/pai/install.sh -o /tmp/pai-install.sh &amp;&amp; sh /tmp/pai-install.sh</code></pre>
    <p>Windows (PowerShell):</p>
    <pre><code>irm $SITE/pai/install.ps1 -OutFile \$env:TEMP\\pai-install.ps1; powershell -ExecutionPolicy Bypass -File \$env:TEMP\\pai-install.ps1</code></pre>
    <p class="note">The installers download from <a href="latest/">latest/</a>, check the file against <code>SHA256SUMS</code>, and refuse to install on a mismatch. Read them first: <a href="install.sh">install.sh</a>, <a href="install.ps1">install.ps1</a>.</p>
  </section>

  <section>
    <h2>Files</h2>
    <table>$rows
    </table>
    <p class="note">Check a download yourself with <code>shasum -a 256 &lt;file&gt;</code> (macOS), <code>sha256sum &lt;file&gt;</code> (Linux) or <code>Get-FileHash &lt;file&gt;</code> (Windows). Each version stays at <code>/pai/&lt;version&gt;/</code>; checksums by version:$older.</p>
  </section>
</main>
</body>
</html>
EOF
