use jtp::protocol::{
    encode_varint_to_buf, read_image_ids, read_varint_u32,
    send_cancel_ack, send_catalog_buffered, send_error, send_image_with_options,
    send_watch_event, validate_request_flags, ErrorCode, ImageCatalog, ImageId,
    WatchEvent, REQUEST_BATCH, REQUEST_CANCEL, REQUEST_FLAG_KEEP_ALIVE,
    REQUEST_GET_BY_ID, REQUEST_LIST, REQUEST_LIST_AND_GET, REQUEST_WATCH,
    RESPONSE_BATCH, RESPONSE_CANCEL, RESPONSE_GET_BY_ID, RESPONSE_LIST_AND_GET,
    RESPONSE_WATCH, write_varint_u32,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use std::collections::{HashMap, HashSet};
use std::io::BufReader;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, Mutex, RwLock};
use tokio_rustls::TlsAcceptor;
use unicode_normalization::UnicodeNormalization;

// ── Rate limiter ──────────────────────────────────────────────────────────────

struct RateLimiter {
    requests:     Mutex<HashMap<IpAddr, Vec<Instant>>>,
    max_requests: usize,
    window:       Duration,
}

impl RateLimiter {
    fn new(max_requests: usize, window: Duration) -> Self {
        Self { requests: Mutex::new(HashMap::new()), max_requests, window }
    }

    async fn check(&self, ip: IpAddr) -> bool {
        let mut requests   = self.requests.lock().await;
        let now            = Instant::now();
        let window_start   = now - self.window;
        let timestamps     = requests.entry(ip).or_insert_with(Vec::new);
        timestamps.retain(|&t| t > window_start);
        if timestamps.len() >= self.max_requests {
            false
        } else {
            timestamps.push(now);
            true
        }
    }

    async fn cleanup(&self) {
        let mut requests = self.requests.lock().await;
        let now          = Instant::now();
        let window_start = now - self.window;
        requests.retain(|_, ts| {
            ts.retain(|&t| t > window_start);
            !ts.is_empty()
        });
    }
}

// ── Logging macro ─────────────────────────────────────────────────────────────

macro_rules! vlog {
    ($enabled:expr, $($arg:tt)*) => {
        if $enabled { eprintln!($($arg)*); }
    };
}

// ── TLS helpers ───────────────────────────────────────────────────────────────

async fn load_or_generate_tls_material(
    cert_path: &Path,
    key_path:  &Path,
) -> tokio::io::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    if !cert_path.exists() || !key_path.exists() {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .map_err(|e| tokio::io::Error::new(tokio::io::ErrorKind::InvalidData, e))?;

        tokio::fs::write(cert_path, certified.cert.pem()).await?;
        tokio::fs::write(key_path, certified.key_pair.serialize_pem()).await?;

        println!("Generated self-signed TLS material: cert.pem + key.pem");
        println!("Client must trust cert.pem (same folder as client run)");
    }

    let cert_bytes = tokio::fs::read(cert_path).await?;
    let key_bytes  = tokio::fs::read(key_path).await?;

    let certs: Vec<CertificateDer<'static>> = {
        let mut reader = BufReader::new(std::io::Cursor::new(cert_bytes));
        rustls_pemfile::certs(&mut reader).collect::<Result<Vec<_>, _>>()?
    };
    let key: PrivateKeyDer<'static> = {
        let mut reader = BufReader::new(std::io::Cursor::new(key_bytes));
        rustls_pemfile::private_key(&mut reader)?.ok_or_else(|| {
            tokio::io::Error::new(tokio::io::ErrorKind::InvalidData, "no private key in key.pem")
        })?
    };

    Ok((certs, key))
}

// ── Server configuration ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ServerArgs {
    bind:               String,
    images_dir:         PathBuf,
    cert_path:          PathBuf,
    key_path:           PathBuf,
    only_name_contains: Option<String>,
    compression_threshold: f32,
    verbose:            bool,
    keep_alive_timeout: Duration,
    tcp_nodelay:        bool,
    no_tls:             bool,
    rate_limit:         Option<usize>,
    rate_limit_window:  Duration,
    watch_interval:     Option<u64>, // seconds between rescans; None = WATCH disabled
}

