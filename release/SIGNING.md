# Signing pai releases

A pai release carries two kinds of signature, or will:

- **The release signature.** An Ed25519 signature over `SHA256SUMS`, made with a key kept offline. The installers check it before they install anything. `release/build.sh` makes it, and won't build without the key.
- **Publisher identity.** Apple's Developer ID with notarization, and Windows Authenticode. These stop Gatekeeper and SmartScreen warnings for people who download a binary by hand. They need paid accounts, and pai doesn't have them yet.

## The release signature

`SHA256SUMS` lists the SHA-256 of every file in a release. Before this signature, it came from the same web server as the binaries, so whoever could change a binary on that server could change its checksum too. The signature is made with a private key that never touches the server, so a changed binary or a changed `SHA256SUMS` fails the check.

Each release has two signature files, made with the same key and named after it. `<id>` is the key's ID, the first 16 hex digits of the SHA-256 of its DER public key; the installers and pai's README carry it with the key.

| File | Holds | Checked by |
|---|---|---|
| `SHA256SUMS.<id>.sig` | The 64-byte Ed25519 signature of `SHA256SUMS` | `pai update`; `openssl pkeyutl -verify -rawin`, OpenSSL 3 or later |
| `SHA256SUMS.<id>.sshsig` | The same key's signature in OpenSSH's format, namespace `pai-release` | `ssh-keygen -Y verify`, OpenSSH 8.1 or later |

Releases up to 0.1.9 named them `SHA256SUMS.sig` and `SHA256SUMS.sshsig`, and pai up to 0.1.9 reads only those names. Naming the files after the key lets one release carry two keys' signatures while the key changes (below).

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

`pai-release.key` lives in iCloud Drive, in `DawonAI/`, so either of the operator's Macs can sign (decided 2026-10-27). The file is the passphrase-encrypted key, never a decrypted copy. The passphrase is kept outside the Apple account: not in iCloud Keychain, not in Notes, not in iCloud Drive. Anyone with the Apple account gets only the encrypted file, and anyone with the passphrase alone gets nothing. A second copy stays offline on encrypted removable storage. The key never goes on a server, in a repository, or in CI.

To sign from a Mac, point `PAI_SIGNING_KEY` at it:

```sh
PAI_SIGNING_KEY="$HOME/Library/Mobile Documents/com~apple~CloudDocs/DawonAI/pai-release.key" release/publish.sh
```

What this costs against the removable drive: the encrypted file is on every Mac signed in to the Apple account, all the time, so the passphrase's strength is what protects it from anyone who copies the file, and a Mac compromised while you type the passphrase gives away the key. Use a long, random passphrase.

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

## The backup key

From 0.1.14, pai has two release keys built in: the main key (`release/signing-key.pub`) and a backup key (`release/backup-key.pub`). `pai update` accepts a release whose `SHA256SUMS` carries a valid signature from either one, trying the main key's `SHA256SUMS.<id>.sig` first. So if one key is lost, or a release goes out with one signature wrong, installed copies still update. Each pai before 0.1.14 trusts one key only.

Every release is signed with both keys: `release/build.sh` refuses to build without `PAI_BACKUP_KEY`. Keep the backup key under its own passphrase, not the main key's.

Before anything is published, `release/build.sh` checks every binary it built:

1. Each binary must contain exactly the two keys, and no other Ed25519 key. This catches a stale build. The 0.1.10 Linux and Windows binaries shipped with a throwaway test key, because Docker reused a cached build from a rehearsal on a copy of the repo. Those installs can't update and need the installer run once.
2. Each binary that can run on the build Mac runs `pai check-release <dir>` on the signed release. That checks every file against `SHA256SUMS`, then checks the main key's signature and the backup key's signature separately, with the keys compiled into that binary. It must print `ok <main id>` and `ok <backup id>`. The macOS binaries run natively (Intel under Rosetta), and the Linux binaries run in a container. Windows binaries can't run on a Mac, so step 1 covers them.

The installers check the main key's signature only.

## Passphrases in the login keychain

`release/build.sh` reads each key's passphrase from the Mac's login keychain when it's saved there, and asks only for the ones that aren't (decided 2026-11-02, to sign a release without typing). Save each once, in Terminal; `-w` with no value asks for the passphrase without echoing it:

```sh
security add-generic-password -s dawonai-pai-release -a pai-release-2026-10-27.key -w
security add-generic-password -s dawonai-pai-release -a pai-release-backup-2026-11-02.key -w
security add-generic-password -s dawonai-pai-release -a pai-release.key -w
```

The account is the key file's name. The first time `release/build.sh` reads an item, macOS asks whether `security` may read it; **Always Allow** stops the prompt.

What this costs: the login keychain is unlocked whenever you're logged in, so any program running as your macOS user can read the passphrases once `security` is allowed, and with the key files it can sign a release that every installed pai accepts. That includes Claude Code sessions on this Mac. The items stay in the local login keychain, not iCloud Keychain. To go back to typing, delete them: `security delete-generic-password -s dawonai-pai-release -a <key file>`.

## Changing the key

Every installed pai has the key it was built with, and checks each update against that key alone. Changing the key therefore takes one hand-over release, signed with both:

1. Make the new key (above) and run `release/embed-key.sh`. Save the old public key as `release/handover-key.pub`: `git show HEAD:release/signing-key.pub > release/handover-key.pub`.
2. Publish with both private keys. `release/build.sh` refuses to build without the old one while `release/handover-key.pub` exists:

   ```sh
   PAI_SIGNING_KEY=<new key> PAI_HANDOVER_KEY=<old key> release/publish.sh
   ```

   The release gets `SHA256SUMS.<new id>.sig` and `.sshsig` from the new key, and `SHA256SUMS.sig` and `.sshsig` from the old one. Installed copies check the old key's file, install the release, and from then on check the new key. New installs use the new key at once: each one downloads a fresh installer.
3. Delete `release/handover-key.pub` in the next commit, so later releases carry the new key's signatures only. An installed pai that didn't update during the hand-over can't update after it; it needs the installer run once.
4. Delete every copy of the old private key, and say on the site and GitHub that it's retired.

A leaked key still signs: until each installed copy takes the hand-over release, whoever holds the old key and controls the site or the box could ship it a release. Publish the hand-over release quickly.

## Losing or leaking the key

There's no way to revoke a key. If the key leaks, change it as above. If every copy is lost, there's no hand-over release: make a new key, run `release/embed-key.sh`, publish it everywhere above, and installed copies need the installer run once.

**2026-10-27:** the key with ID `52856a9eb2b9d05b` was retired after its passphrase was pasted into a chat. pai 0.1.10 is the hand-over release to the key with ID `f0a52c411efb5295`.

**2026-10-30:** `release/handover-key.pub` is back, so every release is signed with both keys again until every installed pai is on 0.1.10 or later. Publish with `PAI_SIGNING_KEY` set to the new key and `PAI_HANDOVER_KEY` set to the old one.

**2026-11-02:** pai 0.1.14 adds the backup key (above). From then on, publish with `PAI_BACKUP_KEY` set as well. With two keys built in, losing one is no longer the case above: sign with the other, and make a new second key in a later release.

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
