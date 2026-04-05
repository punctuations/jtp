use jtp::protocol::{
    encode_varint_to_buf,
    read_image_ids,
    read_varint_u32,
    send_cancel_ack,
    send_catalog_buffered,
    send_error,
    send_image_with_options,
    send_watch_event,
    validate_request_flags,
    ErrorCode,
    ImageCatalog,
    ImageId,
    WatchEvent,
    REQUEST_BATCH,
    REQUEST_CANCEL,
    REQUEST_FLAG_KEEP_ALIVE,
    REQUEST_GET_BY_ID,
    REQUEST_LIST,
    REQUEST_LIST_AND_GET,
    REQUEST_WATCH,
    RESPONSE_BATCH,
    RESPONSE_CANCEL,
    RESPONSE_GET_BY_ID,
    RESPONSE_LIST_AND_GET,
    RESPONSE_WATCH,
    write_varint_u32,
};
use jtp::server::{ handle_requests, load_or_generate_tls_material, RateLimiter };
use rustls::pki_types::{ CertificateDer, PrivateKeyDer };
use rustls::ServerConfig;
use std::collections::HashMap;
use std::io::BufReader;
use std::net::{ IpAddr, SocketAddr };
use std::path::{ Path, PathBuf };
use std::sync::Arc;
use std::time::{ Duration, Instant };
use tokio::io::{ AsyncReadExt, AsyncWriteExt, BufWriter };
use tokio::net::TcpListener;
use tokio::sync::{ broadcast, Mutex, RwLock };
use tokio_rustls::TlsAcceptor;
use unicode_normalization::UnicodeNormalization;

// ── Logging macro ─────────────────────────────────────────────────────────────

macro_rules! vlog {
    (
        $enabled:expr,
        $($arg:tt)*
    ) => {
        if $enabled { eprintln!($($arg)*); }
    };
}

// ── Server configuration ──────────────────────────────────────────────────────

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
    /// When true, use TCP + TLS instead of QUIC.
    no_quic: bool,
    rate_limit: Option<usize>,
    rate_limit_window: Duration,
    watch_interval: Option<u64>,
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
    let mut no_quic = false; // QUIC is the default
    let mut rate_limit: Option<usize> = None;
    let mut rate_limit_window = Duration::from_secs(60);
    let mut watch_interval: Option<u64> = None;

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
                    if let Ok(t) = v.parse::<f32>() {
                        compression_threshold = t.clamp(0.0, 1.0);
                    }
                }
            }
            "--keep-alive-timeout" => {
                if let Some(v) = args.next() {
                    if let Ok(s) = v.parse::<u64>() {
                        keep_alive_timeout = Duration::from_secs(s);
                    }
                }
            }
            "--no-tcp-nodelay" => {
                tcp_nodelay = false;
            }
            "--no-quic" => {
                no_quic = true;
            }
            "--rate-limit" => {
                if let Some(v) = args.next() {
                    if let Ok(n) = v.parse::<usize>() {
                        rate_limit = Some(n);
                    }
                }
            }
            "--rate-limit-window" => {
                if let Some(v) = args.next() {
                    if let Ok(s) = v.parse::<u64>() {
                        rate_limit_window = Duration::from_secs(s);
                    }
                }
            }
            "--watch" => {
                if watch_interval.is_none() {
                    watch_interval = Some(5);
                }
            }
            "--watch-interval" => {
                if let Some(v) = args.next() {
                    if let Ok(s) = v.parse::<u64>() {
                        watch_interval = Some(s.max(1));
                    }
                }
            }
            "-v" | "--verbose" => {
                verbose = true;
            }
            "-h" | "--help" => {
                eprintln!(
                    "Usage: server [OPTIONS]\n\n\
Options:\n  \
  --bind ADDR               Bind address (default: 0.0.0.0:8443)\n  \
  --images DIR              Images directory (default: images)\n  \
  --cert PATH               TLS certificate (default: cert.pem)\n  \
  --key PATH                TLS private key  (default: key.pem)\n  \
  --only SUBSTRING          Only serve files whose name contains SUBSTRING\n  \
  --compression-threshold   Min Zstd ratio (default: 0.95)\n  \
  --keep-alive-timeout SEC  TCP keep-alive idle timeout (default: 30)\n  \
  --rate-limit N            Max requests per IP per window (disabled by default)\n  \
  --rate-limit-window SEC   Rate limit window (default: 60)\n  \
  --no-tcp-nodelay          Disable TCP_NODELAY (TCP/TLS mode only)\n  \
  --no-quic                 Use TCP + TLS instead of QUIC\n  \
  --watch                   Enable WATCH (rescan every 5 s)\n  \
  --watch-interval SEC      Rescan interval for WATCH (default: 5)\n  \
  --verbose                 Detailed logs\n\n\
Transport:\n  \
  Default: QUIC with TLS 1.3, ALPN jtp/1. Each request runs on its own\n  \
           bidirectional stream for true parallelism.\n  \
  --no-quic: fall back to TCP + TLS, identical to jtp-01 behaviour."
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
        no_quic,
        rate_limit,
        rate_limit_window,
        watch_interval,
    }
}

