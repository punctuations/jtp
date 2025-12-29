use std::collections::HashMap;
use std::path::PathBuf;
use std::fs;
use xxhash_rust::xxh64::xxh64;
use tokio::io::{ AsyncReadExt, AsyncWriteExt };

pub const BUFFER_SIZE: usize = 65536;

pub const REQUEST_GET_BY_ID: u8 = 0;
pub const REQUEST_LIST: u8 = 1;
pub const REQUEST_BATCH: u8 = 2;

pub const RESPONSE_LIST: &[u8; 4] = b"JTPL";
pub const RESPONSE_BATCH: &[u8; 4] = b"JTPB";

pub type ImageId = u64;
pub const IMAGE_ID_LEN_BYTES: usize = 8;
pub const IMAGE_ID_SEED: u64 = 0;

pub fn compute_image_id(bytes: &[u8]) -> ImageId {
    xxh64(bytes, IMAGE_ID_SEED)
}

pub const FLAGS_FILE_TYPE_MASK: u8 = 0b0000_0111;
pub const FLAG_COMPRESSED: u8 = 1 << 3;
pub const FLAG_ENCRYPTED: u8 = 1 << 4;

pub fn file_type_from_flags(flags: u8) -> u8 {
    flags & FLAGS_FILE_TYPE_MASK
}

pub fn flags_from_file_type(file_type: u8) -> u8 {
    file_type & FLAGS_FILE_TYPE_MASK
}

pub async fn write_varint_u32(
    stream: &mut (impl AsyncWriteExt + Unpin),
    mut value: u32
) -> tokio::io::Result<()> {
    // Unsigned LEB128 encoding; always 1..=5 bytes for u32.
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            stream.write_u8(byte).await?;
            return Ok(());
        }
        stream.write_u8(byte | 0x80).await?;
    }
}

pub async fn read_varint_u32(stream: &mut (impl AsyncReadExt + Unpin)) -> tokio::io::Result<u32> {
    // Unsigned LEB128 decoding; reject >5 bytes.
    let mut result: u32 = 0;
    let mut shift: u32 = 0;

    for i in 0..5 {
        let byte = stream.read_u8().await?;
        let low = (byte & 0x7f) as u32;
        result |= low << shift;

        if (byte & 0x80) == 0 {
            return Ok(result);
        }

        shift += 7;
        if i == 4 {
            break;
        }
    }

    Err(tokio::io::Error::new(tokio::io::ErrorKind::InvalidData, "varint u32 too long"))
}

#[derive(Clone)]
pub struct ImageMetadata {
    pub id: ImageId,
    pub flags: u8,
    pub file_name: PathBuf,
}

// Server-side: map IDs -> files
#[derive(Clone)]
pub struct ImageCatalog {
    pub images: HashMap<ImageId, ImageMetadata>,
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
            let id = compute_image_id(&bytes);

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
                _ => 7,
            };

            let flags = flags_from_file_type(file_type);

            catalog.insert(id, ImageMetadata { id, flags, file_name: path.clone() });
        }

        Self { images: catalog }
    }

    pub fn get_metadata(&self, id: &ImageId) -> Option<&ImageMetadata> {
        self.images.get(id)
    }

    pub fn list_images(&self) -> Vec<ImageId> {
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

        stream.write_u64(metadata.id).await?;
        stream.write_u8(metadata.flags).await?;
        stream.write_u16(name_len).await?;
        stream.write_all(&name_bytes[..name_len as usize]).await?;
        write_varint_u32(stream, size_u32).await?;
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

    // Image packet header:
    // - Flags (u8)
    // - Length (u32 varint, 1..=5 bytes)
    // - ImageID (u64, big-endian)
    let filesize_u32 = filesize.min(u32::MAX as u64) as u32;
    stream.write_u8(metadata.flags).await?;
    write_varint_u32(stream, filesize_u32).await?;
    stream.write_u64(metadata.id).await?;

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
