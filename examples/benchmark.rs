//! JTP vs HTTP Performance Benchmark
//!
//! This benchmark compares JTP performance against HTTP using direct client code
//! (no spawning of external binaries).
//!
//! Run with: cargo run --release --example benchmark -- [OPTIONS]
//!
//! Prerequisites:
//! - JTP server: cargo run --release --bin server -- --images images/
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
    read_varint_u32, write_batch_request_buffered, write_get_request_buffered,
    write_list_and_get_request_buffered, write_list_request_buffered, ImageId,
    REQUEST_FLAG_KEEP_ALIVE, RESPONSE_BATCH, RESPONSE_LIST, RESPONSE_LIST_AND_GET,
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
    jtp_addr: String,
    http_addr: String,
    cert_path: String,
    test_images_dir: PathBuf,
    warmup_iterations: usize,
    test_iterations: usize,
    parallel_workers: usize,
    modes: Vec<BenchmarkMode>,
    no_tls: bool,   // Use plain TCP for JTP
    http_tls: bool, // Use HTTPS for HTTP server (server-https.js)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum BenchmarkMode {
    Http,
    JtpPerImage,   // New connection per image (worst case)
    JtpBatch,      // Single connection, batch request
    JtpKeepAlive,  // Reuse connection with keep-alive flag
    JtpParallel,   // Multiple parallel workers
    JtpDelta,      // BATCH/delta sync mode
    JtpListAndGet, // Combined LIST+GET in single round-trip (fastest)
}

impl BenchmarkMode {
    fn name(&self) -> &'static str {
        match self {
            Self::Http => "HTTP",
            Self::JtpPerImage => "JTP Per-Image",
            Self::JtpBatch => "JTP Batch",
            Self::JtpKeepAlive => "JTP Keep-Alive",
            Self::JtpParallel => "JTP Parallel",
            Self::JtpDelta => "JTP Delta",
            Self::JtpListAndGet => "JTP List+Get",
        }
    }

    fn short_name(&self) -> &'static str {
        match self {
            Self::Http => "HTTP",
            Self::JtpPerImage => "JTP-1conn",
            Self::JtpBatch => "JTP-Batch",
            Self::JtpKeepAlive => "JTP-KA",
            Self::JtpParallel => "JTP-Par",
            Self::JtpDelta => "JTP-Delta",
            Self::JtpListAndGet => "JTP-L+G",
        }
    }
}

impl Default for BenchmarkConfig {
    fn default() -> Self {
        Self {
            jtp_addr: "127.0.0.1:8443".to_string(),
            http_addr: "http://127.0.0.1:8080".to_string(),
            cert_path: "cert.pem".to_string(),
            test_images_dir: PathBuf::from("images"),
            warmup_iterations: 5,
            test_iterations: 10,
            parallel_workers: 4,
            modes: vec![
                BenchmarkMode::Http,
                BenchmarkMode::JtpPerImage,
                BenchmarkMode::JtpBatch,
                BenchmarkMode::JtpKeepAlive,
                BenchmarkMode::JtpParallel,
                BenchmarkMode::JtpListAndGet,
            ],
            no_tls: true,    // Plain TCP by default for JTP
            http_tls: false, // Plain HTTP by default
        }
    }
}

// ============================================================================
// Statistics
// ============================================================================

#[derive(Debug, Clone)]
struct Stats {
    mean: f64,
    median: f64,
    min: f64,
    max: f64,
    std_dev: f64,
}

impl Stats {
    fn from_durations(durations: &[Duration]) -> Self {
        let values: Vec<f64> = durations.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
        Self::from_values(&values)
    }

    fn from_values(values: &[f64]) -> Self {
        if values.is_empty() {
            return Self {
                mean: 0.0,
                median: 0.0,
                min: 0.0,
                max: 0.0,
                std_dev: 0.0,
            };
        }

        let mut sorted = values.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());

        let sum: f64 = values.iter().sum();
        let mean = sum / (values.len() as f64);
        let median = sorted[sorted.len() / 2];
        let min = sorted[0];
        let max = sorted[sorted.len() - 1];

        let variance: f64 =
            values.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (values.len() as f64);
        let std_dev = variance.sqrt();

        Self {
            mean,
            median,
            min,
            max,
            std_dev,
        }
    }
}

// ============================================================================
// Benchmark Results
// ============================================================================

#[derive(Debug, Clone)]
struct BenchmarkResult {
    mode: BenchmarkMode,
    durations: Vec<Duration>,
    total_bytes: u64,
    connections_made: usize,
}

impl BenchmarkResult {
    fn stats(&self) -> Stats {
        Stats::from_durations(&self.durations)
    }

    fn throughput_kbps(&self) -> f64 {
        let stats = self.stats();
        if stats.mean == 0.0 {
            return 0.0;
        }
        (self.total_bytes as f64) / 1024.0 / (stats.mean / 1000.0)
    }
}

// ============================================================================
// JTP Client (embedded) - Plain TCP version for fair HTTP comparison
// ============================================================================

#[derive(Clone, Debug)]
struct ListedImage {
    id: ImageId,
    #[allow(dead_code)]
    flags: u8,
    #[allow(dead_code)]
    filename: String,
    #[allow(dead_code)]
    size: u32,
}

