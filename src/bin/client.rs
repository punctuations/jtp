use jtp::protocol::{
    compute_image_id,
    file_type_from_flags,
    read_varint_u32,
    write_batch_request_buffered,
    write_cancel_request_buffered,
    write_get_request_buffered,
    write_list_request_buffered,
    write_watch_request_buffered,
    ImageId,
    FLAG_COMPRESSED,
    FLAG_ENCRYPTED,
    LIST_FILTER_ALL,
    LIST_FILTER_BMP,
    LIST_FILTER_GIF,
    LIST_FILTER_JPEG,
    LIST_FILTER_PDF,
    LIST_FILTER_PNG,
    LIST_FILTER_WEBP,
    REQUEST_FLAG_KEEP_ALIVE,
    RESPONSE_BATCH,
    RESPONSE_GET_BY_ID,
    RESPONSE_LIST,
    RESPONSE_WATCH,
};
use quinn::crypto::rustls::QuicClientConfig;
use rustls::client::Resumption;
use rustls::pki_types::ServerName;
use rustls::RootCertStore;
use std::collections::{ HashMap, HashSet };
use std::io::BufReader;
use std::path::{ Path, PathBuf };
use std::sync::Arc;
use std::time::{ Duration, Instant };
use tokio::io::{ AsyncReadExt, AsyncWriteExt, BufWriter };
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;
use unicode_normalization::UnicodeNormalization;

macro_rules! vlog {
    (
        $enabled:expr,
        $($arg:tt)*
    ) => {
        if $enabled { eprintln!($($arg)*); }
    };
}

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct ListedImage {
    id: ImageId,
    flags: u8,
    filename: String,
    size: u32,
}

#[derive(Debug, Clone)]
struct ClientArgs {
    addr: String,
    server_name: String,
    cert_path: Option<PathBuf>,
    receive_dir: PathBuf,
    batch: bool,
    watch: bool,
    /// Optional FilterMask for LIST requests. None = all types.
    filter_mask: Option<u8>,
    verbose: bool,
    repeat: usize,
    keep_alive: bool,
    parallel: usize,
    tcp_nodelay: bool,
    /// When true, use TCP + TLS instead of QUIC.
    no_quic: bool,
}

// ── Argument parsing ──────────────────────────────────────────────────────────

/// Parse a comma-separated list of file type names into a FilterMask byte.
/// Recognised names: png, jpeg, jpg, webp, bmp, gif, pdf.
fn parse_filter(s: &str) -> Option<u8> {
    let mut mask = 0u8;
    for token in s.split(',') {
        match token.trim().to_lowercase().as_str() {
            "png" => {
                mask |= LIST_FILTER_PNG;
            }
            "jpeg" | "jpg" => {
                mask |= LIST_FILTER_JPEG;
            }
            "webp" => {
                mask |= LIST_FILTER_WEBP;
            }
            "bmp" => {
                mask |= LIST_FILTER_BMP;
            }
            "gif" => {
                mask |= LIST_FILTER_GIF;
            }
            "pdf" => {
                mask |= LIST_FILTER_PDF;
            }
            other => eprintln!("Warning: unknown filter type '{}' ignored", other),
        }
    }
    if mask == 0 || mask == LIST_FILTER_ALL {
        None
    } else {
        Some(mask)
    }
}

