use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use xxhash_rust::xxh64::xxh64;

pub const BUFFER_SIZE: usize = 65536;

// Adaptive compression levels based on file size
pub const COMPRESSION_LEVEL_SMALL: i32 = 6; // < 100 KB: higher compression
pub const COMPRESSION_LEVEL_MEDIUM: i32 = 3; // 100KB - 1MB: balanced
pub const COMPRESSION_LEVEL_LARGE: i32 = 1; // > 1MB: fast compression

pub const SIZE_THRESHOLD_SMALL: usize = 100 * 1024; // 100 KB
pub const SIZE_THRESHOLD_LARGE: usize = 1024 * 1024; // 1 MB

// Default minimum compression ratio to use compression (5% improvement)
pub const DEFAULT_MIN_COMPRESSION_RATIO: f32 = 0.95;

pub const REQUEST_GET_BY_ID: u8 = 0;
pub const REQUEST_LIST: u8 = 1;
pub const REQUEST_BATCH: u8 = 2;
pub const REQUEST_GET_RANGE: u8 = 3;
pub const REQUEST_HELLO: u8 = 4;
pub const REQUEST_LIST_AND_GET: u8 = 5; // Combined LIST + GET in single round-trip
pub const REQUEST_ERROR: u8 = 0xFE;

pub const RESPONSE_LIST: &[u8; 4] = b"JTPL";
pub const RESPONSE_BATCH: &[u8; 4] = b"JTPB";
pub const RESPONSE_HELLO: &[u8; 4] = b"JTPH";
pub const RESPONSE_ERROR: &[u8; 4] = b"JTPE";
pub const RESPONSE_LIST_AND_GET: &[u8; 4] = b"JTPG"; // Combined LIST + GET response

// Request flags (second byte after ReqType for requests that support it)
pub const REQUEST_FLAG_KEEP_ALIVE: u8 = 1 << 0;
pub const REQUEST_FLAGS_RESERVED_MASK: u8 = 0b1111_1110;

// Error codes for structured error responses
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ErrorCode {
    NotFound = 1,
    InvalidRequest = 2,
    ServerError = 3,
    UnsupportedFeature = 4,
    RateLimited = 5,
}

impl ErrorCode {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(ErrorCode::NotFound),
            2 => Some(ErrorCode::InvalidRequest),
            3 => Some(ErrorCode::ServerError),
            4 => Some(ErrorCode::UnsupportedFeature),
            5 => Some(ErrorCode::RateLimited),
            _ => None,
        }
    }
}

// Capability flags for HELLO negotiation
pub const CAPABILITY_ZSTD: u8 = 1 << 0;
pub const CAPABILITY_ENCRYPTION: u8 = 1 << 1;

pub type ImageId = u64;
pub const IMAGE_ID_LEN_BYTES: usize = 8;
pub const IMAGE_ID_SEED: u64 = 0;

pub fn compute_image_id(bytes: &[u8]) -> ImageId {
    xxh64(bytes, IMAGE_ID_SEED)
}

pub const FLAGS_FILE_TYPE_MASK: u8 = 0b0000_0111;
pub const FLAG_COMPRESSED: u8 = 1 << 3;
pub const FLAG_ENCRYPTED: u8 = 1 << 4;

pub const METADATA_MAX_BYTES: usize = 256; // cap metadata blob

pub fn file_type_from_flags(flags: u8) -> u8 {
    flags & FLAGS_FILE_TYPE_MASK
}

pub fn flags_from_file_type(file_type: u8) -> u8 {
    file_type & FLAGS_FILE_TYPE_MASK
}

pub async fn write_varint_u32(
    stream: &mut (impl AsyncWriteExt + Unpin),
    mut value: u32,
) -> tokio::io::Result<()> {
    // Fast path for common small values (0-127)
    if value < 0x80 {
        stream.write_u8(value as u8).await?;
        return Ok(());
    }
    // Unsigned LEB128 encoding for larger values
    let mut buf = [0u8; 5];
    let mut len = 0;
    while value >= 0x80 {
        buf[len] = ((value & 0x7f) as u8) | 0x80;
        value >>= 7;
        len += 1;
    }
    buf[len] = value as u8;
    stream.write_all(&buf[..=len]).await?;
    Ok(())
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

    Err(tokio::io::Error::new(
        tokio::io::ErrorKind::InvalidData,
        "varint u32 too long",
    ))
}

