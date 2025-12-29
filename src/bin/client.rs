use tokio::net::TcpStream;
use tokio::io::{ AsyncReadExt, AsyncWriteExt };
use jtp::protocol::{ REQUEST_GET_BY_ID, REQUEST_LIST, RESPONSE_LIST };
use rustls::pki_types::{ CertificateDer, ServerName };
use rustls::RootCertStore;
use tokio_rustls::TlsConnector;
use std::sync::Arc;
use std::io::BufReader;
use sha2::{ Digest, Sha256 };
use std::path::Path;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Clone, Debug)]
struct ListedImage {
    id: [u8; 16],
    file_type: u8,
    filename: String,
    size: u32,
}

async fn tls_connect() -> Result<
    tokio_rustls::client::TlsStream<TcpStream>,
    Box<dyn std::error::Error>
> {
    let tcp = TcpStream::connect("127.0.0.1:9999").await?;

    let cert_bytes = tokio::fs::read("cert.pem").await?;
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

    let server_name = ServerName::try_from("localhost")?;
    Ok(connector.connect(server_name, tcp).await?)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let receive_dir = PathBuf::from("output");
    std::fs::create_dir_all(&receive_dir)?;

    // 1) Discover available images.
    let mut list_stream = tls_connect().await?;
    list_stream.write_u8(REQUEST_LIST).await?;

    let mut list_header = [0u8; 4];
    list_stream.read_exact(&mut list_header).await?;
    if &list_header != RESPONSE_LIST {
        return Err(format!("unexpected LIST response header: {:?}", list_header).into());
    }

    let count = list_stream.read_u16().await? as usize;
    let mut listed: Vec<ListedImage> = Vec::with_capacity(count);

    for _ in 0..count {
        let mut id = [0u8; 16];
        list_stream.read_exact(&mut id).await?;
        let file_type = list_stream.read_u8().await?;

        let name_len = list_stream.read_u16().await? as usize;
        let mut name_bytes = vec![0u8; name_len];
        list_stream.read_exact(&mut name_bytes).await?;
        let filename = String::from_utf8_lossy(&name_bytes).trim().to_string();

        let size = list_stream.read_u32().await?;

        listed.push(ListedImage { id, file_type, filename, size });
    }

    if listed.is_empty() {
        println!("No images available on server.");
        return Ok(());
    }

    println!("Server catalog:");
    for item in &listed {
        println!("- {}  {}  {} bytes", hex::encode(&item.id[..8]), item.filename, item.size);
    }

    // Demo: download everything listed.
    let ids: Vec<[u8; 16]> = listed
        .iter()
        .map(|i| i.id)
        .collect();
    let mut by_id: HashMap<[u8; 16], ListedImage> = HashMap::new();
    for item in listed {
        by_id.insert(item.id, item);
    }

    // 2) Request selected images by ID.
    let mut stream = tls_connect().await?;
    stream.write_u8(REQUEST_GET_BY_ID).await?;
    if ids.len() > (u8::MAX as usize) {
        return Err(format!("too many images to request in one batch: {}", ids.len()).into());
    }
    stream.write_u8(ids.len() as u8).await?;
    for id in &ids {
        stream.write_all(id).await?;
    }

    // receive images
    for _ in &ids {
        let mut header = [0u8; 4];
        stream.read_exact(&mut header).await?;
        if &header != b"JTP1" {
            return Err(format!("unexpected protocol header: {:?}", header).into());
        }
        let file_type = stream.read_u8().await?;
        let mut id_bytes = [0u8; 16];
        stream.read_exact(&mut id_bytes).await?;

        let name_len = stream.read_u16().await? as usize;
        let mut name_bytes = vec![0u8; name_len];
        stream.read_exact(&mut name_bytes).await?;
        let server_filename = std::str
            ::from_utf8(&name_bytes)
            .ok()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());

        let length = stream.read_u32().await?;

        let mut buf = vec![0u8; length as usize];
        stream.read_exact(&mut buf).await?;

        // IDs are defined as the first 16 bytes of SHA-256(file_contents).
        // After we have the file contents, recompute the ID and use it to select metadata.
        let computed = Sha256::digest(&buf);
        let mut computed_id = [0u8; 16];
        computed_id.copy_from_slice(&computed[..16]);

        if computed_id != id_bytes {
            eprintln!(
                "warning: server ID does not match file contents (server={}, computed={})",
                hex::encode(&id_bytes[..8]),
                hex::encode(&computed_id[..8])
            );
        }

        let effective_metadata = by_id.get(&computed_id).or_else(|| by_id.get(&id_bytes));
        let effective_file_type = effective_metadata.map(|m| m.file_type).unwrap_or(file_type);

        let ext = match effective_file_type {
            0 => "png",
            1 => "jpg",
            2 => "webp",
            3 => "bmp",
            4 => "gif",
            _ => "bin",
        };

        // Prefer catalog filename (from LIST), then the filename included in the response.
        let preferred_name = effective_metadata.map(|m| m.filename.as_str()).or(server_filename);

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

        // Fallback: output_<first_8_bytes_of_id>.<ext>
        let id_for_name = computed_id;
        let output_name = output_name.unwrap_or_else(|| {
            format!("output_{}.{}", hex::encode(&id_for_name[..8]), ext)
        });

        let output_path = receive_dir.join(output_name);
        std::fs::write(output_path, buf)?;
    }

    Ok(())
}
