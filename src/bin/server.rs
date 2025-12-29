use tokio::net::TcpListener;
use tokio::io::{ AsyncReadExt, AsyncWriteExt };
use jtp::protocol::{
    ImageCatalog,
    send_catalog,
    send_image,
    read_varint_u32,
    write_varint_u32,
    ImageId,
    REQUEST_BATCH,
    REQUEST_GET_BY_ID,
    REQUEST_LIST,
    RESPONSE_BATCH,
};
use tokio_rustls::TlsAcceptor;
use rustls::ServerConfig;
use rustls::pki_types::{ CertificateDer, PrivateKeyDer };
use std::sync::Arc;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;
use std::collections::HashSet;

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

async fn load_or_generate_tls_material() -> tokio::io::Result<
    (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)
> {
    let cert_path = Path::new("cert.pem");
    let key_path = Path::new("key.pem");

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
    only_name_contains: Option<String>,
    verbose: bool,
}

fn parse_args() -> ServerArgs {
    let mut bind = String::from("0.0.0.0:8443");
    let mut images_dir = PathBuf::from("images");
    let mut only_name_contains: Option<String> = None;
    let mut verbose = false;

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
            "--only" | "--name-contains" => {
                if let Some(v) = args.next() {
                    only_name_contains = Some(v);
                }
            }
            "-v" | "--verbose" => {
                verbose = true;
            }
            "-h" | "--help" => {
                eprintln!(
                    "Usage: server [--bind ADDR] [--images DIR] [--only SUBSTRING] [--verbose]\n\n  --bind      Bind address (default: 0.0.0.0:8443)\n  --images    Images directory to scan (default: images)\n  --only      Only serve files whose basename contains SUBSTRING (case-insensitive)\n  --verbose   Print per-connection and per-request logs"
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }

    ServerArgs { bind, images_dir, only_name_contains, verbose }
}

#[tokio::main]
async fn main() -> tokio::io::Result<()> {
    let args = parse_args();

    vlog!(
        args.verbose,
        "Server args: bind={}, images_dir={}, only={:?}",
        args.bind,
        args.images_dir.display(),
        args.only_name_contains
    );

    let catalog = Arc::new(
        ImageCatalog::from_dir(&args.images_dir, args.only_name_contains.as_deref())
    );
    println!("Loaded {} images", catalog.images.len());

    // TLS is used purely to encrypt the JTP protocol bytes.
    let (certs, key) = load_or_generate_tls_material().await?;

    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    // This server speaks only the JTP protocol; TLS is used purely to encrypt it.
    config.alpn_protocols = vec![b"jtp/1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(config));

    let listener = TcpListener::bind(&args.bind).await?;
    println!("JTP secure server listening on {}", args.bind);

    let verbose = args.verbose;

    loop {
        let (socket, addr) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let catalog = Arc::clone(&catalog);

        vlog!(verbose, "Accepted TCP connection from {}", addr);

        tokio::spawn(async move {
            let mut stream = match acceptor.accept(socket).await {
                Ok(s) => s,
                Err(e) => {
                    vlog!(verbose, "TLS accept failed: {}", e);
                    return;
                }
            };

            vlog!(verbose, "TLS handshake complete");

            let first = match stream.read_u8().await {
                Ok(b) => b,
                Err(_) => {
                    vlog!(verbose, "Client disconnected before sending request");
                    return;
                }
            };

            // Request format:
            // - [ReqType:u8] ...
            let request_type = first;
            match request_type {
                REQUEST_LIST => vlog!(verbose, "Request: LIST"),
                REQUEST_GET_BY_ID => vlog!(verbose, "Request: GET_BY_ID"),
                REQUEST_BATCH => vlog!(verbose, "Request: BATCH"),
                other => {
                    vlog!(verbose, "Unknown request type: {}", other);
                    return;
                }
            }

            if request_type == REQUEST_LIST {
                if let Err(e) = send_catalog(&mut stream, &catalog).await {
                    vlog!(verbose, "Failed to send catalog: {}", e);
                } else {
                    vlog!(verbose, "Sent catalog ({} images)", catalog.images.len());
                }
                return;
            }

            if request_type == REQUEST_BATCH {
                // BATCH request format:
                // - [ReqType:u8=2]
                // - [HaveCount:varint u32]
                // - [ImageID:u64 BE] x HaveCount
                let have_count = match read_varint_u32(&mut stream).await {
                    Ok(v) => v as usize,
                    Err(e) => {
                        vlog!(verbose, "Failed to read BATCH have_count: {}", e);
                        return;
                    }
                };

                vlog!(verbose, "BATCH have_count={}", have_count);

                // Basic sanity cap to avoid pathological allocations.
                if have_count > 1_000_000 {
                    vlog!(verbose, "BATCH have_count too large: {}", have_count);
                    return;
                }

                let mut have: HashSet<ImageId> = HashSet::with_capacity(have_count);
                for _ in 0..have_count {
                    let id = match stream.read_u64().await {
                        Ok(v) => v,
                        Err(e) => {
                            vlog!(verbose, "Failed to read BATCH have id: {}", e);
                            return;
                        }
                    };
                    have.insert(id);
                }

                let missing: Vec<_> = catalog
                    .list_metadata_sorted()
                    .into_iter()
                    .filter(|m| !have.contains(&m.id))
                    .collect();

                let missing_count_u32 = missing.len().min(u32::MAX as usize) as u32;
                vlog!(verbose, "BATCH missing_count={}", missing_count_u32);

                if let Err(e) = stream.write_all(RESPONSE_BATCH).await {
                    vlog!(verbose, "Failed to write BATCH header: {}", e);
                    return;
                }
                if let Err(e) = write_varint_u32(&mut stream, missing_count_u32).await {
                    vlog!(verbose, "Failed to write BATCH missing_count: {}", e);
                    return;
                }

                for metadata in missing.into_iter().take(missing_count_u32 as usize) {
                    if let Err(e) = send_image(&mut stream, metadata).await {
                        vlog!(
                            verbose,
                            "Failed to send image {}: {}",
                            hex::encode(metadata.id.to_be_bytes()),
                            e
                        );
                        return;
                    }
                }

                return;
            }

            // GET_BY_ID
            let count = stream.read_u8().await.unwrap_or(0) as usize;

            vlog!(verbose, "GET_BY_ID count={}", count);

            let mut ids_buf = vec![0u8; count * 8];
            if let Err(e) = stream.read_exact(&mut ids_buf).await {
                vlog!(verbose, "Failed to read {} id bytes: {}", count * 8, e);
                return;
            }

            for i in 0..count {
                let mut id_bytes = [0u8; 8];
                id_bytes.copy_from_slice(&ids_buf[i * 8..(i + 1) * 8]);
                let id: ImageId = u64::from_be_bytes(id_bytes);

                vlog!(verbose, "Requested id={}", hex::encode(id.to_be_bytes()));
                if let Some(metadata) = catalog.get_metadata(&id) {
                    if let Err(e) = send_image(&mut stream, metadata).await {
                        vlog!(
                            verbose,
                            "Failed to send image {}: {}",
                            hex::encode(id.to_be_bytes()),
                            e
                        );
                    } else {
                        vlog!(
                            verbose,
                            "Sent image file={} flags=0x{:02x}",
                            metadata.file_name.display(),
                            metadata.flags
                        );
                    }
                } else {
                    vlog!(verbose, "No matching image for id={}", hex::encode(id.to_be_bytes()));
                }
            }
        });
    }
}