fn parse_args() -> ClientArgs {
    fn clean_addr(raw: &str) -> String {
        raw.strip_prefix("jtp://").unwrap_or(raw).to_string()
    }

    let mut addr = String::from("127.0.0.1:8443");
    let mut server_name = String::from("localhost");
    let mut cert_path: Option<PathBuf> = None;
    let mut receive_dir = PathBuf::from("output");
    let mut batch = false;
    let mut watch = false;
    let mut filter_mask: Option<u8> = None;
    let mut verbose = false;
    let mut repeat = 1;
    let mut keep_alive = false;
    let mut parallel: usize = 1;
    let mut tcp_nodelay = true;
    let mut no_quic = false; // QUIC is the default
    let mut addr_set = false;
    let mut server_name_set = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--addr" => {
                if let Some(v) = args.next() {
                    addr = clean_addr(&v);
                    addr_set = true;
                }
            }
            "--server-name" => {
                if let Some(v) = args.next() {
                    server_name = v;
                    server_name_set = true;
                }
            }
            "--cert" => {
                if let Some(v) = args.next() {
                    cert_path = Some(PathBuf::from(v));
                }
            }
            "--out" | "--output" => {
                if let Some(v) = args.next() {
                    receive_dir = PathBuf::from(v);
                }
            }
            "--batch" | "--delta" => {
                batch = true;
            }
            "--watch" | "-w" => {
                watch = true;
            }
            "--filter" | "-f" => {
                if let Some(v) = args.next() {
                    filter_mask = parse_filter(&v);
                }
            }
            "--no-quic" => {
                no_quic = true;
            }
            "-v" | "--verbose" => {
                verbose = true;
            }
            "--repeat" => {
                if let Some(v) = args.next() {
                    if let Ok(n) = v.parse::<usize>() {
                        repeat = n.max(1);
                    }
                }
            }
            "--keep-alive" | "-k" => {
                keep_alive = true;
            }
            "--parallel" | "-p" => {
                if let Some(v) = args.next() {
                    if let Ok(n) = v.parse::<usize>() {
                        parallel = n.max(1).min(32);
                    }
                }
            }
            "--no-tcp-nodelay" => {
                tcp_nodelay = false;
            }
            "-h" | "--help" => {
                eprintln!(
                    "Usage: client [OPTIONS]\n\n\
Options:\n  \
  --addr HOST:PORT         Server address (default: 127.0.0.1:8443)\n  \
  --server-name NAME       TLS/QUIC SNI name (default: localhost)\n  \
  --cert PATH              Path to CA cert\n  \
  --out DIR                Output directory (default: output)\n  \
  --batch                  Delta sync: download missing files only\n  \
  --watch, -w              Subscribe to server-push new-file events\n  \
  --filter TYPE[,TYPE...]  Limit LIST to specific types.\n  \
                           Types: png, jpeg, webp, bmp, gif, pdf\n  \
  --repeat N               Download files N times\n  \
  --keep-alive, -k         Reuse connection (TCP+TLS mode)\n  \
  --parallel N, -p N       Parallel workers (max 32)\n  \
  --no-quic                Use TCP + TLS instead of QUIC\n  \
  --no-tcp-nodelay         Disable TCP_NODELAY (TCP+TLS only)\n  \
  --verbose                Detailed logs\n\n\
Transport:\n  \
  Default: QUIC. Each request runs on its own stream for true parallelism.\n  \
  --no-quic: TCP + TLS, compatible with servers running --no-quic."
                );
                std::process::exit(0);
            }
            _ => {
                if !arg.starts_with('-') && !addr_set {
                    addr = clean_addr(&arg);
                    addr_set = true;
                }
            }
        }
    }

    addr = clean_addr(&addr);
    if !addr.contains(':') {
        addr = format!("{}:8443", addr);
    }
    if !server_name_set {
        if let Some((host, _)) = addr.split_once(':') {
            server_name = host.to_string();
        }
    }

    ClientArgs {
        addr,
        server_name,
        cert_path,
        receive_dir,
        batch,
        watch,
        filter_mask,
        verbose,
        repeat,
        keep_alive,
        parallel,
        tcp_nodelay,
        no_quic,
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn collect_have_ids(dir: &Path, verbose: bool) -> std::io::Result<Vec<ImageId>> {
    let mut have: HashSet<ImageId> = HashSet::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let id = compute_image_id(&bytes);
        vlog!(verbose, "Have {} => id={}", path.display(), hex::encode(id.to_be_bytes()));
        have.insert(id);
    }
    Ok(have.into_iter().collect())
}

// ── Root store builder ────────────────────────────────────────────────────────

async fn build_root_store(
    cert_path: Option<&Path>,
    verbose: bool
) -> Result<RootCertStore, Box<dyn std::error::Error + Send + Sync>> {
    if let Some(path) = cert_path {
        vlog!(verbose, "Loading trusted certs from {}...", path.display());
        let bytes: Vec<u8> = tokio::fs::read(path).await?;
        let certs: Vec<rustls::pki_types::CertificateDer<'static>> = {
            let mut r = BufReader::new(std::io::Cursor::new(bytes));
            rustls_pemfile::certs(&mut r).collect::<Result<Vec<_>, _>>()?
        };
        let mut store = RootCertStore::empty();
        for cert in certs {
            store.add(cert)?;
        }
        Ok(store)
    } else {
        vlog!(verbose, "Using system root certificates...");
        let mut store = RootCertStore::empty();
        store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Ok(store)
    }
}

// ── QUIC connection pool ──────────────────────────────────────────────────────
//
// A single QUIC endpoint can multiplex many bidirectional streams over one
// UDP connection, amortising the handshake cost across all requests.

struct QuicPool {
    endpoint: quinn::Endpoint,
    conn: Option<quinn::Connection>,
    addr: std::net::SocketAddr,
    server_name: String,
    verbose: bool,
}

