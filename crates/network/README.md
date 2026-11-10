# filmcraft-network

Native client for FilmCraft's separate RedWeb LAN relay (L5). `RelayServices` implements the
engine's filesystem interface and `ByteReader` supplies bounded, version-pinned byte ranges.
The desktop UI connects through File → Network Project. The web build is unchanged.

See [Network projects](../../docs/network-projects.md) for setup, behavior and limits.

New direct transport dependencies use permissive licenses: ureq, url, sha2, serde and serde_json
are MIT OR Apache-2.0; rustls-rustcrypto is MIT OR Apache-2.0. The ureq/RustCrypto TLS configuration
matches FilmCraft's speech downloader and verifies HTTPS certificates with the platform verifier.
The separate Node.js service pins RedWeb 0.16.5 (ISC).

Tests:

```sh
npm --prefix tools/network-relay ci
npm --prefix tools/network-relay test
cargo test -p filmcraft-network --lib -- --include-ignored
```

The explicitly ignored integration test starts the real RedWeb relay on loopback, opens media
through the native client, edits/saves, uploads an added local file, refuses a stale writer and
checks failure after a real disconnection. It requires Node.js, rather than silently substituting
a mock transport. Other tests run without Node.js.