// ── WATCH background rescan task ──────────────────────────────────────────────

async fn catalog_watch_task(
    images_dir: PathBuf,
    name_contains: Option<String>,
    interval_secs: u64,
    catalog: Arc<RwLock<ImageCatalog>>,
    tx: broadcast::Sender<WatchEvent>,
    verbose: bool
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
    ticker.tick().await; // skip first immediate tick

    loop {
        ticker.tick().await;

        let new_catalog = ImageCatalog::from_dir(&images_dir, name_contains.as_deref());

        let new_metas: Vec<jtp::protocol::ImageMetadata> = {
            let existing = catalog.read().await;
            new_catalog.images
                .iter()
                .filter(|(id, _)| !existing.images.contains_key(id))
                .map(|(_, meta)| meta.clone())
                .collect()
        };

        if new_metas.is_empty() {
            continue;
        }

        vlog!(verbose, "WATCH: {} new file(s) detected", new_metas.len());

        let mut cat = catalog.write().await;
        for meta in new_metas {
            let name: String = meta.file_name
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .nfc()
                .collect();
            let flags = meta.flags;
            let size = meta.cached_data
                .as_ref()
                .map(|d| d.len() as u32)
                .unwrap_or_else(|| {
                    std::fs
                        ::metadata(&meta.file_name)
                        .map(|m| m.len().min(u32::MAX as u64) as u32)
                        .unwrap_or(0)
                });
            let id = meta.id;
            cat.add_image(meta, verbose);
            let _ = tx.send(WatchEvent { id, flags, filename: name, size });
        }
    }
}

// ── Request handler ───────────────────────────────────────────────────────────

