# Signing pai releases

A pai release carries two kinds of signature, or will:

- **The release signature.** An Ed25519 signature over `SHA256SUMS`, made with a key kept offline. The installers check it before they install anything. `release/build.sh` makes it, and won't build without the key.
- **Publisher identity.** Apple's Developer ID with notarization, and Windows Authenticode. These stop Gatekeeper and SmartScreen warnings for people who download a binary by hand. They need paid accounts, and pai doesn't have them yet.

## The release signature

`SHA256SUMS` lists the SHA-256 of every file in a release. Before this signature, it came from the same web server as the binaries, so whoever could change a binary on that server could change its checksum too. The signature is made with a private key that never touches the server, so a changed binary or a changed `SHA256SUMS` fails the check.

Each release has two signature files, made with the same key:

| File | Holds | Checked by |
|---|---|---|
| `SHA256SUMS.sig` | The 64-byte Ed25519 signature of `SHA256SUMS` | `openssl pkeyutl -verify -rawin`, OpenSSL 3 or later |
| `SHA256SUMS.sshsig` | The same key's signature in OpenSSH's format, namespace `pai-release` | `ssh-keygen -Y verify`, OpenSSH 8.1 or later |

There are two because no one tool that checks Ed25519 ships on every OS. `release/signing.sh` makes both with OpenSSL alone.

**What the signature doesn't cover.** The one-line install downloads `install.sh` or `install.ps1` from the same server as the binaries, and the installer carries the key. Whoever controls that server can serve an installer with their own key. The signature holds against them for anyone who compares the key in the installer with the one in pai's README on GitHub, or who downloads the installer from GitHub. And a signed `SHA256SUMS` doesn't say which version is newest, so a server could serve an older signed release.

## What can check the signature

| Platform | Tool the installer uses | Ships with the OS |
|---|---|---|
| macOS | `ssh-keygen` from OpenSSH. `/usr/bin/openssl` is LibreSSL (3.3.6 on macOS 27), which has no Ed25519. | Yes |
| Linux | `openssl` 3, or `ssh-keygen` from OpenSSH 8.1 or later | OpenSSL 3 in Ubuntu 22.04, Debian 12, Fedora 36, RHEL 9 and later. OpenSSH 8.1 or later in Ubuntu 20.04 (8.2) and Debian 11 (8.4), where it's installed. RHEL 8 has OpenSSL 1.1.1 and OpenSSH 8.0, so neither. Minimal containers, such as Alpine, often have neither. |
| Windows | `ssh-keygen` from the Windows OpenSSH client, 8.1 or later; or OpenSSL 3, such as the one Git for Windows carries | The OpenSSH client is an optional feature, in **Settings > System > Optional features**. Microsoft doesn't publish which OpenSSH version each Windows build carries; `ssh -V` shows it. Windows PowerShell 5.1 runs on .NET Framework 4, which has no Ed25519. |

Where neither tool is there, the installers warn and install with only the checksum check. Checking is up to the user. A signature that doesn't match always stops them.

## Making the key

Do this once, on a computer that's not connected to a network, with OpenSSL 3:

```sh
openssl genpkey -algorithm ed25519 -aes-256-cbc -out pai-release.key
openssl pkey -in pai-release.key -pubout -out signing-key.pub
```

The first command asks for a passphrase, which encrypts the key file. Keep the passphrase in a password manager.

Keep `pai-release.key` on encrypted removable storage, with a second copy in a different place. It never goes on a server, in a repository, in CI, or on the build Mac's own disk.

`signing-key.pub` is public. Copy it to `release/signing-key.pub` in place of the placeholder, then run:

```sh
sh release/embed-key.sh
```

That writes the key into `install.sh`, `install.ps1` and `README.md`. Commit the four files together. `release/build.sh` checks that all of them carry the key in `release/signing-key.pub`.

## Publishing the public key

The key only helps if people can get it from somewhere other than the server it protects. Publish it in all of these, so that changing it means changing every one:

- **The README on GitHub.** `release/embed-key.sh` writes it there; push the commit.
- **The installers.** `release/embed-key.sh` writes it into both.
- **The release page.** `release/index.sh` shows `release/signing-key.pub` on `https://<domain>/pai/`.
- **The DawonAI site.** Add it to the site's pai page, in the privacy-ai repo.
- **A GitHub release** for each version, with the key in its notes.

## Signing a release

Plug in the key's storage, then:

```sh
PAI_SIGNING_KEY=/Volumes/<drive>/pai-release.key release/publish.sh
```

`release/publish.sh` checks that `api.<domain>` serves the key in `release/current-api-pin`, takes the spare pin from `release/spare-api-pin`, runs `release/build.sh`, then rebuilds the DawonAI site with the release in `/pai/latest/`, deploys it to Cloudflare Pages and checks every file the site serves against the build. To build without publishing:

```sh
PAI_SIGNING_KEY=/Volumes/<drive>/pai-release.key PAI_RELEASE_API_PINS=<current>,<spare> release/build.sh
```