/// Plain TCP JTP client (no TLS) - fair comparison with HTTP
struct PlainJtpClient {
    addr: String,
}

impl PlainJtpClient {
    fn new(addr: &str) -> Self {
        Self {
            addr: addr.to_string(),
        }
    }

    async fn connect(&self) -> Result<TcpStream, Box<dyn std::error::Error + Send + Sync>> {
        let tcp = TcpStream::connect(&self.addr).await?;
        tcp.set_nodelay(true)?;
        Ok(tcp)
    }

    async fn list_images(
        &self,
    ) -> Result<Vec<ListedImage>, Box<dyn std::error::Error + Send + Sync>> {
        let stream = self.connect().await?;
        let mut writer = BufWriter::with_capacity(64 * 1024, stream);

        write_list_request_buffered(&mut writer, 0).await?;
        writer.flush().await?;

        let mut header = [0u8; 4];
        writer.get_mut().read_exact(&mut header).await?;
        if &header != RESPONSE_LIST {
            // Check if we got TLS handshake bytes (server running in TLS mode)
            if header[0] == 0x15 || header[0] == 0x16 || header[0] == 0x17 {
                return Err(
                    "Received TLS handshake - server is running in TLS mode. Use --tls flag or start server with --no-tls".into()
                );
            }
            return Err(
                format!(
                    "Invalid LIST response: got {:02x?} (expected JTPL). Is server running with --no-tls?",
                    header
                ).into()
            );
        }

        let count = writer.get_mut().read_u16().await? as usize;
        let mut images = Vec::with_capacity(count);

        for _ in 0..count {
            let id = writer.get_mut().read_u64().await?;
            let flags = writer.get_mut().read_u8().await?;
            let name_len = writer.get_mut().read_u16().await? as usize;
            let mut name_bytes = vec![0u8; name_len];
            writer.get_mut().read_exact(&mut name_bytes).await?;
            let filename = String::from_utf8_lossy(&name_bytes).to_string();
            let size = read_varint_u32(writer.get_mut()).await?;
            images.push(ListedImage {
                id,
                flags,
                filename,
                size,
            });
        }

        Ok(images)
    }

    async fn consume_image(
        stream: &mut TcpStream,
    ) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        let _flags = stream.read_u8().await?;
        let length = read_varint_u32(stream).await?;
        let _id = stream.read_u64().await?;

        let mut remaining = length as usize;
        let mut buf = vec![0u8; 65536];
        while remaining > 0 {
            let to_read = remaining.min(buf.len());
            stream.read_exact(&mut buf[..to_read]).await?;
            remaining -= to_read;
        }

        Ok(length as u64)
    }

    async fn download_per_image(
        &self,
        ids: &[ImageId],
    ) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        let mut total_bytes = 0u64;
        let connections = ids.len();

        for id in ids {
            let stream = self.connect().await?;
            let mut writer = BufWriter::with_capacity(64 * 1024, stream);

            write_get_request_buffered(&mut writer, 0, &[*id]).await?;
            writer.flush().await?;

            total_bytes += Self::consume_image(writer.get_mut()).await?;
        }

        Ok((total_bytes, connections))
    }

    async fn download_batch(
        &self,
        ids: &[ImageId],
    ) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        let stream = self.connect().await?;
        let mut writer = BufWriter::with_capacity(64 * 1024, stream);
        let mut total_bytes = 0u64;
        let mut connections = 1;

        for chunk in ids.chunks(255) {
            if chunk.as_ptr() != ids.as_ptr() {
                drop(writer);
                let stream = self.connect().await?;
                writer = BufWriter::with_capacity(64 * 1024, stream);
                connections += 1;
            }

            write_get_request_buffered(&mut writer, 0, chunk).await?;
            writer.flush().await?;

            for _ in chunk {
                total_bytes += Self::consume_image(writer.get_mut()).await?;
            }
        }

        Ok((total_bytes, connections))
    }

    async fn download_keepalive(
        &self,
        ids: &[ImageId],
    ) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        let stream = self.connect().await?;
        let mut writer = BufWriter::with_capacity(64 * 1024, stream);
        let mut total_bytes = 0u64;

        for chunk in ids.chunks(255) {
            write_get_request_buffered(&mut writer, REQUEST_FLAG_KEEP_ALIVE, chunk).await?;
            writer.flush().await?;

            for _ in chunk {
                total_bytes += Self::consume_image(writer.get_mut()).await?;
            }
        }

        Ok((total_bytes, 1))
    }

    async fn download_parallel(
        &self,
        ids: &[ImageId],
        num_workers: usize,
    ) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        let chunk_size = (ids.len() + num_workers - 1) / num_workers;
        let semaphore = Arc::new(Semaphore::new(num_workers));
        let addr = self.addr.clone();

        let mut handles = Vec::new();

        for chunk in ids.chunks(chunk_size) {
            let chunk = chunk.to_vec();
            let semaphore = Arc::clone(&semaphore);
            let addr = addr.clone();

            handles.push(tokio::spawn(async move {
                let _permit = semaphore.acquire().await.unwrap();

                let tcp = TcpStream::connect(&addr).await?;
                tcp.set_nodelay(true)?;
                let mut writer = BufWriter::with_capacity(64 * 1024, tcp);
                let mut bytes = 0u64;

                for sub_chunk in chunk.chunks(255) {
                    write_get_request_buffered(&mut writer, REQUEST_FLAG_KEEP_ALIVE, sub_chunk)
                        .await?;
                    writer.flush().await?;

                    for _ in sub_chunk {
                        bytes += Self::consume_image(writer.get_mut()).await?;
                    }
                }

                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(bytes)
            }));
        }

        let mut total_bytes = 0u64;
        for handle in handles {
            total_bytes += handle.await??;
        }

        Ok((total_bytes, num_workers))
    }

    async fn download_delta(
        &self,
        have_ids: &[ImageId],
    ) -> Result<(u64, usize, usize), Box<dyn std::error::Error + Send + Sync>> {
        let stream = self.connect().await?;
        let mut writer = BufWriter::with_capacity(64 * 1024, stream);

        write_batch_request_buffered(&mut writer, 0, have_ids).await?;
        writer.flush().await?;

        let mut header = [0u8; 4];
        writer.get_mut().read_exact(&mut header).await?;
        if &header != RESPONSE_BATCH {
            return Err("Invalid BATCH response".into());
        }

        let missing_count = read_varint_u32(writer.get_mut()).await? as usize;
        let mut total_bytes = 0u64;

        for _ in 0..missing_count {
            total_bytes += Self::consume_image(writer.get_mut()).await?;
        }

        Ok((total_bytes, 1, missing_count))
    }

    async fn download_list_and_get(
        &self,
    ) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        let stream = self.connect().await?;
        let mut writer = BufWriter::with_capacity(64 * 1024, stream);

        write_list_and_get_request_buffered(&mut writer, 0).await?;
        writer.flush().await?;

        let mut header = [0u8; 4];
        writer.get_mut().read_exact(&mut header).await?;
        if &header != RESPONSE_LIST_AND_GET {
            return Err(format!("Invalid LIST_AND_GET response: {:?}", header).into());
        }

        let count = writer.get_mut().read_u16().await? as usize;
        let mut total_bytes = 0u64;

        for _ in 0..count {
            total_bytes += Self::consume_image(writer.get_mut()).await?;
        }

        Ok((total_bytes, 1))
    }
}

