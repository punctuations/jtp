//! JTP vs HTTP Performance Benchmark
//!
//! This benchmark compares JTP performance against HTTP using direct client code
//! (no spawning of external binaries).
//!
//! Run with: cargo run --release --example benchmark -- [OPTIONS]
//!
//! Prerequisites:
//! - JTP server: cargo run --release --bin server -- --images images/
//! - JTP server (WATCH): cargo run --release --bin server -- --images images/ --watch
//! - HTTP server: node examples/benchmark/http-server/server.js

use std::collections::HashSet;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use colored::Colorize;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;

use jtp::protocol::{
    read_varint_u32, write_batch_request_buffered, write_cancel_request_buffered,
    write_get_request_buffered, write_list_and_get_request_buffered,
    write_list_request_buffered, write_watch_request_buffered, ImageId,
    REQUEST_FLAG_KEEP_ALIVE, RESPONSE_BATCH, RESPONSE_CANCEL, RESPONSE_GET_BY_ID,
    RESPONSE_LIST, RESPONSE_LIST_AND_GET, RESPONSE_WATCH,
};
use rustls::client::Resumption;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::RootCertStore;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;

// ============================================================================
// Configuration
// ============================================================================

#[derive(Debug, Clone)]
struct BenchmarkConfig {
    jtp_addr:            String,
    http_addr:           String,
    cert_path:           String,
    test_images_dir:     PathBuf,
    warmup_iterations:   usize,
    test_iterations:     usize,
    parallel_workers:    usize,
    modes:               Vec<BenchmarkMode>,
    no_tls:              bool, // Use plain TCP for JTP
    http_tls:            bool, // Use HTTPS for HTTP server (server-https.js)
    cancel_after:        usize, // CANCEL benchmark: abort after this many packets
    watch_timeout_ms:    u64,   // WATCH benchmark: wait up to this long for first event
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum BenchmarkMode {
    Http,
    JtpPerImage,    // New connection per image (worst case)
    JtpBatch,       // Single connection, batch request
    JtpKeepAlive,   // Reuse connection with keep-alive flag
    JtpParallel,    // Multiple parallel workers
    JtpDelta,       // BATCH/delta sync mode
    JtpListAndGet,  // Combined LIST+GET in single round-trip (fastest)
    JtpCancel,      // GET_BY_ID with mid-stream CANCEL — measures abort latency
    JtpWatch,       // WATCH subscription — measures time-to-first-event
}

impl BenchmarkMode {
    fn name(&self) -> &'static str {
        match self {
            Self::Http         => "HTTP",
            Self::JtpPerImage  => "JTP Per-Image",
            Self::JtpBatch     => "JTP Batch",
            Self::JtpKeepAlive => "JTP Keep-Alive",
            Self::JtpParallel  => "JTP Parallel",
            Self::JtpDelta     => "JTP Delta",
            Self::JtpListAndGet => "JTP List+Get",
            Self::JtpCancel    => "JTP Cancel",
            Self::JtpWatch     => "JTP Watch",
        }
    }

    fn short_name(&self) -> &'static str {
        match self {
            Self::Http          => "HTTP",
            Self::JtpPerImage   => "JTP-1conn",
            Self::JtpBatch      => "JTP-Batch",
            Self::JtpKeepAlive  => "JTP-KA",
            Self::JtpParallel   => "JTP-Par",
            Self::JtpDelta      => "JTP-Delta",
            Self::JtpListAndGet => "JTP-L+G",
            Self::JtpCancel     => "JTP-Cancel",
            Self::JtpWatch      => "JTP-Watch",
        }
    }
}

impl Default for BenchmarkConfig {
    fn default() -> Self {
        Self {
            jtp_addr:            "127.0.0.1:8443".to_string(),
            http_addr:           "http://127.0.0.1:8080".to_string(),
            cert_path:           "cert.pem".to_string(),
            test_images_dir:     PathBuf::from("images"),
            warmup_iterations:   5,
            test_iterations:     10,
            parallel_workers:    4,
            modes: vec![
                BenchmarkMode::Http,
                BenchmarkMode::JtpPerImage,
                BenchmarkMode::JtpBatch,
                BenchmarkMode::JtpKeepAlive,
                BenchmarkMode::JtpParallel,
                BenchmarkMode::JtpListAndGet,
            ],
            no_tls:              true,  // Plain TCP by default for JTP
            http_tls:            false, // Plain HTTP by default
            cancel_after:        1,     // Cancel after the first image packet
            watch_timeout_ms:    2000,  // Wait up to 2 s for first WATCH event
        }
    }
}

// ============================================================================
// Statistics
// ============================================================================

#[derive(Debug, Clone)]
struct Stats {
    mean:    f64,
    median:  f64,
    min:     f64,
    max:     f64,
    std_dev: f64,
}

impl Stats {
    fn from_durations(durations: &[Duration]) -> Self {
        let values: Vec<f64> = durations.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
        Self::from_values(&values)
    }

    fn from_values(values: &[f64]) -> Self {
        if values.is_empty() {
            return Self { mean: 0.0, median: 0.0, min: 0.0, max: 0.0, std_dev: 0.0 };
        }

        let mut sorted = values.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());

        let sum: f64 = values.iter().sum();
        let mean     = sum / (values.len() as f64);
        let median   = sorted[sorted.len() / 2];
        let min      = sorted[0];
        let max      = sorted[sorted.len() - 1];
        let variance = values.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (values.len() as f64);
        let std_dev  = variance.sqrt();

        Self { mean, median, min, max, std_dev }
    }
}

// ============================================================================
// Benchmark Results
// ============================================================================

#[derive(Debug, Clone)]
struct BenchmarkResult {
    mode:             BenchmarkMode,
    durations:        Vec<Duration>,
    total_bytes:      u64,
    connections_made: usize,
    /// Extra label printed below the result (e.g. "Received M=3 out of N=5")
    note:             Option<String>,
}

impl BenchmarkResult {
    fn stats(&self) -> Stats {
        Stats::from_durations(&self.durations)
    }

    fn throughput_kbps(&self) -> f64 {
        let stats = self.stats();
        if stats.mean == 0.0 { return 0.0; }
        (self.total_bytes as f64) / 1024.0 / (stats.mean / 1000.0)
    }
}

// ============================================================================
// Shared catalog entry
// ============================================================================

#[derive(Clone, Debug)]
struct ListedImage {
    id:       ImageId,
    #[allow(dead_code)]
    flags:    u8,
    #[allow(dead_code)]
    filename: String,
    #[allow(dead_code)]
    size:     u32,
}

// ============================================================================
// Plain TCP JTP client — no TLS, fair comparison with HTTP
// ============================================================================

struct PlainJtpClient {
    addr: String,
}

impl PlainJtpClient {
    fn new(addr: &str) -> Self {
        Self { addr: addr.to_string() }
    }

    async fn connect(&self) -> Result<TcpStream, Box<dyn std::error::Error + Send + Sync>> {
        let tcp = TcpStream::connect(&self.addr).await?;
        tcp.set_nodelay(true)?;
        Ok(tcp)
    }

    // ── Catalog ──────────────────────────────────────────────────────────────

