use jtp::protocol::{
    compute_image_id, file_type_from_flags, read_varint_u32,
    write_batch_request_buffered, write_cancel_request_buffered,
    write_get_request_buffered, write_list_request_buffered,
    write_watch_request_buffered, ImageId, FLAG_COMPRESSED, FLAG_ENCRYPTED,
    REQUEST_FLAG_KEEP_ALIVE, RESPONSE_BATCH, RESPONSE_GET_BY_ID, RESPONSE_LIST,
    RESPONSE_WATCH,
};
use rustls::client::Resumption;
use rustls::pki_types::ServerName;
use rustls::RootCertStore;
use std::collections::{HashMap, HashSet};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;
use unicode_normalization::UnicodeNormalization;

macro_rules! vlog {
    ($enabled:expr, $($arg:tt)*) => {
        if $enabled { eprintln!($($arg)*); }
    };
}

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct ListedImage {
    id:       ImageId,
    flags:    u8,
    filename: String,
    size:     u32,
}

#[derive(Debug, Clone)]
struct ClientArgs {
    addr:           String,
    server_name:    String,
    cert_path:      Option<PathBuf>,
    receive_dir:    PathBuf,
    batch:          bool,
    watch:          bool,
    verbose:        bool,
    repeat:         usize,
    keep_alive:     bool,
    parallel:       usize,
    tcp_nodelay:    bool,
    #[allow(dead_code)]
    no_tls:         bool,
}

// ── Argument parsing ──────────────────────────────────────────────────────────

fn parse_args() -> ClientArgs {
    fn clean_addr(raw: &str) -> String {
        raw.strip_prefix("jtp://").unwrap_or(raw).to_string()
    }

    let mut addr         = String::from("127.0.0.1:8443");
    let mut server_name  = String::from("localhost");
    let mut cert_path:   Option<PathBuf> = None;
    let mut receive_dir  = PathBuf::from("output");
    let mut batch        = false;
    let mut watch        = false;
    let mut verbose      = false;
    let mut repeat       = 1;
    let mut keep_alive   = false;
    let mut parallel:    usize = 1;
    let mut tcp_nodelay  = true;
    let mut no_tls       = true;
    let mut addr_set     = false;
    let mut server_name_set = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--addr" => {
                if let Some(v) = args.next() { addr = clean_addr(&v); addr_set = true; }
            }
            "--server-name" => {
                if let Some(v) = args.next() { server_name = v; server_name_set = true; }
            }
            "--cert" => {
                if let Some(v) = args.next() { cert_path = Some(PathBuf::from(v)); }
            }
            "--out" | "--output" => {
                if let Some(v) = args.next() { receive_dir = PathBuf::from(v); }
            }
            "--batch" | "--delta" => { batch = true; }
            "--watch" | "-w"      => { watch = true; }
            "--tls" | "--secure"  => {
                no_tls = false;
                if !addr_set { addr = String::from("127.0.0.1:8443"); }
            }
            "--no-tls" | "--plain" => { no_tls = true; }
            "-v" | "--verbose"    => { verbose = true; }
            "--repeat" => {
                if let Some(v) = args.next() {
                    if let Ok(n) = v.parse::<usize>() { repeat = n.max(1); }
                }
            }
            "--keep-alive" | "-k" => { keep_alive = true; }
            "--parallel" | "-p"   => {
                if let Some(v) = args.next() {
                    if let Ok(n) = v.parse::<usize>() { parallel = n.max(1).min(32); }
                }
            }
            "--no-tcp-nodelay" => { tcp_nodelay = false; }
            "-h" | "--help" => {
                eprintln!(
                    "Usage: client [OPTIONS]\n\n\
Options:\n  \
  --addr HOST:PORT    Server address (default: 127.0.0.1:8443)\n  \
  --tls, --secure     Use TLS encryption\n  \
  --no-tls, --plain   Use plain TCP (default)\n  \
  --server-name NAME  TLS SNI name (default: localhost)\n  \
  --cert PATH         Path to CA cert\n  \
  --out DIR           Output directory (default: output)\n  \
  --batch             Delta sync: download missing images only\n  \
  --watch, -w         Subscribe to server-push new-image events\n  \
  --repeat N          Download images N times\n  \
  --keep-alive, -k    Reuse connection\n  \
  --parallel N, -p N  Parallel workers (max 32)\n  \
  --no-tcp-nodelay    Disable TCP_NODELAY\n  \
  --verbose           Detailed logs"
                );
                std::process::exit(0);
            }
            _ => {
                if !arg.starts_with('-') && !addr_set {
                    addr    = clean_addr(&arg);
                    addr_set = true;
                }
            }
        }
    }

    addr = clean_addr(&addr);
    if !addr.contains(':') { addr = format!("{}:8443", addr); }
    if !server_name_set {
        if let Some((host, _)) = addr.split_once(':') { server_name = host.to_string(); }
    }

    ClientArgs {
        addr, server_name, cert_path, receive_dir, batch, watch,
        verbose, repeat, keep_alive, parallel, tcp_nodelay, no_tls,
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn collect_have_ids(dir: &Path, verbose: bool) -> std::io::Result<Vec<ImageId>> {
    let mut have: HashSet<ImageId> = HashSet::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return Ok(Vec::new()); };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() { continue; }
        let Ok(bytes) = std::fs::read(&path) else { continue; };
        let id = compute_image_id(&bytes);
        vlog!(verbose, "Have {} => id={}", path.display(), hex::encode(id.to_be_bytes()));
        have.insert(id);
    }
    Ok(have.into_iter().collect())
}