#[derive(Clone)]
pub struct ImageMetadata {
    pub id: ImageId,
    pub flags: u8,
    pub file_name: PathBuf,
    pub cached_data: Option<Arc<Vec<u8>>>,
}

#[derive(Clone)]
pub struct ImageCatalog {
    pub images: HashMap<ImageId, ImageMetadata>,
    cached_sorted: Arc<Vec<ImageId>>, // Cache sorted IDs for LIST response
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
            return Self {
                images: catalog,
                cached_sorted: Arc::new(Vec::new()),
            };
        };

        for entry in paths.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            // Only process image files
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
            let file_type = match ext.to_lowercase().as_str() {
                "png" => 0,
                "jpg" | "jpeg" => 1,
                "webp" => 2,
                "bmp" => 3,
                "gif" => 4,
                _ => {
                    continue;
                } // Skip non-image files
            };

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

            let flags = flags_from_file_type(file_type);

            // Cache small files to avoid disk reads per request
            let meta = if bytes.len() <= SIZE_THRESHOLD_LARGE {
                ImageMetadata {
                    id,
                    flags,
                    file_name: path.clone(),
                    cached_data: Some(Arc::new(bytes)),
                }
            } else {
                ImageMetadata {
                    id,
                    flags,
                    file_name: path.clone(),
                    cached_data: None,
                }
            };

            catalog.insert(id, meta);
        }

        // Pre-sort by filename once at catalog creation
        let mut sorted_ids: Vec<ImageId> = catalog.values().map(|m| m.id).collect();
        sorted_ids.sort_by(|a, b| {
            let a_name = catalog
                .get(a)
                .and_then(|m| m.file_name.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("");
            let b_name = catalog
                .get(b)
                .and_then(|m| m.file_name.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("");
            a_name.cmp(b_name)
        });

        Self {
            images: catalog,
            cached_sorted: Arc::new(sorted_ids),
        }
    }

    pub fn get_metadata(&self, id: &ImageId) -> Option<&ImageMetadata> {
        self.images.get(id)
    }

    pub fn list_images(&self) -> Vec<ImageId> {
        self.images.keys().copied().collect()
    }

    pub fn sorted_ids(&self) -> &[ImageId] {
        &self.cached_sorted
    }
}

