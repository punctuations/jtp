use tokio::net::TcpStream;
use tokio::io::{ AsyncReadExt, AsyncWriteExt, BufWriter };
use tokio::sync::Semaphore;
use jtp::protocol::{
    compute_image_id,
    file_type_from_flags,
    read_varint_u32,
    write_list_request_buffered,
    write_get_request_buffered,
    write_batch_request_buffered,
    FLAG_COMPRESSED,
    FLAG_ENCRYPTED,
    ImageId,
    REQUEST_FLAG_KEEP_ALIVE,
    RESPONSE_BATCH,
    RESPONSE_LIST,
};
use rustls::pki_types::ServerName;
use rustls::RootCertStore;
use rustls::client::Resumption;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use std::sync::Arc;
use std::io::BufReader;
use std::path::Path;
use std::collections::{ HashMap, HashSet };
use std::path::PathBuf;
use std::time::{ Duration, Instant };

macro_rules! vlog {
    (
        $enabled:expr,
        $($arg:tt)*
    ) => {
        if $enabled {
            eprintln!($($arg)*);
        }
    };
}

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
    verbose: bool,
    repeat: usize,
    keep_alive: bool,
    parallel: usize,
    tcp_nodelay: bool,
    #[allow(dead_code)] // For future use
    no_tls: bool,
}

fn parse_args() -> ClientArgs {
    fn clean_addr(raw: &str) -> String {
        raw.strip_prefix("jtp://").unwrap_or(raw).to_string()
    }

    let mut addr = String::from("127.0.0.1:8443"); // JTP always on 8443
    let mut server_name = String::from("localhost");
    let mut cert_path: Option<PathBuf> = None;
    let mut receive_dir = PathBuf::from("output");
    let mut batch = false;
    let mut verbose = false;
    let mut repeat = 1;
    let mut keep_alive = false;
    let mut parallel: usize = 1;
    let mut tcp_nodelay = true;
    let mut no_tls = true; // Default to plain TCP (like HTTP)
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
            "--tls" | "--secure" => {
                no_tls = false;
                // If TLS enabled and addr not explicitly set, use TLS port
                if !addr_set {
                    addr = String::from("127.0.0.1:8443");
                }
            }
            "--no-tls" | "--plain" => {
                no_tls = true;
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
  --addr HOST:PORT    Server address (default: 127.0.0.1:8443)\n  \
  --tls, --secure     Use TLS encryption (default port: 8443)\n  \
  --no-tls, --plain   Use plain TCP (default)\n  \
  --server-name NAME  TLS SNI name (default: localhost)\n  \
  --cert PATH         Path to custom CA cert (default: system roots)\n  \
  --out DIR           Output directory (default: output)\n  \
  --batch             Delta sync: download missing images only\n  \
  --repeat N          Download images N times (default: 1)\n  \
  --keep-alive, -k    Reuse connection for multiple requests\n  \
  --parallel N, -p N  Parallel download workers (default: 1, max: 32)\n  \
  --no-tcp-nodelay    Disable TCP_NODELAY\n  \
  --verbose           Print detailed logs"
                );
                std::process::exit(0);
            }
            _ => {
                // Treat the first bare value (no flag) as the address for convenience
                if !arg.starts_with('-') && !addr_set {
                    addr = clean_addr(&arg);
                    addr_set = true;
                }
            }
        }
    }

    // Strip optional scheme if it survived earlier
    addr = clean_addr(&addr);

    // If the address omitted a port (e.g., hostname only), default to 8443
    if !addr.contains(':') {
        addr = format!("{}:8443", addr);
    }

    // Default SNI to the host portion of the address when user did not override it
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
        verbose,
        repeat,
        keep_alive,
        parallel,
        tcp_nodelay,
        no_tls,
    }
}

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
        vlog!(verbose, "Have file {} => id={}", path.display(), hex::encode(id.to_be_bytes()));
        have.insert(id);
    }

    Ok(have.into_iter().collect())
}

// Plain TCP connection pool (no TLS) - for future use
#[allow(dead_code)]
struct PlainConnectionPool {
    connections: Vec<(TcpStream, Instant)>,
    addr: String,
    max_idle: Duration,
    tcp_nodelay: bool,
    verbose: bool,
}

#[allow(dead_code)]
impl PlainConnectionPool {
    fn new(addr: String, tcp_nodelay: bool, verbose: bool) -> Self {
        Self {
            connections: Vec::new(),
            addr,
            max_idle: Duration::from_secs(30),
            tcp_nodelay,
            verbose,
        }
    }