    /// Issue a LIST request and parse the response.
    async fn list_images(&self) -> Result<Vec<ListedImage>, Box<dyn std::error::Error + Send + Sync>> {
        let stream = self.connect().await?;
        let mut w  = BufWriter::with_capacity(64 * 1024, stream);

        write_list_request_buffered(&mut w, 0).await?;
        w.flush().await?;

        let mut header = [0u8; 4];
        w.get_mut().read_exact(&mut header).await?;

        if &header != RESPONSE_LIST {
            // Detect a TLS server (alert 0x15, handshake 0x16, app data 0x17)
            if matches!(header[0], 0x15 | 0x16 | 0x17) {
                return Err(
                    "Received TLS bytes — server is in TLS mode. \
                     Use --tls or start server with --no-tls".into()
                );
            }
            return Err(format!(
                "Invalid LIST response: got {:02x?} (expected JTPL). \
                 Is server running with --no-tls?", header
            ).into());
        }

        let count = read_varint_u32(w.get_mut()).await? as usize;
        let mut images = Vec::with_capacity(count);

        for _ in 0..count {
            let id       = w.get_mut().read_u64().await?;
            let flags    = w.get_mut().read_u8().await?;
            let name_len = w.get_mut().read_u16().await? as usize;
            let mut name = vec![0u8; name_len];
            w.get_mut().read_exact(&mut name).await?;
            let filename = String::from_utf8_lossy(&name).to_string();
            let size     = read_varint_u32(w.get_mut()).await?;
            images.push(ListedImage { id, flags, filename, size });
        }

        Ok(images)
    }

    // ── Image consumption ─────────────────────────────────────────────────────

    /// Drain a single image packet from stream; return bytes consumed.
    async fn consume_image(stream: &mut TcpStream) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        let _flags  = stream.read_u8().await?;
        let length  = read_varint_u32(stream).await?;
        let _id     = stream.read_u64().await?;

        let mut rem = length as usize;
        let mut buf = vec![0u8; 65536];
        while rem > 0 {
            let n = rem.min(buf.len());
            stream.read_exact(&mut buf[..n]).await?;
            rem -= n;
        }

