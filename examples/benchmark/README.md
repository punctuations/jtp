# JTP vs HTTP Performance Benchmark

This benchmark compares JTP transfer performance against HTTP across multiple
download strategies.

## Directory Structure

```
examples/
├── README.md                 # This file
├── benchmark.rs              # Rust benchmark (main entry point)
└── benchmark/
    ├── cert.pem              # TLS certificate for JTP
    ├── key.pem               # TLS private key
    └── http/
        ├── server.js         # HTTP server (plain)
        └── server-https.js   # HTTPS server
```

## Quick Start

```bash
# Terminal 1: Start JTP server (plain TCP for fair comparison)
cargo run --release --bin server -- --no-tls --images ./images

# Terminal 2: Start HTTP server
bun no-tls

# Terminal 3: Run benchmark
cargo run --release --example benchmark
```

### With TLS (fair encrypted comparison)

```bash
# Terminal 1: Start JTP server with TLS (port 8443)
cargo run --release --bin server -- --images ./images

# Terminal 2: Start HTTPS server (port 8080, uses same certs as JTP)
bun tls

# Terminal 3: Run benchmark (auto-detects TLS for both)
cargo run --release --example benchmark
```

## Benchmark Modes

| Mode               | Description                                      |
| ------------------ | ------------------------------------------------ |
| **HTTP**           | Download via HTTP (one request per image)        |
| **JTP Per-Image**  | New connection per image (worst case)            |
| **JTP Batch**      | Single connection, batch GET request             |
| **JTP Keep-Alive** | Reuse connection with keep-alive flag            |
| **JTP Parallel**   | Multiple parallel workers with keep-alive        |
| **JTP Delta**      | BATCH sync (only download missing images)        |
| **JTP List+Get**   | Combined LIST+GET in single round-trip (fastest) |

## Command-Line Options

```
cargo run --release --example benchmark -- [OPTIONS]

Options:
  --jtp-addr ADDR       JTP server address (default: 127.0.0.1:8443)
  --http-addr URL       HTTP server URL (default: http://127.0.0.1:8080)
  --cert PATH           TLS certificate path (default: cert.pem)
  --images DIR          Test images directory (default: images/)
  --warmup N            Warmup iterations (default: 5)
  --iterations N, -n N  Benchmark iterations (default: 10)
  --parallel N, -p N    Parallel workers for JTP-Parallel mode (default: 4)
  --mode MODE           Run specific mode:
                          http, per-image, batch, keepalive, parallel,
                          delta, list-and-get, jtp (all JTP modes), or all
  --help                Show help
```

Both JTP (TLS vs plain TCP) and HTTP (HTTP vs HTTPS) are **auto-detected**.

## Example Output

```
======================================================================
JTP vs HTTP Image Download Benchmark
======================================================================

  Test images directory: "images"
  HTTP server: http://127.0.0.1:8080
  JTP server: 127.0.0.1:8443
  JTP TLS: disabled (plain TCP)

  Found 7 test images:
    - bateman.jpg (178.23 KB)
    - derulo.png (1485.32 KB)
    - momoa.bmp (44521.00 KB)
    ...

--------------------------------------------------
Running JTP List+Get Benchmark...
--------------------------------------------------
  Warmup (5 runs)...
    OK Warmup complete
  Run 1/10... OK 12.45 ms
  Run 2/10... OK 11.89 ms
  ...

======================================================================
BENCHMARK RESULTS
======================================================================

Comparison:

  Mode         |   Avg Time |   Median |       Min |       Max | Throughput | Conns
  ---------------------------------------------------------------------------
  HTTP         |   89.23 ms | 87.45 ms |  82.34 ms |  98.76 ms |  512 KB/s  |     7
  JTP-1conn    |   67.45 ms | 65.23 ms |  61.45 ms |  75.89 ms |  678 KB/s  |     7
  JTP-Batch    |   23.67 ms | 22.89 ms |  21.34 ms |  26.45 ms | 1934 KB/s  |     1
  JTP-KA       |   21.45 ms | 20.78 ms |  19.56 ms |  24.23 ms | 2132 KB/s  |     1
  JTP-Par      |   15.23 ms | 14.89 ms |  13.45 ms |  17.67 ms | 3004 KB/s  |     4
  JTP-L+G      |   11.89 ms | 11.45 ms |  10.23 ms |  13.56 ms | 3847 KB/s  |     1 <-

Relative Performance:

  JTP-L+G: baseline (fastest)
  JTP-Par: 1.28x slower (28.1% slower)
  JTP-KA: 1.80x slower (80.4% slower)
  JTP-Batch: 1.99x slower (99.1% slower)
  JTP-1conn: 5.67x slower (467.4% slower)
  HTTP: 7.51x slower (650.6% slower)
```

## Important Notes

### No Disk I/O

The benchmark **does not save images to disk**. Downloaded images are read into
memory and discarded. This isolates network/protocol performance from disk I/O.

### Auto-Detection

The benchmark automatically detects whether the HTTP server is running HTTP
(port 8080) or HTTPS (port 8443). Just start whichever server you want and the
benchmark will find it.

### Fair Comparison

For a fair encrypted comparison, run both servers with TLS:

- JTP with `--tls` flag
- HTTPS server (`server-https.js`)

The HTTPS server uses the same self-signed certificates as the JTP server,
ensuring equivalent TLS overhead.

### Test Images

The benchmark uses images from the `--images` directory (default: the main
`images/` folder). For consistent results, use the same images for both HTTP and
JTP servers.

## Server Setup

### JTP Server

```bash
# Plain TCP (recommended for benchmarking)
cargo run --release --bin server -- --no-tls --images ./images

# With TLS
cargo run --release --bin server -- --images ./images
```

### HTTP Server

```bash
# Plain HTTP (port 8080)
bun no-tls

# HTTPS (port 8080, uses same certs as JTP)
bun tls
```

Both servers use port 8080, leaving port 8443 free for the JTP TLS server.

## Understanding Results

### When JTP Excels

- **Multiple images**: Amortizes connection setup across many transfers
- **Keep-alive**: Single connection for all requests
- **List+Get**: Single round-trip for catalog + all images
- **Delta sync**: Only transfers missing images

### Throughput Calculation

```
Throughput (KB/s) = Total Bytes / Average Time (seconds) / 1024
```

### Connections Column

Shows how many TCP connections were made per benchmark run:

- HTTP: One per image
- JTP Per-Image: One per image
- JTP Batch/Keep-Alive/List+Get: One total
- JTP Parallel: One per worker

## License

MIT