    async fn get(&mut self) -> Result<TcpStream, Box<dyn std::error::Error + Send + Sync>> {
        while let Some((conn, created)) = self.connections.pop() {
            if created.elapsed() < self.max_idle {
                vlog!(self.verbose, "Reusing pooled connection (age: {:?})", created.elapsed());
                return Ok(conn);
            }
            vlog!(self.verbose, "Discarding stale connection (age: {:?})", created.elapsed());
        }

        vlog!(self.verbose, "Creating new connection to {}...", self.addr);
        let tcp = TcpStream::connect(&self.addr).await?;

        if self.tcp_nodelay {
            tcp.set_nodelay(true)?;
        }

        Ok(tcp)
    }

    fn return_connection(&mut self, conn: TcpStream) {
        vlog!(self.verbose, "Returning connection to pool");
        self.connections.push((conn, Instant::now()));
    }
}

// TLS connection pool
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
        let root_store = if let Some(path) = cert_path {
            vlog!(verbose, "Loading trusted certs from {}...", path.display());
            let cert_bytes = tokio::fs::read(path).await?;
            let certs: Vec<rustls::pki_types::CertificateDer<'static>> = {
                let mut reader = BufReader::new(std::io::Cursor::new(cert_bytes));
                rustls_pemfile::certs(&mut reader).collect::<Result<Vec<_>, _>>()?
            };
            let mut store = RootCertStore::empty();
            for cert in certs {
                store.add(cert)?;
            }
            store
        } else {
            vlog!(verbose, "Using system root certificates...");
            let mut store = RootCertStore::empty();
            store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            store
        };

        let mut client_config = rustls::ClientConfig
            ::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();

        client_config.resumption = Resumption::default().tls12_resumption(
            rustls::client::Tls12Resumption::SessionIdOrTickets
        );

        let connector = TlsConnector::from(Arc::new(client_config));

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
                vlog!(self.verbose, "Reusing pooled connection (age: {:?})", created.elapsed());
                return Ok(conn);
            }
            vlog!(self.verbose, "Discarding stale connection (age: {:?})", created.elapsed());
        }

        vlog!(self.verbose, "Creating new connection to {}...", self.addr);
        let tcp = TcpStream::connect(&self.addr).await?;

        if self.tcp_nodelay {
            tcp.set_nodelay(true)?;
        }

        let server_name = ServerName::try_from(self.server_name.clone())?;
        let tls = self.connector.connect(server_name, tcp).await?;
        vlog!(self.verbose, "TLS handshake complete");

        Ok(tls)
    }

    fn return_connection(&mut self, conn: TlsStream<TcpStream>) {
        vlog!(self.verbose, "Returning connection to pool");
        self.connections.push((conn, Instant::now()));
    }
}

// Simple plain TCP connect - for future use
#[allow(dead_code)]
async fn plain_connect(
    addr: &str,
    tcp_nodelay: bool,
    verbose: bool
) -> Result<TcpStream, Box<dyn std::error::Error + Send + Sync>> {
    vlog!(verbose, "Connecting TCP to {}...", addr);
    let tcp = TcpStream::connect(addr).await?;

    if tcp_nodelay {
        tcp.set_nodelay(true)?;
    }

    Ok(tcp)
}

// Legacy function for backward compatibility
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

    let root_store = if let Some(path) = cert_path {
        vlog!(verbose, "Loading trusted certs from {}...", path.display());
        let cert_bytes = tokio::fs::read(path).await?;
        let certs: Vec<rustls::pki_types::CertificateDer<'static>> = {
            let mut reader = BufReader::new(std::io::Cursor::new(cert_bytes));
            rustls_pemfile::certs(&mut reader).collect::<Result<Vec<_>, _>>()?
        };
        let mut store = RootCertStore::empty();
        for cert in certs {
            store.add(cert)?;
        }
        store
    } else {
        vlog!(verbose, "Using system root certificates...");
        let mut store = RootCertStore::empty();
        store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        store
    };

    let mut client_config = rustls::ClientConfig
        ::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    // Enable TLS session resumption
    client_config.resumption = Resumption::default().tls12_resumption(
        rustls::client::Tls12Resumption::SessionIdOrTickets
    );

    let connector = TlsConnector::from(Arc::new(client_config));

    let server_name = ServerName::try_from(server_name.to_owned())?;
    Ok(connector.connect(server_name, tcp).await?)
}