`release/build.sh` needs OpenSSL 3 (`brew install openssl@3`; it finds Homebrew's, or set `PAI_OPENSSL`). Before building, it asks for the passphrase once, and checks that the key is the private half of `release/signing-key.pub` and that the installers and README carry the same key. After building, it signs `SHA256SUMS`, checks both signatures the way the installers will, and only then copies the release to `dist/latest/`.

Remove the storage when the build is done. The build Mac is online, so the key is exposed on it while it's plugged in. Building on the Mac and signing `SHA256SUMS` on the offline computer would keep it off every networked computer; `release/build.sh` doesn't split the two yet.

## Checking a release by hand

With OpenSSL 3, and the key saved as `pai-release.pub`:

```sh
openssl pkeyutl -verify -pubin -inkey pai-release.pub -rawin -in SHA256SUMS -sigfile SHA256SUMS.sig
```

With OpenSSH, and the allowed-signers line from the README saved as `allowed_signers`:

```sh
ssh-keygen -Y verify -f allowed_signers -I pai-release -n pai-release -s SHA256SUMS.sshsig < SHA256SUMS
```

Then check the file against `SHA256SUMS`: `shasum -a 256 -c SHA256SUMS --ignore-missing` (macOS), `sha256sum -c SHA256SUMS --ignore-missing` (Linux), or compare `Get-FileHash <file>` by eye (Windows).

## Losing or leaking the key

There's no way to revoke a key. If the key leaks, or both copies are lost, make a new one, run `release/embed-key.sh`, publish it everywhere above, and say on the site and GitHub that the old key is retired. Each install downloads a fresh installer, so new installs use the new key at once.

## Publisher identity: macOS

The release binaries carry no publisher identity yet. Every arm64 binary must carry a signature to run at all; the Rust linker adds an ad-hoc one (`codesign -dv` shows `adhoc,linker-signed`), which is enough to run but not for Gatekeeper to trust a downloaded file.

`install.sh` downloads with curl or wget, which don't set the `com.apple.quarantine` mark, so Gatekeeper doesn't check what it installs. A binary downloaded in a browser, AirDrop or a chat app does get the mark, and Gatekeeper blocks it.

To pass Gatekeeper:

1. **Apple Developer Program membership** ($99 a year) and a **Developer ID Application** certificate, made in the developer portal and kept in the login keychain of the Mac that signs.
2. **Sign** each macOS binary with the hardened runtime and a secure timestamp:

   ```sh
   codesign --force --options runtime --timestamp \
     --sign "Developer ID Application: <Name> (<TEAMID>)" dist/<version>/pai-aarch64-apple-darwin
   ```

   Do the same for `pai-x86_64-apple-darwin`. `pai` needs no entitlements.
3. **Notarize** with `notarytool`. It takes a zip, pkg or dmg, not a bare binary:

   ```sh
   xcrun notarytool store-credentials pai-notary --apple-id <id> --team-id <TEAMID>   # once; uses an app-specific password
   ditto -c -k dist/<version>/pai-aarch64-apple-darwin /tmp/pai-arm64.zip
   xcrun notarytool submit /tmp/pai-arm64.zip --keychain-profile pai-notary --wait
   ```

   A bare binary can't have its ticket stapled, so Gatekeeper looks the ticket up online the first time it runs. To ship a stapled file, wrap the binary in a signed `.pkg` or `.dmg`, notarize that, and `xcrun stapler staple` it.
4. **Do it before the checksums.** Signing changes the file, so it belongs in `release/build.sh` after the macOS binaries are built and before `SHA256SUMS` is written and signed.

## Publisher identity: Windows

`install.ps1` downloads with `Invoke-WebRequest`, which doesn't add a Mark of the Web, so SmartScreen doesn't check what it installs; Defender can still scan it. A binary downloaded in a browser gets the mark, and SmartScreen warns.

1. **A code signing certificate** from a CA (OV or EV). Since June 2023 the private key has to live in a hardware module, so in practice this is a cloud signing service such as Azure Artifact Signing (formerly Trusted Signing), DigiCert KeyLocker or SSL.com eSigner, or a USB token. Each is a paid subscription.
2. **Sign** both `.exe` files with a timestamp, so the signature outlives the certificate:

   ```powershell
   signtool sign /fd sha256 /tr http://timestamp.digicert.com /td sha256 /a pai-x86_64-pc-windows-msvc.exe
   ```

   From macOS or Linux, `osslsigncode` or the signing service's own client does the same.
3. **Do it before the checksums**, for the same reason as on macOS.

A signature alone doesn't silence SmartScreen. SmartScreen builds reputation per certificate and file as people download them, and an EV certificate no longer skips that. Early downloads may still show the warning.

## Until then, for a binary downloaded by hand

- **Check the signature and the hash first**, as in "Checking a release by hand".
- **macOS**: remove the quarantine mark, then run it:

  ```sh
  xattr -d com.apple.quarantine ./pai-aarch64-apple-darwin
  chmod +x ./pai-aarch64-apple-darwin
  ```

  Or try to run it once, then choose **Open Anyway** in **System Settings > Privacy & Security**. Since macOS 15, Control-clicking the file no longer offers to open it.
- **Windows**: `Unblock-File .\pai-x86_64-pc-windows-msvc.exe` removes the Mark of the Web. Or, at the SmartScreen prompt, choose **More info**, then **Run anyway**.
