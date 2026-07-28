# Known Differences from the Python Original

Goal: zero unintentional behavioral differences. This file lists deliberate divergences from the Python original.

## Upstream envelope sender

The relay always uses `UPSTREAM_USERNAME` as the upstream SMTP `MAIL FROM`
identity. The RFC 5322 message headers, including `From`, are not changed.

This deliberately differs from the reference implementation, which forwards
the inbound envelope sender upstream.

## `DATA_TIMEOUT` enforcement under a slow-but-completing upstream

**Scenario:** upstream SMTP server accepts the `DATA` payload but delays its final `250` response beyond `DATA_TIMEOUT` before eventually responding successfully.

- **Python original:** replies `250 OK` — the timeout does **not** fire.
- **Rust reimplementation:** replies `451 Timeout processing mail` — the timeout **does** fire, as documented in the spec (`DATA_TIMEOUT wraps processing with "451 Timeout processing mail" / "451 Internal error" on failure`).

**Root cause:** in the Python original, `RelayHandler.handle_DATA` wraps `_process(envelope)` in `asyncio.wait_for(..., timeout=DATA_TIMEOUT)`. `_process()` is an `async def`, but its body contains no `await` at all — every I/O call inside it (`sl_client.get_reverse_alias`, `smtplib.SMTP(...)`) is a **synchronous, blocking** call using `requests` and `smtplib`. Because the coroutine never yields control back to the event loop while it runs, `asyncio.wait_for`'s cancellation can only take effect *between* awaits — and there are none inside `_process`. In practice this means `DATA_TIMEOUT` in the original is effectively inert against slow blocking I/O: it can only be observed if the blocking call itself raises/returns around the same wall-clock moment the deadline is checked, which does not happen for a call that is merely slow-but-successful (as verified directly against a live clone of the original, see `tests/differential/run.py`).

**Why we don't reproduce the bug:** the environment variable's name, default, and documented purpose ("`DATA_TIMEOUT` wraps processing... `451 Timeout processing mail`... on failure") describe intended behavior that only the Rust reimplementation actually delivers. The Rust port runs the equivalent blocking work inside `tokio::task::spawn_blocking`, wrapped by a real `tokio::time::timeout`, so a slow upstream is genuinely preempted at the timeout boundary. Silently copying the original's non-functional timeout would be reproducing a bug, not a feature, and would leave `DATA_TIMEOUT` meaningless in the Rust build as well — considered a worse outcome than a documented, deliberate improvement.

This is the **only** known divergence; every other scenario in the differential harness (plain `To`, multiple `To`, `To`+`Cc` with display names, unmapped/unknown addresses, `Bcc` stripping, alias-not-found error, no-reverse-alias error) passes with byte-identical SMTP response codes and relayed message/recipient content between the two implementations.