        Ok(length as u64)
    }

    // ── Download modes ────────────────────────────────────────────────────────

    /// One connection per image. §9.2: reads JTPD header + M before images.
    async fn download_per_image(&self, ids: &[ImageId]) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        let mut total_bytes  = 0u64;
        let     connections  = ids.len();

        for id in ids {
            let stream = self.connect().await?;
            let mut w  = BufWriter::with_capacity(64 * 1024, stream);

            write_get_request_buffered(&mut w, 0, &[*id]).await?;
            w.flush().await?;

            // §9.2: read JTPD header + M count
            let mut hdr = [0u8; 4];
            w.get_mut().read_exact(&mut hdr).await?;
            if &hdr != RESPONSE_GET_BY_ID {
                return Err(format!("unexpected GET_BY_ID header: {:?}", hdr).into());
            }
            let m = w.get_mut().read_u8().await? as usize;
            for _ in 0..m {
                total_bytes += Self::consume_image(w.get_mut()).await?;
            }
        }

        Ok((total_bytes, connections))
    }

    /// Batch all IDs into one GET_BY_ID request. §9.2: reads JTPD header + M.
    async fn download_batch(&self, ids: &[ImageId]) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        let stream    = self.connect().await?;
        let mut w     = BufWriter::with_capacity(64 * 1024, stream);
        let mut total = 0u64;
        let mut conns = 1;

        for chunk in ids.chunks(255) {
            // Only the first chunk reuses the connection opened above.
            if chunk.as_ptr() != ids.as_ptr() {
                let s = self.connect().await?;
                w     = BufWriter::with_capacity(64 * 1024, s);
                conns += 1;
            }

            write_get_request_buffered(&mut w, 0, chunk).await?;
            w.flush().await?;

            // §9.2: read JTPD header + M count
            let mut hdr = [0u8; 4];
            w.get_mut().read_exact(&mut hdr).await?;
            if &hdr != RESPONSE_GET_BY_ID {
                return Err(format!("unexpected GET_BY_ID header: {:?}", hdr).into());
            }
            let m = w.get_mut().read_u8().await? as usize;
            for _ in 0..m {
                total += Self::consume_image(w.get_mut()).await?;
            }
        }

        Ok((total, conns))
    }

    /// Keep-alive across chunks. §9.2: reads JTPD header + M per chunk.
    async fn download_keepalive(&self, ids: &[ImageId]) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        let stream = self.connect().await?;
        let mut w  = BufWriter::with_capacity(64 * 1024, stream);
        let mut total = 0u64;

        for chunk in ids.chunks(255) {
            write_get_request_buffered(&mut w, REQUEST_FLAG_KEEP_ALIVE, chunk).await?;
            w.flush().await?;

            // §9.2: read JTPD header + M count
            let mut hdr = [0u8; 4];
            w.get_mut().read_exact(&mut hdr).await?;
            if &hdr != RESPONSE_GET_BY_ID {
                return Err(format!("unexpected GET_BY_ID header: {:?}", hdr).into());
            }
            let m = w.get_mut().read_u8().await? as usize;
            for _ in 0..m {
                total += Self::consume_image(w.get_mut()).await?;
            }
        }

        Ok((total, 1))
    }

    /// Parallel workers, each using keep-alive for their sub-slice of IDs.
    /// §9.2: each worker reads JTPD header + M per chunk.
    async fn download_parallel(&self, ids: &[ImageId], num_workers: usize) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        let chunk_size = (ids.len() + num_workers - 1) / num_workers;
        let semaphore  = Arc::new(Semaphore::new(num_workers));
        let addr       = self.addr.clone();
        let mut handles = Vec::new();

        for chunk in ids.chunks(chunk_size) {
            let chunk     = chunk.to_vec();
            let sem       = Arc::clone(&semaphore);
            let addr      = addr.clone();

            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                let tcp     = TcpStream::connect(&addr).await?;
                tcp.set_nodelay(true)?;
                let mut w   = BufWriter::with_capacity(64 * 1024, tcp);
                let mut bytes = 0u64;

                for sub in chunk.chunks(255) {
                    write_get_request_buffered(&mut w, REQUEST_FLAG_KEEP_ALIVE, sub).await?;
                    w.flush().await?;

                    // §9.2: read JTPD header + M count
                    let mut hdr = [0u8; 4];
                    w.get_mut().read_exact(&mut hdr).await?;
                    if &hdr != RESPONSE_GET_BY_ID {
                        return Err::<_, Box<dyn std::error::Error + Send + Sync>>(
                            format!("unexpected GET_BY_ID header: {:?}", hdr).into()
                        );
                    }
                    let m = w.get_mut().read_u8().await? as usize;
                    for _ in 0..m {
                        bytes += PlainJtpClient::consume_image(w.get_mut()).await?;
                    }
                }

                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(bytes)
            }));
        }

        let mut total = 0u64;
        for h in handles { total += h.await??; }
        Ok((total, num_workers))
    }

    /// Delta sync: client sends have-IDs, server returns only missing images.
    async fn download_delta(&self, have_ids: &[ImageId]) -> Result<(u64, usize, usize), Box<dyn std::error::Error + Send + Sync>> {
        let stream = self.connect().await?;
        let mut w  = BufWriter::with_capacity(64 * 1024, stream);

        write_batch_request_buffered(&mut w, 0, have_ids).await?;
        w.flush().await?;

        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await?;
        if &hdr != RESPONSE_BATCH {
            return Err(format!("unexpected BATCH header: {:?}", hdr).into());
        }

        let missing = read_varint_u32(w.get_mut()).await? as usize;
        let mut total = 0u64;
        for _ in 0..missing {
            total += Self::consume_image(w.get_mut()).await?;
        }

        Ok((total, 1, missing))
    }

    /// Combined LIST + GET in a single round-trip.
    async fn download_list_and_get(&self) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        let stream = self.connect().await?;
        let mut w  = BufWriter::with_capacity(64 * 1024, stream);

        write_list_and_get_request_buffered(&mut w, 0).await?;
        w.flush().await?;

        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await?;
        if &hdr != RESPONSE_LIST_AND_GET {
            return Err(format!("unexpected LIST_AND_GET header: {:?}", hdr).into());
        }

        let count = read_varint_u32(w.get_mut()).await? as usize;
        let mut total = 0u64;
        for _ in 0..count {
            total += Self::consume_image(w.get_mut()).await?;
        }

        Ok((total, 1))
    }

    /// CANCEL benchmark (§8.5, §9.2, §9.6).
    ///
    /// Sends a GET_BY_ID for all IDs on a keep-alive connection, receives
    /// `cancel_after` image packets, then sends CANCEL and waits for JTPC.
    /// Measures the round-trip from CANCEL transmission to JTPC receipt.
    async fn download_with_cancel(
        &self,
        ids:          &[ImageId],
        cancel_after: usize,
    ) -> Result<(u64, Duration, usize), Box<dyn std::error::Error + Send + Sync>> {
        if ids.is_empty() {
            return Ok((0, Duration::ZERO, 0));
        }

        let chunk: &[ImageId] = &ids[..ids.len().min(255)];
        let stream = self.connect().await?;
        let mut w  = BufWriter::with_capacity(64 * 1024, stream);

        // Keep-alive so we can send CANCEL after the first few images.
        write_get_request_buffered(&mut w, REQUEST_FLAG_KEEP_ALIVE, chunk).await?;
        w.flush().await?;

        // §9.2: read JTPD header + M count
        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await?;
        if &hdr != RESPONSE_GET_BY_ID {
            return Err(format!("unexpected GET_BY_ID header: {:?}", hdr).into());
        }
        let m = w.get_mut().read_u8().await? as usize;

        // Receive exactly `cancel_after` packets, then cancel.
        let received = cancel_after.min(m);
        let mut bytes_before = 0u64;
        for _ in 0..received {
            bytes_before += Self::consume_image(w.get_mut()).await?;
        }

        // Send CANCEL and time how long until JTPC arrives.
        let cancel_start = Instant::now();
        write_cancel_request_buffered(w.get_mut()).await?;
        w.flush().await?;

        // Drain any in-flight packets before the server processes CANCEL.
        // The server may already have sent more packets into the buffer;
        // we read until we see JTPC (which starts with 'J').
        // Simple approach: read bytes one at a time until we have the 4-byte JTPC magic.
        let mut ack_buf = [0u8; 4];
        let mut pos = 0;
        while pos < 4 {
            let b = w.get_mut().read_u8().await?;
            if pos == 0 && b != RESPONSE_CANCEL[0] {
                // Discard: this is a stray image packet byte; reset.
                continue;
            }
            if b == RESPONSE_CANCEL[pos] {
                ack_buf[pos] = b;
                pos += 1;
            } else {
                pos = 0;
            }
        }

        let cancel_rtt = cancel_start.elapsed();

        if &ack_buf != RESPONSE_CANCEL {
            return Err(format!("unexpected CANCEL ack: {:?}", ack_buf).into());
        }

        Ok((bytes_before, cancel_rtt, received))
    }

    /// WATCH benchmark (§8.6, §9.7).
    ///
    /// Subscribes with WATCH and measures time until the first JTPW event
    /// arrives, then cancels the subscription. Requires the server to be started
    /// with --watch so it periodically rescans the images directory.
    async fn watch_time_to_first_event(
        &self,
        timeout: Duration,
    ) -> Result<Duration, Box<dyn std::error::Error + Send + Sync>> {
        let stream = self.connect().await?;
        let mut w  = BufWriter::with_capacity(64 * 1024, stream);

        write_watch_request_buffered(w.get_mut()).await?;
        w.flush().await?;

        let start = Instant::now();

        // Wait up to `timeout` for the first JTPW frame.
        let result = tokio::time::timeout(timeout, async {
            let mut hdr = [0u8; 4];
            w.get_mut().read_exact(&mut hdr).await?;

            if &hdr != RESPONSE_WATCH {
                return Err::<_, Box<dyn std::error::Error + Send + Sync>>(
                    format!("unexpected WATCH frame: {:?}", hdr).into()
                );
            }

            // Drain the event fields (id + flags + name_len + name + size).
            let _id       = w.get_mut().read_u64().await?;
            let _flags    = w.get_mut().read_u8().await?;
            let name_len  = w.get_mut().read_u16().await? as usize;
            let mut name  = vec![0u8; name_len];
            w.get_mut().read_exact(&mut name).await?;
            let _size     = read_varint_u32(w.get_mut()).await?;

            Ok(())
        })
        .await;

        let elapsed = start.elapsed();

        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_)     => return Err(format!(
                "No WATCH event received within {:?}. \
                 Is the server running with --watch?", timeout
            ).into()),
        }

        // Cancel the subscription so the connection closes cleanly.
        let _ = write_cancel_request_buffered(w.get_mut()).await;
        let _ = w.flush().await;

        Ok(elapsed)
    }
}

// ============================================================================
// JTP client wrapper — handles both plain TCP and TLS transparently
// ============================================================================

enum JtpClientWrapper {
    Plain(PlainJtpClient),
    Tls(TlsJtpClient),
}

impl JtpClientWrapper {
    /// Try plain TCP first; fall back to TLS based on what the server responds.
    async fn auto_detect(
        addr:      &str,
        cert_path: &str,
    ) -> Result<(Self, bool), Box<dyn std::error::Error + Send + Sync>> {
        let tcp = TcpStream::connect(addr).await?;
        tcp.set_nodelay(true)?;
        let mut w = BufWriter::with_capacity(64 * 1024, tcp);

        write_list_request_buffered(&mut w, 0).await?;
        w.flush().await?;

        let mut header = [0u8; 4];
        w.get_mut().read_exact(&mut header).await?;

        // §9.1 header is "JTPL" → plain TCP is working.
        if &header == RESPONSE_LIST {
            drop(w);
            return Ok((Self::Plain(PlainJtpClient::new(addr)), false));
        }

        // TLS alert/handshake/app-data bytes → try TLS.
        if matches!(header[0], 0x15 | 0x16 | 0x17) {
            drop(w);
            let client = TlsJtpClient::new(addr, cert_path).await?;
            return Ok((Self::Tls(client), true));
        }

        Err(format!(
            "Unrecognised server response: {:02x?}. \
             Expected JTPL or TLS handshake.", header
        ).into())
    }

    // ── Forwarding methods ────────────────────────────────────────────────────

