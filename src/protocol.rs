use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use unicode_normalization::UnicodeNormalization;
use xxhash_rust::xxh64::xxh64;

pub const BUFFER_SIZE: usize = 65536;

// Adaptive compression levels based on file size
pub const COMPRESSION_LEVEL_SMALL: i32 = 6; // < 100 KB
pub const COMPRESSION_LEVEL_MEDIUM: i32 = 3; // 100 KB – 1 MB
pub const COMPRESSION_LEVEL_LARGE: i32 = 1;  // > 1 MB

pub const SIZE_THRESHOLD_SMALL: usize = 100 * 1024;
pub const SIZE_THRESHOLD_LARGE: usize = 1024 * 1024;

pub const DEFAULT_MIN_COMPRESSION_RATIO: f32 = 0.95;

// ── Request type codes ────────────────────────────────────────────────────────
// ReqType values 6–255 are reserved for future use.
pub const REQUEST_GET_BY_ID:    u8 = 0;
pub const REQUEST_LIST:         u8 = 1;
pub const REQUEST_BATCH:        u8 = 2;
pub const REQUEST_CANCEL:       u8 = 3; // RFC §8.5
pub const REQUEST_WATCH:        u8 = 4; // RFC §8.6
pub const REQUEST_LIST_AND_GET: u8 = 5;
pub const REQUEST_ERROR:        u8 = 0xFE;

// ── Response magic headers ────────────────────────────────────────────────────
pub const RESPONSE_LIST:         &[u8; 4] = b"JTPL";
pub const RESPONSE_GET_BY_ID:    &[u8; 4] = b"JTPD"; // RFC §9.2
pub const RESPONSE_BATCH:        &[u8; 4] = b"JTPB";
pub const RESPONSE_LIST_AND_GET: &[u8; 4] = b"JTPG";
pub const RESPONSE_CANCEL:       &[u8; 4] = b"JTPC"; // RFC §9.6
pub const RESPONSE_WATCH:        &[u8; 4] = b"JTPW"; // RFC §9.7
pub const RESPONSE_ERROR:        &[u8; 4] = b"JTPE";

// ── Request flags ─────────────────────────────────────────────────────────────
pub const REQUEST_FLAG_KEEP_ALIVE:     u8 = 1 << 0;
pub const REQUEST_FLAGS_RESERVED_MASK: u8 = 0b1111_1110;

