# Contributing to JTP

Thanks for your interest in contributing to **JTP (Jason Transfer Protocol)**.

## Project Structure

- `src/protocol.rs` - Core protocol implementation
- `src/bin/server.rs` - TLS server binary
- `src/bin/client.rs` - TLS client binary
- `crates/jtp-wasm/` - WebAssembly bindings for ImageID computation

## Development Setup

### Prerequisites

- Rust toolchain (stable, 1.72+)

### Build and Test

```bash
cargo build --workspace
cargo test --workspace
```

### Run Server and Client

```bash
# Terminal 1: Start server
cargo run --bin server

# Terminal 2: Run client
cargo run --bin client
```

## Ways to Contribute

- Fix bugs in the client/server/protocol
- Improve performance and error handling
- Add tests or fuzzing
- Improve documentation

## Protocol Compatibility

Changes to the binary protocol must be handled carefully:

1. **Update documentation**: Modify `README.md` and `RFC.md`
2. **Keep implementations consistent**: Server, client, and jtp-wasm must match
3. **Prefer additions over breaking changes**: Add new request types rather than
   modifying existing ones

### ImageID

ImageIDs are defined as:

```
ImageID = xxHash64(file_bytes, seed=0)
```

Encoded on the wire as `u64` big-endian.

If you change this, update:

- `src/protocol.rs`
- `src/bin/server.rs`
- `src/bin/client.rs`
- `crates/jtp-wasm/src/lib.rs`
- Documentation

## Code Style

- Run `cargo fmt` before committing
- Run `cargo clippy` to catch common issues
- Keep changes focused and minimal
- Avoid adding dependencies unless necessary

## Submitting Changes

1. Fork the repository
2. Create a feature branch
3. Make your changes
4. Ensure `cargo build` and `cargo test` pass
5. Open a pull request with:
   - What changed
   - Why it changed
   - Protocol compatibility notes (if applicable)
   - How to test

## Reporting Bugs

When filing an issue, please include:

- OS and Rust version
- Steps to reproduce
- Expected vs actual behavior
- Logs with `--verbose` if relevant
- If protocol-related: hexdump of the bytes exchanged