// ============================================================================
// JTP Client Wrapper (handles both Plain and TLS modes)
// ============================================================================

enum JtpClientWrapper {
    Plain(PlainJtpClient),
    Tls(JtpClient),
}

impl JtpClientWrapper {
    async fn new_plain(addr: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let client = PlainJtpClient::new(addr);
        // Test connection
        client.connect().await?;
        Ok(Self::Plain(client))
    }

    async fn new_tls(
        addr: &str,
        cert_path: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let client = JtpClient::new(addr, cert_path).await?;
        Ok(Self::Tls(client))
    }

    /// Auto-detect whether server is running in TLS or plain TCP mode
    async fn auto_detect(
        addr: &str,
        cert_path: &str,
    ) -> Result<(Self, bool), Box<dyn std::error::Error + Send + Sync>> {
        // Try plain TCP first - send LIST request and check response
        let tcp = TcpStream::connect(addr).await?;
        tcp.set_nodelay(true)?;
        let mut writer = BufWriter::with_capacity(64 * 1024, tcp);

        write_list_request_buffered(&mut writer, 0).await?;
        writer.flush().await?;

        let mut header = [0u8; 4];
        writer.get_mut().read_exact(&mut header).await?;

        // Check if we got a valid JTP response
        if &header == RESPONSE_LIST {
            // Plain TCP works! But we need to create a fresh client
            // (this connection already consumed the LIST response)
            drop(writer);
            let client = PlainJtpClient::new(addr);
            return Ok((Self::Plain(client), false));
        }

        // Check if we got TLS handshake bytes (0x15=alert, 0x16=handshake, 0x17=app data)
        if header[0] == 0x15 || header[0] == 0x16 || header[0] == 0x17 {
            // Server is running TLS, try TLS connection
            drop(writer);
            let client = JtpClient::new(addr, cert_path).await?;
            return Ok((Self::Tls(client), true));
        }

        // Unknown response
        Err(format!(
            "Unknown server response: {:02x?}. Expected JTP (JTPL) or TLS handshake.",
            header
        )
        .into())
    }

    async fn list_images(
        &self,
    ) -> Result<Vec<ListedImage>, Box<dyn std::error::Error + Send + Sync>> {
        match self {
            Self::Plain(c) => c.list_images().await,
            Self::Tls(c) => c.list_images().await,
        }
    }

