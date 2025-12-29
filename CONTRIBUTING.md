# Contributing to JTP

Thanks for your interest in contributing to **JTP (Jason Transfer Protocol)**.

## Quick start

- Rust workspace lives at the repo root (binaries: `client`, `server`).
- Docs site is a Next.js app in `docs/`.
- Protocol code lives in `src/protocol.rs`.

## Ways to contribute

- Fix bugs in the client/server/protocol.
- Improve protocol documentation and examples.
- Add interoperability tests or fuzzing (if you're into that).
- Improve performance, error handling, and UX.

## Development setup

### Prerequisites

- Rust toolchain (stable)
- Node.js + npm (for `docs/`)

### Build / test (Rust)

From the repo root:

```bash
cargo build --workspace
cargo test --workspace
```

Run the reference server/client:

```bash
cargo run --bin server
cargo run --bin client
```

### Build / test (Docs)

From `docs/`:

```bash
npm install
npm run build
```

## Protocol compatibility rules

This repo includes a custom binary protocol. Changes to framing/encoding must be
treated carefully.

When changing the protocol:

- Update the authoritative spec in `README.md` and the docs page in
  `docs/app/page.tsx`.
- Keep the Rust server and Rust client consistent.
- Keep the wasm helper (`crates/jtp-wasm`) consistent if ImageID computation or
  encoding changes.
- Prefer adding new request/response types rather than breaking existing ones,
  unless the change is explicitly a breaking version bump.

### ImageID

ImageIDs are defined as:

- `ImageID = xxHash64(file_bytes, seed=0)`
- Encoded on the wire as `u64` big-endian.

If you change this, update:

- `src/protocol.rs`
- `src/bin/server.rs`
- `src/bin/client.rs`
- `crates/jtp-wasm/src/lib.rs`
- Documentation

## Code style

- Keep changes focused and minimal.
- Follow existing formatting (use `cargo fmt` if you have it).
- Avoid adding new dependencies unless there's a clear win.

## Submitting changes

1. Create a branch.
2. Make your change.
3. Ensure builds pass:
   - `cargo build --workspace`
   - `cargo test --workspace`
   - `npm run build` (if docs changed)
4. Open a PR with:
   - What changed
   - Why it changed
   - Any protocol compatibility notes
   - How to test

## Reporting bugs

When filing an issue, please include:

- OS + Rust version
- Steps to reproduce
- Expected vs actual behavior
- Logs with `--verbose` if relevant
- If protocol-related: a hexdump / framing description of the bytes exchanged
