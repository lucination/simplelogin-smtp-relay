# simplelogin-smtp-relay

An independent Rust reimplementation of [Hoshinowo-Yuki/simple-login-smtp-relay](https://github.com/Hoshinowo-Yuki/simple-login-smtp-relay) (星野有希) — a small SMTP relay that resolves [SimpleLogin](https://simplelogin.io) reverse aliases for outgoing mail and forwards it upstream (e.g. to Gmail).

All credit for the original design, protocol behavior, and API usage pattern goes to Hoshinowo-Yuki's Python original (`server.py` + `utils.py`, built on `aiosmtpd`). This project is a from-scratch Rust rewrite aiming for observable behavioral parity — no source code from the original was copied — built and validated against a live differential test harness that runs both implementations side by side.

## What it does

1. Accepts an authenticated SMTP session (`AUTH LOGIN` / `AUTH PLAIN` against `RELAY_USERNAME` / `RELAY_PASSWORD`).
2. On `DATA`, for every envelope recipient, looks up the corresponding SimpleLogin **reverse alias** via the SimpleLogin API:
   - `GET /api/v2/aliases?page_id=&query=` (paginated, cached per sender alias) to resolve the sender alias to its id.
   - `POST /api/aliases/{id}/contacts {"contact": recipient}` to obtain the `reverse_alias` for that recipient.
3. Rewrites the `To`/`Cc` headers to use the reverse-alias addresses, preserving the original display names. Addresses with no known reverse alias are left untouched.
4. Strips any `Bcc` header before relaying (envelope recipients already carry the real destinations).
5. Relays the rewritten message upstream over SMTP (`STARTTLS` + `AUTH LOGIN` + `MAIL FROM`/`RCPT TO`/`DATA`), defaulting to `smtp.gmail.com:587`. The upstream envelope sender is always `UPSTREAM_USERNAME`; RFC 5322 message headers remain unchanged.
6. The whole `DATA` processing path is wrapped in a `DATA_TIMEOUT`; on timeout it replies `451 Timeout processing mail`, upstream SMTP failures return a sanitized single-line `451 Upstream SMTP error` diagnostic when a numeric response is available, other failures return `451 Internal error`, and success returns `250 OK`.

## Configuration

Same environment variable names and defaults as the Python original:

| Variable | Default | Notes |
|---|---|---|
| `RELAY_HOST` | `0.0.0.0` | Address the relay listens on |
| `RELAY_PORT` | `8025` | Port the relay listens on |
| `RELAY_USERNAME` | *(required)* | SMTP AUTH username clients must present |
| `RELAY_PASSWORD` | *(required)* | SMTP AUTH password clients must present |
| `TLS_ENABLED` | `false` | Enable server-side TLS / `STARTTLS` |
| `TLS_CERT` | `` | PEM certificate path (required if `TLS_ENABLED=true`) |
| `TLS_KEY` | `` | PEM private key path (required if `TLS_ENABLED=true`) |
| `SL_API_URL` | `https://app.simplelogin.io` | SimpleLogin API base URL |
| `SL_API_KEY` | *(required)* | SimpleLogin API key |
| `UPSTREAM_HOST` | `smtp.gmail.com` | Upstream SMTP host to relay through |
| `UPSTREAM_PORT` | `587` | Upstream SMTP port |
| `UPSTREAM_USERNAME` | *(required)* | Upstream SMTP AUTH username |
| `UPSTREAM_PASSWORD` | *(required)* | Upstream SMTP AUTH password |
| `UPSTREAM_STARTTLS` | `true` | Use `STARTTLS` against the upstream |

| `UPSTREAM_TIMEOUT` | `15` | Seconds, upstream SMTP connection/IO timeout |
| `DATA_TIMEOUT` | `30` | Seconds, timeout wrapping the whole `DATA` processing path |
| `LOG_LEVEL` | `INFO` | `env_logger` level |

See `.env.example` for a ready-to-copy template.

## Running

```bash
cargo build --release
RELAY_USERNAME=relay RELAY_PASSWORD=secret \
SL_API_KEY=sl_xxx \
UPSTREAM_USERNAME=you@gmail.com UPSTREAM_PASSWORD=app-password \
./target/release/smtp-relay
```

Or via Docker — see `Dockerfile` / `docker-compose.yml`.

### Docker images

Two Dockerfiles are provided:

| File | Base | libc | When to use |
|---|---|---|---|
| `Dockerfile` | `debian:bookworm-slim` | glibc | Default; smallest maintenance surface, well-trodden path |
| `Dockerfile.alpine` | `alpine:latest` | musl | Smaller image, musl-based hosts/orchestrators, or when a minimal attack surface matters more than glibc compatibility |

Both produce the same `smtp-relay` binary and behave identically. TLS (server-side `STARTTLS` and the client connection to upstream) is handled by `native-tls`; on the Alpine/musl build this uses openssl-sys's `vendored` feature (statically links a from-source OpenSSL build) so there's no dependency on musl-libc's OpenSSL packaging or dynamic linking against `libssl`/`libcrypto` at runtime — confirmed via `ldd` showing only `ld-musl-x86_64.so.1` linked, no `libssl`/`libcrypto`.

Build locally:

```bash
docker build -t simplelogin-smtp-relay:latest .
docker build -f Dockerfile.alpine -t simplelogin-smtp-relay:alpine .
```

Or via compose (the alpine variant is behind the `alpine` profile):

```bash
docker compose up smtp-relay                          # debian/glibc
docker compose --profile alpine up smtp-relay-alpine   # alpine/musl
```

### Published images (multi-arch)

CI builds and pushes both variants to GHCR for `linux/amd64` and `linux/arm64` on every push to `main`:

- `ghcr.io/lucination/simplelogin-smtp-relay:latest` — debian/glibc, multi-arch
- `ghcr.io/lucination/simplelogin-smtp-relay:alpine` — alpine/musl, multi-arch
- `ghcr.io/lucination/simplelogin-smtp-relay:<git-sha>` / `:<git-sha>-alpine` — immutable per-commit tags

```bash
docker pull ghcr.io/lucination/simplelogin-smtp-relay:latest
docker pull ghcr.io/lucination/simplelogin-smtp-relay:alpine
```

### Healthcheck

Both images ship a `HEALTHCHECK` that invokes the binary itself in a lightweight self-check mode (`smtp-relay --healthcheck`) rather than requiring `curl`/`nc` in the image: it opens a TCP connection to `127.0.0.1:${RELAY_PORT:-8025}`, reads the SMTP greeting, and exits `0` if it starts with `2` (e.g. `220 ...`) or `1` otherwise. `docker-compose.yml` wires the same command into each service's `healthcheck:` block.

Test manually:

```bash
docker exec <container> /app/smtp-relay --healthcheck; echo $?   # 0 = healthy
docker inspect --format='{{json .State.Health}}' <container>
```

## Testing

See [`TESTING.md`](TESTING.md) for `cargo test` instructions and how to run the differential parity harness against a live clone of the Python original. Any intentional, unavoidable behavioral divergence discovered by that harness is documented in [`KNOWN_DIFFERENCES.md`](KNOWN_DIFFERENCES.md).

## License

MIT — see [`LICENSE`](LICENSE). This project is an independent reimplementation and does not embed the original project's source or copyright text; see the "Credit" section above for attribution.