    async fn download_per_image(
        &self,
        ids: &[ImageId],
    ) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        match self {
            Self::Plain(c) => c.download_per_image(ids).await,
            Self::Tls(c) => c.download_per_image(ids).await,
        }
    }

    async fn download_batch(
        &self,
        ids: &[ImageId],
    ) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        match self {
            Self::Plain(c) => c.download_batch(ids).await,
            Self::Tls(c) => c.download_batch(ids).await,
        }
    }

    async fn download_keepalive(
        &self,
        ids: &[ImageId],
    ) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        match self {
            Self::Plain(c) => c.download_keepalive(ids).await,
            Self::Tls(c) => c.download_keepalive(ids).await,
        }
    }

    async fn download_parallel(
        &self,
        ids: &[ImageId],
        num_workers: usize,
    ) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        match self {
            Self::Plain(c) => c.download_parallel(ids, num_workers).await,
            Self::Tls(c) => c.download_parallel(ids, num_workers).await,
        }
    }

    async fn download_delta(
        &self,
        have_ids: &[ImageId],
    ) -> Result<(u64, usize, usize), Box<dyn std::error::Error + Send + Sync>> {
        match self {
            Self::Plain(c) => c.download_delta(have_ids).await,
            Self::Tls(c) => c.download_delta(have_ids).await,
        }
    }

    async fn download_list_and_get(
        &self,
    ) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        match self {
            Self::Plain(c) => c.download_list_and_get().await,
            Self::Tls(c) => c.download_list_and_get().await,
        }
    }
}

/// TLS JTP client
struct JtpClient {
    connector: TlsConnector,
    addr: String,
    server_name: ServerName<'static>,
}

impl JtpClient {
    async fn new(
        addr: &str,
        cert_path: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let cert_bytes = tokio::fs::read(cert_path).await?;
        let certs: Vec<CertificateDer<'static>> = {
            let mut reader = BufReader::new(std::io::Cursor::new(cert_bytes));
            rustls_pemfile::certs(&mut reader).collect::<Result<Vec<_>, _>>()?
        };

        let mut root_store = RootCertStore::empty();
        for cert in certs {
            root_store.add(cert)?;
        }

        let mut config = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();

        config.resumption = Resumption::default()
            .tls12_resumption(rustls::client::Tls12Resumption::SessionIdOrTickets);

        let connector = TlsConnector::from(Arc::new(config));
        let server_name = ServerName::try_from("localhost".to_string())?;

