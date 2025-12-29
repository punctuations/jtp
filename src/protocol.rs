use std::collections::HashMap;
use std::path::PathBuf;
use std::fs;
use sha2::{ Sha256, Digest };
use tokio::io::{ AsyncReadExt, AsyncWriteExt };

pub const BUFFER_SIZE: usize = 65536;

pub const REQUEST_GET_BY_ID: u8 = 0;
pub const REQUEST_LIST: u8 = 1;

pub const RESPONSE_IMAGE: &[u8; 4] = b"JTP1";
pub const RESPONSE_LIST: &[u8; 4] = b"JTPL";

#[derive(Clone)]
pub struct ImageMetadata {
    pub id: [u8; 16],
    pub file_type: u8,
    pub file_name: PathBuf,
}

// Server-side: map IDs -> files
#[derive(Clone)]
pub struct ImageCatalog {
    pub images: HashMap<[u8; 16], ImageMetadata>,
}

impl ImageCatalog {
    pub fn new() -> Self {
        Self::from_dir("images", None)
    }

    pub fn from_dir(images_dir: impl Into<PathBuf>, name_contains: Option<&str>) -> Self {
        let images_dir = images_dir.into();
        let mut catalog = HashMap::new();
        let name_contains = name_contains.map(|s| s.to_ascii_lowercase());

        let Ok(paths) = fs::read_dir(&images_dir) else {
            return Self { images: catalog };
        };

        for entry in paths.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            if let Some(filter) = &name_contains {
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if !name.contains(filter) {
                    continue;
                }
            }

            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            let hash = Sha256::digest(&bytes);
            let mut id = [0u8; 16];
            id.copy_from_slice(&hash[..16]);

            let ext = path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            let file_type = match ext.to_lowercase().as_str() {
                "png" => 0,
                "jpg" | "jpeg" => 1,
                "webp" => 2,
                "bmp" => 3,
                "gif" => 4,
                _ => 255,
            };

            catalog.insert(id, ImageMetadata { id, file_type, file_name: path.clone() });
        }

        Self { images: catalog }
    }

    pub fn get_metadata(&self, id: &[u8; 16]) -> Option<&ImageMetadata> {
        self.images.get(id)
    }

    pub fn list_images(&self) -> Vec<[u8; 16]> {
        self.images.keys().copied().collect()
    }

    pub fn list_metadata_sorted(&self) -> Vec<&ImageMetadata> {
        let mut values: Vec<&ImageMetadata> = self.images.values().collect();
        values.sort_by(|a, b| a.file_name.cmp(&b.file_name));
        values
    }
}

impl Default for ImageCatalog {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn send_catalog(
    stream: &mut (impl AsyncWriteExt + Unpin),
    catalog: &ImageCatalog
) -> tokio::io::Result<()> {
    let entries = catalog.list_metadata_sorted();
    let count = entries.len().min(u16::MAX as usize) as u16;

    stream.write_all(RESPONSE_LIST).await?;
    stream.write_u16(count).await?;

    for metadata in entries.into_iter().take(count as usize) {
        let size = tokio::fs::metadata(&metadata.file_name).await?.len();
        let size_u32 = size.min(u32::MAX as u64) as u32;

        let name_str = metadata.file_name
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let name_bytes = name_str.as_bytes();
        let name_len = name_bytes.len().min(u16::MAX as usize) as u16;

        stream.write_all(&metadata.id).await?;
        stream.write_u8(metadata.file_type).await?;
        stream.write_u16(name_len).await?;
        stream.write_all(&name_bytes[..name_len as usize]).await?;
        stream.write_u32(size_u32).await?;
    }

    Ok(())
}

// Send image over a TCP/TLS stream
pub async fn send_image(
    stream: &mut (impl AsyncWriteExt + Unpin),
    metadata: &ImageMetadata
) -> tokio::io::Result<()> {
    let mut file = tokio::fs::File::open(&metadata.file_name).await?;
    let filesize = file.metadata().await?.len();

    let name_str = metadata.file_name
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let name_bytes = name_str.as_bytes();
    let name_len = name_bytes.len().min(u16::MAX as usize) as u16;

    // Header: JTP1 + file_type + ID + name_len(u16) + filename + length(u32)
    stream.write_all(RESPONSE_IMAGE).await?;
    stream.write_u8(metadata.file_type).await?;
    stream.write_all(&metadata.id).await?;
    stream.write_u16(name_len).await?;
    stream.write_all(&name_bytes[..name_len as usize]).await?;
    stream.write_u32(filesize as u32).await?;

    let mut buffer = [0u8; BUFFER_SIZE];
    loop {
        let n = file.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        stream.write_all(&buffer[..n]).await?;
    }

    Ok(())
}