#[cfg(test)]
pub async fn test_handle_requests<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    stream: tokio::io::BufWriter<S>,
    catalog: Arc<tokio::sync::RwLock<jtp::protocol::ImageCatalog>>,
    compression_threshold: f32,
    keep_alive_timeout: std::time::Duration,
    verbose: bool,
    watch_tx: Option<Arc<tokio::sync::broadcast::Sender<jtp::protocol::WatchEvent>>>
) {
    handle_requests(
        stream,
        catalog,
        compression_threshold,
        keep_alive_timeout,
        verbose,
        watch_tx
    ).await
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> tokio::io::Result<()> {
    let args = parse_args();

    vlog!(
        args.verbose,
        "Server: bind={} images={} no_quic={} rate_limit={:?} watch_interval={:?}",
        args.bind,
        args.images_dir.display(),
        args.no_quic,
        args.rate_limit,
        args.watch_interval
    );

    let catalog: Arc<RwLock<ImageCatalog>> = Arc::new(
        RwLock::new(ImageCatalog::from_dir(&args.images_dir, args.only_name_contains.as_deref()))
    );
    println!("Loaded {} files", catalog.read().await.images.len());

    let rate_limiter: Option<Arc<RateLimiter>> = args.rate_limit.map(|limit|
        Arc::new(RateLimiter::new(limit, args.rate_limit_window))
    );

    if let Some(ref limiter) = rate_limiter {
        let l = Arc::clone(limiter);
        let interval = args.rate_limit_window;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                l.cleanup().await;
            }
        });
    }

    let watch_tx: Option<Arc<broadcast::Sender<WatchEvent>>> = if
        let Some(interval_secs) = args.watch_interval
    {
        let (tx, _) = broadcast::channel::<WatchEvent>(256);
        let tx = Arc::new(tx);
        let tx_bg = Arc::clone(&tx);
        let cat_bg = Arc::clone(&catalog);
        let dir_bg = args.images_dir.clone();
        let name_bg = args.only_name_contains.clone();
        let verbose = args.verbose;

        tokio::spawn(async move {
            catalog_watch_task(
                dir_bg,
                name_bg,
                interval_secs,
                cat_bg,
                (*tx_bg).clone(),
                verbose
            ).await;
        });

        println!("WATCH enabled (rescan every {}s)", interval_secs);
        Some(tx)
    } else {
        None
    };

    if args.verbose {
        println!(
            "Compression threshold: {:.1}%, keep-alive timeout: {:?}, transport: {}",
            (1.0 - args.compression_threshold) * 100.0,
            args.keep_alive_timeout,
            if args.no_quic {
                "TCP+TLS"
            } else {
                "QUIC"
            }
        );
        if let Some(limit) = args.rate_limit {
            println!("Rate limit: {} req per {:?} per IP", limit, args.rate_limit_window);
        }
    }

    let bind_addr: SocketAddr = args.bind
        .parse()
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid bind address: {}", e)
            )
        })?;

    let verbose = args.verbose;
    let compression = args.compression_threshold;
    let keep_alive_timeout = args.keep_alive_timeout;

    if args.no_quic {
        // ── TCP + TLS (--no-quic fallback) ────────────────────────────────────
        let (certs, key) = load_or_generate_tls_material(&args.cert_path, &args.key_path).await?;
        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        config.alpn_protocols = vec![b"jtp/1".to_vec()];
        let acceptor = TlsAcceptor::from(Arc::new(config));

        let listener = TcpListener::bind(bind_addr).await?;
        println!("JTP server (TCP+TLS) listening on {}", bind_addr);

        loop {
            let (socket, addr) = listener.accept().await?;
            let acceptor = acceptor.clone();
            let catalog = Arc::clone(&catalog);
            let rate_limiter = rate_limiter.clone();
            let watch_tx = watch_tx.clone();

            vlog!(verbose, "TCP connection from {}", addr);
            if args.tcp_nodelay {
                if let Err(e) = socket.set_nodelay(true) {
                    vlog!(verbose, "Failed to set TCP_NODELAY: {}", e);
                }
            }

            tokio::spawn(async move {
                if let Some(ref limiter) = rate_limiter {
                    if !limiter.check(addr.ip()).await {
                        vlog!(verbose, "Rate limited: {}", addr.ip());
                        if let Ok(tls) = acceptor.accept(socket).await {
                            let mut s = BufWriter::with_capacity(64 * 1024, tls);
                            let _ = send_error(
                                &mut s,
                                ErrorCode::RateLimited,
                                "too many requests"
                            ).await;
                            let _ = s.flush().await;
                        }
                        return;
                    }
                }

                let tls = match acceptor.accept(socket).await {
                    Ok(s) => s,
                    Err(e) => {
                        vlog!(verbose, "TLS accept failed: {}", e);
                        return;
                    }
                };
                vlog!(verbose, "TLS handshake complete");

                let stream = BufWriter::with_capacity(64 * 1024, tls);
                handle_requests(
                    stream,
                    catalog,
                    compression,
                    keep_alive_timeout,
                    verbose,
                    watch_tx
                ).await;
            });
        }
    } else {
        // ── QUIC (default) ────────────────────────────────────────────────────
        //
        // Build a quinn Endpoint with the same certificate used for TCP+TLS.
        // Each incoming QUIC connection can open multiple bidirectional streams;
        // we spawn one handle_requests task per stream, enabling true parallelism
        // within a single connection without head-of-line blocking.

        let (certs, key) = load_or_generate_tls_material(&args.cert_path, &args.key_path).await?;

        // Convert rustls key to quinn-compatible form.
        let key_der = match key {
            PrivateKeyDer::Pkcs8(k) => k,
            other => {
                return Err(
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("unsupported key format for QUIC: {:?}", other.secret_der())
                    )
                );
            }
        };

        let mut tls_config = rustls::ServerConfig
            ::builder()
            .with_no_client_auth()
            .with_single_cert(certs, PrivateKeyDer::Pkcs8(key_der))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        tls_config.alpn_protocols = vec![b"jtp/1".to_vec()];

        let quic_server_config = quinn::ServerConfig::with_crypto(
            Arc::new(
                quinn::crypto::rustls::QuicServerConfig
                    ::try_from(tls_config)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
            )
        );

        let endpoint = quinn::Endpoint::server(quic_server_config, bind_addr)?;
        println!("JTP server (QUIC) listening on {}", bind_addr);

        while let Some(incoming) = endpoint.accept().await {
            let catalog = Arc::clone(&catalog);
            let rate_limiter = rate_limiter.clone();
            let watch_tx = watch_tx.clone();

            tokio::spawn(async move {
                let conn = match incoming.await {
                    Ok(c) => c,
                    Err(e) => {
                        vlog!(verbose, "QUIC connection failed: {}", e);
                        return;
                    }
                };

                let remote = conn.remote_address();
                vlog!(verbose, "QUIC connection from {}", remote);

                if let Some(ref limiter) = rate_limiter {
                    if !limiter.check(remote.ip()).await {
                        vlog!(verbose, "Rate limited: {}", remote.ip());
                        conn.close(quinn::VarInt::from_u32(5), b"rate limited");
                        return;
                    }
                }

                // Each bidirectional stream is an independent request/response
                // exchange.  Spawn a task per stream so streams run in parallel.
                loop {
                    let stream = match conn.accept_bi().await {
                        Ok(s) => s,
                        Err(quinn::ConnectionError::ApplicationClosed { .. }) => {
                            break;
                        }
                        Err(quinn::ConnectionError::ConnectionClosed { .. }) => {
                            break;
                        }
                        Err(e) => {
                            vlog!(verbose, "QUIC stream error: {}", e);
                            break;
                        }
                    };

                    let catalog = Arc::clone(&catalog);
                    let watch_tx = watch_tx.clone();

                    tokio::spawn(async move {
                        // quinn's SendStream + RecvStream are fused into a
                        // single AsyncRead + AsyncWrite view via a small adapter.
                        let (send, recv) = stream;
                        let combined = QuicStream { send, recv };
                        let buffered = BufWriter::with_capacity(64 * 1024, combined);

                        // QUIC streams are inherently single-request: the client
                        // closes its write side after sending, so keep_alive_timeout
                        // is effectively unused.  We still pass it for API
                        // compatibility with the TCP path.
                        handle_requests(
                            buffered,
                            catalog,
                            compression,
                            Duration::from_secs(30),
                            verbose,
                            watch_tx
                        ).await;
                    });
                }
            });
        }
    }

    Ok(())
}

// ── QuicStream adapter ────────────────────────────────────────────────────────
//
// Fuses a quinn (SendStream, RecvStream) pair into a single type that
// implements AsyncRead + AsyncWrite, matching the signature expected by
// handle_requests<S: AsyncReadExt + AsyncWriteExt + Unpin>.

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
