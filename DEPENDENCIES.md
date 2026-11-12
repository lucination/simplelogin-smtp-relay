# Dependency refresh (2026-11-12)

All 18 direct/dev Cargo dependencies were checked against the live registry,
including newer major releases. Their locked versions were already the latest
stable releases; `Cargo.toml` now makes those versions the minimum compatible
versions instead of allowing older patches. Caret-compatible future updates
remain enabled. `cargo update` refreshed 95 existing transitive package names
and changed the lockfile from 243 to 241 packages.

Not every transitive crate can use its independently latest major version:
their parents still require older, incompatible series. Examples include
`base64` 0.22 alongside our direct 0.23 dependency, `syn` 2 alongside 3,
`openssl-src` 300 (OpenSSL 3) rather than 400 (OpenSSL 4), `tower-http` 0.6,
and `parking_lot` 0.12. No dependency forks or forced incompatible patches were
introduced. The audit reports no known vulnerabilities, but retains the
existing **RUSTSEC-2025-0134** unmaintained warning for `rustls-pemfile` 2.2.0;
there is no newer stable release of that crate.

The standard container now uses Debian 13 (trixie) for both stages and the
renamed `libssl3t64` runtime package. The Rust and Alpine tags continue following
stable releases and were explicitly pulled during verification. CI actions use
the latest stable release tags checked through GitHub's API. The Rust action
continues tracking `stable`. Ubuntu 24.04 Arm remains the latest GA hosted Arm
runner; Ubuntu 26.04 runners are still marked preview.

## Differential reference

The Python harness has no project-owned third-party dependency manifest. For
this refresh it ran on Python 3.12 with `aiosmtpd` 1.4.6 and `requests` 2.34.2,
using the actual upstream implementation pinned at
`2dc80b45c5247265e53b0c61b7dfaa1ffea0e927`. This is the original revision matching
the documented timeout-divergence contract. Current upstream HEAD has moved
`server.py` into `relay/` and changed behavior; it is not that historical oracle.

Before running the existing harness, clone the upstream into
`.differential-original/`, check out the revision above, and install its
`requirements.txt` in an isolated environment. The seven live scenarios retain
the documented timeout and sender/header differences; they do not claim parity
with current upstream HEAD or exercise real mail providers.

Rust 1.99.1 strict Clippy exposed existing lint violations. The only Rust edits
replace a checked `Option::unwrap` with an equivalent STARTTLS pattern match
and replace boolean `assert_eq!` calls with `assert!`. No relay policy change
is intended.

Verification passed: formatting, all-target checking, 22 Rust tests, strict
Clippy, three harness tests, seven live differential scenarios, and native
amd64 builds of both Dockerfiles. Both images passed healthcheck and
authentication smoke tests with and without STARTTLS. The TLS probe explicitly
consumes the relay's existing extra `220` greeting after the handshake; this
characterizes existing behavior rather than claiming strict SMTP conformance.