    async fn list_images(&self) -> Result<Vec<ListedImage>, Box<dyn std::error::Error + Send + Sync>> {
        match self { Self::Plain(c) => c.list_images().await, Self::Tls(c) => c.list_images().await }
    }

    async fn download_per_image(&self, ids: &[ImageId]) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        match self { Self::Plain(c) => c.download_per_image(ids).await, Self::Tls(c) => c.download_per_image(ids).await }
    }

    async fn download_batch(&self, ids: &[ImageId]) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        match self { Self::Plain(c) => c.download_batch(ids).await, Self::Tls(c) => c.download_batch(ids).await }
    }

    async fn download_keepalive(&self, ids: &[ImageId]) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        match self { Self::Plain(c) => c.download_keepalive(ids).await, Self::Tls(c) => c.download_keepalive(ids).await }
    }

    async fn download_parallel(&self, ids: &[ImageId], workers: usize) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        match self { Self::Plain(c) => c.download_parallel(ids, workers).await, Self::Tls(c) => c.download_parallel(ids, workers).await }
    }

    async fn download_delta(&self, have: &[ImageId]) -> Result<(u64, usize, usize), Box<dyn std::error::Error + Send + Sync>> {
        match self { Self::Plain(c) => c.download_delta(have).await, Self::Tls(c) => c.download_delta(have).await }
    }

    async fn download_list_and_get(&self) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        match self { Self::Plain(c) => c.download_list_and_get().await, Self::Tls(c) => c.download_list_and_get().await }
    }

    async fn download_with_cancel(&self, ids: &[ImageId], cancel_after: usize) -> Result<(u64, Duration, usize), Box<dyn std::error::Error + Send + Sync>> {
        match self { Self::Plain(c) => c.download_with_cancel(ids, cancel_after).await, Self::Tls(c) => c.download_with_cancel(ids, cancel_after).await }
    }

    async fn watch_time_to_first_event(&self, timeout: Duration) -> Result<Duration, Box<dyn std::error::Error + Send + Sync>> {
        match self { Self::Plain(c) => c.watch_time_to_first_event(timeout).await, Self::Tls(c) => c.watch_time_to_first_event(timeout).await }
    }
}

// ============================================================================
// TLS JTP client — mirrors PlainJtpClient but over TLS streams
// ============================================================================

struct TlsJtpClient {
    connector:   TlsConnector,
    addr:        String,
    server_name: ServerName<'static>,
}

impl TlsJtpClient {
    async fn new(addr: &str, cert_path: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let cert_bytes = tokio::fs::read(cert_path).await?;
        let certs: Vec<CertificateDer<'static>> = {
            let mut r = BufReader::new(std::io::Cursor::new(cert_bytes));
            rustls_pemfile::certs(&mut r).collect::<Result<Vec<_>, _>>()?
        };
        let mut store = RootCertStore::empty();
        for cert in certs { store.add(cert)?; }

        let mut config = rustls::ClientConfig::builder()
            .with_root_certificates(store)
            .with_no_client_auth();
        config.resumption =
            Resumption::default().tls12_resumption(rustls::client::Tls12Resumption::SessionIdOrTickets);