// ── Plain TCP connection pool (future use) ────────────────────────────────────

#[allow(dead_code)]
struct PlainConnectionPool {
    connections: Vec<(TcpStream, Instant)>,
    addr:        String,
    max_idle:    Duration,
    tcp_nodelay: bool,
    verbose:     bool,
}

#[allow(dead_code)]
impl PlainConnectionPool {
    fn new(addr: String, tcp_nodelay: bool, verbose: bool) -> Self {
        Self { connections: Vec::new(), addr, max_idle: Duration::from_secs(30), tcp_nodelay, verbose }
    }

    async fn get(&mut self) -> Result<TcpStream, Box<dyn std::error::Error + Send + Sync>> {
        while let Some((conn, created)) = self.connections.pop() {
            if created.elapsed() < self.max_idle {
                return Ok(conn);
            }
        }
        let tcp = TcpStream::connect(&self.addr).await?;
        if self.tcp_nodelay { tcp.set_nodelay(true)?; }
        Ok(tcp)
    }

    fn return_connection(&mut self, conn: TcpStream) {
        self.connections.push((conn, Instant::now()));
    }
}

// ── TLS connection pool ───────────────────────────────────────────────────────

struct TlsConnectionPool {
    connections: Vec<(TlsStream<TcpStream>, Instant)>,
    addr:        String,
    server_name: String,
    connector:   TlsConnector,
    max_idle:    Duration,
    tcp_nodelay: bool,
    verbose:     bool,
}

impl TlsConnectionPool {
    async fn new(
        addr:        String,
        server_name: String,
        cert_path:   Option<&Path>,
        tcp_nodelay: bool,
        verbose:     bool,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let root_store = build_root_store(cert_path, verbose).await?;
        let mut config = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        config.resumption =
            Resumption::default().tls12_resumption(rustls::client::Tls12Resumption::SessionIdOrTickets);
        let connector = TlsConnector::from(Arc::new(config));
        Ok(Self {
            connections: Vec::new(),
            addr, server_name, connector,
            max_idle: Duration::from_secs(30),
            tcp_nodelay, verbose,
        })
    }

    async fn get(&mut self) -> Result<TlsStream<TcpStream>, Box<dyn std::error::Error + Send + Sync>> {
        while let Some((conn, created)) = self.connections.pop() {
            if created.elapsed() < self.max_idle {
                vlog!(self.verbose, "Reusing pooled connection (age: {:?})", created.elapsed());
                return Ok(conn);
            }
            vlog!(self.verbose, "Discarding stale connection");
        }
        vlog!(self.verbose, "Connecting to {}...", self.addr);
        let tcp = TcpStream::connect(&self.addr).await?;
        if self.tcp_nodelay { tcp.set_nodelay(true)?; }
        let sn  = ServerName::try_from(self.server_name.clone())?;
        let tls = self.connector.connect(sn, tcp).await?;
        vlog!(self.verbose, "TLS handshake complete");
        Ok(tls)
    }