impl QuicPool {
    async fn new(
        addr: &str,
        server_name: &str,
        cert_path: Option<&Path>,
        verbose: bool
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let root_store = build_root_store(cert_path, verbose).await?;
        let mut config = rustls::ClientConfig
            ::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"jtp/1".to_vec()];

        let quic_config = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(config)?));

        // Bind to an ephemeral local UDP port.
        let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
        endpoint.set_default_client_config(quic_config);

        let addr: std::net::SocketAddr = addr.parse()?;
        Ok(Self { endpoint, conn: None, addr, server_name: server_name.to_string(), verbose })
    }

    /// Return an open bidirectional stream, establishing the QUIC connection
    /// on first call (or reconnecting if it has been closed).
    async fn open_stream(
        &mut self
    ) -> Result<(quinn::SendStream, quinn::RecvStream), Box<dyn std::error::Error + Send + Sync>> {
        // Try to reuse the existing connection.
        if let Some(ref conn) = self.conn {
            match conn.open_bi().await {
                Ok(stream) => {
                    return Ok(stream);
                }
                Err(e) => {
                    vlog!(self.verbose, "QUIC stream open failed ({}), reconnecting...", e);
                    self.conn = None;
                }
            }
        }

        // Establish a new connection.
        vlog!(self.verbose, "QUIC connecting to {}...", self.addr);
        let conn = self.endpoint.connect(self.addr, &self.server_name)?.await?;
        vlog!(self.verbose, "QUIC handshake complete ({:?})", conn.remote_address());
        let stream = conn.open_bi().await?;
        self.conn = Some(conn);
        Ok(stream)
    }
}

// ── TLS connection pool (TCP + TLS, --no-quic) ────────────────────────────────

struct TlsConnectionPool {
    connections: Vec<(TlsStream<TcpStream>, Instant)>,
    addr: String,
    server_name: String,
    connector: TlsConnector,
    max_idle: Duration,
    tcp_nodelay: bool,
    verbose: bool,
}

impl TlsConnectionPool {
    async fn new(
        addr: String,
        server_name: String,
        cert_path: Option<&Path>,
        tcp_nodelay: bool,
        verbose: bool
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let root_store = build_root_store(cert_path, verbose).await?;
        let mut config = rustls::ClientConfig
            ::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        config.resumption = Resumption::default().tls12_resumption(
            rustls::client::Tls12Resumption::SessionIdOrTickets
        );
        let connector = TlsConnector::from(Arc::new(config));
        Ok(Self {
            connections: Vec::new(),
            addr,
            server_name,
            connector,
            max_idle: Duration::from_secs(30),
            tcp_nodelay,
            verbose,
        })
    }

    async fn get(
        &mut self
    ) -> Result<TlsStream<TcpStream>, Box<dyn std::error::Error + Send + Sync>> {
        while let Some((conn, created)) = self.connections.pop() {
            if created.elapsed() < self.max_idle {
                vlog!(self.verbose, "Reusing pooled TLS connection (age: {:?})", created.elapsed());
                return Ok(conn);
            }
            vlog!(self.verbose, "Discarding stale TLS connection");
        }
        vlog!(self.verbose, "Connecting TCP to {}...", self.addr);
        let tcp = TcpStream::connect(&self.addr).await?;
        if self.tcp_nodelay {
            tcp.set_nodelay(true)?;
        }
        let sn = ServerName::try_from(self.server_name.clone())?;
        let tls = self.connector.connect(sn, tcp).await?;
        vlog!(self.verbose, "TLS handshake complete");
        Ok(tls)
    }

    fn return_connection(&mut self, conn: TlsStream<TcpStream>) {
        self.connections.push((conn, Instant::now()));
    }
}

async fn tls_connect(
    addr: &str,
    server_name: &str,
    cert_path: Option<&Path>,
    tcp_nodelay: bool,
    verbose: bool
) -> Result<TlsStream<TcpStream>, Box<dyn std::error::Error + Send + Sync>> {
    vlog!(verbose, "Connecting TCP to {}...", addr);
    let tcp = TcpStream::connect(addr).await?;
    if tcp_nodelay {
        tcp.set_nodelay(true)?;
    }
    let root_store = build_root_store(cert_path, verbose).await?;
    let mut config = rustls::ClientConfig
        ::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    config.resumption = Resumption::default().tls12_resumption(
        rustls::client::Tls12Resumption::SessionIdOrTickets
    );
    let connector = TlsConnector::from(Arc::new(config));
    let sn = ServerName::try_from(server_name.to_owned())?;
    Ok(connector.connect(sn, tcp).await?)
}

// ── QuicStream adapter (same as server-side) ──────────────────────────────────