// Helper to receive a single image from stream
async fn receive_image(
    stream: &mut TlsStream<TcpStream>,
    receive_dir: &Path,
    by_id: &HashMap<ImageId, ListedImage>,
    verbose: bool
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let flags = stream.read_u8().await?;
    let length = read_varint_u32(stream).await?;
    let id = stream.read_u64().await?;

    if (flags & FLAG_ENCRYPTED) != 0 {
        return Err(
            format!("unsupported image flags=0x{:02x} (encryption not implemented)", flags).into()
        );
    }

    let file_type = file_type_from_flags(flags);

    vlog!(
        verbose,
        "Image id={} type={} length={} bytes",
        hex::encode(id.to_be_bytes()),
        file_type,
        length
    );

    // Handle compressed or large files appropriately
    let data = if (flags & FLAG_COMPRESSED) != 0 {
        let mut buf = vec![0u8; length as usize];
        stream.read_exact(&mut buf).await?;
        let decompressed = jtp::protocol::decompress(&buf)?;
        vlog!(verbose, "Decompressed {} -> {} bytes", length, decompressed.len());
        decompressed
    } else if length > 10_000_000 {
        let temp_path = receive_dir.join(format!("temp_{}.bin", hex::encode(id.to_be_bytes())));
        let mut temp_file = tokio::fs::File::create(&temp_path).await?;
        let mut remaining = length as usize;
        let mut buf = vec![0u8; 65536];
        while remaining > 0 {
            let to_read = remaining.min(buf.len());
            stream.read_exact(&mut buf[..to_read]).await?;
            temp_file.write_all(&buf[..to_read]).await?;
            remaining -= to_read;
        }
        vlog!(verbose, "Streamed {} bytes to temp file", length);
        let data = tokio::fs::read(&temp_path).await?;
        tokio::fs::remove_file(&temp_path).await?;
        data
    } else {
        let mut buf = vec![0u8; length as usize];
        stream.read_exact(&mut buf).await?;
        buf
    };

    let effective_metadata = by_id.get(&id);
    let effective_file_type = effective_metadata
        .map(|m| file_type_from_flags(m.flags))
        .unwrap_or(file_type);

    let ext = match effective_file_type {
        0 => "png",
        1 => "jpg",
        2 => "webp",
        3 => "bmp",
        4 => "gif",
        _ => "bin",
    };

    let preferred_name = effective_metadata.map(|m| m.filename.as_str());
    let output_name = if let Some(name) = preferred_name {
        let base = Path::new(name)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .trim();
        if base.is_empty() {
            None
        } else if base.contains('.') {
            Some(base.to_string())
        } else {
            Some(format!("{}.{}", base, ext))
        }
    } else {
        None
    };

    let output_name = output_name.unwrap_or_else(|| {
        format!("output_{}.{}", hex::encode(id.to_be_bytes()), ext)
    });

    let output_path = receive_dir.join(output_name);
    vlog!(verbose, "Writing {} bytes to {}", data.len(), output_path.display());
    std::fs::write(output_path, data)?;

    Ok(())
}

// Download a batch of images using a single connection with keep-alive
async fn download_batch_keepalive(
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

    // Split into chunks of 255 (max for u8 count)
    for chunk in ids.chunks(255) {
        vlog!(verbose, "Sending GET_BY_ID request ({} ids) with keep-alive", chunk.len());

        // Send request with keep-alive flag - single syscall for all header + IDs
        write_get_request_buffered(&mut writer, REQUEST_FLAG_KEEP_ALIVE, chunk).await?;
        writer.flush().await?;

        // Receive images
        for _ in chunk {
            receive_image(writer.get_mut(), receive_dir, by_id, verbose).await?;
        }
    }

    pool.return_connection(writer.into_inner());
    Ok(())
}

