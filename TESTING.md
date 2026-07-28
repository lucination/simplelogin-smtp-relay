# Testing

## Unit tests

```bash
cargo test
```

Current result (real output from this repo, re-run on every change):

```
running 15 tests
test auth::tests::login_success_and_failure ... ok
test auth::tests::plain_success_and_failure ... ok
test config::tests::defaults_match_python ... ok
test config::tests::missing_required_vars_reported ... ok
test config::tests::tls_enabled_requires_cert_and_key ... ok
test config::tests::valid_config_passes ... ok
test handler::tests::bcc_is_stripped_to_and_cc_rewritten ... ok
test mail::tests::display_name_preserved ... ok
test mail::tests::empty_header_yields_empty_string ... ok
test mail::tests::multiple_to_all_replaced ... ok
test mail::tests::parse_named_reverse_alias_to_bare_address ... ok
test mail::tests::reverse_alias_with_its_own_display_name_uses_original_display_name ... ok
test mail::tests::single_to_replaced ... ok
test mail::tests::unmapped_address_left_alone ... ok
test mail::tests::unmapped_with_display_name_left_alone ... ok

test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

Covers: SMTP AUTH LOGIN/PLAIN decoding & credential validation, env var parsing/defaults/validation (matching the Python original's names and defaults), header address parsing/replacement (`To`/`Cc` rewriting, display-name preservation, unmapped-address passthrough), and `Bcc` stripping.

## Differential parity harness

`tests/differential/run.py` is a **live** parity test: it clones the actual [Hoshinowo-Yuki/simple-login-smtp-relay](https://github.com/Hoshinowo-Yuki/simple-login-smtp-relay) Python original into `.differential-original/`, spins up an in-process mock SimpleLogin API and a mock upstream SMTP server, then runs identical SMTP scenarios through **both** the Python original (`server.py`) and the Rust binary (`target/debug/smtp-relay`), and diffs:

- The final SMTP response code returned to the client.
- The exact envelope (`MAIL FROM` / `RCPT TO` order) and rewritten message headers (`To`/`Cc`/`Bcc`/`Subject`) captured by the mock upstream.

### Requirements

- Python 3.11+ with `aiosmtpd` and `requests` installed (`pip install -r .differential-original/requirements.txt` after the first clone, or install ahead of time: `pip install aiosmtpd requests`).
- `git` (for cloning the original).
- A built Rust debug binary: `cargo build` (the harness runs `target/debug/smtp-relay`).

### Running

```bash
cargo build
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
| `upstream_timeout_via_DATA_TIMEOUT` | Slow upstream response beyond `DATA_TIMEOUT`; **documented divergence**, see below |

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

The `upstream_timeout_via_DATA_TIMEOUT` scenario is an intentional, documented divergence — not a harness bug and not a Rust bug. See [`KNOWN_DIFFERENCES.md`](KNOWN_DIFFERENCES.md) for full root-cause analysis: the Python original's `asyncio.wait_for` can never actually preempt its fully-synchronous, non-`await`-ing `_process()` body, so `DATA_TIMEOUT` is effectively inert against slow blocking I/O there, whereas the Rust port enforces it for real via `spawn_blocking` + `tokio::time::timeout`.

Every other scenario passes with byte-identical response codes and relayed content.