struct QuicStream {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
}

impl tokio::io::AsyncRead for QuicStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.recv).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for QuicStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8]
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin
            ::new(&mut self.send)
            .poll_write(cx, buf)
            .map(|res| res.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)))
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.send).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.send).poll_shutdown(cx)
    }
}

impl Unpin for QuicStream {}

// ── Generic stream helpers ────────────────────────────────────────────────────
//
// These functions work over any S: AsyncReadExt + Unpin, so the same logic
// serves both QUIC streams and TLS streams without duplication.

/// Read a LIST response from any stream, applying optional filter mask.
async fn read_list_response<S: AsyncReadExt + Unpin>(
    stream: &mut S,
    verbose: bool
) -> Result<Vec<ListedImage>, Box<dyn std::error::Error + Send + Sync>> {
    let mut list_header = [0u8; 4];
    stream.read_exact(&mut list_header).await?;
    if &list_header != RESPONSE_LIST {
        return Err(format!("unexpected LIST header: {:?}", list_header).into());
    }
    vlog!(verbose, "LIST response header OK (JTPL)");

    let count = read_varint_u32(stream).await? as usize;
    vlog!(verbose, "LIST count={}", count);

    let mut listed = Vec::with_capacity(count);
    let mut name_buf = vec![0u8; 256];

    for _ in 0..count {
        let id = stream.read_u64().await?;
        let flags = stream.read_u8().await?;
        let name_len = stream.read_u16().await? as usize;
        if name_len > name_buf.len() {
            name_buf.resize(name_len, 0);
        }
        stream.read_exact(&mut name_buf[..name_len]).await?;

        // §5.3: NFC-normalise filename on receipt.
        let filename: String = String::from_utf8_lossy(&name_buf[..name_len])
            .nfc()
            .collect::<String>()
            .trim()
            .to_string();

        let size = read_varint_u32(stream).await?;
        listed.push(ListedImage { id, flags, filename, size });
    }

    Ok(listed)
}

/// Receive a single image packet, verify its ImageID, and write it to disk.
async fn receive_image<S: AsyncReadExt + Unpin + AsyncWriteExt>(
    stream: &mut S,
    receive_dir: &Path,
    by_id: &HashMap<ImageId, ListedImage>,
    verbose: bool
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let flags = stream.read_u8().await?;
    let length = read_varint_u32(stream).await?;
    let id = stream.read_u64().await?;

    if (flags & FLAG_ENCRYPTED) != 0 {
        return Err(
            format!("unsupported flags=0x{:02x} (encryption not implemented)", flags).into()
        );
    }

    let file_type = file_type_from_flags(flags);
    vlog!(
        verbose,
        "File id={} type={} length={}",
        hex::encode(id.to_be_bytes()),
        file_type,
        length
    );

    // Receive raw (possibly compressed) bytes.
    let raw_data = if length > 10_000_000 {
        let tmp = receive_dir.join(format!("tmp_{}.bin", hex::encode(id.to_be_bytes())));
        let mut f = tokio::fs::File::create(&tmp).await?;
        let mut rem = length as usize;
        let mut buf = vec![0u8; 65536];
        while rem > 0 {
            let n = rem.min(buf.len());
            stream.read_exact(&mut buf[..n]).await?;
            f.write_all(&buf[..n]).await?;
            rem -= n;
        }
        drop(f);
        let data = tokio::fs::read(&tmp).await?;
        tokio::fs::remove_file(&tmp).await?;
        data
    } else {
        let mut buf = vec![0u8; length as usize];
        stream.read_exact(&mut buf).await?;
        buf
    };

    // Decompress if needed.
    let data = if (flags & FLAG_COMPRESSED) != 0 {
        let dec = jtp::protocol::decompress(&raw_data)?;
        vlog!(verbose, "Decompressed {} -> {} bytes", length, dec.len());
        dec
    } else {
        raw_data
    };

    // §6.2: verify ImageID after decompression.
    let computed = compute_image_id(&data);
    if computed != id {
        return Err(
            format!(
                "ImageID mismatch for {:016x}: computed {:016x} — discarding corrupt packet",
                id,
                computed
            ).into()
        );
    }

    // Determine output filename and extension.
    let effective_meta = by_id.get(&id);
    let effective_type = effective_meta.map(|m| file_type_from_flags(m.flags)).unwrap_or(file_type);
    // §7.1: PDF is FileType 5.
    let ext = match effective_type {
        0 => "png",
        1 => "jpg",
        2 => "webp",
        3 => "bmp",
        4 => "gif",
        5 => "pdf",
        _ => "bin",
    };

    let output_name = if let Some(meta) = effective_meta {
        // §5.3: NFC-normalise before using as a path.
        let base: String = Path::new(&meta.filename)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .nfc()
            .collect();
        let base = base.trim();

        // Strip path separators to prevent traversal.
        let base = base.replace(['/', '\\'], "");

        if base.is_empty() || base == "." || base == ".." {
            None
        } else if base.contains('.') {
            Some(base)
        } else {
            Some(format!("{}.{}", base, ext))
        }
    } else {
        None
    };

    let output_name = output_name.unwrap_or_else(||
        format!("output_{}.{}", hex::encode(id.to_be_bytes()), ext)
    );

    let output_path = receive_dir.join(&output_name);
    vlog!(verbose, "Writing {} bytes to {}", data.len(), output_path.display());
    std::fs::write(&output_path, &data)?;

    Ok(())
}