impl Default for ImageCatalog {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn send_catalog(
    stream: &mut (impl AsyncWriteExt + Unpin),
    catalog: &ImageCatalog,
) -> tokio::io::Result<()> {
    let sorted = catalog.sorted_ids();
    let count = sorted.len().min(u16::MAX as usize) as u16;

    stream.write_all(RESPONSE_LIST).await?;
    stream.write_u16(count).await?;

    for id in sorted.iter().take(count as usize) {
        if let Some(metadata) = catalog.images.get(id) {
            let size = if let Some(cached) = &metadata.cached_data {
                cached.len() as u64
            } else {
                tokio::fs::metadata(&metadata.file_name).await?.len()
            };
            let size_u32 = size.min(u32::MAX as u64) as u32;

            let name_str = metadata
                .file_name
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
    }

    Ok(())
}

// Compression statistics
#[derive(Debug, Clone)]
pub struct CompressionStats {
    pub original_size: usize,
    pub compressed_size: usize,
    pub ratio: f32,
    pub used_compression: bool,
}

impl CompressionStats {
    pub fn no_compression(size: usize) -> Self {
        Self {
            original_size: size,
            compressed_size: size,
            ratio: 1.0,
            used_compression: false,
        }
    }
}

// Get adaptive compression level based on data size
fn get_compression_level(size: usize) -> i32 {
    if size < SIZE_THRESHOLD_SMALL {
        COMPRESSION_LEVEL_SMALL
    } else if size < SIZE_THRESHOLD_LARGE {
        COMPRESSION_LEVEL_MEDIUM
    } else {
        COMPRESSION_LEVEL_LARGE
    }
}

// Compress data with zstd, return compressed data only if it meets threshold
pub fn try_compress(
    data: &[u8],
    min_ratio: f32,
) -> Result<(Option<Vec<u8>>, CompressionStats), std::io::Error> {
    let level = get_compression_level(data.len());
    let mut encoder = zstd::Encoder::new(Vec::new(), level)?;
    encoder.write_all(data)?;
    let compressed = encoder.finish()?;

    let compressed_size = compressed.len();
    let ratio = (compressed_size as f32) / (data.len() as f32);

    // Only use compression if it meets the threshold
    if ratio < min_ratio {
        Ok((
            Some(compressed),
            CompressionStats {
                original_size: data.len(),
                compressed_size,
                ratio,
                used_compression: true,
            },
        ))
    } else {
        Ok((None, CompressionStats::no_compression(data.len())))
    }
}

// Decompress zstd data
pub fn decompress(data: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let mut decoder = zstd::Decoder::new(data)?;
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed)?;
    Ok(decompressed)
}

// Send image over a TCP/TLS stream with optional compression
pub async fn send_image(
    stream: &mut (impl AsyncWriteExt + Unpin),
    metadata: &ImageMetadata,
) -> tokio::io::Result<()> {
    send_image_with_options(stream, metadata, DEFAULT_MIN_COMPRESSION_RATIO, false).await
}

// Send image with compression options and optional verbose logging
pub async fn send_image_with_options(
    stream: &mut (impl AsyncWriteExt + Unpin),
    metadata: &ImageMetadata,
    min_compression_ratio: f32,
    verbose: bool,
) -> tokio::io::Result<()> {
    // Stream large uncached files to avoid loading them fully into memory.
    if metadata.cached_data.is_none() {
        let mut file = tokio::fs::File::open(&metadata.file_name).await?;
        let filesize = file.metadata().await?.len();
        let flags = metadata.flags;
        let length_u32 = filesize.min(u32::MAX as u64) as u32;

        // Write header in single syscall
        write_image_header_buffered(stream, flags, length_u32, metadata.id).await?;

        let mut buffer = [0u8; BUFFER_SIZE];
        loop {
            let n = file.read(&mut buffer).await?;
            if n == 0 {
                break;
            }
            stream.write_all(&buffer[..n]).await?;
        }

        if verbose {
            eprintln!(
                "Streamed {} as {} bytes without compression",
                metadata.file_name.display(),
                filesize
            );
        }

        return Ok(());
    }

    // Cached or small files: use reference to avoid clone
    let data_arc = metadata
        .cached_data
        .as_ref()
        .expect("cached data should exist for compression path");

    let mut flags = metadata.flags;

    // Try compression (skip for already compressed formats)
    let file_type = file_type_from_flags(flags);
    let should_try_compress = matches!(file_type, 3 | 7); // BMP or unknown formats

    if should_try_compress {
        let (compressed_opt, stats) = try_compress(data_arc, min_compression_ratio)?;

        if let Some(compressed) = compressed_opt {
            flags |= FLAG_COMPRESSED;
            let length = compressed.len().min(u32::MAX as usize) as u32;

            // Write header in single syscall
            write_image_header_buffered(stream, flags, length, metadata.id).await?;
            stream.write_all(&compressed).await?;

            if verbose {
                eprintln!(
                    "Compressed {} from {} to {} bytes ({:.1}% reduction, level {})",
                    metadata.file_name.display(),
                    stats.original_size,
                    stats.compressed_size,
                    (1.0 - stats.ratio) * 100.0,
                    get_compression_level(data_arc.len())
                );
            }
        } else {
            let length = data_arc.len().min(u32::MAX as usize) as u32;
            write_image_header_buffered(stream, flags, length, metadata.id).await?;
            stream.write_all(data_arc).await?;

            if verbose && file_type == 3 {
                eprintln!(
                    "Skipped compression for {} ({}ratio {:.2} > threshold {:.2})",
                    metadata.file_name.display(),
                    stats.compressed_size,
                    stats.ratio,
                    min_compression_ratio
                );
            }
        }
    } else {
        let length = data_arc.len().min(u32::MAX as usize) as u32;
        write_image_header_buffered(stream, flags, length, metadata.id).await?;
        stream.write_all(data_arc).await?;
    }

    Ok(())
}

// Validate that reserved flags are not set
pub fn validate_request_flags(flags: u8) -> Result<(), std::io::Error> {
    if (flags & REQUEST_FLAGS_RESERVED_MASK) != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "reserved request flags are set",
        ));
    }
    Ok(())
}

