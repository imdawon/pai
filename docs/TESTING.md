# Testing pai against staging

For people working on DawonAI, not for users. Nothing here appears in `pai --help` or the
README, on purpose.

## Test mode

A staging service has its own certificate authority and its own box key, so a release `pai`
refuses it: it trusts only the public certificate authorities, and pins only the real box's key.
Test mode swaps both:

- **A test CA**, trusted *instead of* the public certificate authorities, for the relay and the API.
- **Test pins** for the API's key, *instead of* the release's.

Pin checking itself is never turned off.

## Staging builds

Test mode exists only in a staging build of `pai`, made with the `test-mode` feature:

```sh
cargo build --release --features test-mode
```

`pai --version` on a staging build says `(staging build, with test mode)`. Keep staging builds
on the machines that test staging, and never publish one.

A release build (`release/build.sh`, or `cargo build --release` with no features) has no test
mode at all: the `--test-ca` and `--test-pin` flags don't exist, `PAI_TEST_CA` and
`PAI_TEST_API_PINS` do nothing, and `pai` trusts only the public certificate authorities and the
pins it was built with. So a "setup prompt" that tells a user to pair with an attacker's CA and
pins fails on every release `pai`.

A release build that finds a pairing made in test mode (a `test` block in `config.json`) refuses
to use it: `pai token`, `pai proxy`, `pai status`, `pai pair` and `pai service install` stop with
an error that tells the user to run `pai unpair` and pair again. `pai unpair` still works, with the
release's trust.

## Using test mode

In a staging build, turn test mode on when pairing; it's kept with the pairing until `pai unpair`:

```sh
pai pair <code> \
  --relay https://relay.<staging-domain>:8443 --api https://api.<staging-domain>:8443 \
  --test-ca staging-ca.crt --test-pin <pin>
```

`pai pair` copies the CA to `test-ca.pem` next to the config. Both flags are needed together.
While test mode is on, every `pai` command prints a `TEST MODE` warning on stderr, and
`pai status` shows it.

For a one-off run, `PAI_TEST_CA=<pem>` with `PAI_TEST_API_PINS=<pin,pin>` does the same without
pairing in test mode. `PAI_TEST_API_PINS` does nothing without `PAI_TEST_CA`.

A pin is the base64 SHA-256 of the server's public key:

```sh
openssl pkey -in key.pem -pubout -outform der | openssl dgst -sha256 -binary | base64
openssl x509 -in cert.pem -pubkey -noout | openssl pkey -pubin -outform der | openssl dgst -sha256 -binary | base64
```

An `http://` API URL (a gateway on this machine) skips TLS and pins entirely, in both builds.

## Tests

The tests that use test mode (`tests/proxy.rs`, which runs a fake model API with its own CA) run
only with the feature. Run both sets, and clippy on both:

```sh
cargo test
cargo test --features test-mode
cargo clippy --all-targets
cargo clippy --all-targets --features test-mode
```

`tests/release_build.rs` runs only without the feature, and checks that a release build has no
test mode.

## Releases

`release/build.sh` builds without the `test-mode` feature. It needs `PAI_RELEASE_API_PINS`: the
box's current key and an offline spare, comma-separated. It's compiled into the binary, and it's
what a release build pins. See `pai-gateway/deploy/CERTIFICATES.md`.