// ── QUIC download helpers ─────────────────────────────────────────────────────

/// Issue a LIST request over a new QUIC stream and return the catalog.
async fn quic_list(
    pool: &mut QuicPool,
    filter_mask: Option<u8>,
    verbose: bool
) -> Result<Vec<ListedImage>, Box<dyn std::error::Error + Send + Sync>> {
    let (send, recv) = pool.open_stream().await?;
    let combined = QuicStream { send, recv };
    let mut w = BufWriter::with_capacity(64 * 1024, combined);

    write_list_request_buffered(&mut w, 0, filter_mask).await?;
    w.flush().await?;

    read_list_response(w.get_mut(), verbose).await
}

/// Download a slice of images over individual QUIC streams (parallel-friendly).
/// Each chunk of up to 255 IDs opens its own stream, so multiple chunks can
/// fly in parallel if this function is called from multiple tasks.
async fn quic_get_by_id(
    pool: &mut QuicPool,
    ids: &[ImageId],
    receive_dir: &Path,
    by_id: &HashMap<ImageId, ListedImage>,
    verbose: bool
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    for chunk in ids.chunks(255) {
        let (send, recv) = pool.open_stream().await?;
        let combined = QuicStream { send, recv };
        let mut w = BufWriter::with_capacity(64 * 1024, combined);

        write_get_request_buffered(&mut w, 0, chunk).await?;
        w.flush().await?;

        // §9.2: read JTPD header + M count.
        let mut hdr = [0u8; 4];
        w.get_mut().recv.read_exact(&mut hdr).await?;
        if &hdr != RESPONSE_GET_BY_ID {
            return Err(format!("unexpected GET_BY_ID header: {:?}", hdr).into());
        }
        let m = w.get_mut().recv.read_u8().await? as usize;
        vlog!(verbose, "GET_BY_ID stream returned M={} packets", m);

        for _ in 0..m {
            receive_image(w.get_mut(), receive_dir, by_id, verbose).await?;
        }
    }
    Ok(())
}

// ── TLS keep-alive download ───────────────────────────────────────────────────

async fn tls_download_keepalive(
    pool: &mut TlsConnectionPool,
    ids: &[ImageId],
    receive_dir: &Path,
    by_id: &HashMap<ImageId, ListedImage>,
    verbose: bool
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if ids.is_empty() {
        return Ok(());
    }

    let stream = pool.get().await?;
    let mut writer = BufWriter::with_capacity(64 * 1024, stream);

    for chunk in ids.chunks(255) {
        vlog!(verbose, "GET_BY_ID ({} ids) with keep-alive", chunk.len());

        write_get_request_buffered(&mut writer, REQUEST_FLAG_KEEP_ALIVE, chunk).await?;
        writer.flush().await?;

        let mut header = [0u8; 4];
        writer.get_mut().read_exact(&mut header).await?;
        if &header != RESPONSE_GET_BY_ID {
            return Err(format!("unexpected GET_BY_ID header: {:?}", header).into());
        }
        let m = writer.get_mut().read_u8().await? as usize;
        vlog!(verbose, "GET_BY_ID returned M={} packets", m);

        for _ in 0..m {
            receive_image(writer.get_mut(), receive_dir, by_id, verbose).await?;
        }
    }

    pool.return_connection(writer.into_inner());
    Ok(())
}

// ── Parallel QUIC worker ──────────────────────────────────────────────────────

