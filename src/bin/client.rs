use tokio::net::TcpStream;
use tokio::io::{ AsyncReadExt, AsyncWriteExt };
use jtp::protocol::{
    compute_image_id,
    file_type_from_flags,
    read_varint_u32,
    write_varint_u32,
    FLAG_COMPRESSED,
    FLAG_ENCRYPTED,
    ImageId,
    REQUEST_BATCH,
    REQUEST_GET_BY_ID,
    REQUEST_LIST,
    RESPONSE_BATCH,
    RESPONSE_LIST,
};
use rustls::pki_types::{ CertificateDer, ServerName };
use rustls::RootCertStore;
use tokio_rustls::TlsConnector;
use std::sync::Arc;
use std::io::BufReader;
use std::path::Path;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;

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
    cert_path: PathBuf,
    receive_dir: PathBuf,
    batch: bool,
    verbose: bool,
}

fn parse_args() -> ClientArgs {
    let mut addr = String::from("127.0.0.1:8443");
    let mut server_name = String::from("localhost");
    let mut cert_path = PathBuf::from("cert.pem");
    let mut receive_dir = PathBuf::from("output");
    let mut batch = false;
    let mut verbose = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--addr" => {
                if let Some(v) = args.next() {
                    addr = v;
                }
            }
            "--server-name" => {
                if let Some(v) = args.next() {
                    server_name = v;
                }
            }
            "--cert" => {
                if let Some(v) = args.next() {
                    cert_path = PathBuf::from(v);
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
            "-v" | "--verbose" => {
                verbose = true;
            }
            "-h" | "--help" => {
                eprintln!(
                    "Usage: client [--addr HOST:PORT] [--server-name NAME] [--cert PATH] [--out DIR] [--batch] [--verbose]\n\n  --addr         Server address (default: 127.0.0.1:8443)\n  --server-name  TLS SNI name (default: localhost)\n  --cert         Path to server certificate to trust (default: cert.pem)\n  --out          Output directory (default: output)\n  --batch        Delta sync: send IDs you already have; download missing only\n  --verbose      Print request/response and file write logs"
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }

    ClientArgs { addr, server_name, cert_path, receive_dir, batch, verbose }
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

async fn tls_connect(
    addr: &str,
    server_name: &str,
    cert_path: &Path
) -> Result<tokio_rustls::client::TlsStream<TcpStream>, Box<dyn std::error::Error>> {
    vlog!(true, "Connecting TCP to {}...", addr);
    let tcp = TcpStream::connect(addr).await?;

    vlog!(true, "Loading trusted certs from {}...", cert_path.display());
    let cert_bytes = tokio::fs::read(cert_path).await?;
    let certs: Vec<CertificateDer<'static>> = {
        let mut reader = BufReader::new(std::io::Cursor::new(cert_bytes));
        rustls_pemfile::certs(&mut reader).collect::<Result<Vec<_>, _>>()?
    };
    let mut root_store = RootCertStore::empty();
    for cert in certs {
        root_store.add(cert)?;
    }

    let client_config = rustls::ClientConfig
        ::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_config));

    let server_name = ServerName::try_from(server_name.to_owned())?;
    Ok(connector.connect(server_name, tcp).await?)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args();
    let verbose = args.verbose;

    vlog!(
        verbose,
        "Client args: addr={}, server_name={}, cert={}, out={}",
        args.addr,
        args.server_name,
        args.cert_path.display(),
        args.receive_dir.display()
    );

    let receive_dir = args.receive_dir;
    std::fs::create_dir_all(&receive_dir)?;

    if args.batch {
        let have_ids = collect_have_ids(&receive_dir, verbose)?;
        vlog!(verbose, "Delta sync: sending {} have IDs", have_ids.len());

        let mut stream = tls_connect(&args.addr, &args.server_name, &args.cert_path).await?;
        vlog!(verbose, "TLS connected; sending BATCH request");
        stream.write_u8(REQUEST_BATCH).await?;
        write_varint_u32(&mut stream, have_ids.len().min(u32::MAX as usize) as u32).await?;
        for id in &have_ids {
            stream.write_u64(*id).await?;
        }

        let mut header = [0u8; 4];
        stream.read_exact(&mut header).await?;
        if &header != RESPONSE_BATCH {
            return Err(format!("unexpected BATCH response header: {:?}", header).into());
        }

        let missing_count = read_varint_u32(&mut stream).await? as usize;
        println!("Server missing_count={}", missing_count);

        for _ in 0..missing_count {
            let flags = stream.read_u8().await?;
            let length = read_varint_u32(&mut stream).await?;
            let id = stream.read_u64().await?;

            if (flags & (FLAG_COMPRESSED | FLAG_ENCRYPTED)) != 0 {
                return Err(
                    format!(
                        "unsupported image flags=0x{:02x} (compression/encryption not implemented)",
                        flags
                    ).into()
                );
            }

            let mut buf = vec![0u8; length as usize];
            stream.read_exact(&mut buf).await?;

            let computed_id = compute_image_id(&buf);
            if computed_id != id {
                eprintln!(
                    "warning: server ID does not match file contents (server={}, computed={})",
                    hex::encode(id.to_be_bytes()),
                    hex::encode(computed_id.to_be_bytes())
                );
            }

            let effective_id = computed_id;
            let file_type = file_type_from_flags(flags);
            let ext = match file_type {
                0 => "png",
                1 => "jpg",
                2 => "webp",
                3 => "bmp",
                4 => "gif",
                _ => "bin",
            };

            let output_name = format!("output_{}.{}", hex::encode(effective_id.to_be_bytes()), ext);
            let output_path = receive_dir.join(output_name);
            vlog!(verbose, "Writing {} bytes to {}", buf.len(), output_path.display());
            std::fs::write(output_path, buf)?;
        }

        return Ok(());
    }

    // 1) Discover available images.
    vlog!(verbose, "Opening LIST connection...");
    let mut list_stream = tls_connect(&args.addr, &args.server_name, &args.cert_path).await?;
    vlog!(verbose, "TLS connected; sending LIST request");
    list_stream.write_u8(REQUEST_LIST).await?;

    let mut list_header = [0u8; 4];
    list_stream.read_exact(&mut list_header).await?;
    if &list_header != RESPONSE_LIST {
        return Err(format!("unexpected LIST response header: {:?}", list_header).into());
    }

    vlog!(verbose, "LIST response header OK (JTPL)");

    let count = list_stream.read_u16().await? as usize;
    vlog!(verbose, "LIST count={}", count);
    let mut listed: Vec<ListedImage> = Vec::with_capacity(count);

    for _ in 0..count {
        let id = list_stream.read_u64().await?;
        let flags = list_stream.read_u8().await?;

        let name_len = list_stream.read_u16().await? as usize;
        let mut name_bytes = vec![0u8; name_len];
        list_stream.read_exact(&mut name_bytes).await?;
        let filename = String::from_utf8_lossy(&name_bytes).trim().to_string();

        let size = read_varint_u32(&mut list_stream).await?;

        listed.push(ListedImage { id, flags, filename, size });
    }

    vlog!(verbose, "Parsed {} catalog entries", listed.len());

    if listed.is_empty() {
        println!("No images available on server.");
        return Ok(());
    }

    println!("Server catalog:");
    for item in &listed {
        println!(
            "- {}  {}  {} bytes",
            hex::encode(item.id.to_be_bytes()),
            item.filename,
            item.size
        );
    }

    // Demo: download everything listed.
    let ids: Vec<ImageId> = listed
        .iter()
        .map(|i| i.id)
        .collect();
    let mut by_id: HashMap<ImageId, ListedImage> = HashMap::new();
    for item in listed {
        by_id.insert(item.id, item);
    }

    // 2) Request selected images by ID.
    vlog!(verbose, "Opening GET_BY_ID connection...");
    let mut stream = tls_connect(&args.addr, &args.server_name, &args.cert_path).await?;
    vlog!(verbose, "TLS connected; sending GET_BY_ID request ({} ids)", ids.len());
    stream.write_u8(REQUEST_GET_BY_ID).await?;
    if ids.len() > (u8::MAX as usize) {
        return Err(format!("too many images to request in one batch: {}", ids.len()).into());
    }
    stream.write_u8(ids.len() as u8).await?;
    for id in &ids {
        stream.write_u64(*id).await?;
    }

    // receive images
    for _ in &ids {
        let flags = stream.read_u8().await?;
        let length = read_varint_u32(&mut stream).await?;
        let id = stream.read_u64().await?;

        if (flags & (FLAG_COMPRESSED | FLAG_ENCRYPTED)) != 0 {
            return Err(
                format!(
                    "unsupported image flags=0x{:02x} (compression/encryption not implemented)",
                    flags
                ).into()
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

        let mut buf = vec![0u8; length as usize];
        stream.read_exact(&mut buf).await?;

        let computed_id = compute_image_id(&buf);

        if computed_id != id {
            eprintln!(
                "warning: server ID does not match file contents (server={}, computed={})",
                hex::encode(id.to_be_bytes()),
                hex::encode(computed_id.to_be_bytes())
            );
        }

        let effective_metadata = by_id.get(&computed_id).or_else(|| by_id.get(&id));
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

        // Prefer catalog filename (from LIST).
        let preferred_name = effective_metadata.map(|m| m.filename.as_str());

        // Sanitize to a basename.
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

        // Fallback: output_<image_id>.<ext>
        let id_for_name = computed_id;
        let output_name = output_name.unwrap_or_else(|| {
            format!("output_{}.{}", hex::encode(id_for_name.to_be_bytes()), ext)
        });

        let output_path = receive_dir.join(output_name);
        vlog!(verbose, "Writing {} bytes to {}", buf.len(), output_path.display());
        std::fs::write(output_path, buf)?;
    }

    Ok(())
}