    fn return_connection(&mut self, conn: TlsStream<TcpStream>) {
        self.connections.push((conn, Instant::now()));
    }
}

// ── TLS helpers ───────────────────────────────────────────────────────────────

async fn build_root_store(
    cert_path: Option<&Path>,
    verbose:   bool,
) -> Result<RootCertStore, Box<dyn std::error::Error + Send + Sync>> {
    if let Some(path) = cert_path {
        vlog!(verbose, "Loading trusted certs from {}...", path.display());
        let bytes: Vec<u8> = tokio::fs::read(path).await?;
        let certs: Vec<rustls::pki_types::CertificateDer<'static>> = {
            let mut r = BufReader::new(std::io::Cursor::new(bytes));
            rustls_pemfile::certs(&mut r).collect::<Result<Vec<_>, _>>()?
        };
        let mut store = RootCertStore::empty();
        for cert in certs { store.add(cert)?; }
        Ok(store)
    } else {
        vlog!(verbose, "Using system root certificates...");
        let mut store = RootCertStore::empty();
        store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Ok(store)
    }
}

#[allow(dead_code)]
async fn plain_connect(
    addr:        &str,
    tcp_nodelay: bool,
    verbose:     bool,
) -> Result<TcpStream, Box<dyn std::error::Error + Send + Sync>> {
    vlog!(verbose, "Connecting TCP to {}...", addr);
    let tcp = TcpStream::connect(addr).await?;
    if tcp_nodelay { tcp.set_nodelay(true)?; }
    Ok(tcp)
}

async fn tls_connect(
    addr:        &str,
    server_name: &str,
    cert_path:   Option<&Path>,
    tcp_nodelay: bool,
    verbose:     bool,
) -> Result<TlsStream<TcpStream>, Box<dyn std::error::Error + Send + Sync>> {
    vlog!(verbose, "Connecting TCP to {}...", addr);
    let tcp = TcpStream::connect(addr).await?;
    if tcp_nodelay { tcp.set_nodelay(true)?; }

    let root_store = build_root_store(cert_path, verbose).await?;
    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    config.resumption =
        Resumption::default().tls12_resumption(rustls::client::Tls12Resumption::SessionIdOrTickets);

    let connector = TlsConnector::from(Arc::new(config));
    let sn = ServerName::try_from(server_name.to_owned())?;
    Ok(connector.connect(sn, tcp).await?)
}

// ── Image receiver ────────────────────────────────────────────────────────────