async fn parallel_quic_worker(
    worker_id: usize,
    addr: String,
    server_name: String,
    cert_path: Option<PathBuf>,
    ids: Vec<ImageId>,
    receive_dir: PathBuf,
    by_id: Arc<HashMap<ImageId, ListedImage>>,
    verbose: bool,
    semaphore: Arc<Semaphore>
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let _permit = semaphore.acquire().await?;
    vlog!(verbose, "Worker {} starting with {} files", worker_id, ids.len());

    let mut pool = QuicPool::new(&addr, &server_name, cert_path.as_deref(), verbose).await?;
    quic_get_by_id(&mut pool, &ids, &receive_dir, &by_id, verbose).await?;

    vlog!(verbose, "Worker {} completed {} files", worker_id, ids.len());
    Ok(ids.len())
}

// ── Parallel TLS worker ───────────────────────────────────────────────────────

async fn parallel_tls_worker(
    worker_id: usize,
    addr: String,
    server_name: String,
    cert_path: Option<PathBuf>,
    ids: Vec<ImageId>,
    receive_dir: PathBuf,
    by_id: Arc<HashMap<ImageId, ListedImage>>,
    tcp_nodelay: bool,
    verbose: bool,
    semaphore: Arc<Semaphore>
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let _permit = semaphore.acquire().await?;
    vlog!(verbose, "Worker {} starting with {} files", worker_id, ids.len());

    let mut pool = TlsConnectionPool::new(
        addr,
        server_name,
        cert_path.as_deref(),
        tcp_nodelay,
        verbose
    ).await?;
    tls_download_keepalive(&mut pool, &ids, &receive_dir, &by_id, verbose).await?;

    vlog!(verbose, "Worker {} completed {} files", worker_id, ids.len());
    Ok(ids.len())
}

// ── WATCH mode ────────────────────────────────────────────────────────────────

async fn watch_mode(
    args: &ClientArgs,
    verbose: bool
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("Connecting for WATCH subscription...");

    // WATCH uses a single long-lived stream; open one and loop.
    if args.no_quic {
        let tls = tls_connect(
            &args.addr,
            &args.server_name,
            args.cert_path.as_deref(),
            args.tcp_nodelay,
            verbose
        ).await?;
        let mut stream = BufWriter::with_capacity(64 * 1024, tls);
        write_watch_request_buffered(&mut stream).await?;
        stream.flush().await?;
        watch_loop(stream.get_mut(), verbose).await
    } else {
        let mut pool = QuicPool::new(
            &args.addr,
            &args.server_name,
            args.cert_path.as_deref(),
            verbose
        ).await?;
        let (send, recv) = pool.open_stream().await?;
        let combined = QuicStream { send, recv };
        let mut stream = BufWriter::with_capacity(64 * 1024, combined);
        write_watch_request_buffered(&mut stream).await?;
        stream.flush().await?;
        watch_loop(stream.get_mut(), verbose).await
    }
}

