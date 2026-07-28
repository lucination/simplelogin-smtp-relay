# Known Differences from the Python Original

Goal: zero unintentional behavioral differences. This file lists deliberate divergences from the Python original.

## Upstream envelope sender and visible `From`

The relay always uses `UPSTREAM_USERNAME` as both the upstream SMTP `MAIL FROM`
identity and the addr-spec in every parseable RFC 5322 `From` header. A
pre-existing display name is retained (for example, `Alerts <old@example.test>`
becomes `Alerts <UPSTREAM_USERNAME>`). This is unconditional and has no
configuration override.

If a message has no `From`, the relay does not add one. If an existing `From`
is malformed, a group, or contains multiple mailboxes, it is left unchanged to
avoid inventing or ambiguously rewriting an invalid header.

This deliberately differs from the Python reference implementation, which
forwards the inbound envelope sender and preserves the visible `From` header.

## `DATA_TIMEOUT` enforcement under a slow-but-completing upstream

**Scenario:** upstream SMTP server accepts the `DATA` payload but delays its final `250` response beyond `DATA_TIMEOUT` before eventually responding successfully. The differential fixture uses a two-second response delay, a one-second `DATA_TIMEOUT`, and a five-second upstream socket timeout, so the observation cannot be confused with an upstream socket timeout.

- **Python original:** replies `250 OK` — the timeout does **not** fire.
- **Rust reimplementation:** replies `451 Timeout processing mail` — the timeout **does** fire, as documented in the spec (`DATA_TIMEOUT wraps processing with "451 Timeout processing mail" / "451 Internal error" on failure`).

**Root cause:** in the Python original, `RelayHandler.handle_DATA` wraps `_process(envelope)` in `asyncio.wait_for(..., timeout=DATA_TIMEOUT)`. `_process()` is an `async def`, but its body contains no `await` at all — every I/O call inside it (`sl_client.get_reverse_alias`, `smtplib.SMTP(...)`) is a **synchronous, blocking** call using `requests` and `smtplib`. Because the coroutine never yields control back to the event loop while it runs, `asyncio.wait_for`'s cancellation can only take effect *between* awaits — and there are none inside `_process`. In practice this means `DATA_TIMEOUT` in the original is effectively inert against slow blocking I/O: it can only be observed if the blocking call itself raises/returns around the same wall-clock moment the deadline is checked, which does not happen for a call that is merely slow-but-successful (as verified directly against a live clone of the original, see `tests/differential/run.py`).

**Why we don't reproduce the bug:** the environment variable's name, default, and documented purpose ("`DATA_TIMEOUT` wraps processing... `451 Timeout processing mail`... on failure") describe intended behavior that only the Rust reimplementation actually delivers. The Rust port runs the equivalent blocking work inside `tokio::task::spawn_blocking`, wrapped by a real `tokio::time::timeout`, so a slow upstream is genuinely preempted at the timeout boundary. Silently copying the original's non-functional timeout would be reproducing a bug, not a feature, and would leave `DATA_TIMEOUT` meaningless in the Rust build as well — considered a worse outcome than a documented, deliberate improvement.

Aside from these documented differences, every other scenario in the differential harness (plain `To`, named `From`, multiple `To`, `To`+`Cc` with display names, unmapped/unknown addresses, `Bcc` stripping, alias-not-found error, no-reverse-alias error) passes with identical SMTP response codes and relayed recipients plus non-`From` message-header content. The harness explicitly checks the intentionally different upstream envelope senders and Rust-visible `From` normalization rather than treating either as parity failures.
