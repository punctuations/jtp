use tokio::net::TcpListener;
use tokio::io::{ AsyncReadExt, AsyncWriteExt, BufWriter };
use jtp::protocol::{
    ImageCatalog,
    send_catalog_buffered,
    send_image_with_options,
    send_error,
    read_varint_u32,
    validate_request_flags,
    read_image_ids,
    encode_varint_to_buf,
    ImageId,
    ErrorCode,
    REQUEST_BATCH,
    REQUEST_GET_BY_ID,
    REQUEST_LIST,
    REQUEST_LIST_AND_GET,
    REQUEST_FLAG_KEEP_ALIVE,
    RESPONSE_BATCH,
    RESPONSE_LIST_AND_GET,
};
use tokio_rustls::TlsAcceptor;
use rustls::ServerConfig;
use rustls::pki_types::{ CertificateDer, PrivateKeyDer };
use std::sync::Arc;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;
use std::collections::HashSet;
use std::time::Duration;

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

async fn load_or_generate_tls_material(
    cert_path: &Path,
    key_path: &Path
) -> tokio::io::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    if !cert_path.exists() || !key_path.exists() {
        let certified = rcgen
            ::generate_simple_self_signed(vec!["localhost".to_string()])
            .map_err(|e| tokio::io::Error::new(tokio::io::ErrorKind::InvalidData, e))?;

        let cert_pem = certified.cert.pem();
        let key_pem = certified.key_pair.serialize_pem();

        tokio::fs::write(cert_path, cert_pem).await?;
        tokio::fs::write(key_path, key_pem).await?;

        println!("Generated self-signed TLS material: cert.pem + key.pem");
        println!("Client must trust cert.pem (same folder as client run)");
    }

    let cert_bytes = tokio::fs::read(cert_path).await?;
    let key_bytes = tokio::fs::read(key_path).await?;

    let certs: Vec<CertificateDer<'static>> = {
        let mut reader = BufReader::new(std::io::Cursor::new(cert_bytes));
        rustls_pemfile::certs(&mut reader).collect::<Result<Vec<_>, _>>()?
    };

    let key: PrivateKeyDer<'static> = {
        let mut reader = BufReader::new(std::io::Cursor::new(key_bytes));
        rustls_pemfile
            ::private_key(&mut reader)?
            .ok_or_else(|| {
                tokio::io::Error::new(
                    tokio::io::ErrorKind::InvalidData,
                    "no private key found in key.pem"
                )
            })?
    };

    Ok((certs, key))
}

#[derive(Debug, Clone)]
struct ServerArgs {
    bind: String,
    images_dir: PathBuf,
    cert_path: PathBuf,
    key_path: PathBuf,
    only_name_contains: Option<String>,
    compression_threshold: f32,
    verbose: bool,
    keep_alive_timeout: Duration,
    tcp_nodelay: bool,
    no_tls: bool, // Plain TCP mode for trusted networks/benchmarking
}