async fn watch_loop<S: AsyncReadExt + Unpin>(
    stream: &mut S,
    verbose: bool
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("WATCH active. Waiting for new files (Ctrl-C to cancel)...");

    loop {
        let mut header = [0u8; 4];
        stream.read_exact(&mut header).await?;

        match &header {
            h if h == RESPONSE_WATCH => {
                let id = stream.read_u64().await?;
                let flags = stream.read_u8().await?;
                let name_len = stream.read_u16().await? as usize;
                let mut name_buf = vec![0u8; name_len];
                stream.read_exact(&mut name_buf).await?;
                let size = read_varint_u32(stream).await?;

                // §5.3: NFC-normalise filename on receipt.
                let filename: String = String::from_utf8_lossy(&name_buf).nfc().collect();

                let file_type = file_type_from_flags(flags);
                // §7.1: include pdf in the human-readable type string.
                let type_str = match file_type {
                    0 => "png",
                    1 => "jpg",
                    2 => "webp",
                    3 => "bmp",
                    4 => "gif",
                    5 => "pdf",
                    _ => "unknown",
                };
                println!(
                    "NEW  id={} type={} size={} name={}",
                    hex::encode(id.to_be_bytes()),
                    type_str,
                    size,
                    filename
                );
            }
            h if h == b"JTPE" => {
                let code = stream.read_u8().await?;
                let msg_len = stream.read_u16().await? as usize;
                let mut msg = vec![0u8; msg_len];
                stream.read_exact(&mut msg).await?;
                return Err(
                    format!(
                        "Server error during WATCH (code {}): {}",
                        code,
                        String::from_utf8_lossy(&msg)
                    ).into()
                );
            }
            other => {
                return Err(format!("Unexpected WATCH frame header: {:?}", other).into());
            }
        }
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = parse_args();
    let verbose = args.verbose;

    vlog!(
        verbose,
        "Client: addr={} server_name={} cert={:?} out={} parallel={} keep_alive={} watch={} filter={:?} no_quic={}",
        args.addr,
        args.server_name,
        args.cert_path,
        args.receive_dir.display(),
        args.parallel,
        args.keep_alive,
        args.watch,
        args.filter_mask,
        args.no_quic
    );

    let receive_dir = args.receive_dir.clone();
    std::fs::create_dir_all(&receive_dir)?;

    // ── WATCH mode ────────────────────────────────────────────────────────────
    if args.watch {
        return watch_mode(&args, verbose).await;
    }

    // ── BATCH (delta sync) mode ───────────────────────────────────────────────
    if args.batch {
        vlog!(verbose, "BATCH mode: fetching catalog first...");

        // Step 1: LIST to get filenames.
        let listed = if args.no_quic {
            let tls = tls_connect(
                &args.addr,
                &args.server_name,
                args.cert_path.as_deref(),
                args.tcp_nodelay,
                verbose
            ).await?;
            let mut stream = BufWriter::with_capacity(64 * 1024, tls);
            // §8.1: pass filter mask (None for BATCH — we want all types).
            write_list_request_buffered(&mut stream, 0, None).await?;
            stream.flush().await?;
            read_list_response(stream.get_mut(), verbose).await?
        } else {
            let mut pool = QuicPool::new(
                &args.addr,
                &args.server_name,
                args.cert_path.as_deref(),
                verbose
            ).await?;
            quic_list(&mut pool, None, verbose).await?
        };

        let by_id: HashMap<ImageId, ListedImage> = listed
            .into_iter()
            .map(|item| (item.id, item))
            .collect();

        let have_ids = collect_have_ids(&receive_dir, verbose)?;
        vlog!(verbose, "BATCH: sending {} have IDs", have_ids.len());

        // Step 2: BATCH request.
        if args.no_quic {
            let tls = tls_connect(
                &args.addr,
                &args.server_name,
                args.cert_path.as_deref(),
                args.tcp_nodelay,
                verbose
            ).await?;
            let mut stream = BufWriter::with_capacity(64 * 1024, tls);
            let req_flags = if args.keep_alive { REQUEST_FLAG_KEEP_ALIVE } else { 0 };
            write_batch_request_buffered(&mut stream, req_flags, &have_ids).await?;
            stream.flush().await?;

            let mut header = [0u8; 4];
            stream.get_mut().read_exact(&mut header).await?;
            if &header != RESPONSE_BATCH {
                return Err(format!("unexpected BATCH header: {:?}", header).into());
            }
            let missing = read_varint_u32(stream.get_mut()).await? as usize;
            println!("Server reports {} missing files", missing);
            for _ in 0..missing {
                receive_image(stream.get_mut(), &receive_dir, &by_id, verbose).await?;
            }
        } else {
            let mut pool = QuicPool::new(
                &args.addr,
                &args.server_name,
                args.cert_path.as_deref(),
                verbose
            ).await?;
            let (send, recv) = pool.open_stream().await?;
            let combined = QuicStream { send, recv };
            let mut stream = BufWriter::with_capacity(64 * 1024, combined);
            write_batch_request_buffered(&mut stream, 0, &have_ids).await?;
            stream.flush().await?;

            let mut header = [0u8; 4];
            stream.get_mut().recv.read_exact(&mut header).await?;
            if &header != RESPONSE_BATCH {
                return Err(format!("unexpected BATCH header: {:?}", header).into());
            }
            let missing = read_varint_u32(&mut stream.get_mut().recv).await? as usize;
            println!("Server reports {} missing files", missing);
            for _ in 0..missing {
                receive_image(stream.get_mut(), &receive_dir, &by_id, verbose).await?;
            }
        }

        return Ok(());
    }

    // ── Standard LIST + GET mode ──────────────────────────────────────────────
    vlog!(verbose, "Fetching catalog (filter={:?})...", args.filter_mask);

    let listed = if args.no_quic {
        let tls = tls_connect(
            &args.addr,
            &args.server_name,
            args.cert_path.as_deref(),
            args.tcp_nodelay,
            verbose
        ).await?;
        let mut stream = BufWriter::with_capacity(64 * 1024, tls);
        // §8.1: pass filter mask.
        write_list_request_buffered(&mut stream, 0, args.filter_mask).await?;
        stream.flush().await?;
        read_list_response(stream.get_mut(), verbose).await?
    } else {
        let mut pool = QuicPool::new(
            &args.addr,
            &args.server_name,
            args.cert_path.as_deref(),
            verbose
        ).await?;
        quic_list(&mut pool, args.filter_mask, verbose).await?
    };

    if listed.is_empty() {
        println!("No files available on server.");
        return Ok(());
    }

    println!("Server catalog ({} files):", listed.len());
    for item in &listed {
        let type_str = match file_type_from_flags(item.flags) {
            0 => "png",
            1 => "jpg",
            2 => "webp",
            3 => "bmp",
            4 => "gif",
            5 => "pdf",
            _ => "?",
        };
        println!(
            "- {}  [{}]  {}  {} bytes",
            hex::encode(item.id.to_be_bytes()),
            type_str,
            item.filename,
            item.size
        );
    }

    let ids: Vec<ImageId> = listed
        .iter()
        .map(|i| i.id)
        .collect();
    let by_id: Arc<HashMap<ImageId, ListedImage>> = Arc::new(
        listed
            .into_iter()
            .map(|item| (item.id, item))
            .collect()
    );

    let start = Instant::now();

    for iteration in 0..args.repeat {
        vlog!(verbose, "Iteration {}/{}", iteration + 1, args.repeat);

        if args.parallel > 1 {
            let chunk_size = (ids.len() + args.parallel - 1) / args.parallel;
            let semaphore = Arc::new(Semaphore::new(args.parallel));
            let mut handles = Vec::new();

            for (worker_id, chunk) in ids.chunks(chunk_size).enumerate() {
                if args.no_quic {
                    handles.push(
                        tokio::spawn(
                            parallel_tls_worker(
                                worker_id,
                                args.addr.clone(),
                                args.server_name.clone(),
                                args.cert_path.clone(),
                                chunk.to_vec(),
                                receive_dir.clone(),
                                Arc::clone(&by_id),
                                args.tcp_nodelay,
                                verbose,
                                Arc::clone(&semaphore)
                            )
                        )
                    );
                } else {
                    handles.push(
                        tokio::spawn(
                            parallel_quic_worker(
                                worker_id,
                                args.addr.clone(),
                                args.server_name.clone(),
                                args.cert_path.clone(),
                                chunk.to_vec(),
                                receive_dir.clone(),
                                Arc::clone(&by_id),
                                verbose,
                                Arc::clone(&semaphore)
                            )
                        )
                    );
                }
            }

            let mut total = 0;
            for handle in handles {
                match handle.await? {
                    Ok(n) => {
                        total += n;
                    }
                    Err(e) => eprintln!("Worker error: {}", e),
                }
            }
            vlog!(verbose, "Parallel download complete: {} files", total);
        } else if args.keep_alive && args.no_quic {
            // TLS keep-alive pool (--no-quic only).
            let mut pool = TlsConnectionPool::new(
                args.addr.clone(),
                args.server_name.clone(),
                args.cert_path.as_deref(),
                args.tcp_nodelay,
                verbose
            ).await?;
            tls_download_keepalive(&mut pool, &ids, &receive_dir, &by_id, verbose).await?;
        } else if !args.no_quic {
            // QUIC: each chunk opens its own stream, so we get per-chunk
            // parallelism even in the non-parallel path.
            let mut pool = QuicPool::new(
                &args.addr,
                &args.server_name,
                args.cert_path.as_deref(),
                verbose
            ).await?;
            quic_get_by_id(&mut pool, &ids, &receive_dir, &by_id, verbose).await?;
        } else {
            // TCP + TLS, single connection, no keep-alive.
            vlog!(
                verbose,
                "Opening GET_BY_ID connection (iteration {}/{})...",
                iteration + 1,
                args.repeat
            );
            let tls = tls_connect(
                &args.addr,
                &args.server_name,
                args.cert_path.as_deref(),
                args.tcp_nodelay,
                verbose
            ).await?;
            let mut stream = BufWriter::with_capacity(64 * 1024, tls);

            if ids.len() > (u8::MAX as usize) {
                return Err(format!("too many files for single request: {}", ids.len()).into());
            }

            write_get_request_buffered(&mut stream, 0, &ids).await?;
            stream.flush().await?;

            let mut header = [0u8; 4];
            stream.get_mut().read_exact(&mut header).await?;
            if &header != RESPONSE_GET_BY_ID {
                return Err(format!("unexpected GET_BY_ID header: {:?}", header).into());
            }
            let m = stream.get_mut().read_u8().await? as usize;
            vlog!(verbose, "GET_BY_ID returned M={} packets", m);

            for _ in 0..m {
                receive_image(stream.get_mut(), &receive_dir, &by_id, verbose).await?;
            }
        }
    }

    let elapsed = start.elapsed();
    let total_files = ids.len() * args.repeat;
    println!(
        "Downloaded {} files in {:?} ({:.1} files/sec)",
        total_files,
        elapsed,
        (total_files as f64) / elapsed.as_secs_f64()
    );

    Ok(())
}