/// Receive a single image packet from the stream, verify its ImageID, write
/// it to disk, and return.
async fn receive_image(
    stream:      &mut TlsStream<TcpStream>,
    receive_dir: &Path,
    by_id:       &HashMap<ImageId, ListedImage>,
    verbose:     bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let flags  = stream.read_u8().await?;
    let length = read_varint_u32(stream).await?;
    let id     = stream.read_u64().await?;

    if (flags & FLAG_ENCRYPTED) != 0 {
        return Err(format!(
            "unsupported flags=0x{:02x} (encryption not implemented)", flags
        ).into());
    }

    let file_type = file_type_from_flags(flags);
    vlog!(verbose, "Image id={} type={} length={}", hex::encode(id.to_be_bytes()), file_type, length);

    // Receive raw (possibly compressed) bytes.
    let raw_data = if length > 10_000_000 {
        // Stream very large images through a temporary file.
        let tmp = receive_dir.join(format!("tmp_{}.bin", hex::encode(id.to_be_bytes())));
        let mut f   = tokio::fs::File::create(&tmp).await?;
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
        return Err(format!(
            "ImageID mismatch for {:016x}: computed {:016x} — discarding corrupt packet",
            id, computed,
        ).into());
    }

    // Determine output filename.
    let effective_meta = by_id.get(&id);
    let effective_type = effective_meta.map(|m| file_type_from_flags(m.flags)).unwrap_or(file_type);
    let ext = match effective_type { 0 => "png", 1 => "jpg", 2 => "webp", 3 => "bmp", 4 => "gif", _ => "bin" };

    let output_name = if let Some(meta) = effective_meta {
        // §5.3: NFC-normalise before using as a filesystem path.
        let base: String = Path::new(&meta.filename)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .nfc()
            .collect();
        let base = base.trim();

        // Strip any path separators that a malicious server might embed.
        let base = base.replace(['/', '\\'], "");

        // Reject relative traversal components.
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

    let output_name = output_name
        .unwrap_or_else(|| format!("output_{}.{}", hex::encode(id.to_be_bytes()), ext));

    let output_path = receive_dir.join(&output_name);
    vlog!(verbose, "Writing {} bytes to {}", data.len(), output_path.display());
    std::fs::write(&output_path, &data)?;

    Ok(())
}

// ── Keep-alive download ───────────────────────────────────────────────────────

/// Download a slice of images over a single keep-alive connection.
async fn download_batch_keepalive(
    pool:        &mut TlsConnectionPool,
    ids:         &[ImageId],
    receive_dir: &Path,
    by_id:       &HashMap<ImageId, ListedImage>,
    verbose:     bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if ids.is_empty() { return Ok(()); }

    let stream = pool.get().await?;
    let mut writer = BufWriter::with_capacity(64 * 1024, stream);

    for chunk in ids.chunks(255) {
        vlog!(verbose, "GET_BY_ID ({} ids) with keep-alive", chunk.len());

        write_get_request_buffered(&mut writer, REQUEST_FLAG_KEEP_ALIVE, chunk).await?;
        writer.flush().await?;

        // Fix §9.2: read JTPD header + returned count M.
        let mut header = [0u8; 4];
        writer.get_mut().read_exact(&mut header).await?;
        if &header != RESPONSE_GET_BY_ID {
            return Err(format!(
                "unexpected GET_BY_ID response header: {:?}", header
            ).into());
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

// ── Parallel download worker ──────────────────────────────────────────────────

async fn parallel_download_worker(
    worker_id:   usize,
    addr:        String,
    server_name: String,
    cert_path:   Option<PathBuf>,
    ids:         Vec<ImageId>,
    receive_dir: PathBuf,
    by_id:       Arc<HashMap<ImageId, ListedImage>>,
    tcp_nodelay: bool,
    verbose:     bool,
    semaphore:   Arc<Semaphore>,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let _permit = semaphore.acquire().await?;
    vlog!(verbose, "Worker {} starting with {} images", worker_id, ids.len());

    let mut pool = TlsConnectionPool::new(addr, server_name, cert_path.as_deref(), tcp_nodelay, verbose).await?;
    download_batch_keepalive(&mut pool, &ids, &receive_dir, &by_id, verbose).await?;

    vlog!(verbose, "Worker {} completed {} images", worker_id, ids.len());
    Ok(ids.len())
}

// ── WATCH mode ────────────────────────────────────────────────────────────────

/// Subscribe to WATCH events and print new-image notifications until Ctrl-C.
async fn watch_mode(
    args:    &ClientArgs,
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("Connecting for WATCH subscription...");
    let tls    = tls_connect(&args.addr, &args.server_name, args.cert_path.as_deref(), args.tcp_nodelay, verbose).await?;
    let mut stream = BufWriter::with_capacity(64 * 1024, tls);

    write_watch_request_buffered(&mut stream).await?;
    stream.flush().await?;
    println!("WATCH subscription active. Waiting for new images (Ctrl-C to cancel)...");

    loop {
        let mut header = [0u8; 4];
        stream.get_mut().read_exact(&mut header).await?;

        match &header {
            h if h == RESPONSE_WATCH => {
                let id       = stream.get_mut().read_u64().await?;
                let flags    = stream.get_mut().read_u8().await?;
                let name_len = stream.get_mut().read_u16().await? as usize;
                let mut name_buf = vec![0u8; name_len];
                stream.get_mut().read_exact(&mut name_buf).await?;
                let size     = read_varint_u32(stream.get_mut()).await?;

                // §5.3: NFC-normalise filename on receipt.
                let filename: String = String::from_utf8_lossy(&name_buf).nfc().collect();

                let file_type = file_type_from_flags(flags);
                println!(
                    "NEW  id={} type={} size={} name={}",
                    hex::encode(id.to_be_bytes()), file_type, size, filename,
                );
            }
            h if h == b"JTPE" => {
                // Structured error from server
                let code    = stream.get_mut().read_u8().await?;
                let msg_len = stream.get_mut().read_u16().await? as usize;
                let mut msg = vec![0u8; msg_len];
                stream.get_mut().read_exact(&mut msg).await?;
                return Err(format!(
                    "Server error during WATCH (code {}): {}",
                    code, String::from_utf8_lossy(&msg),
                ).into());
            }
            other => {
                return Err(format!("Unexpected WATCH frame header: {:?}", other).into());
            }
        }
    }
}

// ── LIST helpers ─────────────────────────────────────────────────────────────

/// Read a LIST response from a stream.
async fn read_list_response(
    stream:  &mut BufWriter<TlsStream<TcpStream>>,
    verbose: bool,
) -> Result<Vec<ListedImage>, Box<dyn std::error::Error + Send + Sync>> {
    let mut list_header = [0u8; 4];
    stream.get_mut().read_exact(&mut list_header).await?;
    if &list_header != RESPONSE_LIST {
        return Err(format!("unexpected LIST header: {:?}", list_header).into());
    }
    vlog!(verbose, "LIST response header OK (JTPL)");

    // §9.1: read varint count (was read_u16).
    let count = read_varint_u32(stream.get_mut()).await? as usize;
    vlog!(verbose, "LIST count={}", count);

    let mut listed   = Vec::with_capacity(count);
    let mut name_buf = vec![0u8; 256];

    for _ in 0..count {
        let id       = stream.get_mut().read_u64().await?;
        let flags    = stream.get_mut().read_u8().await?;
        let name_len = stream.get_mut().read_u16().await? as usize;
        if name_len > name_buf.len() { name_buf.resize(name_len, 0); }
        stream.get_mut().read_exact(&mut name_buf[..name_len]).await?;

        // §5.3: NFC-normalise filename on receipt.
        let filename: String = String::from_utf8_lossy(&name_buf[..name_len])
            .nfc()
            .collect::<String>()
            .trim()
            .to_string();

        let size = read_varint_u32(stream.get_mut()).await?;
        listed.push(ListedImage { id, flags, filename, size });
    }

    Ok(listed)
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args    = parse_args();
    let verbose = args.verbose;

    vlog!(
        verbose,
        "Client: addr={} server_name={} cert={:?} out={} parallel={} keep_alive={} watch={}",
        args.addr, args.server_name, args.cert_path,
        args.receive_dir.display(), args.parallel, args.keep_alive, args.watch
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

        let tls        = tls_connect(&args.addr, &args.server_name, args.cert_path.as_deref(), args.tcp_nodelay, verbose).await?;
        let mut stream = BufWriter::with_capacity(64 * 1024, tls);
        write_list_request_buffered(&mut stream, 0).await?;
        stream.flush().await?;

        let listed = read_list_response(&mut stream, verbose).await?;
        drop(stream);

        let by_id: HashMap<ImageId, ListedImage> =
            listed.into_iter().map(|item| (item.id, item)).collect();

        let have_ids = collect_have_ids(&receive_dir, verbose)?;
        vlog!(verbose, "BATCH: sending {} have IDs", have_ids.len());

        let tls        = tls_connect(&args.addr, &args.server_name, args.cert_path.as_deref(), args.tcp_nodelay, verbose).await?;
        let mut stream = BufWriter::with_capacity(64 * 1024, tls);
        let req_flags  = if args.keep_alive { REQUEST_FLAG_KEEP_ALIVE } else { 0 };
        write_batch_request_buffered(&mut stream, req_flags, &have_ids).await?;
        stream.flush().await?;

        let mut header = [0u8; 4];
        stream.get_mut().read_exact(&mut header).await?;
        if &header != RESPONSE_BATCH {
            return Err(format!("unexpected BATCH header: {:?}", header).into());
        }

        let missing_count = read_varint_u32(stream.get_mut()).await? as usize;
        println!("Server reports {} missing images", missing_count);

        for _ in 0..missing_count {
            receive_image(stream.get_mut(), &receive_dir, &by_id, verbose).await?;
        }

        return Ok(());
    }

    // ── Standard LIST + GET mode ──────────────────────────────────────────────
    vlog!(verbose, "Opening LIST connection...");
    let tls        = tls_connect(&args.addr, &args.server_name, args.cert_path.as_deref(), args.tcp_nodelay, verbose).await?;
    let mut stream = BufWriter::with_capacity(64 * 1024, tls);
    write_list_request_buffered(&mut stream, 0).await?;
    stream.flush().await?;

    let listed = read_list_response(&mut stream, verbose).await?;
    drop(stream);

    vlog!(verbose, "Parsed {} catalog entries", listed.len());

    if listed.is_empty() {
        println!("No images available on server.");
        return Ok(());
    }

    println!("Server catalog:");
    for item in &listed {
        println!(
            "- {}  {}  {} bytes",
            hex::encode(item.id.to_be_bytes()), item.filename, item.size,
        );
    }

    let ids:   Vec<ImageId>                   = listed.iter().map(|i| i.id).collect();
    let by_id: Arc<HashMap<ImageId, ListedImage>> =
        Arc::new(listed.into_iter().map(|item| (item.id, item)).collect());

    let start = Instant::now();

    for iteration in 0..args.repeat {
        vlog!(verbose, "Iteration {}/{}", iteration + 1, args.repeat);

        if args.parallel > 1 {
            let chunk_size = (ids.len() + args.parallel - 1) / args.parallel;
            let semaphore  = Arc::new(Semaphore::new(args.parallel));
            let mut handles = Vec::new();

            for (worker_id, chunk) in ids.chunks(chunk_size).enumerate() {
                handles.push(tokio::spawn(parallel_download_worker(
                    worker_id,
                    args.addr.clone(),
                    args.server_name.clone(),
                    args.cert_path.clone(),
                    chunk.to_vec(),
                    receive_dir.clone(),
                    Arc::clone(&by_id),
                    args.tcp_nodelay,
                    verbose,
                    Arc::clone(&semaphore),
                )));
            }

            let mut total = 0;
            for handle in handles {
                match handle.await? {
                    Ok(n)  => total += n,
                    Err(e) => eprintln!("Worker error: {}", e),
                }
            }
            vlog!(verbose, "Parallel download complete: {} images", total);

        } else if args.keep_alive {
            let mut pool = TlsConnectionPool::new(
                args.addr.clone(), args.server_name.clone(),
                args.cert_path.as_deref(), args.tcp_nodelay, verbose,
            ).await?;
            download_batch_keepalive(&mut pool, &ids, &receive_dir, &by_id, verbose).await?;

        } else {
            // One connection, no keep-alive.
            vlog!(verbose, "Opening GET_BY_ID connection (iteration {}/{})...", iteration + 1, args.repeat);
            let tls        = tls_connect(&args.addr, &args.server_name, args.cert_path.as_deref(), args.tcp_nodelay, verbose).await?;
            let mut stream = BufWriter::with_capacity(64 * 1024, tls);

            if ids.len() > u8::MAX as usize {
                return Err(format!("too many images for single request: {}", ids.len()).into());
            }

            // No keep-alive on this connection.
            write_get_request_buffered(&mut stream, 0, &ids).await?;
            stream.flush().await?;

            // §9.2: read JTPD header + M count.
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

    let elapsed     = start.elapsed();
    let total_images = ids.len() * args.repeat;
    println!(
        "Downloaded {} images in {:?} ({:.1} images/sec)",
        total_images, elapsed,
        (total_images as f64) / elapsed.as_secs_f64(),
    );

    Ok(())
}