// Send a structured error response
pub async fn send_error(
    stream: &mut (impl AsyncWriteExt + Unpin),
    code: ErrorCode,
    message: &str,
) -> tokio::io::Result<()> {
    let msg_bytes = message.as_bytes();
    let msg_len = msg_bytes.len().min(u16::MAX as usize) as u16;

    stream.write_all(RESPONSE_ERROR).await?;
    stream.write_u8(code as u8).await?;
    stream.write_u16(msg_len).await?;
    stream.write_all(&msg_bytes[..msg_len as usize]).await?;

    Ok(())
}

// Read an error response
pub async fn read_error(
    stream: &mut (impl AsyncReadExt + Unpin),
) -> tokio::io::Result<(ErrorCode, String)> {
    let code_byte = stream.read_u8().await?;
    let code = ErrorCode::from_u8(code_byte).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "unknown error code")
    })?;

    let msg_len = stream.read_u16().await? as usize;
    let mut msg_bytes = vec![0u8; msg_len];
    stream.read_exact(&mut msg_bytes).await?;
    let message = String::from_utf8_lossy(&msg_bytes).to_string();

    Ok((code, message))
}

// Buffered write helper - combines header fields into single write
pub async fn write_image_header_buffered(
    stream: &mut (impl AsyncWriteExt + Unpin),
    flags: u8,
    length: u32,
    id: ImageId,
) -> tokio::io::Result<()> {
    // Pre-allocate buffer for header: flags(1) + varint(1-5) + id(8) = max 14 bytes
    let mut header_buf = [0u8; 14];
    let mut pos = 0;

    // Flags
    header_buf[pos] = flags;
    pos += 1;

    // Varint-encoded length
    pos += encode_varint_to_buf(length, &mut header_buf[pos..]);

    // ImageID (big-endian)
    header_buf[pos..pos + 8].copy_from_slice(&id.to_be_bytes());
    pos += 8;

    // Single write for entire header
    stream.write_all(&header_buf[..pos]).await
}

// Encode varint into buffer, return bytes written
#[inline]
pub fn encode_varint_to_buf(mut value: u32, buf: &mut [u8]) -> usize {
    let mut pos = 0;
    if value < 0x80 {
        buf[pos] = value as u8;
        return 1;
    }
    while value >= 0x80 {
        buf[pos] = ((value & 0x7f) as u8) | 0x80;
        value >>= 7;
        pos += 1;
    }
    buf[pos] = value as u8;
    pos + 1
}