// ── Error codes ───────────────────────────────────────────────────────────────
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ErrorCode {
    NotFound           = 1,
    InvalidRequest     = 2,
    ServerError        = 3,
    UnsupportedFeature = 4,
    RateLimited        = 5,
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

// ── WatchEvent ────────────────────────────────────────────────────────────────
// Broadcast payload for WATCH subscriptions (RFC §9.7).
#[derive(Debug, Clone)]
pub struct WatchEvent {
    pub id:       ImageId,
    pub flags:    u8,
    pub filename: String, // NFC-normalised UTF-8 basename
    pub size:     u32,    // bytes of image data
}

// ── Identifiers ───────────────────────────────────────────────────────────────
pub type ImageId = u64;
pub const IMAGE_ID_LEN_BYTES: usize = 8;
pub const IMAGE_ID_SEED:      u64   = 0;

pub fn compute_image_id(bytes: &[u8]) -> ImageId {
    xxh64(bytes, IMAGE_ID_SEED)
}

// ── Flags ─────────────────────────────────────────────────────────────────────
pub const FLAGS_FILE_TYPE_MASK: u8 = 0b0000_0111;
pub const FLAG_COMPRESSED:      u8 = 1 << 3;
pub const FLAG_ENCRYPTED:       u8 = 1 << 4;

pub fn file_type_from_flags(flags: u8) -> u8 {
    flags & FLAGS_FILE_TYPE_MASK
}

pub fn flags_from_file_type(file_type: u8) -> u8 {
    file_type & FLAGS_FILE_TYPE_MASK
}

// ── Varint (unsigned LEB128) ──────────────────────────────────────────────────

pub async fn write_varint_u32(
    stream: &mut (impl AsyncWriteExt + Unpin),
    mut value: u32,
) -> tokio::io::Result<()> {
    if value < 0x80 {
        stream.write_u8(value as u8).await?;
        return Ok(());
    }
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

pub async fn read_varint_u32(
    stream: &mut (impl AsyncReadExt + Unpin),
) -> tokio::io::Result<u32> {
    let mut result: u32 = 0;
    let mut shift:  u32 = 0;
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

// Encode a varint into a stack buffer; returns the number of bytes written.
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

// ── Catalog ───────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ImageMetadata {
    pub id:          ImageId,
    pub flags:       u8,
    pub file_name:   PathBuf,
    pub cached_data: Option<Arc<Vec<u8>>>,
}

#[derive(Clone)]
pub struct ImageCatalog {
    pub images:        HashMap<ImageId, ImageMetadata>,
    cached_sorted:     Arc<Vec<ImageId>>,
}

impl ImageCatalog {
    pub fn new() -> Self {
        Self::from_dir("images", None)
    }

    pub fn from_dir(images_dir: impl Into<PathBuf>, name_contains: Option<&str>) -> Self {
        let images_dir    = images_dir.into();
        let mut catalog   = HashMap::new();
        let name_contains = name_contains.map(|s| s.to_ascii_lowercase());

        let Ok(paths) = fs::read_dir(&images_dir) else {
            return Self { images: catalog, cached_sorted: Arc::new(Vec::new()) };
        };

        for entry in paths.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            let ext       = path.extension().and_then(|s| s.to_str()).unwrap_or("");
            let file_type = match ext.to_lowercase().as_str() {
                "png"        => 0,
                "jpg"|"jpeg" => 1,
                "webp"       => 2,
                "bmp"        => 3,
                "gif"        => 4,
                _            => continue,
            };

            if let Some(filter) = &name_contains {
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if !name.contains(filter.as_str()) {
                    continue;
                }
            }

            let Ok(bytes) = fs::read(&path) else { continue };
            let id = compute_image_id(&bytes);

            // §6.2: reject collisions rather than silently overwriting.
            if let Some(existing) = catalog.get(&id) {
                eprintln!(
                    "ImageID collision: {} and {} both hash to {:016x} — skipping {}",
                    existing.file_name.display(),
                    path.display(),
                    id,
                    path.display(),
                );
                continue;
            }

            let flags = flags_from_file_type(file_type);
            let meta  = if bytes.len() <= SIZE_THRESHOLD_LARGE {
                ImageMetadata { id, flags, file_name: path.clone(), cached_data: Some(Arc::new(bytes)) }
            } else {
                ImageMetadata { id, flags, file_name: path.clone(), cached_data: None }
            };

            catalog.insert(id, meta);
        }

        let mut cat = Self { images: catalog, cached_sorted: Arc::new(Vec::new()) };
        cat.rebuild_sorted();
        cat
    }

    /// Add a single new image and rebuild the sorted index.
    pub fn add_image(&mut self, meta: ImageMetadata) {
        self.images.insert(meta.id, meta);
        self.rebuild_sorted();
    }

    fn rebuild_sorted(&mut self) {
        let mut sorted: Vec<ImageId> = self.images.values().map(|m| m.id).collect();
        let images = &self.images;
        sorted.sort_by(|a, b| {
            let a_name = images.get(a)
                .and_then(|m| m.file_name.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("");
            let b_name = images.get(b)
                .and_then(|m| m.file_name.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("");
            a_name.cmp(b_name)
        });
        self.cached_sorted = Arc::new(sorted);
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
    fn default() -> Self { Self::new() }
}

// ── Catalog wire I/O ──────────────────────────────────────────────────────────

/// Send a LIST response.
pub async fn send_catalog(
    stream: &mut (impl AsyncWriteExt + Unpin),
    catalog: &ImageCatalog,
) -> tokio::io::Result<()> {
    let sorted = catalog.sorted_ids();
    let count  = sorted.len().min(u32::MAX as usize) as u32;

    stream.write_all(RESPONSE_LIST).await?;
    write_varint_u32(stream, count).await?;

    for id in sorted.iter().take(count as usize) {
        if let Some(metadata) = catalog.images.get(id) {
            let size = if let Some(cached) = &metadata.cached_data {
                cached.len() as u64
            } else {
                tokio::fs::metadata(&metadata.file_name).await?.len()
            };
            let size_u32 = size.min(u32::MAX as u64) as u32;

            // §5.3: NFC-normalise filename before sending.
            let name_str: String = metadata
                .file_name
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .nfc()
                .collect();
            let name_bytes = name_str.as_bytes();
            let name_len   = name_bytes.len().min(u16::MAX as usize) as u16;

            stream.write_u64(metadata.id).await?;
            stream.write_u8(metadata.flags).await?;
            stream.write_u16(name_len).await?;
            stream.write_all(&name_bytes[..name_len as usize]).await?;
            write_varint_u32(stream, size_u32).await?;
        }
    }

    Ok(())
}

/// Buffered (single-syscall) LIST response.
pub async fn send_catalog_buffered(
    stream: &mut (impl AsyncWriteExt + Unpin),
    catalog: &ImageCatalog,
) -> tokio::io::Result<()> {
    let sorted = catalog.sorted_ids();
    let count  = sorted.len().min(u32::MAX as usize) as u32;

    // Estimate buffer: 4 (header) + 5 (varint count) + N * ~280
    let estimated = 9 + (count as usize) * 280;
    let mut buf   = Vec::with_capacity(estimated.min(1024 * 1024));

    // Header
    buf.extend_from_slice(RESPONSE_LIST);

    // varint count
    let mut varint_buf = [0u8; 5];
    let varint_len     = encode_varint_to_buf(count, &mut varint_buf);
    buf.extend_from_slice(&varint_buf[..varint_len]);

    for id in sorted.iter().take(count as usize) {
        if let Some(metadata) = catalog.images.get(id) {
            let size = if let Some(cached) = &metadata.cached_data {
                cached.len() as u32
            } else {
                match std::fs::metadata(&metadata.file_name) {
                    Ok(m) => m.len().min(u32::MAX as u64) as u32,
                    Err(_) => continue,
                }
            };

            // §5.3: NFC-normalise filename before sending.
            let name_str: String = metadata
                .file_name
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .nfc()
                .collect();
            let name_bytes = name_str.as_bytes();
            let name_len   = name_bytes.len().min(u16::MAX as usize) as u16;

            buf.extend_from_slice(&metadata.id.to_be_bytes());
            buf.push(metadata.flags);
            buf.extend_from_slice(&name_len.to_be_bytes());
            buf.extend_from_slice(&name_bytes[..name_len as usize]);

            let mut vb  = [0u8; 5];
            let vb_len  = encode_varint_to_buf(size, &mut vb);
            buf.extend_from_slice(&vb[..vb_len]);
        }
    }

    stream.write_all(&buf).await
}

// ── Compression ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CompressionStats {
    pub original_size:   usize,
    pub compressed_size: usize,
    pub ratio:           f32,
    pub used_compression: bool,
}

impl CompressionStats {
    pub fn no_compression(size: usize) -> Self {
        Self { original_size: size, compressed_size: size, ratio: 1.0, used_compression: false }
    }
}

fn get_compression_level(size: usize) -> i32 {
    if size < SIZE_THRESHOLD_SMALL { COMPRESSION_LEVEL_SMALL }
    else if size < SIZE_THRESHOLD_LARGE { COMPRESSION_LEVEL_MEDIUM }
    else { COMPRESSION_LEVEL_LARGE }
}

pub fn try_compress(
    data: &[u8],
    min_ratio: f32,
) -> Result<(Option<Vec<u8>>, CompressionStats), std::io::Error> {
    let level      = get_compression_level(data.len());
    let mut encoder = zstd::Encoder::new(Vec::new(), level)?;
    encoder.write_all(data)?;
    let compressed = encoder.finish()?;

    let compressed_size = compressed.len();
    let ratio           = (compressed_size as f32) / (data.len() as f32);

    if ratio < min_ratio {
        Ok((Some(compressed), CompressionStats {
            original_size: data.len(), compressed_size, ratio, used_compression: true,
        }))
    } else {
        Ok((None, CompressionStats::no_compression(data.len())))
    }
}

pub fn decompress(data: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let mut decoder     = zstd::Decoder::new(data)?;
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed)?;
    Ok(decompressed)
}

// ── Image I/O ─────────────────────────────────────────────────────────────────

pub async fn send_image(
    stream: &mut (impl AsyncWriteExt + Unpin),
    metadata: &ImageMetadata,
) -> tokio::io::Result<()> {
    send_image_with_options(stream, metadata, DEFAULT_MIN_COMPRESSION_RATIO, false).await
}

pub async fn send_image_with_options(
    stream: &mut (impl AsyncWriteExt + Unpin),
    metadata: &ImageMetadata,
    min_compression_ratio: f32,
    verbose: bool,
) -> tokio::io::Result<()> {
    // Stream large uncached files directly to avoid loading them into memory.
    if metadata.cached_data.is_none() {
        let mut file     = tokio::fs::File::open(&metadata.file_name).await?;
        let filesize     = file.metadata().await?.len();
        let flags        = metadata.flags;
        let length_u32   = filesize.min(u32::MAX as u64) as u32;

        write_image_header_buffered(stream, flags, length_u32, metadata.id).await?;

        let mut buffer = [0u8; BUFFER_SIZE];
        loop {
            let n = file.read(&mut buffer).await?;
            if n == 0 { break; }
            stream.write_all(&buffer[..n]).await?;
        }

        if verbose {
            eprintln!(
                "Streamed {} as {} bytes without compression",
                metadata.file_name.display(), filesize
            );
        }
        return Ok(());
    }

    let data_arc   = metadata.cached_data.as_ref().expect("cached data present");
    let mut flags  = metadata.flags;
    let file_type  = file_type_from_flags(flags);
    let should_try_compress = matches!(file_type, 3 | 7);

    if should_try_compress {
        let (compressed_opt, stats) = try_compress(data_arc, min_compression_ratio)?;

        if let Some(compressed) = compressed_opt {
            flags |= FLAG_COMPRESSED;
            let length = compressed.len().min(u32::MAX as usize) as u32;

            write_image_header_buffered(stream, flags, length, metadata.id).await?;
            stream.write_all(&compressed).await?;

            if verbose {
                eprintln!(
                    "Compressed {} from {} to {} bytes ({:.1}% reduction, level {})",
                    metadata.file_name.display(),
                    stats.original_size,
                    stats.compressed_size,
                    (1.0 - stats.ratio) * 100.0,
                    get_compression_level(data_arc.len()),
                );
            }
        } else {
            let length = data_arc.len().min(u32::MAX as usize) as u32;
            write_image_header_buffered(stream, flags, length, metadata.id).await?;
            stream.write_all(data_arc).await?;

            if verbose && file_type == 3 {
                eprintln!(
                    "Skipped compression for {} (ratio {:.2} > threshold {:.2})",
                    metadata.file_name.display(),
                    stats.ratio,
                    min_compression_ratio,
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

// ── Request/response helpers ──────────────────────────────────────────────────

pub fn validate_request_flags(flags: u8) -> Result<(), std::io::Error> {
    if (flags & REQUEST_FLAGS_RESERVED_MASK) != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "reserved request flags are set",
        ));
    }
    Ok(())
}

pub async fn send_error(
    stream: &mut (impl AsyncWriteExt + Unpin),
    code: ErrorCode,
    message: &str,
) -> tokio::io::Result<()> {
    let msg_bytes = message.as_bytes();
    let msg_len   = msg_bytes.len().min(u16::MAX as usize) as u16;

    stream.write_all(RESPONSE_ERROR).await?;
    stream.write_u8(code as u8).await?;
    stream.write_u16(msg_len).await?;
    stream.write_all(&msg_bytes[..msg_len as usize]).await?;
    Ok(())
}

pub async fn read_error(
    stream: &mut (impl AsyncReadExt + Unpin),
) -> tokio::io::Result<(ErrorCode, String)> {
    let code_byte = stream.read_u8().await?;
    let code      = ErrorCode::from_u8(code_byte).ok_or_else(|| {
        std::io::Error::new(tokio::io::ErrorKind::InvalidData, "unknown error code")
    })?;
    let msg_len   = stream.read_u16().await? as usize;
    let mut msg   = vec![0u8; msg_len];
    stream.read_exact(&mut msg).await?;
    Ok((code, String::from_utf8_lossy(&msg).to_string()))
}

/// Write an image packet header (flags + varint length + ImageID) in one call.
pub async fn write_image_header_buffered(
    stream: &mut (impl AsyncWriteExt + Unpin),
    flags:  u8,
    length: u32,
    id:     ImageId,
) -> tokio::io::Result<()> {
    let mut header_buf = [0u8; 14]; // flags(1) + varint(1-5) + id(8)
    let mut pos = 0;

    header_buf[pos] = flags;
    pos += 1;

    pos += encode_varint_to_buf(length, &mut header_buf[pos..]);

    header_buf[pos..pos + 8].copy_from_slice(&id.to_be_bytes());
    pos += 8;

    stream.write_all(&header_buf[..pos]).await
}

// ── Buffered request writers ──────────────────────────────────────────────────

pub async fn write_get_request_buffered(
    stream: &mut (impl AsyncWriteExt + Unpin),
    flags:  u8,
    ids:    &[ImageId],
) -> tokio::io::Result<()> {
    let mut buf = Vec::with_capacity(3 + ids.len() * 8);
    buf.push(REQUEST_GET_BY_ID);
    buf.push(flags);
    buf.push(ids.len().min(255) as u8);
    for id in ids.iter().take(255) {
        buf.extend_from_slice(&id.to_be_bytes());
    }
    stream.write_all(&buf).await
}

pub async fn write_batch_request_buffered(
    stream:   &mut (impl AsyncWriteExt + Unpin),
    flags:    u8,
    have_ids: &[ImageId],
) -> tokio::io::Result<()> {
    let mut buf = Vec::with_capacity(7 + have_ids.len() * 8);
    buf.push(REQUEST_BATCH);
    buf.push(flags);

    let mut varint_buf = [0u8; 5];
    let varint_len     = encode_varint_to_buf(
        have_ids.len().min(u32::MAX as usize) as u32,
        &mut varint_buf,
    );
    buf.extend_from_slice(&varint_buf[..varint_len]);

    for id in have_ids {
        buf.extend_from_slice(&id.to_be_bytes());
    }
    stream.write_all(&buf).await
}

pub async fn write_list_request_buffered(
    stream: &mut (impl AsyncWriteExt + Unpin),
    flags:  u8,
) -> tokio::io::Result<()> {
    stream.write_all(&[REQUEST_LIST, flags]).await
}

pub async fn write_list_and_get_request_buffered(
    stream: &mut (impl AsyncWriteExt + Unpin),
    flags:  u8,
) -> tokio::io::Result<()> {
    stream.write_all(&[REQUEST_LIST_AND_GET, flags]).await
}

/// Write a CANCEL request. RequestFlags MUST be 0 (§8.5).
pub async fn write_cancel_request_buffered(
    stream: &mut (impl AsyncWriteExt + Unpin),
) -> tokio::io::Result<()> {
    stream.write_all(&[REQUEST_CANCEL, 0u8]).await
}

/// Write a WATCH request. RequestFlags MUST be 0, keep-alive is implicit (§8.6).
pub async fn write_watch_request_buffered(
    stream: &mut (impl AsyncWriteExt + Unpin),
) -> tokio::io::Result<()> {
    stream.write_all(&[REQUEST_WATCH, 0u8]).await
}

/// Send a CANCEL acknowledgement (JTPC, §9.6).
pub async fn send_cancel_ack(
    stream: &mut (impl AsyncWriteExt + Unpin),
) -> tokio::io::Result<()> {
    stream.write_all(RESPONSE_CANCEL).await
}

/// Send a WATCH event frame (JTPW, §9.7).
pub async fn send_watch_event(
    stream: &mut (impl AsyncWriteExt + Unpin),
    event:  &WatchEvent,
) -> tokio::io::Result<()> {
    // NFC-normalise filename before sending (§5.3).
    let name_str: String  = event.filename.nfc().collect();
    let name_bytes        = name_str.as_bytes();
    let name_len          = name_bytes.len().min(u16::MAX as usize) as u16;

    let mut buf = Vec::with_capacity(4 + 8 + 1 + 2 + name_len as usize + 5);
    buf.extend_from_slice(RESPONSE_WATCH);
    buf.extend_from_slice(&event.id.to_be_bytes());
    buf.push(event.flags);
    buf.extend_from_slice(&name_len.to_be_bytes());
    buf.extend_from_slice(&name_bytes[..name_len as usize]);

    let mut vb = [0u8; 5];
    let vb_len = encode_varint_to_buf(event.size, &mut vb);
    buf.extend_from_slice(&vb[..vb_len]);

    stream.write_all(&buf).await
}

// ── Bulk ID reader ────────────────────────────────────────────────────────────

pub async fn read_image_ids(
    stream: &mut (impl AsyncReadExt + Unpin),
    count:  usize,
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