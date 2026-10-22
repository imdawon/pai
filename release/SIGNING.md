# Signing pai releases

The release binaries are not signed with a publisher identity yet. This page covers what each OS checks, what signing takes, and what users can do until then.

## Who sees a warning

macOS and Windows warn only about files that carry a "downloaded from the internet" mark.

| Path | Marked? | Warning |
|---|---|---|
| `install.sh` (curl or wget) | No: curl and wget don't set `com.apple.quarantine` | None |
| `install.ps1` (`Invoke-WebRequest`) | No: it doesn't add a Mark of the Web (`Zone.Identifier`) | None, though Defender can still scan the file |
| Binary downloaded in a browser, AirDrop or a chat app | Yes | Gatekeeper on macOS, SmartScreen on Windows |

So the installers work today without signing, and the SHA-256 check is what protects them. Signing matters for people who download a binary from the release page by hand, and for organizations whose policies allow only signed software.

## macOS: Developer ID and notarization

Every arm64 binary must carry a signature to run at all. The Rust linker adds an ad-hoc one (`codesign -dv` shows `adhoc,linker-signed`), which is enough to run but not for Gatekeeper to trust a quarantined file.

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
4. **Regenerate `SHA256SUMS` after signing.** Signing changes the file, so checksums made before it no longer match and the installers refuse the download. Sign, then notarize, then checksum and copy to `latest/`.

## Windows: Authenticode

1. **A code signing certificate** from a CA (OV or EV). Since June 2023 the private key has to live in a hardware module, so in practice this is a cloud signing service such as Azure Artifact Signing (formerly Trusted Signing), DigiCert KeyLocker or SSL.com eSigner, or a USB token.
2. **Sign** both `.exe` files with a timestamp, so the signature outlives the certificate:

   ```powershell
   signtool sign /fd sha256 /tr http://timestamp.digicert.com /td sha256 /a pai-x86_64-pc-windows-msvc.exe
   ```

   From macOS or Linux, `osslsigncode` or the signing service's own client does the same.
3. **Regenerate `SHA256SUMS`** afterwards, for the same reason as on macOS.

A signature alone doesn't silence SmartScreen. SmartScreen builds reputation per certificate and file as people download them, and an EV certificate no longer skips that. Early downloads may still show the warning.

## Until then

Tell users to install with the one-liners, which avoid both warnings. For someone who downloaded a binary by hand:

- **Check the hash first**: `shasum -a 256 <file>` (macOS), `sha256sum <file>` (Linux) or `Get-FileHash <file>` (Windows), against `SHA256SUMS`.
- **macOS**: remove the quarantine mark, then run it:

  ```sh
  xattr -d com.apple.quarantine ./pai-aarch64-apple-darwin
  chmod +x ./pai-aarch64-apple-darwin
  ```

  Or try to run it once, then choose **Open Anyway** in **System Settings > Privacy & Security**. Since macOS 15, Control-clicking the file no longer offers to open it.
- **Windows**: `Unblock-File .\pai-x86_64-pc-windows-msvc.exe` removes the Mark of the Web. Or, at the SmartScreen prompt, choose **More info**, then **Run anyway**.

`SHA256SUMS` only helps if it comes from somewhere the attacker can't also change. Serve it over HTTPS from the same origin as the installers. A detached signature over `SHA256SUMS` (minisign or cosign) would let users verify a release without trusting the web server; it costs nothing and needs no certificate.