// Batched catalog write - single syscall per entry batch
pub async fn send_catalog_buffered(
    stream: &mut (impl AsyncWriteExt + Unpin),
    catalog: &ImageCatalog,
) -> tokio::io::Result<()> {
    let sorted = catalog.sorted_ids();
    let count = sorted.len().min(u16::MAX as usize) as u16;

    // Build entire response in memory for small catalogs
    // Estimate: 4 (header) + 2 (count) + count * (8 + 1 + 2 + 256 + 5) = ~272 bytes per entry
    let estimated_size = 6 + (count as usize) * 280;
    let mut buf = Vec::with_capacity(estimated_size.min(1024 * 1024)); // Cap at 1MB

    // Header
    buf.extend_from_slice(RESPONSE_LIST);
    buf.extend_from_slice(&count.to_be_bytes());

    for id in sorted.iter().take(count as usize) {
        if let Some(metadata) = catalog.images.get(id) {
            let size = if let Some(cached) = &metadata.cached_data {
                cached.len() as u32
            } else {
                // For uncached files, we need to stat - this is unavoidable
                match std::fs::metadata(&metadata.file_name) {
                    Ok(m) => m.len().min(u32::MAX as u64) as u32,
                    Err(_) => continue,
                }
            };

            let name_str = metadata
                .file_name
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            let name_bytes = name_str.as_bytes();
            let name_len = name_bytes.len().min(u16::MAX as usize) as u16;

            // Append entry to buffer
            buf.extend_from_slice(&metadata.id.to_be_bytes());
            buf.push(metadata.flags);
            buf.extend_from_slice(&name_len.to_be_bytes());
            buf.extend_from_slice(&name_bytes[..name_len as usize]);

            // Varint for size
            let mut varint_buf = [0u8; 5];
            let varint_len = encode_varint_to_buf(size, &mut varint_buf);
            buf.extend_from_slice(&varint_buf[..varint_len]);
        }
    }

    // Single write for entire catalog
    stream.write_all(&buf).await
}

// Batched request header write for GET_BY_ID
pub async fn write_get_request_buffered(
    stream: &mut (impl AsyncWriteExt + Unpin),
    flags: u8,
    ids: &[ImageId],
) -> tokio::io::Result<()> {
    // Header: req_type(1) + flags(1) + count(1) + ids(8*N)
    let mut buf = Vec::with_capacity(3 + ids.len() * 8);
    buf.push(REQUEST_GET_BY_ID);
    buf.push(flags);
    buf.push(ids.len().min(255) as u8);
    for id in ids.iter().take(255) {
        buf.extend_from_slice(&id.to_be_bytes());
    }
    stream.write_all(&buf).await
}

// Batched request header write for BATCH
pub async fn write_batch_request_buffered(
    stream: &mut (impl AsyncWriteExt + Unpin),
    flags: u8,
    have_ids: &[ImageId],
) -> tokio::io::Result<()> {
    // Header: req_type(1) + flags(1) + count_varint(1-5) + ids(8*N)
    let mut buf = Vec::with_capacity(7 + have_ids.len() * 8);
    buf.push(REQUEST_BATCH);
    buf.push(flags);

    // Varint for count
    let mut varint_buf = [0u8; 5];
    let varint_len = encode_varint_to_buf(
        have_ids.len().min(u32::MAX as usize) as u32,
        &mut varint_buf,
    );
    buf.extend_from_slice(&varint_buf[..varint_len]);

    for id in have_ids {
        buf.extend_from_slice(&id.to_be_bytes());
    }
    stream.write_all(&buf).await
}

// Batched request header write for LIST
pub async fn write_list_request_buffered(
    stream: &mut (impl AsyncWriteExt + Unpin),
    flags: u8,
) -> tokio::io::Result<()> {
    let buf = [REQUEST_LIST, flags];
    stream.write_all(&buf).await
}

// Batched request header write for LIST_AND_GET (combined operation)
// This requests a catalog listing AND all images in a single round-trip
pub async fn write_list_and_get_request_buffered(
    stream: &mut (impl AsyncWriteExt + Unpin),
    flags: u8,
) -> tokio::io::Result<()> {
    let buf = [REQUEST_LIST_AND_GET, flags];
    stream.write_all(&buf).await
}

// Read multiple image IDs in one syscall
pub async fn read_image_ids(
    stream: &mut (impl AsyncReadExt + Unpin),
    count: usize,
) -> tokio::io::Result<Vec<ImageId>> {
    let mut buf = vec![0u8; count * 8];
    stream.read_exact(&mut buf).await?;

    let mut ids = Vec::with_capacity(count);
    for i in 0..count {
        let mut id_bytes = [0u8; 8];
        id_bytes.copy_from_slice(&buf[i * 8..(i + 1) * 8]);
        ids.push(u64::from_be_bytes(id_bytes));
    }
    Ok(ids)
}