        let connector   = TlsConnector::from(Arc::new(config));
        let server_name = ServerName::try_from("localhost".to_string())?;
        Ok(Self { connector, addr: addr.to_string(), server_name })
    }

    async fn connect(&self) -> Result<TlsStream<TcpStream>, Box<dyn std::error::Error + Send + Sync>> {
        let tcp = TcpStream::connect(&self.addr).await?;
        tcp.set_nodelay(true)?;
        Ok(self.connector.connect(self.server_name.clone(), tcp).await?)
    }

    // ── Catalog ───────────────────────────────────────────────────────────────

    async fn list_images(&self) -> Result<Vec<ListedImage>, Box<dyn std::error::Error + Send + Sync>> {
        let stream = self.connect().await?;
        let mut w  = BufWriter::with_capacity(64 * 1024, stream);

        write_list_request_buffered(&mut w, 0).await?;
        w.flush().await?;

        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await?;
        if &hdr != RESPONSE_LIST {
            return Err(format!("invalid LIST response: {:?}", hdr).into());
        }

        let count = read_varint_u32(w.get_mut()).await? as usize;
        let mut images = Vec::with_capacity(count);

        for _ in 0..count {
            let id       = w.get_mut().read_u64().await?;
            let flags    = w.get_mut().read_u8().await?;
            let name_len = w.get_mut().read_u16().await? as usize;
            let mut name = vec![0u8; name_len];
            w.get_mut().read_exact(&mut name).await?;
            let filename = String::from_utf8_lossy(&name).to_string();
            let size     = read_varint_u32(w.get_mut()).await?;
            images.push(ListedImage { id, flags, filename, size });
        }

        Ok(images)
    }

    // ── Image consumption ─────────────────────────────────────────────────────

    async fn consume_image(stream: &mut TlsStream<TcpStream>) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        let _flags = stream.read_u8().await?;
        let length = read_varint_u32(stream).await?;
        let _id    = stream.read_u64().await?;

        let mut rem = length as usize;
        let mut buf = vec![0u8; 65536];
        while rem > 0 {
            let n = rem.min(buf.len());
            stream.read_exact(&mut buf[..n]).await?;
            rem -= n;
        }
        Ok(length as u64)
    }

    // ── Download modes ────────────────────────────────────────────────────────

    /// §9.2: reads JTPD header + M count before images.
    async fn download_per_image(&self, ids: &[ImageId]) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        let mut total = 0u64;
        let conns     = ids.len();

        for id in ids {
            let stream = self.connect().await?;
            let mut w  = BufWriter::with_capacity(64 * 1024, stream);
            write_get_request_buffered(&mut w, 0, &[*id]).await?;
            w.flush().await?;

            let mut hdr = [0u8; 4];
            w.get_mut().read_exact(&mut hdr).await?;
            if &hdr != RESPONSE_GET_BY_ID {
                return Err(format!("unexpected GET_BY_ID header: {:?}", hdr).into());
            }
            let m = w.get_mut().read_u8().await? as usize;
            for _ in 0..m { total += Self::consume_image(w.get_mut()).await?; }
        }

        Ok((total, conns))
    }

    /// §9.2: reads JTPD header + M count per chunk.
    async fn download_batch(&self, ids: &[ImageId]) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        let stream = self.connect().await?;
        let mut w  = BufWriter::with_capacity(64 * 1024, stream);
        let mut total = 0u64;
        let mut conns = 1;

        for chunk in ids.chunks(255) {
            if chunk.as_ptr() != ids.as_ptr() {
                let s = self.connect().await?;
                w     = BufWriter::with_capacity(64 * 1024, s);
                conns += 1;
            }
            write_get_request_buffered(&mut w, 0, chunk).await?;
            w.flush().await?;

            let mut hdr = [0u8; 4];
            w.get_mut().read_exact(&mut hdr).await?;
            if &hdr != RESPONSE_GET_BY_ID {
                return Err(format!("unexpected GET_BY_ID header: {:?}", hdr).into());
            }
            let m = w.get_mut().read_u8().await? as usize;
            for _ in 0..m { total += Self::consume_image(w.get_mut()).await?; }
        }

        Ok((total, conns))
    }

    /// §9.2: reads JTPD header + M count per keep-alive chunk.
    async fn download_keepalive(&self, ids: &[ImageId]) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        let stream = self.connect().await?;
        let mut w  = BufWriter::with_capacity(64 * 1024, stream);
        let mut total = 0u64;

        for chunk in ids.chunks(255) {
            write_get_request_buffered(&mut w, REQUEST_FLAG_KEEP_ALIVE, chunk).await?;
            w.flush().await?;

            let mut hdr = [0u8; 4];
            w.get_mut().read_exact(&mut hdr).await?;
            if &hdr != RESPONSE_GET_BY_ID {
                return Err(format!("unexpected GET_BY_ID header: {:?}", hdr).into());
            }
            let m = w.get_mut().read_u8().await? as usize;
            for _ in 0..m { total += Self::consume_image(w.get_mut()).await?; }
        }

        Ok((total, 1))
    }

    /// §9.2: each worker reads JTPD header + M count per chunk.
    async fn download_parallel(&self, ids: &[ImageId], num_workers: usize) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        let chunk_size  = (ids.len() + num_workers - 1) / num_workers;
        let semaphore   = Arc::new(Semaphore::new(num_workers));
        let addr        = self.addr.clone();
        let connector   = self.connector.clone();
        let server_name = self.server_name.clone();
        let mut handles = Vec::new();

        for chunk in ids.chunks(chunk_size) {
            let chunk       = chunk.to_vec();
            let sem         = Arc::clone(&semaphore);
            let addr        = addr.clone();
            let connector   = connector.clone();
            let server_name = server_name.clone();

            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                let tcp     = TcpStream::connect(&addr).await?;
                tcp.set_nodelay(true)?;
                let tls     = connector.connect(server_name, tcp).await?;
                let mut w   = BufWriter::with_capacity(64 * 1024, tls);
                let mut bytes = 0u64;

                for sub in chunk.chunks(255) {
                    write_get_request_buffered(&mut w, REQUEST_FLAG_KEEP_ALIVE, sub).await?;
                    w.flush().await?;

                    let mut hdr = [0u8; 4];
                    w.get_mut().read_exact(&mut hdr).await?;
                    if &hdr != RESPONSE_GET_BY_ID {
                        return Err::<_, Box<dyn std::error::Error + Send + Sync>>(
                            format!("unexpected GET_BY_ID header: {:?}", hdr).into()
                        );
                    }
                    let m = w.get_mut().read_u8().await? as usize;
                    for _ in 0..m { bytes += TlsJtpClient::consume_image(w.get_mut()).await?; }
                }

                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(bytes)
            }));
        }

        let mut total = 0u64;
        for h in handles { total += h.await??; }
        Ok((total, num_workers))
    }

    async fn download_delta(&self, have_ids: &[ImageId]) -> Result<(u64, usize, usize), Box<dyn std::error::Error + Send + Sync>> {
        let stream = self.connect().await?;
        let mut w  = BufWriter::with_capacity(64 * 1024, stream);

        write_batch_request_buffered(&mut w, 0, have_ids).await?;
        w.flush().await?;

        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await?;
        if &hdr != RESPONSE_BATCH {
            return Err(format!("unexpected BATCH header: {:?}", hdr).into());
        }

        let missing = read_varint_u32(w.get_mut()).await? as usize;
        let mut total = 0u64;
        for _ in 0..missing { total += Self::consume_image(w.get_mut()).await?; }
        Ok((total, 1, missing))
    }

    async fn download_list_and_get(&self) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        let stream = self.connect().await?;
        let mut w  = BufWriter::with_capacity(64 * 1024, stream);

        write_list_and_get_request_buffered(&mut w, 0).await?;
        w.flush().await?;

        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await?;
        if &hdr != RESPONSE_LIST_AND_GET {
            return Err(format!("unexpected LIST_AND_GET header: {:?}", hdr).into());
        }

        let count = read_varint_u32(w.get_mut()).await? as usize;
        let mut total = 0u64;
        for _ in 0..count { total += Self::consume_image(w.get_mut()).await?; }
        Ok((total, 1))
    }

    /// §8.5 CANCEL + §9.2 JTPD + §9.6 JTPC.
    async fn download_with_cancel(
        &self,
        ids:          &[ImageId],
        cancel_after: usize,
    ) -> Result<(u64, Duration, usize), Box<dyn std::error::Error + Send + Sync>> {
        if ids.is_empty() { return Ok((0, Duration::ZERO, 0)); }

        let chunk  = &ids[..ids.len().min(255)];
        let stream = self.connect().await?;
        let mut w  = BufWriter::with_capacity(64 * 1024, stream);

        write_get_request_buffered(&mut w, REQUEST_FLAG_KEEP_ALIVE, chunk).await?;
        w.flush().await?;

        // §9.2: read JTPD header + M count
        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await?;
        if &hdr != RESPONSE_GET_BY_ID {
            return Err(format!("unexpected GET_BY_ID header: {:?}", hdr).into());
        }
        let m        = w.get_mut().read_u8().await? as usize;
        let received = cancel_after.min(m);

        let mut bytes_before = 0u64;
        for _ in 0..received {
            bytes_before += Self::consume_image(w.get_mut()).await?;
        }

        // Send CANCEL and wait for JTPC acknowledgement.
        let cancel_start = Instant::now();
        write_cancel_request_buffered(w.get_mut()).await?;
        w.flush().await?;

        // Drain in-flight image bytes until JTPC 4-byte magic appears.
        let mut ack_buf = [0u8; 4];
        let mut pos = 0;
        while pos < 4 {
            let b = w.get_mut().read_u8().await?;
            if pos == 0 && b != RESPONSE_CANCEL[0] { continue; }
            if b == RESPONSE_CANCEL[pos] { ack_buf[pos] = b; pos += 1; } else { pos = 0; }
        }

        let cancel_rtt = cancel_start.elapsed();
        if &ack_buf != RESPONSE_CANCEL {
            return Err(format!("unexpected CANCEL ack: {:?}", ack_buf).into());
        }

        Ok((bytes_before, cancel_rtt, received))
    }

    /// §8.6 WATCH + §9.7 JTPW + §8.5 CANCEL to clean up.
    async fn watch_time_to_first_event(
        &self,
        timeout: Duration,
    ) -> Result<Duration, Box<dyn std::error::Error + Send + Sync>> {
        let stream = self.connect().await?;
        let mut w  = BufWriter::with_capacity(64 * 1024, stream);

        write_watch_request_buffered(w.get_mut()).await?;
        w.flush().await?;

        let start = Instant::now();

        let result = tokio::time::timeout(timeout, async {
            let mut hdr = [0u8; 4];
            w.get_mut().read_exact(&mut hdr).await?;
            if &hdr != RESPONSE_WATCH {
                return Err::<_, Box<dyn std::error::Error + Send + Sync>>(
                    format!("unexpected WATCH frame: {:?}", hdr).into()
                );
            }
            let _id      = w.get_mut().read_u64().await?;
            let _flags   = w.get_mut().read_u8().await?;
            let name_len = w.get_mut().read_u16().await? as usize;
            let mut name = vec![0u8; name_len];
            w.get_mut().read_exact(&mut name).await?;
            let _size    = read_varint_u32(w.get_mut()).await?;
            Ok(())
        })
        .await;

        let elapsed = start.elapsed();

        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(format!(
                "No WATCH event within {:?}. \
                 Is the server running with --watch?", timeout
            ).into()),
        }

        let _ = write_cancel_request_buffered(w.get_mut()).await;
        let _ = w.flush().await;
        Ok(elapsed)
    }
}