fn parse_args() -> ServerArgs {
    let mut bind               = String::from("0.0.0.0:8443");
    let mut images_dir         = PathBuf::from("images");
    let mut cert_path          = PathBuf::from("cert.pem");
    let mut key_path           = PathBuf::from("key.pem");
    let mut only_name_contains: Option<String> = None;
    let mut compression_threshold = jtp::protocol::DEFAULT_MIN_COMPRESSION_RATIO;
    let mut verbose            = false;
    let mut keep_alive_timeout = Duration::from_secs(30);
    let mut tcp_nodelay        = true;
    let mut no_tls             = false;
    let mut rate_limit:        Option<usize>   = None;
    let mut rate_limit_window  = Duration::from_secs(60);
    let mut watch_interval:    Option<u64>     = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => { if let Some(v) = args.next() { bind = v; } }
            "--images" | "--images-dir" => {
                if let Some(v) = args.next() { images_dir = PathBuf::from(v); }
            }
            "--cert" => { if let Some(v) = args.next() { cert_path = PathBuf::from(v); } }
            "--key"  => { if let Some(v) = args.next() { key_path  = PathBuf::from(v); } }
            "--only" | "--name-contains" => {
                if let Some(v) = args.next() { only_name_contains = Some(v); }
            }
            "--compression-threshold" | "--compress-threshold" => {
                if let Some(v) = args.next() {
                    if let Ok(t) = v.parse::<f32>() { compression_threshold = t.clamp(0.0, 1.0); }
                }
            }
            "--keep-alive-timeout" => {
                if let Some(v) = args.next() {
                    if let Ok(s) = v.parse::<u64>() { keep_alive_timeout = Duration::from_secs(s); }
                }
            }
            "--no-tcp-nodelay"   => { tcp_nodelay = false; }
            "--no-tls" | "--plain" => { no_tls = true; }
            "--rate-limit"       => {
                if let Some(v) = args.next() {
                    if let Ok(n) = v.parse::<usize>() { rate_limit = Some(n); }
                }
            }
            "--rate-limit-window" => {
                if let Some(v) = args.next() {
                    if let Ok(s) = v.parse::<u64>() { rate_limit_window = Duration::from_secs(s); }
                }
            }
            "--watch" => {
                // Enable WATCH with default 5-second rescan interval.
                if watch_interval.is_none() { watch_interval = Some(5); }
            }
            "--watch-interval" => {
                if let Some(v) = args.next() {
                    if let Ok(s) = v.parse::<u64>() { watch_interval = Some(s.max(1)); }
                }
            }
            "-v" | "--verbose" => { verbose = true; }
            "-h" | "--help" => {
                eprintln!(
                    "Usage: server [OPTIONS]\n\n\
Options:\n  \
  --bind ADDR               Bind address (default: 0.0.0.0:8443)\n  \
  --images DIR              Images directory (default: images)\n  \
  --cert PATH               TLS certificate (default: cert.pem)\n  \
  --key PATH                TLS private key  (default: key.pem)\n  \
  --only SUBSTRING          Only serve files whose name contains SUBSTRING\n  \
  --compression-threshold   Min ratio to use compression (default: 0.95)\n  \
  --keep-alive-timeout SEC  Keep-alive idle timeout in seconds (default: 30)\n  \
  --rate-limit N            Max requests per IP per window (default: disabled)\n  \
  --rate-limit-window SEC   Rate limit window in seconds (default: 60)\n  \
  --no-tcp-nodelay          Disable TCP_NODELAY\n  \
  --no-tls, --plain         Plain TCP (no encryption)\n  \
  --watch                   Enable WATCH (rescan every 5 s)\n  \
  --watch-interval SEC      Rescan interval for WATCH (default: 5)\n  \
  --verbose                 Detailed logs"
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }

    ServerArgs {
        bind, images_dir, cert_path, key_path, only_name_contains,
        compression_threshold, verbose, keep_alive_timeout, tcp_nodelay,
        no_tls, rate_limit, rate_limit_window, watch_interval,
    }
}

// ── WATCH background rescan task ──────────────────────────────────────────────
//
// Periodically re-scans the images directory. For each new ImageID found
// it updates the shared catalog and broadcasts a WatchEvent to all subscribers.

async fn catalog_watch_task(
    images_dir:    PathBuf,
    name_contains: Option<String>,
    interval_secs: u64,
    catalog:       Arc<RwLock<ImageCatalog>>,
    tx:            broadcast::Sender<WatchEvent>,
    verbose:       bool,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
    ticker.tick().await; // skip first immediate tick

    loop {
        ticker.tick().await;

        let new_catalog = ImageCatalog::from_dir(&images_dir, name_contains.as_deref());

        // Collect new entries not yet in the shared catalog.
        let new_metas: Vec<jtp::protocol::ImageMetadata> = {
            let existing = catalog.read().await;
            new_catalog
                .images
                .iter()
                .filter(|(id, _)| !existing.images.contains_key(id))
                .map(|(_, meta)| meta.clone())
                .collect()
        };

        if new_metas.is_empty() {
            continue;
        }

        vlog!(verbose, "WATCH: {} new image(s) detected", new_metas.len());

        let mut cat = catalog.write().await;
        for meta in new_metas {
            let name: String = meta
                .file_name
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .nfc()
                .collect();
            let flags = meta.flags;
            let size  = meta
                .cached_data
                .as_ref()
                .map(|d| d.len() as u32)
                .unwrap_or_else(|| {
                    std::fs::metadata(&meta.file_name)
                        .map(|m| m.len().min(u32::MAX as u64) as u32)
                        .unwrap_or(0)
                });
            let id = meta.id;
            cat.add_image(meta);
            let event = WatchEvent { id, flags, filename: name, size };
            // send() only errors if there are no receivers; that's fine.
            let _ = tx.send(event);
        }
    }
}

// ── Request handler ───────────────────────────────────────────────────────────

async fn handle_requests<S>(
    mut stream:            BufWriter<S>,
    catalog:               Arc<RwLock<ImageCatalog>>,
    compression_threshold: f32,
    keep_alive_timeout:    Duration,
    verbose:               bool,
    watch_tx:              Option<Arc<broadcast::Sender<WatchEvent>>>,
) where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let mut request_count      = 0u64;
    let mut connection_keep_alive = false;

    'request_loop: loop {
        let read_timeout = if request_count == 0 {
            Duration::from_secs(60)
        } else {
            keep_alive_timeout
        };

        let mut header = [0u8; 2];
        let header_result =
            tokio::time::timeout(read_timeout, stream.get_mut().read_exact(&mut header)).await;

        let (request_type, request_flags) = match header_result {
            Ok(Ok(_))  => (header[0], header[1]),
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
        if keep_alive { connection_keep_alive = true; }

        vlog!(
            verbose,
            "Request #{}: type={} keep-alive={}",
            request_count, request_type, keep_alive
        );

        // ── CANCEL ────────────────────────────────────────────────────────────
        if request_type == REQUEST_CANCEL {
            // RequestFlags MUST be 0; keep-alive is only valid on a connection
            // that was already established with keep-alive.
            if !connection_keep_alive {
                vlog!(verbose, "CANCEL received on non-keep-alive connection");
                let _ = send_error(
                    &mut stream, ErrorCode::InvalidRequest,
                    "CANCEL requires an active keep-alive connection",
                ).await;
                let _ = stream.flush().await;
                return;
            }
            // Send JTPC acknowledgement and stay open.
            if let Err(e) = send_cancel_ack(&mut stream).await {
                vlog!(verbose, "Failed to send CANCEL ack: {}", e);
                return;
            }
            if let Err(e) = stream.flush().await {
                vlog!(verbose, "Failed to flush CANCEL ack: {}", e);
                return;
            }
            vlog!(verbose, "CANCEL ack sent");
            continue 'request_loop;
        }

        // ── LIST_AND_GET ──────────────────────────────────────────────────────
        if request_type == REQUEST_LIST_AND_GET {
            let (count, sorted): (u32, Vec<ImageId>) = {
                let cat = catalog.read().await;
                let s   = cat.sorted_ids().to_vec();
                let c   = s.len().min(u32::MAX as usize) as u32;
                (c, s)
            };

            // Fix §9.5: count is now varint(u32), not u16.
            if let Err(e) = stream.write_all(RESPONSE_LIST_AND_GET).await {
                vlog!(verbose, "Failed to write LIST_AND_GET header: {}", e);
                return;
            }
            if let Err(e) = write_varint_u32(&mut stream, count).await {
                vlog!(verbose, "Failed to write LIST_AND_GET count: {}", e);
                return;
            }

            for id in sorted.iter().take(count as usize) {
                let meta = {
                    let cat = catalog.read().await;
                    cat.images.get(id).cloned()
                };
                if let Some(metadata) = meta {
                    if let Err(e) = send_image_with_options(
                        &mut stream, &metadata, compression_threshold, verbose,
                    ).await {
                        vlog!(verbose, "Failed to send image: {}", e);
                        return;
                    }
                }
            }

            if let Err(e) = stream.flush().await {
                vlog!(verbose, "Failed to flush LIST_AND_GET: {}", e);
                return;
            }
            vlog!(verbose, "Sent {} images via LIST_AND_GET", count);
            if !keep_alive { return; }
            continue 'request_loop;
        }

        // ── LIST ──────────────────────────────────────────────────────────────
        if request_type == REQUEST_LIST {
            let cat = catalog.read().await;
            if let Err(e) = send_catalog_buffered(&mut stream, &cat).await {
                vlog!(verbose, "Failed to send catalog: {}", e);
                return;
            }
            drop(cat);
            if let Err(e) = stream.flush().await {
                vlog!(verbose, "Failed to flush LIST: {}", e);
                return;
            }
            let img_count = catalog.read().await.images.len();
            vlog!(verbose, "Sent catalog ({} images)", img_count);
            if !keep_alive { return; }
            continue 'request_loop;
        }

        // ── BATCH ─────────────────────────────────────────────────────────────
        if request_type == REQUEST_BATCH {
            let have_count = match read_varint_u32(stream.get_mut()).await {
                Ok(v)  => v as usize,
                Err(e) => {
                    vlog!(verbose, "Failed to read BATCH have_count: {}", e);
                    return;
                }
            };

            vlog!(verbose, "BATCH have_count={}", have_count);

            if have_count > 1_000_000 {
                let _ = send_error(
                    &mut stream, ErrorCode::InvalidRequest, "have_count too large",
                ).await;
                let _ = stream.flush().await;
                return;
            }

            let have_ids = match read_image_ids(stream.get_mut(), have_count).await {
                Ok(ids) => ids,
                Err(e)  => { vlog!(verbose, "Failed to read BATCH IDs: {}", e); return; }
            };
            let have: HashSet<ImageId> = have_ids.into_iter().collect();

            let missing: Vec<jtp::protocol::ImageMetadata> = {
                let cat = catalog.read().await;
                cat.sorted_ids()
                    .iter()
                    .filter_map(|id| {
                        if !have.contains(id) { cat.images.get(id).cloned() } else { None }
                    })
                    .collect()
            };

            let missing_count = missing.len().min(u32::MAX as usize) as u32;
            vlog!(verbose, "BATCH missing_count={}", missing_count);

            let mut batch_header = [0u8; 9];
            batch_header[0..4].copy_from_slice(RESPONSE_BATCH);
            let vl = encode_varint_to_buf(missing_count, &mut batch_header[4..]);
            if let Err(e) = stream.write_all(&batch_header[..4 + vl]).await {
                vlog!(verbose, "Failed to write BATCH header: {}", e);
                return;
            }

            for metadata in missing.iter().take(missing_count as usize) {
                if let Err(e) = send_image_with_options(
                    &mut stream, metadata, compression_threshold, verbose,
                ).await {
                    vlog!(verbose, "Failed to send image: {}", e);
                    return;
                }
            }

            if let Err(e) = stream.flush().await {
                vlog!(verbose, "Failed to flush BATCH: {}", e);
                return;
            }
            vlog!(verbose, "BATCH complete");
            if !keep_alive { return; }
            continue 'request_loop;
        }

        // ── WATCH ─────────────────────────────────────────────────────────────
        if request_type == REQUEST_WATCH {
            let tx = match watch_tx.as_ref() {
                Some(t) => t,
                None => {
                    vlog!(verbose, "WATCH requested but not enabled on this server");
                    let _ = send_error(
                        &mut stream, ErrorCode::UnsupportedFeature,
                        "WATCH not enabled; start server with --watch",
                    ).await;
                    let _ = stream.flush().await;
                    return;
                }
            };

            let mut rx = tx.subscribe();
            vlog!(verbose, "WATCH subscription active");

            // Loop: send JTPW events as they arrive; break on CANCEL from client.
            //
            // Note: tokio::select! between rx.recv() and stream.get_mut().read_u8()
            // means the read future may be dropped mid-flight if a watch event
            // arrives first. This is safe here because we only expect REQUEST_CANCEL
            // (one byte) on the wire and the connection is single-threaded; any
            // partial read will be retried on the next select iteration.
            'watch_loop: loop {
                tokio::select! {
                    biased;

                    // Check for incoming CANCEL from the client.
                    read_result = stream.get_mut().read_u8() => {
                        match read_result {
                            Ok(b) if b == REQUEST_CANCEL => {
                                vlog!(verbose, "WATCH cancelled by client");
                                if let Err(e) = send_cancel_ack(&mut stream).await {
                                    vlog!(verbose, "Failed to send WATCH cancel ack: {}", e);
                                    return;
                                }
                                if let Err(e) = stream.flush().await {
                                    vlog!(verbose, "Failed to flush WATCH cancel ack: {}", e);
                                    return;
                                }
                                break 'watch_loop;
                            }
                            Ok(other) => {
                                vlog!(verbose, "Unexpected byte 0x{:02x} during WATCH", other);
                                return;
                            }
                            Err(e) => {
                                vlog!(verbose, "Client disconnected during WATCH: {}", e);
                                return;
                            }
                        }
                    }

                    // Send a WATCH event to the client.
                    recv_result = rx.recv() => {
                        match recv_result {
                            Ok(event) => {
                                if let Err(e) = send_watch_event(&mut stream, &event).await {
                                    vlog!(verbose, "Failed to send WATCH event: {}", e);
                                    return;
                                }
                                if let Err(e) = stream.flush().await {
                                    vlog!(verbose, "Failed to flush WATCH event: {}", e);
                                    return;
                                }
                                vlog!(
                                    verbose,
                                    "WATCH: sent event id={}",
                                    hex::encode(event.id.to_be_bytes())
                                );
                            }
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                vlog!(verbose, "WATCH subscriber lagged, dropped {} events", n);
                            }
                            Err(broadcast::error::RecvError::Closed) => {
                                vlog!(verbose, "WATCH broadcast channel closed");
                                return;
                            }
                        }
                    }
                }
            }

            // After CANCEL, keep-alive is implicit for WATCH connections.
            continue 'request_loop;
        }

        // ── GET_BY_ID ─────────────────────────────────────────────────────────
        if request_type == REQUEST_GET_BY_ID {
            let count = match stream.get_mut().read_u8().await {
                Ok(n)  => n as usize,
                Err(e) => { vlog!(verbose, "Failed to read GET_BY_ID count: {}", e); return; }
            };

            vlog!(verbose, "GET_BY_ID count={}", count);

            let ids = match read_image_ids(stream.get_mut(), count).await {
                Ok(ids) => ids,
                Err(e)  => { vlog!(verbose, "Failed to read IDs: {}", e); return; }
            };

            // Collect the images that actually exist in the catalog.
            let images_to_send: Vec<jtp::protocol::ImageMetadata> = {
                let cat = catalog.read().await;
                ids.iter().filter_map(|id| cat.images.get(id).cloned()).collect()
            };

            let m = images_to_send.len().min(255) as u8;

            // §9.2: write JTPD header + returned count (M) before images.
            let mut hdr = [0u8; 5];
            hdr[..4].copy_from_slice(RESPONSE_GET_BY_ID);
            hdr[4] = m;
            if let Err(e) = stream.write_all(&hdr).await {
                vlog!(verbose, "Failed to write GET_BY_ID header: {}", e);
                return;
            }

            let mut cancelled = false;
            for metadata in images_to_send.iter().take(m as usize) {
                vlog!(verbose, "Sending id={}", hex::encode(metadata.id.to_be_bytes()));
                if let Err(e) = send_image_with_options(
                    &mut stream, metadata, compression_threshold, verbose,
                ).await {
                    vlog!(verbose, "Failed to send image: {}", e);
                    return;
                }
                if let Err(e) = stream.flush().await {
                    vlog!(verbose, "Failed to flush between images: {}", e);
                    return;
                }

                // Non-blocking peek for CANCEL between packets.
                // Duration::ZERO yields the current task once, allowing the
                // runtime to deliver any bytes already in the socket buffer.
                let cancel_peek = tokio::time::timeout(
                    Duration::ZERO,
                    stream.get_mut().read_u8(),
                ).await;
                if let Ok(Ok(b)) = cancel_peek {
                    if b == REQUEST_CANCEL {
                        vlog!(verbose, "GET_BY_ID cancelled mid-stream");
                        cancelled = true;
                        break;
                    }
                }
            }

            if cancelled {
                if let Err(e) = send_cancel_ack(&mut stream).await {
                    vlog!(verbose, "Failed to send CANCEL ack: {}", e);
                    return;
                }
                if let Err(e) = stream.flush().await {
                    vlog!(verbose, "Failed to flush CANCEL ack: {}", e);
                    return;
                }
                if !keep_alive { return; }
                continue 'request_loop;
            }

            if let Err(e) = stream.flush().await {
                vlog!(verbose, "Failed to flush GET_BY_ID: {}", e);
                return;
            }
            if !keep_alive { return; }
            continue 'request_loop;
        }

        // ── Unknown request type ──────────────────────────────────────────────
        // §12: unknown ReqType → UnsupportedFeature
        vlog!(verbose, "Unknown request type: 0x{:02x}", request_type);
        let _ = send_error(
            &mut stream,
            ErrorCode::UnsupportedFeature,
            "unknown request type",
        ).await;
        let _ = stream.flush().await;
        return;
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> tokio::io::Result<()> {
    let args = parse_args();

    vlog!(
        args.verbose,
        "Server: bind={} images={} no_tls={} rate_limit={:?} watch_interval={:?}",
        args.bind, args.images_dir.display(), args.no_tls,
        args.rate_limit, args.watch_interval
    );

    // Wrap catalog in RwLock so the WATCH background task can update it.
    let catalog: Arc<RwLock<ImageCatalog>> = Arc::new(RwLock::new(
        ImageCatalog::from_dir(&args.images_dir, args.only_name_contains.as_deref())
    ));
    println!("Loaded {} images", catalog.read().await.images.len());

    // Rate limiter
    let rate_limiter: Option<Arc<RateLimiter>> = args
        .rate_limit
        .map(|limit| Arc::new(RateLimiter::new(limit, args.rate_limit_window)));

    if let Some(ref limiter) = rate_limiter {
        let l = Arc::clone(limiter);
        let interval = args.rate_limit_window;
        tokio::spawn(async move {
            loop { tokio::time::sleep(interval).await; l.cleanup().await; }
        });
    }

    // WATCH broadcast channel + background rescan task
    let watch_tx: Option<Arc<broadcast::Sender<WatchEvent>>> =
        if let Some(interval_secs) = args.watch_interval {
            let (tx, _) = broadcast::channel::<WatchEvent>(256);
            let tx      = Arc::new(tx);
            let tx_bg   = Arc::clone(&tx);
            let cat_bg  = Arc::clone(&catalog);
            let dir_bg  = args.images_dir.clone();
            let name_bg = args.only_name_contains.clone();
            let verbose = args.verbose;

            tokio::spawn(async move {
                catalog_watch_task(dir_bg, name_bg, interval_secs, cat_bg, (*tx_bg).clone(), verbose).await;
            });

            println!("WATCH enabled (rescan every {}s)", interval_secs);
            Some(tx)
        } else {
            None
        };

    if args.verbose {
        println!(
            "Compression threshold: {:.1}%, keep-alive timeout: {:?}, TLS: {}",
            (1.0 - args.compression_threshold) * 100.0,
            args.keep_alive_timeout,
            !args.no_tls,
        );
        if let Some(limit) = args.rate_limit {
            println!("Rate limit: {} req per {:?} per IP", limit, args.rate_limit_window);
        }
    }

    let listener = TcpListener::bind(&args.bind).await?;

    // Shared config captured once
    let verbose            = args.verbose;
    let compression        = args.compression_threshold;
    let keep_alive_timeout = args.keep_alive_timeout;
    let tcp_nodelay        = args.tcp_nodelay;

    if args.no_tls {
        println!("JTP server (PLAIN TCP) listening on {}", args.bind);
        println!("WARNING: No TLS — use only on trusted networks!");

        loop {
            let (socket, addr)   = listener.accept().await?;
            let catalog          = Arc::clone(&catalog);
            let rate_limiter     = rate_limiter.clone();
            let watch_tx         = watch_tx.clone();

            vlog!(verbose, "Accepted TCP connection from {}", addr);
            if tcp_nodelay {
                if let Err(e) = socket.set_nodelay(true) {
                    vlog!(verbose, "Failed to set TCP_NODELAY: {}", e);
                }
            }

            tokio::spawn(async move {
                if let Some(ref limiter) = rate_limiter {
                    if !limiter.check(addr.ip()).await {
                        vlog!(verbose, "Rate limited: {}", addr.ip());
                        let mut s = BufWriter::with_capacity(64 * 1024, socket);
                        let _ = send_error(&mut s, ErrorCode::RateLimited, "too many requests").await;
                        let _ = s.flush().await;
                        return;
                    }
                }
                let stream = BufWriter::with_capacity(64 * 1024, socket);
                handle_requests(stream, catalog, compression, keep_alive_timeout, verbose, watch_tx).await;
            });
        }
    } else {
        let (certs, key) = load_or_generate_tls_material(&args.cert_path, &args.key_path).await?;
        let mut config   = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        config.alpn_protocols = vec![b"jtp/1".to_vec()];
        let acceptor = TlsAcceptor::from(Arc::new(config));

        println!("JTP secure server listening on {}", args.bind);

        loop {
            let (socket, addr)   = listener.accept().await?;
            let acceptor         = acceptor.clone();
            let catalog          = Arc::clone(&catalog);
            let rate_limiter     = rate_limiter.clone();
            let watch_tx         = watch_tx.clone();

            vlog!(verbose, "Accepted TCP connection from {}", addr);
            if tcp_nodelay {
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
                            let _ = send_error(&mut s, ErrorCode::RateLimited, "too many requests").await;
                            let _ = s.flush().await;
                        }
                        return;
                    }
                }

                let tls = match acceptor.accept(socket).await {
                    Ok(s)  => s,
                    Err(e) => { vlog!(verbose, "TLS accept failed: {}", e); return; }
                };
                vlog!(verbose, "TLS handshake complete");

                let stream = BufWriter::with_capacity(64 * 1024, tls);
                handle_requests(stream, catalog, compression, keep_alive_timeout, verbose, watch_tx).await;
            });
        }
    }
}