# Testing

## Unit tests

```bash
cargo test
```

Current result (real output from this repo, re-run on every change):

```
running 17 tests
test auth::tests::login_success_and_failure ... ok
test auth::tests::plain_success_and_failure ... ok
test config::tests::defaults_match_python ... ok

test config::tests::missing_required_vars_reported ... ok
test config::tests::tls_enabled_requires_cert_and_key ... ok

test config::tests::valid_config_passes ... ok
test handler::tests::bcc_is_stripped_to_and_cc_rewritten ... ok
test handler::tests::upstream_envelope_sender_uses_authenticated_identity_and_preserves_from_header ... ok
test handler::tests::upstream_rejection_is_a_sanitized_single_line_451_response ... ok
test mail::tests::display_name_preserved ... ok
test mail::tests::empty_header_yields_empty_string ... ok
test mail::tests::multiple_to_all_replaced ... ok
test mail::tests::parse_named_reverse_alias_to_bare_address ... ok
test mail::tests::reverse_alias_with_its_own_display_name_uses_original_display_name ... ok
test mail::tests::single_to_replaced ... ok
test mail::tests::unmapped_address_left_alone ... ok
test mail::tests::unmapped_with_display_name_left_alone ... ok

test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

Covers: SMTP AUTH LOGIN/PLAIN decoding & credential validation; env parsing/defaults/validation; header address parsing/replacement (`To`/`Cc` rewriting, display-name preservation, unmapped-address passthrough), and `Bcc` stripping; mock-upstream verification that the upstream authenticated identity is used as the envelope sender while the RFC 5322 `From` header remains unchanged; and sanitized single-line upstream SMTP error responses.

## Docker image testing

Build and smoke-test both Dockerfile variants:

```bash
docker build -t simplelogin-smtp-relay:latest .
docker build -f Dockerfile.alpine -t simplelogin-smtp-relay:alpine .

# confirm the alpine binary only depends on musl libc (no libssl/libcrypto,
# since openssl-sys `vendored` statically links OpenSSL):
docker run --rm --entrypoint sh simplelogin-smtp-relay:alpine -c 'ldd /app/smtp-relay || true'
```

Real output from this repo:

```
$ docker run --rm --entrypoint sh simplelogin-smtp-relay:alpine -c 'ldd /app/smtp-relay || true'
        /lib/ld-musl-x86_64.so.1 (0x...)
```

### Healthcheck

Run a container and confirm `--healthcheck` and the Docker `HEALTHCHECK` both report healthy once the relay is listening:

```bash
docker run -d --name relay-test -p 18025:8025 \
  -e RELAY_USERNAME=u -e RELAY_PASSWORD=p -e SL_API_KEY=k \
  -e UPSTREAM_USERNAME=u -e UPSTREAM_PASSWORD=p \
  simplelogin-smtp-relay:alpine

docker exec relay-test /app/smtp-relay --healthcheck; echo $?   # expect 0
docker inspect --format='{{json .State.Health}}' relay-test    # expect "Status":"healthy" after start_period
```

Real result from this repo: `--healthcheck` exits `0` immediately once the listener is up, and `docker inspect` reports `{"Status":"healthy", ...}` after the 10s `start_period`.

### Multi-arch builds

CI (`.github/workflows/ci.yml`) uses `docker/setup-qemu-action` + `docker/setup-buildx-action` + `docker/build-push-action` with `platforms: linux/amd64,linux/arm64` to build and push both Dockerfiles for both architectures to GHCR. To reproduce locally (requires QEMU binfmt registration, e.g. `docker run --privileged --rm tonistiigi/binfmt --install all`):

```bash
docker buildx build --platform linux/amd64,linux/arm64 -f Dockerfile .
docker buildx build --platform linux/amd64,linux/arm64 -f Dockerfile.alpine .
```

Note: local `arm64` builds run under QEMU emulation and are significantly slower (often 10-20+ minutes) than the native runners GitHub Actions uses; treat a full local multi-arch build as optional verification, and check the Actions run for the authoritative result.

## Differential parity harness

`tests/differential/run.py` is a **live** parity test: it clones the actual [Hoshinowo-Yuki/simple-login-smtp-relay](https://github.com/Hoshinowo-Yuki/simple-login-smtp-relay) Python original into `.differential-original/`, spins up an in-process mock SimpleLogin API and a mock upstream SMTP server, then runs identical SMTP scenarios through **both** the Python original (`server.py`) and the Rust binary (`target/debug/smtp-relay`), and diffs:

- The final SMTP response code returned to the client.
- The upstream envelope sender expected for each implementation, `RCPT TO` order, and rewritten message headers (`To`/`Cc`/`Bcc`/`Subject`) captured by the mock upstream. The Python original is checked against the inbound sender; Rust is checked against `UPSTREAM_USERNAME`.

### Requirements

- Python 3.11+ with `aiosmtpd` and `requests` installed (`pip install -r .differential-original/requirements.txt` after the first clone, or install ahead of time: `pip install aiosmtpd requests`).
- `git` (for cloning the original).
- A built Rust debug binary: `cargo build` (the harness runs `target/debug/smtp-relay`).

### Running

```bash
cargo build
python3 tests/differential/test_run.py
python3 tests/differential/run.py
```

### Scenarios covered

| Scenario | What it checks |
|---|---|
| `plain_to` | Single `To` address is rewritten to its reverse alias |
| `multiple_to` | Multiple `To` addresses, all rewritten, envelope order preserved |
| `to_cc_display_bcc` | `To`+`Cc` with display names, an unmapped address left untouched, `Bcc` stripped |
| `alias_not_found` | Sender has no matching SimpleLogin alias → `451` |
| `no_reverse_alias` | SimpleLogin returns no `reverse_alias` for a recipient → `451` |
| `upstream_timeout_via_DATA_TIMEOUT` | Slow upstream response beyond `DATA_TIMEOUT` while its socket timeout remains higher; **documented divergence**, see below |

### Result (real output from a live run against a freshly cloned original)

```
PASS plain_to: response=250 relays=1
PASS multiple_to: response=250 relays=1
PASS to_cc_display_bcc: response=250 relays=1
PASS alias_not_found: response=451 relays=0
PASS no_reverse_alias: response=451 relays=0
PASS upstream_timeout_via_DATA_TIMEOUT (documented divergence, see KNOWN_DIFFERENCES.md): python=250 (asyncio.wait_for can't preempt sync blocking I/O) rust=451 (real timeout enforced)
Differential parity PASS: 6/6 scenarios
```

The `upstream_timeout_via_DATA_TIMEOUT` scenario is an intentional, documented divergence — not a harness bug and not a Rust bug. Its fixture holds the upstream socket timeout above the delayed response, so it tests `DATA_TIMEOUT` rather than a socket-timeout race. See [`KNOWN_DIFFERENCES.md`](KNOWN_DIFFERENCES.md) for full root-cause analysis: the Python original's `asyncio.wait_for` can never actually preempt its fully-synchronous, non-`await`-ing `_process()` body, so `DATA_TIMEOUT` is effectively inert against slow blocking I/O there, whereas the Rust port enforces it for real via `spawn_blocking` + `tokio::time::timeout`.

Every other scenario passes with identical response codes plus identical relayed recipient and message-header content; the harness separately verifies each implementation's documented envelope sender.