        Ok(Self {
            connector,
            addr: addr.to_string(),
            server_name,
        })
    }

    async fn connect(
        &self,
    ) -> Result<TlsStream<TcpStream>, Box<dyn std::error::Error + Send + Sync>> {
        let tcp = TcpStream::connect(&self.addr).await?;
        tcp.set_nodelay(true)?;
        Ok(self
            .connector
            .connect(self.server_name.clone(), tcp)
            .await?)
    }

    async fn list_images(
        &self,
    ) -> Result<Vec<ListedImage>, Box<dyn std::error::Error + Send + Sync>> {
        let stream = self.connect().await?;
        let mut writer = BufWriter::with_capacity(64 * 1024, stream);

        write_list_request_buffered(&mut writer, 0).await?;
        writer.flush().await?;

        let mut header = [0u8; 4];
        writer.get_mut().read_exact(&mut header).await?;
        if &header != RESPONSE_LIST {
            return Err("Invalid LIST response".into());
        }

        let count = writer.get_mut().read_u16().await? as usize;
        let mut images = Vec::with_capacity(count);

        for _ in 0..count {
            let id = writer.get_mut().read_u64().await?;
            let flags = writer.get_mut().read_u8().await?;
            let name_len = writer.get_mut().read_u16().await? as usize;
            let mut name_bytes = vec![0u8; name_len];
            writer.get_mut().read_exact(&mut name_bytes).await?;
            let filename = String::from_utf8_lossy(&name_bytes).to_string();
            let size = read_varint_u32(writer.get_mut()).await?;
            images.push(ListedImage {
                id,
                flags,
                filename,
                size,
            });
        }

        Ok(images)
    }

    /// Read and discard image data from stream, return bytes read
    async fn consume_image(
        stream: &mut TlsStream<TcpStream>,
    ) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        let _flags = stream.read_u8().await?;
        let length = read_varint_u32(stream).await?;
        let _id = stream.read_u64().await?;

        let mut remaining = length as usize;
        let mut buf = vec![0u8; 65536];
        while remaining > 0 {
            let to_read = remaining.min(buf.len());
            stream.read_exact(&mut buf[..to_read]).await?;
            remaining -= to_read;
        }

        Ok(length as u64)
    }

    /// Download one image per connection (worst case scenario)
    async fn download_per_image(
        &self,
        ids: &[ImageId],
    ) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        let mut total_bytes = 0u64;
        let connections = ids.len();

        for id in ids {
            let stream = self.connect().await?;
            let mut writer = BufWriter::with_capacity(64 * 1024, stream);

            write_get_request_buffered(&mut writer, 0, &[*id]).await?;
            writer.flush().await?;

            total_bytes += Self::consume_image(writer.get_mut()).await?;
        }

        Ok((total_bytes, connections))
    }

    /// Download all images in a single batch request (no keep-alive)
    async fn download_batch(
        &self,
        ids: &[ImageId],
    ) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        let stream = self.connect().await?;
        let mut writer = BufWriter::with_capacity(64 * 1024, stream);
        let mut total_bytes = 0u64;
        let mut connections = 1;

        for chunk in ids.chunks(255) {
            if chunk.as_ptr() != ids.as_ptr() {
                // Need new connection for subsequent chunks
                drop(writer);
                let stream = self.connect().await?;
                writer = BufWriter::with_capacity(64 * 1024, stream);
                connections += 1;
            }

            write_get_request_buffered(&mut writer, 0, chunk).await?;
            writer.flush().await?;

            for _ in chunk {
                total_bytes += Self::consume_image(writer.get_mut()).await?;
            }
        }

        Ok((total_bytes, connections))
    }

    /// Download with keep-alive - single connection reused
    async fn download_keepalive(
        &self,
        ids: &[ImageId],
    ) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        let stream = self.connect().await?;
        let mut writer = BufWriter::with_capacity(64 * 1024, stream);
        let mut total_bytes = 0u64;

        for chunk in ids.chunks(255) {
            write_get_request_buffered(&mut writer, REQUEST_FLAG_KEEP_ALIVE, chunk).await?;
            writer.flush().await?;

            for _ in chunk {
                total_bytes += Self::consume_image(writer.get_mut()).await?;
            }
        }

        Ok((total_bytes, 1))
    }

    /// Download with parallel workers
    async fn download_parallel(
        &self,
        ids: &[ImageId],
        num_workers: usize,
    ) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        let chunk_size = (ids.len() + num_workers - 1) / num_workers;
        let semaphore = Arc::new(Semaphore::new(num_workers));
        let addr = self.addr.clone();
        let connector = self.connector.clone();
        let server_name = self.server_name.clone();

        let mut handles = Vec::new();

        for chunk in ids.chunks(chunk_size) {
            let chunk = chunk.to_vec();
            let semaphore = Arc::clone(&semaphore);
            let addr = addr.clone();
            let connector = connector.clone();
            let server_name = server_name.clone();

            handles.push(tokio::spawn(async move {
                let _permit = semaphore.acquire().await.unwrap();

                let tcp = TcpStream::connect(&addr).await?;
                tcp.set_nodelay(true)?;
                let stream = connector.connect(server_name, tcp).await?;
                let mut writer = BufWriter::with_capacity(64 * 1024, stream);
                let mut bytes = 0u64;

                for sub_chunk in chunk.chunks(255) {
                    write_get_request_buffered(&mut writer, REQUEST_FLAG_KEEP_ALIVE, sub_chunk)
                        .await?;
                    writer.flush().await?;

                    for _ in sub_chunk {
                        bytes += Self::consume_image(writer.get_mut()).await?;
                    }
                }

                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(bytes)
            }));
        }

        let mut total_bytes = 0u64;
        for handle in handles {
            total_bytes += handle.await??;
        }

        Ok((total_bytes, num_workers))
    }

    /// Delta sync - send "have" IDs, receive only missing
    async fn download_delta(
        &self,
        have_ids: &[ImageId],
    ) -> Result<(u64, usize, usize), Box<dyn std::error::Error + Send + Sync>> {
        let stream = self.connect().await?;
        let mut writer = BufWriter::with_capacity(64 * 1024, stream);

        write_batch_request_buffered(&mut writer, 0, have_ids).await?;
        writer.flush().await?;

        let mut header = [0u8; 4];
        writer.get_mut().read_exact(&mut header).await?;
        if &header != RESPONSE_BATCH {
            return Err("Invalid BATCH response".into());
        }

        let missing_count = read_varint_u32(writer.get_mut()).await? as usize;
        let mut total_bytes = 0u64;

        for _ in 0..missing_count {
            total_bytes += Self::consume_image(writer.get_mut()).await?;
        }

        Ok((total_bytes, 1, missing_count))
    }

    /// Combined LIST + GET in single round-trip (fastest mode)
    /// Server sends catalog header followed by all image data
    async fn download_list_and_get(
        &self,
    ) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
        let stream = self.connect().await?;
        let mut writer = BufWriter::with_capacity(64 * 1024, stream);

        // Single request to get everything
        write_list_and_get_request_buffered(&mut writer, 0).await?;
        writer.flush().await?;

        // Read response header
        let mut header = [0u8; 4];
        writer.get_mut().read_exact(&mut header).await?;
        if &header != RESPONSE_LIST_AND_GET {
            return Err(format!("Invalid LIST_AND_GET response: {:?}", header).into());
        }

        // Read image count
        let count = writer.get_mut().read_u16().await? as usize;
        let mut total_bytes = 0u64;

        // Receive all images
        for _ in 0..count {
            total_bytes += Self::consume_image(writer.get_mut()).await?;
        }

        Ok((total_bytes, 1))
    }
}

// ============================================================================
// HTTP Client
// ============================================================================

async fn download_http(
    client: &reqwest::Client,
    base_url: &str,
    filenames: &[String],
) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
    let mut total_bytes = 0u64;

    for filename in filenames {
        let url = format!("{}/image/{}", base_url, filename);
        let response = client.get(&url).send().await?;
        let bytes = response.bytes().await?;
        total_bytes += bytes.len() as u64;
    }

    Ok((total_bytes, filenames.len()))
}

// ============================================================================
// Output Helpers
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
        "Run".yellow(),
        run,
        total,
        "OK".green(),
        duration_ms
    );
}

// ============================================================================
// Argument Parsing
// ============================================================================

