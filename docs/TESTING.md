# Testing pai against staging

For people working on DawonAI, not for users. Nothing here appears in `pai --help` or the
README, on purpose.

## Test mode

A staging service has its own certificate authority and its own box key, so a release `pai`
refuses it: it trusts only the public certificate authorities, and pins only the real box's key.
Test mode swaps both:

- **A test CA**, trusted *instead of* the public certificate authorities, for the relay and the API.
- **Test pins** for the API's key, *instead of* the release's.

Pin checking itself is never turned off. While test mode is on, every `pai` command prints a
`TEST MODE` warning on stderr, and `pai status` shows it, so a user talked into it by someone else
has a chance to notice.

Turn it on when pairing; it's kept with the pairing until `pai unpair`:

```sh
pai pair <code> \
  --relay https://relay.<staging-domain>:8443 --api https://api.<staging-domain>:8443 \
  --test-ca staging-ca.crt --test-pin <pin>
```

`pai pair` copies the CA to `test-ca.pem` next to the config. Both flags are needed together.

For a one-off run, `PAI_TEST_CA=<pem>` with `PAI_TEST_API_PINS=<pin,pin>` does the same without
pairing in test mode. `PAI_TEST_API_PINS` does nothing without `PAI_TEST_CA`.

A pin is the base64 SHA-256 of the server's public key:

```sh
openssl pkey -in key.pem -pubout -outform der | openssl dgst -sha256 -binary | base64
openssl x509 -in cert.pem -pubkey -noout | openssl pkey -pubin -outform der | openssl dgst -sha256 -binary | base64
```

An `http://` API URL (a gateway on this machine) skips TLS and pins entirely.

## Releases

`release/build.sh` needs `PAI_RELEASE_API_PINS`: the box's current key and an offline spare,
comma-separated. It's compiled into the binary, and it's what normal mode pins. See
`pai-gateway/deploy/CERTIFICATES.md`.
