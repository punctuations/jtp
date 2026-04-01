# Jason Transfer Protocol (JTP)

> [!NOTE]
> JTP has been submitted as an Internet-Draft to the IETF. As such, it is a work in progress and subject to change based on ongoing discussion and review within the IETF community.


**JTP** is a high-performance binary protocol for transferring images over TCP
with optional TLS encryption and intelligent compression.

## Features

- **Content-addressed**: Images identified by xxHash64 of their bytes
- **Efficient**: Compact binary framing with varint encoding
- **Secure**: Optional TLS encryption with ALPN support (`jtp/1`)
- **Compression**: Adaptive Zstd compression for compressible formats
- **Connection reuse**: Keep-alive support to avoid repeated TLS handshakes
- **Delta sync**: BATCH mode downloads only missing images

## Getting Started

### Prerequisites

- Rust 1.72+
- On Windows, CMake may be required for TLS dependencies

### Building

```bash
git clone https://github.com/punctuations/jtp.git
cd jtp
cargo build --release
```

### Running

**Server** (listens on `0.0.0.0:8443` by default):

```bash
cargo run --bin server -- --images ./images
```

**Client** (connects to `127.0.0.1:8443` by default):

```bash
cargo run --bin client
```

### Command-Line Options

**Server:**

```
--bind ADDR               Bind address (default: 0.0.0.0:8443)
--images DIR              Images directory (default: images)
--only SUBSTRING          Only serve files containing SUBSTRING
--compression-threshold   Min ratio to use compression (default: 0.95)
--keep-alive-timeout SEC  Idle timeout in seconds (default: 30)
--rate-limit N            Max requests per window (default: unlimited)
--rate-limit-window SEC   Rate limit window in seconds (default: 1)
--no-tls, --plain         Plain TCP mode (no encryption)
--verbose, -v             Enable detailed logging
```

**Client:**

```
--addr HOST:PORT          Server address (default: 127.0.0.1:8443)
--tls, --secure           Use TLS encryption
--no-tls, --plain         Use plain TCP (default)
--server-name NAME        TLS SNI name (default: localhost)
--cert PATH               Server certificate path (default: cert.pem)
--out DIR                 Output directory (default: output)
--batch                   Delta sync: download only missing images
--keep-alive, -k          Reuse connection for multiple requests
--parallel N, -p N        Parallel workers (default: 1, max: 32)
--repeat N                Download N times
--verbose, -v             Enable detailed logging
```

## Protocol Overview

JTP uses a request/response model over TCP (optionally TLS-wrapped).

### ImageID

Images are identified by a 64-bit content hash:

```
ImageID = xxHash64(image_bytes, seed=0)
```

### Request Types

| Type         | Code | Description                                     |
| ------------ | ---- | ----------------------------------------------- |
| LIST         | 1    | Get catalog of available images                 |
| GET_BY_ID    | 0    | Request specific images by ID                   |
| BATCH        | 2    | Delta sync (send IDs you have, receive missing) |
| LIST_AND_GET | 5    | Combined catalog + all images                   |

### Response Headers

| Header | Description             |
| ------ | ----------------------- |
| `JTPL` | LIST response (catalog) |
| `JTPB` | BATCH response          |
| `JTPG` | LIST_AND_GET response   |
| `JTPE` | ERROR response          |

For the complete protocol specification, see [RFC.md](RFC.md).

## Compression

JTP uses Zstd compression with adaptive levels based on file size. Only
compressible formats (BMP, unknown) are compressed. Already-compressed formats
(PNG, JPEG, WebP, GIF) are sent as-is.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

MIT