// Parallel download worker
async fn parallel_download_worker(
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

    vlog!(verbose, "Worker {} starting with {} images", worker_id, ids.len());

    let mut pool = TlsConnectionPool::new(
        addr,
        server_name,
        cert_path.as_deref(),
        tcp_nodelay,
        verbose
    ).await?;

    download_batch_keepalive(&mut pool, &ids, &receive_dir, &by_id, verbose).await?;

    vlog!(verbose, "Worker {} completed {} images", worker_id, ids.len());
    Ok(ids.len())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = parse_args();
    let verbose = args.verbose;

    vlog!(
        verbose,
        "Client args: addr={}, server_name={}, cert={:?}, out={}, parallel={}, keep_alive={}",
        args.addr,
        args.server_name,
        args.cert_path,
        args.receive_dir.display(),
        args.parallel,
        args.keep_alive
    );

    let receive_dir = args.receive_dir.clone();
    std::fs::create_dir_all(&receive_dir)?;

    // BATCH mode (delta sync)
    if args.batch {
        // First, fetch the catalog to get filenames
        vlog!(verbose, "BATCH mode: fetching catalog first for filenames...");
        let tls_list_stream = tls_connect(
            &args.addr,
            &args.server_name,
            args.cert_path.as_deref(),
            args.tcp_nodelay,
            verbose
        ).await?;
        let mut list_stream = BufWriter::with_capacity(64 * 1024, tls_list_stream);

        write_list_request_buffered(&mut list_stream, 0).await?;
        list_stream.flush().await?;

        let mut list_header = [0u8; 4];
        list_stream.get_mut().read_exact(&mut list_header).await?;
        if &list_header != RESPONSE_LIST {
            return Err(format!("unexpected LIST response header: {:?}", list_header).into());
        }

        let count = list_stream.get_mut().read_u16().await? as usize;
        let mut by_id: HashMap<ImageId, ListedImage> = HashMap::with_capacity(count);
        let mut name_buf = vec![0u8; 256];

        for _ in 0..count {
            let id = list_stream.get_mut().read_u64().await?;
            let flags = list_stream.get_mut().read_u8().await?;
            let name_len = list_stream.get_mut().read_u16().await? as usize;
            if name_len > name_buf.len() {
                name_buf.resize(name_len, 0);
            }
            list_stream.get_mut().read_exact(&mut name_buf[..name_len]).await?;
            let filename = String::from_utf8_lossy(&name_buf[..name_len]).trim().to_string();
            let size = read_varint_u32(list_stream.get_mut()).await?;
            by_id.insert(id, ListedImage { id, flags, filename, size });
        }
        drop(list_stream);
        vlog!(verbose, "Catalog fetched: {} images", by_id.len());

        // Now do the BATCH request
        let have_ids = collect_have_ids(&receive_dir, verbose)?;
        vlog!(verbose, "Delta sync: sending {} have IDs", have_ids.len());

        let tls_stream = tls_connect(
            &args.addr,
            &args.server_name,
            args.cert_path.as_deref(),
            args.tcp_nodelay,
            verbose
        ).await?;
        let mut stream = BufWriter::with_capacity(64 * 1024, tls_stream);
        vlog!(verbose, "TLS connected; sending BATCH request");

        // Send BATCH with optional keep-alive flag - single syscall
        let request_flags = if args.keep_alive { REQUEST_FLAG_KEEP_ALIVE } else { 0 };
        write_batch_request_buffered(&mut stream, request_flags, &have_ids).await?;
        stream.flush().await?;

        let mut header = [0u8; 4];
        stream.get_mut().read_exact(&mut header).await?;
        if &header != RESPONSE_BATCH {
            return Err(format!("unexpected BATCH response header: {:?}", header).into());
        }

        let missing_count = read_varint_u32(stream.get_mut()).await? as usize;
        println!("Server missing_count={}", missing_count);

        for _ in 0..missing_count {
            receive_image(stream.get_mut(), &receive_dir, &by_id, verbose).await?;
        }

        return Ok(());
    }

    // 1) Discover available images via LIST
    vlog!(verbose, "Opening LIST connection...");
    let tls_list_stream = tls_connect(
        &args.addr,
        &args.server_name,
        args.cert_path.as_deref(),
        args.tcp_nodelay,
        verbose
    ).await?;
    let mut list_stream = BufWriter::with_capacity(64 * 1024, tls_list_stream);
    vlog!(verbose, "TLS connected; sending LIST request");

    // Send LIST with flags - single syscall
    write_list_request_buffered(&mut list_stream, 0).await?; // No keep-alive for LIST
    list_stream.flush().await?;

    let mut list_header = [0u8; 4];
    list_stream.get_mut().read_exact(&mut list_header).await?;
    if &list_header != RESPONSE_LIST {
        return Err(format!("unexpected LIST response header: {:?}", list_header).into());
    }

    vlog!(verbose, "LIST response header OK (JTPL)");

    let count = list_stream.get_mut().read_u16().await? as usize;
    vlog!(verbose, "LIST count={}", count);
    let mut listed: Vec<ListedImage> = Vec::with_capacity(count);

    // Reusable buffer for filenames
    let mut name_buf = vec![0u8; 256];

    for _ in 0..count {
        let id = list_stream.get_mut().read_u64().await?;
        let flags = list_stream.get_mut().read_u8().await?;

        let name_len = list_stream.get_mut().read_u16().await? as usize;
        if name_len > name_buf.len() {
            name_buf.resize(name_len, 0);
        }
        list_stream.get_mut().read_exact(&mut name_buf[..name_len]).await?;
        let filename = String::from_utf8_lossy(&name_buf[..name_len])
            .trim()
            .to_string();

        let size = read_varint_u32(list_stream.get_mut()).await?;

        listed.push(ListedImage { id, flags, filename, size });
    }

    vlog!(verbose, "Parsed {} catalog entries", listed.len());

    if listed.is_empty() {
        println!("No images available on server.");
        return Ok(());
    }

    drop(list_stream);

    println!("Server catalog:");
    for item in &listed {
        println!(
            "- {}  {}  {} bytes",
            hex::encode(item.id.to_be_bytes()),
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

    // 2) Download images
    let start_time = Instant::now();

    for iteration in 0..args.repeat {
        vlog!(verbose, "Download iteration {}/{}", iteration + 1, args.repeat);

        if args.parallel > 1 {
            // Parallel download mode
            let chunk_size = (ids.len() + args.parallel - 1) / args.parallel;
            let semaphore = Arc::new(Semaphore::new(args.parallel));

            let mut handles = Vec::new();
            for (worker_id, chunk) in ids.chunks(chunk_size).enumerate() {
                let addr = args.addr.clone();
                let server_name = args.server_name.clone();
                let cert_path = args.cert_path.clone();
                let ids_chunk = chunk.to_vec();
                let receive_dir = receive_dir.clone();
                let by_id = Arc::clone(&by_id);
                let semaphore = Arc::clone(&semaphore);
                let tcp_nodelay = args.tcp_nodelay;

                handles.push(
                    tokio::spawn(
                        parallel_download_worker(
                            worker_id,
                            addr,
                            server_name,
                            cert_path,
                            ids_chunk,
                            receive_dir,
                            by_id,
                            tcp_nodelay,
                            verbose,
                            semaphore
                        )
                    )
                );
            }

            // Wait for all workers
            let mut total_downloaded = 0;
            for handle in handles {
                match handle.await? {
                    Ok(count) => {
                        total_downloaded += count;
                    }
                    Err(e) => eprintln!("Worker error: {}", e),
                }
            }
            vlog!(verbose, "Parallel download complete: {} images", total_downloaded);
        } else if args.keep_alive {
            // Single connection with keep-alive
            let mut pool = TlsConnectionPool::new(
                args.addr.clone(),
                args.server_name.clone(),
                args.cert_path.as_deref(),
                args.tcp_nodelay,
                verbose
            ).await?;

            download_batch_keepalive(&mut pool, &ids, &receive_dir, &by_id, verbose).await?;
        } else {
            // Legacy mode: new connection per request batch
            vlog!(
                verbose,
                "Opening GET_BY_ID connection (iteration {}/{})...",
                iteration + 1,
                args.repeat
            );
            let tls_stream = tls_connect(
                &args.addr,
                &args.server_name,
                args.cert_path.as_deref(),
                args.tcp_nodelay,
                verbose
            ).await?;
            let mut stream = BufWriter::with_capacity(64 * 1024, tls_stream);

            vlog!(verbose, "TLS connected; sending GET_BY_ID request ({} ids)", ids.len());

            // Send request with buffered write - single syscall
            if ids.len() > (u8::MAX as usize) {
                return Err(
                    format!("too many images to request in one batch: {}", ids.len()).into()
                );
            }
            write_get_request_buffered(&mut stream, 0, &ids).await?; // No keep-alive
            stream.flush().await?;

            for _ in &ids {
                receive_image(stream.get_mut(), &receive_dir, &by_id, verbose).await?;
            }
        }
    }

    let elapsed = start_time.elapsed();
    let total_images = ids.len() * args.repeat;
    println!(
        "Downloaded {} images in {:?} ({:.1} images/sec)",
        total_images,
        elapsed,
        (total_images as f64) / elapsed.as_secs_f64()
    );

    Ok(())
}
