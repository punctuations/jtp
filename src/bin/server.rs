use tokio::net::TcpListener;
use tokio::io::AsyncReadExt;
use jtp::protocol::{ ImageCatalog, send_catalog, send_image, REQUEST_GET_BY_ID, REQUEST_LIST };
use tokio_rustls::TlsAcceptor;
use rustls::ServerConfig;
use rustls::pki_types::{ CertificateDer, PrivateKeyDer };
use std::sync::Arc;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;

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
}

fn parse_args() -> ServerArgs {
    let mut bind = String::from("0.0.0.0:8443");
    let mut images_dir = PathBuf::from("images");
    let mut only_name_contains: Option<String> = None;

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
            "-h" | "--help" => {
                eprintln!(
                    "Usage: server [--bind ADDR] [--images DIR] [--only SUBSTRING]\n\n  --bind      Bind address (default: 0.0.0.0:8443)\n  --images    Images directory to scan (default: images)\n  --only      Only serve files whose basename contains SUBSTRING (case-insensitive)"
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }

    ServerArgs { bind, images_dir, only_name_contains }
}

#[tokio::main]
async fn main() -> tokio::io::Result<()> {
    let args = parse_args();
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

    loop {
        let (socket, _addr) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let catalog = Arc::clone(&catalog);

        tokio::spawn(async move {
            let mut stream = acceptor.accept(socket).await.unwrap();

            let first = match stream.read_u8().await {
                Ok(b) => b,
                Err(_) => {
                    return;
                }
            };

            // Request format:
            // - Modern: [RequestType:u8] ...
            // - Legacy: [Count:u8] [ImageID:16*Count]
            let (request_type, legacy_count) = match first {
                REQUEST_GET_BY_ID => (REQUEST_GET_BY_ID, None),
                REQUEST_LIST => (REQUEST_LIST, None),
                count => (REQUEST_GET_BY_ID, Some(count as usize)),
            };

            if request_type == REQUEST_LIST {
                let _ = send_catalog(&mut stream, &catalog).await;
                return;
            }

            // GET_BY_ID
            let count = if let Some(c) = legacy_count {
                c
            } else {
                stream.read_u8().await.unwrap_or(0) as usize
            };

            let mut ids_buf = vec![0u8; count*16];
            stream.read_exact(&mut ids_buf).await.unwrap();

            for i in 0..count {
                let mut id = [0u8; 16];
                id.copy_from_slice(&ids_buf[i * 16..(i + 1) * 16]);
                if let Some(metadata) = catalog.get_metadata(&id) {
                    let _ = send_image(&mut stream, metadata).await;
                }
            }
        });
    }
}