fn parse_args() -> BenchmarkConfig {
    let mut config = BenchmarkConfig::default();
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--jtp-addr" => {
                if let Some(v) = args.next() {
                    config.jtp_addr = v;
                }
            }
            "--http-addr" => {
                if let Some(v) = args.next() {
                    config.http_addr = v;
                }
            }
            "--cert" => {
                if let Some(v) = args.next() {
                    config.cert_path = v;
                }
            }
            "--images" => {
                if let Some(v) = args.next() {
                    config.test_images_dir = PathBuf::from(v);
                }
            }
            "--warmup" => {
                if let Some(v) = args.next() {
                    config.warmup_iterations = v.parse().unwrap_or(5);
                }
            }
            "--iterations" | "-n" => {
                if let Some(v) = args.next() {
                    config.test_iterations = v.parse().unwrap_or(10);
                }
            }
            "--parallel" | "-p" => {
                if let Some(v) = args.next() {
                    config.parallel_workers = v.parse().unwrap_or(4);
                }
            }
            "--tls" | "--secure" => {
                config.no_tls = false;
            }
            "--no-tls" | "--plain" => {
                config.no_tls = true;
            }
            "--http-tls" | "--https" => {
                config.http_tls = true;
                // Update default HTTP address to HTTPS if not explicitly set
                if config.http_addr == "http://127.0.0.1:8080" {
                    config.http_addr = "https://127.0.0.1:8443".to_string();
                }
            }
            "--mode" => {
                if let Some(v) = args.next() {
                    config.modes = match v.as_str() {
                        "http" => vec![BenchmarkMode::Http],
                        "per-image" => vec![BenchmarkMode::JtpPerImage],
                        "batch" => vec![BenchmarkMode::JtpBatch],
                        "keepalive" | "ka" => vec![BenchmarkMode::JtpKeepAlive],
                        "parallel" => vec![BenchmarkMode::JtpParallel],
                        "delta" => vec![BenchmarkMode::JtpDelta],
                        "list-and-get" | "lag" | "combined" => vec![BenchmarkMode::JtpListAndGet],
                        "jtp" => vec![
                            BenchmarkMode::JtpPerImage,
                            BenchmarkMode::JtpBatch,
                            BenchmarkMode::JtpKeepAlive,
                            BenchmarkMode::JtpParallel,
                            BenchmarkMode::JtpListAndGet,
                        ],
                        _ => config.modes,
                    };
                }
            }
            "-h" | "--help" => {
                eprintln!(
                    "JTP vs HTTP Performance Benchmark\n\n\
Usage: benchmark [OPTIONS]\n\n\
Options:\n  \
  --jtp-addr ADDR     JTP server address (default: 127.0.0.1:8443)\n  \
  --http-addr URL     HTTP server address (default: http://127.0.0.1:8080)\n  \
  --cert PATH         TLS certificate for JTP (default: cert.pem)\n  \
  --images DIR        Test images directory (default: images/)\n  \
  --warmup N          Warmup iterations (default: 5)\n  \
  --iterations N      Test iterations (default: 10)\n  \
  --parallel N        Parallel workers for JTP-Parallel mode (default: 4)\n  \
  --mode MODE         Benchmark mode:\n                    \
                        http, per-image, batch, keepalive, parallel, list-and-get, jtp, or all\n  \
  --help              Show this help\n\n\
Auto-Detection:\n  \
  JTP:  Automatically detects TLS vs plain TCP\n  \
  HTTP: Automatically detects HTTP vs HTTPS on port 8080\n\n\
Benchmark Modes:\n  \
  HTTP           - Download via HTTP/HTTPS (one request per image)\n  \
  JTP Per-Image  - New connection per image (worst case)\n  \
  JTP Batch      - Single connection, batch GET request\n  \
  JTP Keep-Alive - Reuse connection with keep-alive flag\n  \
  JTP Parallel   - Multiple parallel workers with keep-alive\n  \
  JTP Delta      - BATCH sync (only download missing images)\n  \
  JTP List+Get   - Combined LIST+GET in single round-trip\n\n\
Prerequisites:\n  \
  JTP server:   cargo run --release --bin server -- --images images/\n  \
  HTTP server:  node examples/benchmark/http/server.js\n  \
  HTTPS server: node examples/benchmark/http/server-https.js"
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

    print_header("JTP vs HTTP Image Download Benchmark");

    // Print setup info
    println!();
    print_info(&format!(
        "Test images directory: {:?}",
        config.test_images_dir
    ));
    print_info(&format!("HTTP server: {}", config.http_addr));
    print_info(&format!("JTP server: {}", config.jtp_addr));
    print_info(&format!(
        "Benchmark modes: {:?}",
        config
            .modes
            .iter()
            .map(|m| m.short_name())
            .collect::<Vec<_>>()
    ));
    print_info(&format!("Warmup runs: {}", config.warmup_iterations));
    print_info(&format!("Benchmark runs: {}", config.test_iterations));
    print_info(&format!("Parallel workers: {}", config.parallel_workers));

    // Get test images from directory
    let image_files: Vec<String> = std::fs::read_dir(&config.test_images_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let path = e.path();
            if let Some(ext) = path.extension() {
                let ext = ext.to_string_lossy().to_lowercase();
                matches!(
                    ext.as_str(),
                    "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp"
                )
            } else {
                false
            }
        })
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();

    if image_files.is_empty() {
        eprintln!(
            "{} No test images found in {:?}",
            "Error:".red().bold(),
            config.test_images_dir
        );
        return Ok(());
    }

    // Calculate total size
    let total_size: u64 = image_files
        .iter()
        .filter_map(|f| {
            let path = config.test_images_dir.join(f);
            std::fs::metadata(&path).ok().map(|m| m.len())
        })
        .sum();

    println!();
    print_info(&format!("Found {} test images:", image_files.len()));
    for img in &image_files {
        let path = config.test_images_dir.join(img);
        if let Ok(meta) = std::fs::metadata(&path) {
            println!("    - {} ({:.2} KB)", img, (meta.len() as f64) / 1024.0);
        }
    }

    // Check servers
    println!();
    print_info("Checking servers...");

    let has_http_mode = config.modes.contains(&BenchmarkMode::Http);
    let has_jtp_mode = config.modes.iter().any(|m| *m != BenchmarkMode::Http);

    // Create HTTP client (accepts self-signed certs for HTTPS auto-detection)
    let http_client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()?;

    // Check/auto-detect HTTP server if needed
    let mut http_addr = config.http_addr.clone();

    if has_http_mode {
        // Try addresses in order until one works
        let addrs_to_try = [
            config.http_addr.clone(),
            "http://127.0.0.1:8080".to_string(),
            "https://127.0.0.1:8080".to_string(),
        ];

        let mut found = false;
        for addr in &addrs_to_try {
            if http_client
                .get(format!("{}/list", addr))
                .send()
                .await
                .is_ok()
            {
                let proto = if addr.starts_with("https://") {
                    "HTTPS"
                } else {
                    "HTTP"
                };
                if addr == &config.http_addr {
                    print_success(&format!("{} server is running at {}", proto, addr));
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
            for addr in &addrs_to_try {
                eprintln!("    - {}", addr);
            }
            eprintln!();
            eprintln!("  Start HTTP:  node examples/benchmark/http/server.js");
            eprintln!("  Start HTTPS: node examples/benchmark/http/server-https.js");
            return Ok(());
        }
    }

    // Check JTP server if needed (auto-detect TLS vs plain TCP)
    let jtp_client: Option<JtpClientWrapper> = if has_jtp_mode {
        match JtpClientWrapper::auto_detect(&config.jtp_addr, &config.cert_path).await {
            Ok((client, is_tls)) => match client.list_images().await {
                Ok(images) => {
                    let mode_str = if is_tls { "TLS" } else { "plain TCP" };
                    print_success(&format!(
                        "JTP server auto-detected ({} images, {})",
                        images.len(),
                        mode_str
                    ));
                    Some(client)
                }
                Err(e) => {
                    eprintln!("{} JTP server error: {}", "Error:".red().bold(), e);
                    return Ok(());
                }
            },
            Err(e) => {
                eprintln!(
                    "{} Cannot connect to JTP server at {}: {}",
                    "Error:".red().bold(),
                    config.jtp_addr,
                    e
                );
                eprintln!();
                eprintln!(
                    "  Start with TLS:   cargo run --release --bin server -- --images images/"
                );
                eprintln!("  Start plain TCP:  cargo run --release --bin server -- --no-tls --images images/");
                return Ok(());
            }
        }
    } else {
        None
    };

    // Get JTP image info
    let jtp_images: Vec<ListedImage> = if let Some(ref client) = jtp_client {
        client.list_images().await?
    } else {
        vec![]
    };
    let jtp_ids: Vec<ImageId> = jtp_images.iter().map(|i| i.id).collect();

    // For delta mode: simulate having half the images already
    let have_ids: Vec<ImageId> = jtp_ids.iter().take(jtp_ids.len() / 2).copied().collect();
    let have_set: HashSet<ImageId> = have_ids.iter().copied().collect();
    let missing_count = jtp_ids.iter().filter(|id| !have_set.contains(id)).count();

    let mut results: Vec<BenchmarkResult> = Vec::new();

    // Run benchmarks for each mode
    for mode in &config.modes {
        print_subheader(&format!("Running {} Benchmark...", mode.name()));

        // Warmup
        print_info(&format!("Warmup ({} runs)...", config.warmup_iterations));
        for _ in 0..config.warmup_iterations {
            match mode {
                BenchmarkMode::Http => {
                    let _ = download_http(&http_client, &http_addr, &image_files).await;
                }
                BenchmarkMode::JtpPerImage => {
                    if let Some(ref client) = jtp_client {
                        let _ = client.download_per_image(&jtp_ids).await;
                    }
                }
                BenchmarkMode::JtpBatch => {
                    if let Some(ref client) = jtp_client {
                        let _ = client.download_batch(&jtp_ids).await;
                    }
                }
                BenchmarkMode::JtpKeepAlive => {
                    if let Some(ref client) = jtp_client {
                        let _ = client.download_keepalive(&jtp_ids).await;
                    }
                }
                BenchmarkMode::JtpParallel => {
                    if let Some(ref client) = jtp_client {
                        let _ = client
                            .download_parallel(&jtp_ids, config.parallel_workers)
                            .await;
                    }
                }
                BenchmarkMode::JtpDelta => {
                    if let Some(ref client) = jtp_client {
                        let _ = client.download_delta(&have_ids).await;
                    }
                }
                BenchmarkMode::JtpListAndGet => {
                    if let Some(ref client) = jtp_client {
                        let _ = client.download_list_and_get().await;
                    }
                }
            }
        }
        print_success("Warmup complete");

        // Benchmark runs
        let mut durations = Vec::with_capacity(config.test_iterations);
        let mut total_bytes = 0u64;
        let mut total_connections = 0usize;

        for run in 1..=config.test_iterations {
            let start = Instant::now();

            let (bytes, conns) = match mode {
                BenchmarkMode::Http => {
                    download_http(&http_client, &http_addr, &image_files).await?
                }
                BenchmarkMode::JtpPerImage => {
                    if let Some(ref client) = jtp_client {
                        client.download_per_image(&jtp_ids).await?
                    } else {
                        (0, 0)
                    }
                }
                BenchmarkMode::JtpBatch => {
                    if let Some(ref client) = jtp_client {
                        client.download_batch(&jtp_ids).await?
                    } else {
                        (0, 0)
                    }
                }
                BenchmarkMode::JtpKeepAlive => {
                    if let Some(ref client) = jtp_client {
                        client.download_keepalive(&jtp_ids).await?
                    } else {
                        (0, 0)
                    }
                }
                BenchmarkMode::JtpParallel => {
                    if let Some(ref client) = jtp_client {
                        client
                            .download_parallel(&jtp_ids, config.parallel_workers)
                            .await?
                    } else {
                        (0, 0)
                    }
                }
                BenchmarkMode::JtpDelta => {
                    if let Some(ref client) = jtp_client {
                        let (bytes, conns, _missing) = client.download_delta(&have_ids).await?;
                        (bytes, conns)
                    } else {
                        (0, 0)
                    }
                }
                BenchmarkMode::JtpListAndGet => {
                    if let Some(ref client) = jtp_client {
                        client.download_list_and_get().await?
                    } else {
                        (0, 0)
                    }
                }
            };

            let duration = start.elapsed();
            total_bytes = bytes;
            total_connections = conns;
            durations.push(duration);

            print_run(run, config.test_iterations, duration.as_secs_f64() * 1000.0);
        }

        // For delta mode, note how many images were actually transferred
        if *mode == BenchmarkMode::JtpDelta {
            print_info(&format!(
                "Delta sync: had {} images, received {} missing",
                have_ids.len(),
                missing_count
            ));
        }

        results.push(BenchmarkResult {
            mode: *mode,
            durations,
            total_bytes,
            connections_made: total_connections,
        });
    }

    // Display results
    print_header("BENCHMARK RESULTS");

    for result in &results {
        let stats = result.stats();
        print_subheader(&format!("{} Results:", result.mode.name()));

        println!("  {} {:.2} ms", "Average time:".green(), stats.mean);
        println!("  {} {:.2} ms", "Median time: ".green(), stats.median);
        println!("  {} {:.2} ms", "Min time:    ".green(), stats.min);
        println!("  {} {:.2} ms", "Max time:    ".green(), stats.max);
        println!("  {} {:.2} ms", "Std dev:     ".green(), stats.std_dev);
        println!(
            "  {} {:.2} KB/s",
            "Throughput:  ".cyan(),
            result.throughput_kbps()
        );
        println!("  {} {}", "Connections: ".cyan(), result.connections_made);
    }

    // Comparison table
    if results.len() > 1 {
        print_subheader("Comparison:");

        println!();
        println!(
            "  {:12} | {:>10} | {:>8} | {:>9} | {:>9} | {:>10} | {:>5}",
            "Mode", "Avg Time", "Median", "Min", "Max", "Throughput", "Conns"
        );
        println!("  {}", "-".repeat(75));

        let fastest = results
            .iter()
            .min_by(|a, b| a.stats().mean.partial_cmp(&b.stats().mean).unwrap())
            .map(|r| r.mode);

        for result in &results {
            let stats = result.stats();
            let is_fastest = Some(result.mode) == fastest;
            let marker = if is_fastest { " <-" } else { "" };

            println!(
                "  {:12} | {:>7.2} ms | {:>5.2} ms | {:>6.2} ms | {:>6.2} ms | {:>7.0} KB/s | {:>5}{}",
                result.mode.short_name(),
                stats.mean,
                stats.median,
                stats.min,
                stats.max,
                result.throughput_kbps(),
                result.connections_made,
                marker.green().bold()
            );
        }

        // Relative performance
        print_subheader("Relative Performance:");

        if let Some(fastest_result) = results
            .iter()
            .min_by(|a, b| a.stats().mean.partial_cmp(&b.stats().mean).unwrap())
        {
            let fastest_mean = fastest_result.stats().mean;

            for result in &results {
                let stats = result.stats();
                if result.mode == fastest_result.mode {
                    println!(
                        "  {}: {} (fastest)",
                        result.mode.short_name().bright_green().bold(),
                        "baseline".green()
                    );
                } else {
                    let ratio = stats.mean / fastest_mean;
                    let slower_pct = (ratio - 1.0) * 100.0;
                    println!(
                        "  {}: {:.2}x slower ({:.1}% slower)",
                        result.mode.short_name().yellow(),
                        ratio,
                        slower_pct
                    );
                }
            }
        }

        println!();
        println!(
            "  {} {:.2} KB",
            "Total data per run:".cyan(),
            (total_size as f64) / 1024.0
        );
    }

    println!();
    println!("{}", "Benchmark complete!".bright_green().bold());

    Ok(())
}