fn parse_args() -> ServerArgs {
    let mut bind = String::from("0.0.0.0:8443");
    let mut images_dir = PathBuf::from("images");
    let mut cert_path = PathBuf::from("cert.pem");
    let mut key_path = PathBuf::from("key.pem");
    let mut only_name_contains: Option<String> = None;
    let mut compression_threshold = jtp::protocol::DEFAULT_MIN_COMPRESSION_RATIO;
    let mut verbose = false;
    let mut keep_alive_timeout = Duration::from_secs(30);
    let mut tcp_nodelay = true;
    let mut no_tls = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => {
                if let Some(v) = args.next() {
                    bind = v;
                }
            }
            "--images" | "--images-dir" => {
                if let Some(v) = args.next() {
                    images_dir = PathBuf::from(v);
                }
            }
            "--cert" => {
                if let Some(v) = args.next() {
                    cert_path = PathBuf::from(v);
                }
            }
            "--key" => {
                if let Some(v) = args.next() {
                    key_path = PathBuf::from(v);
                }
            }
            "--only" | "--name-contains" => {
                if let Some(v) = args.next() {
                    only_name_contains = Some(v);
                }
            }
            "--compression-threshold" | "--compress-threshold" => {
                if let Some(v) = args.next() {
                    if let Ok(threshold) = v.parse::<f32>() {
                        compression_threshold = threshold.clamp(0.0, 1.0);
                    }
                }
            }
            "--keep-alive-timeout" => {
                if let Some(v) = args.next() {
                    if let Ok(secs) = v.parse::<u64>() {
                        keep_alive_timeout = Duration::from_secs(secs);
                    }
                }
            }
            "--no-tcp-nodelay" => {
                tcp_nodelay = false;
            }
            "--no-tls" | "--plain" => {
                no_tls = true;
            }
            "-v" | "--verbose" => {
                verbose = true;
            }
            "-h" | "--help" => {
                eprintln!(
                    "Usage: server [OPTIONS]\n\n\
Options:\n  \
  --bind ADDR               Bind address (default: 0.0.0.0:8443)\n  \
  --images DIR              Images directory to scan (default: images)\n  \
  --cert PATH               Path to TLS certificate (default: cert.pem)\n  \
  --key PATH                Path to TLS private key (default: key.pem)\n  \
  --only SUBSTRING          Only serve files whose basename contains SUBSTRING\n  \
  --compression-threshold   Min ratio to use compression (default: 0.95)\n  \
  --keep-alive-timeout SEC  Keep-alive idle timeout in seconds (default: 30)\n  \
  --no-tcp-nodelay          Disable TCP_NODELAY (Nagle's algorithm enabled)\n  \
  --no-tls, --plain         Plain TCP mode (no TLS) for trusted networks\n  \
  --verbose                 Print per-connection and request logs"
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }

    ServerArgs {
        bind,
        images_dir,
        cert_path,
        key_path,
        only_name_contains,
        compression_threshold,
        verbose,
        keep_alive_timeout,
        tcp_nodelay,
        no_tls,
    }
}

// Generic request handler that works with any AsyncRead + AsyncWrite stream
async fn handle_requests<S>(
    mut stream: BufWriter<S>,
    catalog: Arc<ImageCatalog>,
    compression_threshold: f32,
    keep_alive_timeout: Duration,
    verbose: bool
)
    where S: AsyncReadExt + AsyncWriteExt + Unpin
{
    let mut request_count = 0u64;
    loop {
        let read_timeout = if request_count == 0 {
            Duration::from_secs(60)
        } else {
            keep_alive_timeout
        };

        let mut header = [0u8; 2];
        let header_result = tokio::time::timeout(
            read_timeout,
            stream.get_mut().read_exact(&mut header)
        ).await;

        let (request_type, request_flags) = match header_result {
            Ok(Ok(_)) => (header[0], header[1]),
            Ok(Err(_)) => {
                vlog!(verbose, "Client disconnected (requests served: {})", request_count);
                return;
            }
            Err(_) => {
                vlog!(verbose, "Keep-alive timeout (requests served: {})", request_count);
                return;
            }
        };

        request_count += 1;

        if let Err(e) = validate_request_flags(request_flags) {
            vlog!(verbose, "Invalid request flags: {}", e);
            let _ = send_error(&mut stream, ErrorCode::InvalidRequest, "reserved flags set").await;
            let _ = stream.flush().await;
            return;
        }

        let keep_alive = (request_flags & REQUEST_FLAG_KEEP_ALIVE) != 0;

        match request_type {
            REQUEST_LIST =>
                vlog!(verbose, "Request #{}: LIST (keep-alive={})", request_count, keep_alive),
            REQUEST_GET_BY_ID =>
                vlog!(verbose, "Request #{}: GET_BY_ID (keep-alive={})", request_count, keep_alive),
            REQUEST_BATCH =>
                vlog!(verbose, "Request #{}: BATCH (keep-alive={})", request_count, keep_alive),
            REQUEST_LIST_AND_GET =>
                vlog!(
                    verbose,
                    "Request #{}: LIST_AND_GET (keep-alive={})",
                    request_count,
                    keep_alive
                ),
            other => {
                vlog!(verbose, "Unknown request type: {}", other);
                let _ = send_error(
                    &mut stream,
                    ErrorCode::InvalidRequest,
                    "unknown request type"
                ).await;
                let _ = stream.flush().await;
                return;
            }
        }

        // Handle LIST_AND_GET - combined catalog + all images in single response
        if request_type == REQUEST_LIST_AND_GET {
            let sorted = catalog.sorted_ids();
            let count = sorted.len().min(u16::MAX as usize) as u16;

            // Write combined header: JTPG + count
            if let Err(e) = stream.write_all(RESPONSE_LIST_AND_GET).await {
                vlog!(verbose, "Failed to write LIST_AND_GET header: {}", e);
                return;
            }
            if let Err(e) = stream.write_u16(count).await {
                vlog!(verbose, "Failed to write count: {}", e);
                return;
            }

            // Send all images directly (no separate catalog - IDs come with each image)
            for id in sorted.iter().take(count as usize) {
                if let Some(metadata) = catalog.images.get(id) {
                    if
                        let Err(e) = send_image_with_options(
                            &mut stream,
                            metadata,
                            compression_threshold,
                            verbose
                        ).await
                    {
                        vlog!(verbose, "Failed to send image: {}", e);
                        return;
                    }
                }
            }

            if let Err(e) = stream.flush().await {
                vlog!(verbose, "Failed to flush: {}", e);
                return;
            }
            vlog!(verbose, "Sent {} images via LIST_AND_GET", count);

            if !keep_alive {
                return;
            }
            continue;
        }

        if request_type == REQUEST_LIST {
            if let Err(e) = send_catalog_buffered(&mut stream, &catalog).await {
                vlog!(verbose, "Failed to send catalog: {}", e);
                return;
            }
            if let Err(e) = stream.flush().await {
                vlog!(verbose, "Failed to flush: {}", e);
                return;
            }
            vlog!(verbose, "Sent catalog ({} images)", catalog.images.len());

            if !keep_alive {
                return;
            }
            continue;
        }

        if request_type == REQUEST_BATCH {
            let have_count = match read_varint_u32(stream.get_mut()).await {
                Ok(v) => v as usize,
                Err(e) => {
                    vlog!(verbose, "Failed to read BATCH have_count: {}", e);
                    return;
                }
            };

            vlog!(verbose, "BATCH have_count={}", have_count);

            if have_count > 1_000_000 {
                vlog!(verbose, "BATCH have_count too large: {}", have_count);
                let _ = send_error(
                    &mut stream,
                    ErrorCode::InvalidRequest,
                    "have_count too large"
                ).await;
                let _ = stream.flush().await;
                return;
            }

            let have_ids = match read_image_ids(stream.get_mut(), have_count).await {
                Ok(ids) => ids,
                Err(e) => {
                    vlog!(verbose, "Failed to read BATCH have ids: {}", e);
                    return;
                }
            };
            let have: HashSet<ImageId> = have_ids.into_iter().collect();

            let missing: Vec<_> = catalog
                .sorted_ids()
                .iter()
                .filter_map(|id| {
                    if !have.contains(id) { catalog.images.get(id) } else { None }
                })
                .collect();

            let missing_count_u32 = missing.len().min(u32::MAX as usize) as u32;
            vlog!(verbose, "BATCH missing_count={}", missing_count_u32);

            let mut batch_header = [0u8; 9];
            batch_header[0..4].copy_from_slice(RESPONSE_BATCH);
            let varint_len = encode_varint_to_buf(missing_count_u32, &mut batch_header[4..]);
            if let Err(e) = stream.write_all(&batch_header[..4 + varint_len]).await {
                vlog!(verbose, "Failed to write BATCH header: {}", e);
                return;
            }

            for metadata in missing.into_iter().take(missing_count_u32 as usize) {
                if
                    let Err(e) = send_image_with_options(
                        &mut stream,
                        metadata,
                        compression_threshold,
                        verbose
                    ).await
                {
                    vlog!(verbose, "Failed to send image: {}", e);
                    return;
                }
            }

            if let Err(e) = stream.flush().await {
                vlog!(verbose, "Failed to flush: {}", e);
                return;
            }

            if !keep_alive {
                return;
            }
            continue;
        }

        // GET_BY_ID
        let count = stream.get_mut().read_u8().await.unwrap_or(0) as usize;
        vlog!(verbose, "GET_BY_ID count={}", count);

        let ids = match read_image_ids(stream.get_mut(), count).await {
            Ok(ids) => ids,
            Err(e) => {
                vlog!(verbose, "Failed to read {} ids: {}", count, e);
                return;
            }
        };

        for id in ids {
            vlog!(verbose, "Requested id={}", hex::encode(id.to_be_bytes()));
            if let Some(metadata) = catalog.get_metadata(&id) {
                if
                    let Err(e) = send_image_with_options(
                        &mut stream,
                        metadata,
                        compression_threshold,
                        verbose
                    ).await
                {
                    vlog!(verbose, "Failed to send image: {}", e);
                    return;
                }
            }
        }

        if let Err(e) = stream.flush().await {
            vlog!(verbose, "Failed to flush: {}", e);
            return;
        }

        if !keep_alive {
            return;
        }
    }
}

#[tokio::main]
async fn main() -> tokio::io::Result<()> {
    let args = parse_args();

    vlog!(
        args.verbose,
        "Server args: bind={}, images_dir={}, only={:?}, no_tls={}",
        args.bind,
        args.images_dir.display(),
        args.only_name_contains,
        args.no_tls
    );

    let catalog = Arc::new(
        ImageCatalog::from_dir(&args.images_dir, args.only_name_contains.as_deref())
    );
    println!("Loaded {} images", catalog.images.len());
    if args.verbose {
        println!(
            "Compression threshold: {:.1}% (ratio < {:.2})",
            (1.0 - args.compression_threshold) * 100.0,
            args.compression_threshold
        );
        println!(
            "Keep-alive timeout: {:?}, TCP_NODELAY: {}, TLS: {}",
            args.keep_alive_timeout,
            args.tcp_nodelay,
            !args.no_tls
        );
    }

    let listener = TcpListener::bind(&args.bind).await?;

    if args.no_tls {
        // Plain TCP mode for trusted networks / benchmarking
        println!("JTP server (PLAIN TCP - no encryption) listening on {}", args.bind);
        println!("WARNING: No TLS encryption - use only on trusted networks!");

        let verbose = args.verbose;
        let compression_threshold = args.compression_threshold;
        let keep_alive_timeout = args.keep_alive_timeout;
        let tcp_nodelay = args.tcp_nodelay;

        loop {
            let (socket, addr) = listener.accept().await?;
            let catalog = Arc::clone(&catalog);

            vlog!(verbose, "Accepted TCP connection from {}", addr);

            if tcp_nodelay {
                if let Err(e) = socket.set_nodelay(true) {
                    vlog!(verbose, "Failed to set TCP_NODELAY: {}", e);
                }
            }

            tokio::spawn(async move {
                let stream = BufWriter::with_capacity(64 * 1024, socket);
                handle_requests(
                    stream,
                    catalog,
                    compression_threshold,
                    keep_alive_timeout,
                    verbose
                ).await;
            });
        }
    } else {
        // TLS mode (default)
        let (certs, key) = load_or_generate_tls_material(&args.cert_path, &args.key_path).await?;

        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        config.alpn_protocols = vec![b"jtp/1".to_vec()];
        let acceptor = TlsAcceptor::from(Arc::new(config));

        println!("JTP secure server listening on {}", args.bind);

        let verbose = args.verbose;
        let compression_threshold = args.compression_threshold;
        let keep_alive_timeout = args.keep_alive_timeout;
        let tcp_nodelay = args.tcp_nodelay;

        loop {
            let (socket, addr) = listener.accept().await?;
            let acceptor = acceptor.clone();
            let catalog = Arc::clone(&catalog);

            vlog!(verbose, "Accepted TCP connection from {}", addr);

            if tcp_nodelay {
                if let Err(e) = socket.set_nodelay(true) {
                    vlog!(verbose, "Failed to set TCP_NODELAY: {}", e);
                }
            }

            tokio::spawn(async move {
                let tls_stream = match acceptor.accept(socket).await {
                    Ok(s) => s,
                    Err(e) => {
                        vlog!(verbose, "TLS accept failed: {}", e);
                        return;
                    }
                };

                vlog!(verbose, "TLS handshake complete");

                let stream = BufWriter::with_capacity(64 * 1024, tls_stream);
                handle_requests(
                    stream,
                    catalog,
                    compression_threshold,
                    keep_alive_timeout,
                    verbose
                ).await;
            });
        }
    }
}