// ============================================================================
// HTTP client
// ============================================================================

async fn download_http(
    client:    &reqwest::Client,
    base_url:  &str,
    filenames: &[String],
) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
    let mut total = 0u64;
    for filename in filenames {
        let url   = format!("{}/image/{}", base_url, filename);
        let resp  = client.get(&url).send().await?;
        let bytes = resp.bytes().await?;
        total += bytes.len() as u64;
    }
    Ok((total, filenames.len()))
}

// ============================================================================
// Output helpers
// ============================================================================

fn print_header(msg: &str) {
    println!();
    println!("{}", "=".repeat(70).bright_white().bold());
    println!("{}", msg.cyan().bold());
    println!("{}", "=".repeat(70).bright_white().bold());
}

fn print_subheader(msg: &str) {
    println!();
    println!("{}", msg.blue().bold());
    println!("{}", "-".repeat(50).blue());
}

fn print_success(msg: &str) {
    println!("    {} {}", "OK".green(), msg);
}

fn print_info(msg: &str) {
    println!("  {}", msg.yellow());
}

fn print_run(run: usize, total: usize, duration_ms: f64) {
    println!(
        "  {} {}/{}... {} {:.2} ms",
        "Run".yellow(), run, total, "OK".green(), duration_ms
    );
}

// ============================================================================
// Argument parsing
// ============================================================================

fn parse_args() -> BenchmarkConfig {
    let mut config = BenchmarkConfig::default();
    let mut args   = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--jtp-addr"   => { if let Some(v) = args.next() { config.jtp_addr  = v; } }
            "--http-addr"  => { if let Some(v) = args.next() { config.http_addr = v; } }
            "--cert"       => { if let Some(v) = args.next() { config.cert_path = v; } }
            "--images"     => {
                if let Some(v) = args.next() { config.test_images_dir = PathBuf::from(v); }
            }
            "--warmup"     => {
                if let Some(v) = args.next() { config.warmup_iterations = v.parse().unwrap_or(5); }
            }
            "--iterations" | "-n" => {
                if let Some(v) = args.next() { config.test_iterations = v.parse().unwrap_or(10); }
            }
            "--parallel" | "-p" => {
                if let Some(v) = args.next() { config.parallel_workers = v.parse().unwrap_or(4); }
            }
            "--cancel-after" => {
                if let Some(v) = args.next() { config.cancel_after = v.parse().unwrap_or(1); }
            }
            "--watch-timeout" => {
                if let Some(v) = args.next() { config.watch_timeout_ms = v.parse().unwrap_or(2000); }
            }
            "--tls" | "--secure"    => { config.no_tls = false; }
            "--no-tls" | "--plain"  => { config.no_tls = true;  }
            "--http-tls" | "--https" => {
                config.http_tls = true;
                if config.http_addr == "http://127.0.0.1:8080" {
                    config.http_addr = "https://127.0.0.1:8443".to_string();
                }
            }
            "--mode" => {
                if let Some(v) = args.next() {
                    config.modes = match v.as_str() {
                        "http"                      => vec![BenchmarkMode::Http],
                        "per-image"                 => vec![BenchmarkMode::JtpPerImage],
                        "batch"                     => vec![BenchmarkMode::JtpBatch],
                        "keepalive" | "ka"          => vec![BenchmarkMode::JtpKeepAlive],
                        "parallel"                  => vec![BenchmarkMode::JtpParallel],
                        "delta"                     => vec![BenchmarkMode::JtpDelta],
                        "list-and-get" | "lag"      => vec![BenchmarkMode::JtpListAndGet],
                        "cancel"                    => vec![BenchmarkMode::JtpCancel],
                        "watch"                     => vec![BenchmarkMode::JtpWatch],
                        "jtp" => vec![
                            BenchmarkMode::JtpPerImage,
                            BenchmarkMode::JtpBatch,
                            BenchmarkMode::JtpKeepAlive,
                            BenchmarkMode::JtpParallel,
                            BenchmarkMode::JtpListAndGet,
                            BenchmarkMode::JtpCancel,
                        ],
                        "all" => vec![
                            BenchmarkMode::Http,
                            BenchmarkMode::JtpPerImage,
                            BenchmarkMode::JtpBatch,
                            BenchmarkMode::JtpKeepAlive,
                            BenchmarkMode::JtpParallel,
                            BenchmarkMode::JtpDelta,
                            BenchmarkMode::JtpListAndGet,
                            BenchmarkMode::JtpCancel,
                            BenchmarkMode::JtpWatch,
                        ],
                        _ => config.modes,
                    };
                }
            }
            "-h" | "--help" => {
                eprintln!(
                    "JTP vs HTTP Performance Benchmark (draft-baker-jtp-01)\n\n\
Usage: benchmark [OPTIONS]\n\n\
Options:\n  \
  --jtp-addr ADDR       JTP server address (default: 127.0.0.1:8443)\n  \
  --http-addr URL       HTTP server address (default: http://127.0.0.1:8080)\n  \
  --cert PATH           TLS certificate for JTP (default: cert.pem)\n  \
  --images DIR          Test images directory (default: images/)\n  \
  --warmup N            Warmup iterations (default: 5)\n  \
  --iterations N        Test iterations (default: 10)\n  \
  --parallel N          Parallel workers for JTP-Parallel mode (default: 4)\n  \
  --cancel-after N      Packets to receive before CANCEL (default: 1)\n  \
  --watch-timeout MS    Timeout for first WATCH event in ms (default: 2000)\n  \
  --mode MODE           One of: http, per-image, batch, keepalive, parallel,\n\
                              delta, list-and-get, cancel, watch, jtp, all\n  \
  --tls / --no-tls      Force TLS or plain TCP (auto-detected by default)\n  \
  --http-tls / --https  Use HTTPS for the HTTP server\n  \
  --help                Show this help\n\n\
Benchmark Modes:\n  \
  HTTP           Download via HTTP/HTTPS (one request per image)\n  \
  JTP Per-Image  New connection per image (worst case)\n  \
  JTP Batch      Single connection, batch GET_BY_ID\n  \
  JTP Keep-Alive Reuse connection with keep-alive flag\n  \
  JTP Parallel   Multiple parallel workers with keep-alive\n  \
  JTP Delta      BATCH sync (only download missing images)\n  \
  JTP List+Get   Combined LIST+GET in single round-trip (fastest)\n  \
  JTP Cancel     GET_BY_ID then CANCEL mid-stream; measures abort RTT\n  \
  JTP Watch      WATCH subscription; measures time-to-first-event\n\n\
Prerequisites:\n  \
  JTP server:        cargo run --release --bin server -- --images images/\n  \
  JTP watch server:  cargo run --release --bin server -- --images images/ --watch\n  \
  HTTP server:       node examples/benchmark/http/server.js\n  \
  HTTPS server:      node examples/benchmark/http/server-https.js"
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }

    config
}

// ============================================================================
// Main
// ============================================================================

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config = parse_args();

    print_header("JTP vs HTTP Image Download Benchmark (draft-baker-jtp-01)");

    println!();
    print_info(&format!("Test images directory: {:?}", config.test_images_dir));
    print_info(&format!("HTTP server:  {}", config.http_addr));
    print_info(&format!("JTP server:   {}", config.jtp_addr));
    print_info(&format!("Modes:        {:?}", config.modes.iter().map(|m| m.short_name()).collect::<Vec<_>>()));
    print_info(&format!("Warmup runs:  {}", config.warmup_iterations));
    print_info(&format!("Bench runs:   {}", config.test_iterations));
    print_info(&format!("Parallel workers: {}", config.parallel_workers));
    print_info(&format!("Cancel after: {} packet(s)", config.cancel_after));
    print_info(&format!("Watch timeout: {} ms", config.watch_timeout_ms));

    // Collect image filenames from disk.
    let image_files: Vec<String> = std::fs::read_dir(&config.test_images_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let path = e.path();
            if let Some(ext) = path.extension() {
                matches!(ext.to_string_lossy().to_lowercase().as_str(), "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp")
            } else { false }
        })
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();

    if image_files.is_empty() {
        eprintln!("{} No test images found in {:?}", "Error:".red().bold(), config.test_images_dir);
        return Ok(());
    }

    let total_size: u64 = image_files.iter()
        .filter_map(|f| std::fs::metadata(config.test_images_dir.join(f)).ok().map(|m| m.len()))
        .sum();

    println!();
    print_info(&format!("Found {} test images ({:.2} KB total):", image_files.len(), (total_size as f64) / 1024.0));
    for img in &image_files {
        if let Ok(meta) = std::fs::metadata(config.test_images_dir.join(img)) {
            println!("    - {} ({:.2} KB)", img, (meta.len() as f64) / 1024.0);
        }
    }

    let has_http_mode = config.modes.contains(&BenchmarkMode::Http);
    let has_jtp_mode  = config.modes.iter().any(|m| *m != BenchmarkMode::Http);
    let has_watch_mode = config.modes.contains(&BenchmarkMode::JtpWatch);

    // HTTP client (accepts self-signed certs for HTTPS auto-detection).
    let http_client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()?;

    // Auto-detect HTTP server if needed.
    let mut http_addr = config.http_addr.clone();
    if has_http_mode {
        let try_addrs = [
            config.http_addr.clone(),
            "http://127.0.0.1:8080".to_string(),
            "https://127.0.0.1:8080".to_string(),
        ];
        let mut found = false;
        for addr in &try_addrs {
            if http_client.get(format!("{}/list", addr)).send().await.is_ok() {
                let proto = if addr.starts_with("https://") { "HTTPS" } else { "HTTP" };
                if addr == &config.http_addr {
                    print_success(&format!("{} server running at {}", proto, addr));
                } else {
                    print_success(&format!("{} server auto-detected at {}", proto, addr));
                }
                http_addr = addr.clone();
                found = true;
                break;
            }
        }
        if !found {
            eprintln!("{} No HTTP server found. Tried:", "Error:".red().bold());
            for addr in &try_addrs { eprintln!("    - {}", addr); }
            eprintln!();
            eprintln!("  Start HTTP:  node examples/benchmark/http/server.js");
            eprintln!("  Start HTTPS: node examples/benchmark/http/server-https.js");
            return Ok(());
        }
    }

    // Auto-detect JTP server transport.
    println!();
    print_info("Checking JTP server...");
    let jtp_client: Option<JtpClientWrapper> = if has_jtp_mode {
        match JtpClientWrapper::auto_detect(&config.jtp_addr, &config.cert_path).await {
            Ok((client, is_tls)) => {
                match client.list_images().await {
                    Ok(images) => {
                        let mode = if is_tls { "TLS" } else { "plain TCP" };
                        print_success(&format!("JTP server: {} images, {}", images.len(), mode));
                        if has_watch_mode {
                            print_info("Note: JtpWatch requires server started with --watch");
                        }
                        Some(client)
                    }
                    Err(e) => {
                        eprintln!("{} JTP server error: {}", "Error:".red().bold(), e);
                        return Ok(());
                    }
                }
            }
            Err(e) => {
                eprintln!("{} Cannot connect to JTP server at {}: {}", "Error:".red().bold(), config.jtp_addr, e);
                eprintln!();
                eprintln!("  Start with TLS:  cargo run --release --bin server -- --images images/");
                eprintln!("  Start plain TCP: cargo run --release --bin server -- --no-tls --images images/");
                return Ok(());
            }
        }
    } else {
        None
    };

    // Fetch the catalog once for all JTP modes.
    let jtp_images: Vec<ListedImage> = if let Some(ref c) = jtp_client {
        c.list_images().await?
    } else {
        vec![]
    };
    let jtp_ids: Vec<ImageId> = jtp_images.iter().map(|i| i.id).collect();

    // Delta: simulate having the first half already.
    let have_ids:      Vec<ImageId>         = jtp_ids.iter().take(jtp_ids.len() / 2).copied().collect();
    let have_set:      HashSet<ImageId>     = have_ids.iter().copied().collect();
    let missing_count: usize                = jtp_ids.iter().filter(|id| !have_set.contains(id)).count();

    let watch_timeout = Duration::from_millis(config.watch_timeout_ms);
    let mut results: Vec<BenchmarkResult> = Vec::new();

    // ── Run each mode ─────────────────────────────────────────────────────────

    for mode in &config.modes {
        print_subheader(&format!("Running {} benchmark...", mode.name()));

        // Warmup (silent)
        print_info(&format!("Warmup ({} runs)...", config.warmup_iterations));
        for _ in 0..config.warmup_iterations {
            match mode {
                BenchmarkMode::Http => {
                    let _ = download_http(&http_client, &http_addr, &image_files).await;
                }
                BenchmarkMode::JtpPerImage => {
                    if let Some(ref c) = jtp_client { let _ = c.download_per_image(&jtp_ids).await; }
                }
                BenchmarkMode::JtpBatch => {
                    if let Some(ref c) = jtp_client { let _ = c.download_batch(&jtp_ids).await; }
                }
                BenchmarkMode::JtpKeepAlive => {
                    if let Some(ref c) = jtp_client { let _ = c.download_keepalive(&jtp_ids).await; }
                }
                BenchmarkMode::JtpParallel => {
                    if let Some(ref c) = jtp_client {
                        let _ = c.download_parallel(&jtp_ids, config.parallel_workers).await;
                    }
                }
                BenchmarkMode::JtpDelta => {
                    if let Some(ref c) = jtp_client { let _ = c.download_delta(&have_ids).await; }
                }
                BenchmarkMode::JtpListAndGet => {
                    if let Some(ref c) = jtp_client { let _ = c.download_list_and_get().await; }
                }
                // Cancel and Watch have side-effects; skip warmup to avoid
                // disrupting server state.
                BenchmarkMode::JtpCancel | BenchmarkMode::JtpWatch => {}
            }
        }
        print_success("Warmup complete");

        let mut durations      = Vec::with_capacity(config.test_iterations);
        let mut total_bytes    = 0u64;
        let mut total_conns    = 0usize;
        let mut mode_note: Option<String> = None;

        for run in 1..=config.test_iterations {
            let start = Instant::now();

            let (bytes, conns) = match mode {
                BenchmarkMode::Http => {
                    download_http(&http_client, &http_addr, &image_files).await?
                }
                BenchmarkMode::JtpPerImage => {
                    if let Some(ref c) = jtp_client { c.download_per_image(&jtp_ids).await? } else { (0, 0) }
                }
                BenchmarkMode::JtpBatch => {
                    if let Some(ref c) = jtp_client { c.download_batch(&jtp_ids).await? } else { (0, 0) }
                }
                BenchmarkMode::JtpKeepAlive => {
                    if let Some(ref c) = jtp_client { c.download_keepalive(&jtp_ids).await? } else { (0, 0) }
                }
                BenchmarkMode::JtpParallel => {
                    if let Some(ref c) = jtp_client {
                        c.download_parallel(&jtp_ids, config.parallel_workers).await?
                    } else { (0, 0) }
                }
                BenchmarkMode::JtpDelta => {
                    if let Some(ref c) = jtp_client {
                        let (b, conn, _) = c.download_delta(&have_ids).await?;
                        (b, conn)
                    } else { (0, 0) }
                }
                BenchmarkMode::JtpListAndGet => {
                    if let Some(ref c) = jtp_client { c.download_list_and_get().await? } else { (0, 0) }
                }

                // JtpCancel: measures cancel RTT, not full transfer time.
                // `durations` here captures total elapsed which includes the
                // partial transfer; the note records the CANCEL RTT separately.
                BenchmarkMode::JtpCancel => {
                    if let Some(ref c) = jtp_client {
                        let (b, cancel_rtt, received) =
                            c.download_with_cancel(&jtp_ids, config.cancel_after).await?;
                        mode_note = Some(format!(
                            "Received {} packet(s) then cancelled (CANCEL RTT: {:.2} ms)",
                            received,
                            cancel_rtt.as_secs_f64() * 1000.0,
                        ));
                        (b, 1)
                    } else { (0, 0) }
                }

                // JtpWatch: measures time-to-first-event; uses durations as
                // the time series. `total_bytes` is 0 (no image data transferred).
                BenchmarkMode::JtpWatch => {
                    if let Some(ref c) = jtp_client {
                        let ttfe = c.watch_time_to_first_event(watch_timeout).await?;
                        let elapsed = start.elapsed();
                        durations.push(elapsed);
                        print_run(run, config.test_iterations, ttfe.as_secs_f64() * 1000.0);
                        mode_note = Some(format!(
                            "Time-to-first-event: {:.2} ms",
                            ttfe.as_secs_f64() * 1000.0,
                        ));
                        continue; // skip the generic push below
                    } else { (0, 0) }
                }
            };

            let elapsed   = start.elapsed();
            total_bytes   = bytes;
            total_conns   = conns;
            durations.push(elapsed);
            print_run(run, config.test_iterations, elapsed.as_secs_f64() * 1000.0);
        }

        // Mode-specific info lines
        if *mode == BenchmarkMode::JtpDelta {
            print_info(&format!(
                "Delta sync: had {} images, server had {} missing",
                have_ids.len(), missing_count,
            ));
        }

        results.push(BenchmarkResult {
            mode:             *mode,
            durations,
            total_bytes,
            connections_made: total_conns,
            note:             mode_note,
        });
    }

    // ── Results ───────────────────────────────────────────────────────────────

    print_header("BENCHMARK RESULTS");

    for result in &results {
        let stats = result.stats();
        print_subheader(&format!("{} Results:", result.mode.name()));

        println!("  {} {:.2} ms", "Average time:".green(), stats.mean);
        println!("  {} {:.2} ms", "Median time: ".green(), stats.median);
        println!("  {} {:.2} ms", "Min time:    ".green(), stats.min);
        println!("  {} {:.2} ms", "Max time:    ".green(), stats.max);
        println!("  {} {:.2} ms", "Std dev:     ".green(), stats.std_dev);

        // Throughput is only meaningful for modes that transfer full images.
        if !matches!(result.mode, BenchmarkMode::JtpWatch) {
            println!("  {} {:.2} KB/s", "Throughput:  ".cyan(), result.throughput_kbps());
        }
        println!("  {} {}", "Connections: ".cyan(), result.connections_made);

        if let Some(ref note) = result.note {
            println!("  {} {}", "Note:        ".cyan(), note);
        }
    }

    // Comparison table — exclude Watch (incomparable units).
    let comparable: Vec<&BenchmarkResult> = results.iter()
        .filter(|r| r.mode != BenchmarkMode::JtpWatch)
        .collect();

    if comparable.len() > 1 {
        print_subheader("Comparison (transfer modes):");

        println!();
        println!(
            "  {:12} | {:>10} | {:>8} | {:>9} | {:>9} | {:>10} | {:>5}",
            "Mode", "Avg Time", "Median", "Min", "Max", "Throughput", "Conns"
        );
        println!("  {}", "-".repeat(75));

        let fastest = comparable.iter()
            .min_by(|a, b| a.stats().mean.partial_cmp(&b.stats().mean).unwrap())
            .map(|r| r.mode);

        for result in &comparable {
            let stats    = result.stats();
            let is_best  = Some(result.mode) == fastest;
            let marker   = if is_best { " <-" } else { "" };
            println!(
                "  {:12} | {:>7.2} ms | {:>5.2} ms | {:>6.2} ms | {:>6.2} ms | {:>7.0} KB/s | {:>5}{}",
                result.mode.short_name(),
                stats.mean, stats.median, stats.min, stats.max,
                result.throughput_kbps(),
                result.connections_made,
                marker.green().bold()
            );
        }

        print_subheader("Relative performance:");
        if let Some(fastest_result) = comparable.iter()
            .min_by(|a, b| a.stats().mean.partial_cmp(&b.stats().mean).unwrap())
        {
            let base = fastest_result.stats().mean;
            for result in &comparable {
                let stats = result.stats();
                if result.mode == fastest_result.mode {
                    println!("  {}: {} (fastest)", result.mode.short_name().bright_green().bold(), "baseline".green());
                } else {
                    let ratio      = stats.mean / base;
                    let slower_pct = (ratio - 1.0) * 100.0;
                    println!("  {}: {:.2}x slower ({:.1}% slower)", result.mode.short_name().yellow(), ratio, slower_pct);
                }
            }
        }

        println!();
        println!("  {} {:.2} KB", "Total data per run:".cyan(), (total_size as f64) / 1024.0);
    }

    // WATCH summary (separate section, different unit).
    let watch_results: Vec<&BenchmarkResult> = results.iter()
        .filter(|r| r.mode == BenchmarkMode::JtpWatch)
        .collect();

    if !watch_results.is_empty() {
        print_subheader("WATCH time-to-first-event:");
        for result in watch_results {
            let stats = result.stats();
            println!("  {} {:.2} ms", "Mean TTFE: ".green(), stats.mean);
            println!("  {} {:.2} ms", "Min TTFE:  ".green(), stats.min);
            println!("  {} {:.2} ms", "Max TTFE:  ".green(), stats.max);
        }
    }

    println!();
    println!("{}", "Benchmark complete!".bright_green().bold());

    Ok(())
}