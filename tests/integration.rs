//! Integration and unit tests for the Jason Transfer Protocol (JTP).
//!
//! Organisation:
//!
//!   § 1  Protocol primitives  — varint, flags, error codes, catalog wire format
//!   § 2  ImageCatalog         — collision detection, NFC normalisation, sorting
//!   § 3  Server wire tests    — full request/response flows over a real TCP socket
//!   § 4  CANCEL               — mid-stream cancellation round-trip
//!   § 5  WATCH                — server-push subscription
//!   § 6  Error paths          — reserved flags, unknown ReqType, oversized BATCH
//!
//! Run with:
//!   cargo test                       (all tests)
//!   cargo test protocol::             (§ 1–2)
//!   cargo test server::               (§ 3–6)
//!
//! The server tests bind to port 0 (OS picks an ephemeral port) so multiple
//! test runs never collide.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter, DuplexStream};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, RwLock};

use jtp::protocol::{
    compute_image_id, encode_varint_to_buf, file_type_from_flags, flags_from_file_type,
    read_image_ids, read_varint_u32, send_cancel_ack, send_catalog, send_catalog_buffered,
    send_error, send_image, send_image_with_options, send_watch_event, validate_request_flags,
    write_batch_request_buffered, write_cancel_request_buffered, write_get_request_buffered,
    write_image_header_buffered, write_list_and_get_request_buffered,
    write_list_request_buffered, write_varint_u32, write_watch_request_buffered, ErrorCode,
    ImageCatalog, ImageId, ImageMetadata, WatchEvent, FLAG_COMPRESSED, FLAGS_FILE_TYPE_MASK,
    REQUEST_BATCH, REQUEST_CANCEL, REQUEST_FLAG_KEEP_ALIVE, REQUEST_GET_BY_ID, REQUEST_LIST,
    REQUEST_LIST_AND_GET, REQUEST_WATCH, RESPONSE_BATCH, RESPONSE_CANCEL, RESPONSE_ERROR,
    RESPONSE_GET_BY_ID, RESPONSE_LIST, RESPONSE_LIST_AND_GET, RESPONSE_WATCH,
};

// ============================================================================
// § 0  Helpers used across multiple sections
// ============================================================================

/// Construct a minimal in-memory ImageMetadata with the given raw bytes as
/// content. No file on disk is needed; `cached_data` is populated directly.
fn fake_image(id: ImageId, file_type: u8, data: Vec<u8>) -> ImageMetadata {
    let flags = flags_from_file_type(file_type);
    ImageMetadata {
        id,
        flags,
        file_name:   PathBuf::from(format!("test_{:016x}.png", id)),
        cached_data: Some(Arc::new(data)),
    }
}

/// Build a small ImageCatalog from a list of raw byte vectors (PNG type = 0).
fn make_catalog(images: Vec<Vec<u8>>) -> ImageCatalog {
    let mut cat = ImageCatalog::new_empty();
    for data in images {
        let id   = compute_image_id(&data);
        let meta = fake_image(id, 0, data);
        cat.add_image(meta, false);
    }
    cat
}

/// Spawn a bare-bones JTP server on an ephemeral port and return its address.
/// The server handles a single connection then exits.
///
/// `handler` receives the accepted stream and must write/read the full
/// request/response exchange. Use `run_server_once` for the common case of
/// running `handle_requests`.
async fn bind_ephemeral() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr     = listener.local_addr().unwrap();
    (listener, addr)
}

/// Connect a plain-TCP client to `addr` and return a `BufWriter`-wrapped
/// stream ready for writing requests.
async fn plain_connect(addr: SocketAddr) -> BufWriter<TcpStream> {
    let tcp = TcpStream::connect(addr).await.unwrap();
    tcp.set_nodelay(true).unwrap();
    BufWriter::with_capacity(64 * 1024, tcp)
}

// ============================================================================
// § 1  Protocol primitives
// ============================================================================

mod protocol {
    use super::*;

    // ── Varint encoding ───────────────────────────────────────────────────────

    /// Encode a u32 to a buffer, return it as a vec.
    fn encode(v: u32) -> Vec<u8> {
        let mut buf = [0u8; 5];
        let n = encode_varint_to_buf(v, &mut buf);
        buf[..n].to_vec()
    }

    /// Round-trip a value through the async write + read helpers using a duplex
    /// stream.
    async fn varint_round_trip(value: u32) -> u32 {
        let (mut a, mut b) = tokio::io::duplex(64);
        write_varint_u32(&mut a, value).await.unwrap();
        read_varint_u32(&mut b).await.unwrap()
    }

    #[tokio::test]
    async fn varint_zero() {
        assert_eq!(varint_round_trip(0).await, 0);
        assert_eq!(encode(0), vec![0x00]);
    }

    #[tokio::test]
    async fn varint_single_byte_boundary() {
        // 127 is the last single-byte value.
        assert_eq!(varint_round_trip(127).await, 127);
        assert_eq!(encode(127), vec![0x7f]);
    }

    #[tokio::test]
    async fn varint_two_byte_boundary() {
        // 128 is the first two-byte value.
        assert_eq!(varint_round_trip(128).await, 128);
        assert_eq!(encode(128), vec![0x80, 0x01]);
    }

    #[tokio::test]
    async fn varint_example_from_rfc() {
        // RFC appendix: 4660 (0x1234) → 0xB4 0x24
        assert_eq!(varint_round_trip(4660).await, 4660);
        assert_eq!(encode(4660), vec![0xB4, 0x24]);
    }

    #[tokio::test]
    async fn varint_max_u32() {
        assert_eq!(varint_round_trip(u32::MAX).await, u32::MAX);
        // u32::MAX (0xFFFF_FFFF) requires 5 bytes.
        assert_eq!(encode(u32::MAX).len(), 5);
    }

    #[tokio::test]
    async fn varint_canonical_single_byte_values() {
        // 0–127 must encode to exactly 1 byte.
        for v in 0u32..=127 {
            assert_eq!(encode(v).len(), 1, "value {} should be 1 byte", v);
        }
    }

    #[tokio::test]
    async fn varint_rejects_overlong() {
        // 6-byte varint should return an error (max is 5).
        let (mut a, mut b) = tokio::io::duplex(16);
        // Write 6 bytes all with continuation bit set.
        let overlong = vec![0x80u8; 6];
        a.write_all(&overlong).await.unwrap();
        drop(a);
        let result = read_varint_u32(&mut b).await;
        assert!(result.is_err(), "should reject 6-byte varint");
    }

    // ── ImageID ───────────────────────────────────────────────────────────────

    #[test]
    fn image_id_deterministic() {
        let data = b"hello world";
        assert_eq!(compute_image_id(data), compute_image_id(data));
    }

    #[test]
    fn image_id_differs_for_different_data() {
        assert_ne!(compute_image_id(b"aaa"), compute_image_id(b"bbb"));
    }

    #[test]
    fn image_id_seed_zero() {
        // xxHash64 with seed 0 — spot-check the known output for empty input.
        // xxHash64("", seed=0) = 0xEF46DB3751D8E999
        assert_eq!(compute_image_id(b""), 0xEF46DB3751D8E999u64);
    }

    // ── Flags helpers ─────────────────────────────────────────────────────────

    #[test]
    fn flags_file_type_roundtrip() {
        for ft in 0u8..=7 {
            let flags    = flags_from_file_type(ft);
            let recovered = file_type_from_flags(flags);
            assert_eq!(recovered, ft, "file type {} did not round-trip", ft);
        }
    }

    #[test]
    fn flags_file_type_mask_isolates_low_3_bits() {
        // Compressed + encrypted bits should not leak into file type.
        let flags = FLAG_COMPRESSED | 0b010; // JPEG + compressed
        assert_eq!(file_type_from_flags(flags), 0b010);
    }

    #[test]
    fn flag_compressed_does_not_clobber_file_type() {
        let flags = flags_from_file_type(1) | FLAG_COMPRESSED;
        assert_eq!(file_type_from_flags(flags), 1);
        assert_ne!(flags & FLAG_COMPRESSED, 0);
    }

    // ── validate_request_flags ────────────────────────────────────────────────

    #[test]
    fn request_flags_valid_zero() {
        assert!(validate_request_flags(0x00).is_ok());
    }

    #[test]
    fn request_flags_valid_keep_alive() {
        assert!(validate_request_flags(REQUEST_FLAG_KEEP_ALIVE).is_ok());
    }

    #[test]
    fn request_flags_rejects_reserved_bit_1() {
        assert!(validate_request_flags(0b0000_0010).is_err());
    }

    #[test]
    fn request_flags_rejects_reserved_bits_all_set() {
        assert!(validate_request_flags(0b1111_1110).is_err());
    }

    #[test]
    fn request_flags_rejects_reserved_plus_keepalive() {
        // Bit 0 (keep-alive) is valid; bit 1 is reserved → should fail.
        assert!(validate_request_flags(0b0000_0011).is_err());
    }

    // ── send_error / read_error ───────────────────────────────────────────────

    #[tokio::test]
    async fn error_round_trip_not_found() {
        let (mut a, mut b) = tokio::io::duplex(256);
        send_error(&mut a, ErrorCode::NotFound, "missing").await.unwrap();
        drop(a);

        let mut hdr = [0u8; 4];
        b.read_exact(&mut hdr).await.unwrap();
        assert_eq!(&hdr, RESPONSE_ERROR);

        let (code, msg) = jtp::protocol::read_error(&mut b).await.unwrap();
        assert_eq!(code,  ErrorCode::NotFound);
        assert_eq!(msg,   "missing");
    }

    #[tokio::test]
    async fn error_round_trip_all_codes() {
        let cases = [
            (ErrorCode::NotFound,           "nf"),
            (ErrorCode::InvalidRequest,     "ir"),
            (ErrorCode::ServerError,        "se"),
            (ErrorCode::UnsupportedFeature, "uf"),
            (ErrorCode::RateLimited,        "rl"),
        ];

        for (code, msg) in cases {
            let (mut a, mut b) = tokio::io::duplex(256);
            send_error(&mut a, code, msg).await.unwrap();
            drop(a);

            let mut hdr = [0u8; 4];
            b.read_exact(&mut hdr).await.unwrap();
            assert_eq!(&hdr, RESPONSE_ERROR);

            let (got_code, got_msg) = jtp::protocol::read_error(&mut b).await.unwrap();
            assert_eq!(got_code, code);
            assert_eq!(got_msg,  msg);
        }
    }

    #[tokio::test]
    async fn error_empty_message() {
        let (mut a, mut b) = tokio::io::duplex(64);
        send_error(&mut a, ErrorCode::ServerError, "").await.unwrap();
        drop(a);

        let mut hdr = [0u8; 4];
        b.read_exact(&mut hdr).await.unwrap();
        assert_eq!(&hdr, RESPONSE_ERROR);

        let (code, msg) = jtp::protocol::read_error(&mut b).await.unwrap();
        assert_eq!(code, ErrorCode::ServerError);
        assert_eq!(msg, "");
    }

    // ── Image header ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn image_header_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(64);
        let flags  = flags_from_file_type(0) | FLAG_COMPRESSED;
        let length = 12345u32;
        let id     = 0xDEADBEEFCAFEBABEu64;

        write_image_header_buffered(&mut a, flags, length, id).await.unwrap();
        drop(a);

        let got_flags  = b.read_u8().await.unwrap();
        let got_length = read_varint_u32(&mut b).await.unwrap();
        let got_id     = b.read_u64().await.unwrap();

        assert_eq!(got_flags,  flags);
        assert_eq!(got_length, length);
        assert_eq!(got_id,     id);
    }

    #[tokio::test]
    async fn image_header_length_zero() {
        let (mut a, mut b) = tokio::io::duplex(32);
        write_image_header_buffered(&mut a, 0, 0, 0).await.unwrap();
        drop(a);

        let _flags = b.read_u8().await.unwrap();
        assert_eq!(read_varint_u32(&mut b).await.unwrap(), 0);
        assert_eq!(b.read_u64().await.unwrap(), 0);
    }

    // ── send_cancel_ack ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn cancel_ack_writes_jtpc() {
        let (mut a, mut b) = tokio::io::duplex(8);
        send_cancel_ack(&mut a).await.unwrap();
        drop(a);

        let mut hdr = [0u8; 4];
        b.read_exact(&mut hdr).await.unwrap();
        assert_eq!(&hdr, RESPONSE_CANCEL);
    }

    // ── send_watch_event ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn watch_event_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(256);
        let event = WatchEvent {
            id:       0x0102030405060708,
            flags:    flags_from_file_type(1),
            filename: "photo.jpg".to_string(),
            size:     4096,
        };
        send_watch_event(&mut a, &event).await.unwrap();
        drop(a);

        let mut hdr = [0u8; 4];
        b.read_exact(&mut hdr).await.unwrap();
        assert_eq!(&hdr, RESPONSE_WATCH);

        let got_id       = b.read_u64().await.unwrap();
        let got_flags    = b.read_u8().await.unwrap();
        let name_len     = b.read_u16().await.unwrap() as usize;
        let mut name_buf = vec![0u8; name_len];
        b.read_exact(&mut name_buf).await.unwrap();
        let got_name     = String::from_utf8(name_buf).unwrap();
        let got_size     = read_varint_u32(&mut b).await.unwrap();

        assert_eq!(got_id,    event.id);
        assert_eq!(got_flags, event.flags);
        assert_eq!(got_name,  event.filename);
        assert_eq!(got_size,  event.size);
    }
}

// ============================================================================
// § 2  ImageCatalog
// ============================================================================

mod catalog {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    /// Write a tiny PNG-shaped file and return the temp dir + its path.
    fn write_temp_image(dir: &TempDir, name: &str, content: &[u8]) -> PathBuf {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content).unwrap();
        path
    }

    // ── Collision detection ───────────────────────────────────────────────────

    #[test]
    fn catalog_rejects_collision() {
        // Two identical byte sequences → same ImageID → second must be skipped.
        let bytes = b"duplicate content".to_vec();
        let id    = compute_image_id(&bytes);
        let meta1 = fake_image(id, 0, bytes.clone());
        let meta2 = fake_image(id, 1, bytes.clone()); // same ID, different flags

        let mut cat = ImageCatalog::new_empty();
        cat.add_image(meta1, true);
        cat.add_image(meta2, true); // collision — should be silently dropped (verbose true here so it should log)

        assert_eq!(cat.images.len(), 1, "collision should reduce catalog to 1 entry");
        // The surviving entry should be the first one (PNG, type 0).
        assert_eq!(file_type_from_flags(cat.images[&id].flags), 0);
    }

    // ── sorted_ids ────────────────────────────────────────────────────────────

    #[test]
    fn catalog_sorted_ids_are_stable() {
        let data_a = b"aaaa".to_vec();
        let data_b = b"bbbb".to_vec();
        let id_a   = compute_image_id(&data_a);
        let id_b   = compute_image_id(&data_b);

        let mut cat = ImageCatalog::new_empty();
        cat.add_image(fake_image(id_b, 0, data_b), false);
        cat.add_image(fake_image(id_a, 0, data_a), false);

        // sorted_ids must be sorted by filename, not insertion order.
        // fake_image names are "test_<id_hex>.png" — the lower hex value sorts first.
        let sorted = cat.sorted_ids();
        assert_eq!(sorted.len(), 2);
        // Verify that calling sorted_ids twice gives the same order.
        assert_eq!(sorted, cat.sorted_ids());
    }

    // ── from_dir ─────────────────────────────────────────────────────────────

    #[test]
    fn catalog_from_dir_ignores_non_image_files() {
        let dir = tempfile::tempdir().unwrap();
        write_temp_image(&dir, "image.png", b"\x89PNG...fake");
        write_temp_image(&dir, "readme.txt", b"not an image");
        write_temp_image(&dir, "script.sh",  b"#!/bin/sh");

        let cat = ImageCatalog::from_dir(dir.path(), None);
        assert_eq!(cat.images.len(), 1, "only PNG should be loaded");
    }

    #[test]
    fn catalog_from_dir_name_filter() {
        let dir = tempfile::tempdir().unwrap();
        write_temp_image(&dir, "sunset.jpg",    b"jpg_data_a");
        write_temp_image(&dir, "portrait.jpg",  b"jpg_data_b");
        write_temp_image(&dir, "landscape.png", b"png_data_c");

        let cat = ImageCatalog::from_dir(dir.path(), Some("portrait"));
        assert_eq!(cat.images.len(), 1);
        let only = cat.images.values().next().unwrap();
        assert!(only.file_name.to_string_lossy().contains("portrait"));
    }

    #[test]
    fn catalog_from_dir_missing_dir_returns_empty() {
        let cat = ImageCatalog::from_dir("/this/path/does/not/exist", None);
        assert!(cat.images.is_empty());
        assert!(cat.sorted_ids().is_empty());
    }

    // ── LIST response count field ─────────────────────────────────────────────

    #[tokio::test]
    async fn catalog_send_uses_varint_count_not_u16() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let cat = make_catalog(vec![b"img1".to_vec(), b"img2".to_vec(), b"img3".to_vec()]);

        send_catalog(&mut a, &cat).await.unwrap();
        drop(a);

        // Header: "JTPL"
        let mut hdr = [0u8; 4];
        b.read_exact(&mut hdr).await.unwrap();
        assert_eq!(&hdr, RESPONSE_LIST);

        // §9.1: Count is varint(u32), not a bare u16.
        // For N=3, varint encodes as a single byte 0x03.
        let count_byte = b.read_u8().await.unwrap();
        assert!(count_byte & 0x80 == 0, "count=3 should be a 1-byte varint (no continuation bit)");
        assert_eq!(count_byte, 3);
    }

    #[tokio::test]
    async fn catalog_buffered_matches_unbuffered() {
        let images = vec![b"data_x".to_vec(), b"data_y".to_vec()];
        let cat    = make_catalog(images);

        let (mut a1, mut b1) = tokio::io::duplex(4096);
        let (mut a2, mut b2) = tokio::io::duplex(4096);

        send_catalog(&mut a1, &cat).await.unwrap();
        send_catalog_buffered(&mut a2, &cat).await.unwrap();
        drop(a1);
        drop(a2);

        let mut out1 = Vec::new();
        let mut out2 = Vec::new();
        b1.read_to_end(&mut out1).await.unwrap();
        b2.read_to_end(&mut out2).await.unwrap();

        assert_eq!(out1, out2, "send_catalog and send_catalog_buffered must produce identical bytes");
    }

    // ── NFC normalisation ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn catalog_nfc_normalises_nfd_filename() {
        use unicode_normalization::UnicodeNormalization;

        // "café" in NFD (e + combining accent) vs NFC (é as single codepoint)
        let nfd_name: String = "cafe\u{0301}.png".to_string(); // NFD
        let nfc_name: String = nfd_name.nfc().collect();       // NFC

        let (mut a, mut b) = tokio::io::duplex(4096);

        let data  = b"pixel".to_vec();
        let id    = compute_image_id(&data);
        let flags = flags_from_file_type(0);
        let meta  = ImageMetadata {
            id,
            flags,
            file_name:   PathBuf::from(&nfd_name),
            cached_data: Some(Arc::new(data)),
        };
        let mut cat = ImageCatalog::new_empty();
        cat.add_image(meta, false);

        send_catalog(&mut a, &cat).await.unwrap();
        drop(a);

        let mut hdr = [0u8; 4];
        b.read_exact(&mut hdr).await.unwrap();
        assert_eq!(&hdr, RESPONSE_LIST);

        let count = read_varint_u32(&mut b).await.unwrap();
        assert_eq!(count, 1);

        let _id      = b.read_u64().await.unwrap();
        let _flags   = b.read_u8().await.unwrap();
        let name_len = b.read_u16().await.unwrap() as usize;
        let mut name_buf = vec![0u8; name_len];
        b.read_exact(&mut name_buf).await.unwrap();
        let transmitted = String::from_utf8(name_buf).unwrap();

        // Transmitted filename must be NFC-normalised.
        assert_eq!(transmitted, nfc_name, "filename must be NFC-normalised before transmission");
    }
}

// ============================================================================
// § 3  Server wire tests
// ============================================================================
//
// Each test:
//   1. Creates a small in-memory catalog.
//   2. Spins up handle_requests in a background task listening on port 0.
//   3. Connects a raw TCP client.
//   4. Writes a request manually (byte-exact).
//   5. Asserts the response wire format against the spec.

mod server {
    use super::*;

    // ── Test server harness ───────────────────────────────────────────────────

    /// Spin up a JTP server handling one connection, return its address.
    /// The server task exits after the connection closes.
    async fn spawn_server(catalog: ImageCatalog) -> SocketAddr {
        spawn_server_with_watch(catalog, None).await
    }

    async fn spawn_server_with_watch(
        catalog:  ImageCatalog,
        watch_tx: Option<Arc<broadcast::Sender<WatchEvent>>>,
    ) -> SocketAddr {
        let listener          = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr              = listener.local_addr().unwrap();
        let catalog           = Arc::new(RwLock::new(catalog));
        let compression       = jtp::protocol::DEFAULT_MIN_COMPRESSION_RATIO;
        let keep_alive_timeout = Duration::from_secs(5);

        tokio::spawn(async move {
            eprintln!("[SERVER] Waiting for connection...");
            let (socket, peer_addr) = listener.accept().await.unwrap();
            eprintln!("[SERVER] Accepted connection from {}", peer_addr);
            
            socket.set_nodelay(true).unwrap();
            let stream = BufWriter::with_capacity(64 * 1024, socket);
            eprintln!("[SERVER] Calling handle_requests with timeout={:?}", keep_alive_timeout);
            // Call the server's internal handler. Because handle_requests is
            // private, we re-export it in a cfg(test) block — see below.
            test_handle_requests(stream, catalog, compression, keep_alive_timeout, true, watch_tx).await;
            eprintln!("[SERVER] handle_requests returned, task exiting");
        });

        addr
    }

    // ── LIST ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn list_response_header_and_varint_count() {
        let images = vec![b"a".to_vec(), b"bb".to_vec(), b"ccc".to_vec()];
        let addr   = spawn_server(make_catalog(images)).await;
        let mut w  = plain_connect(addr).await;

        write_list_request_buffered(&mut w, 0).await.unwrap();
        w.flush().await.unwrap();

        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await.unwrap();
        assert_eq!(&hdr, RESPONSE_LIST, "LIST header must be JTPL");

        // §9.1: count is varint(u32).
        let count = read_varint_u32(w.get_mut()).await.unwrap();
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn list_catalog_entry_fields() {
        let data  = b"test_image_data".to_vec();
        let id    = compute_image_id(&data);
        let addr  = spawn_server(make_catalog(vec![data.clone()])).await;
        let mut w = plain_connect(addr).await;

        write_list_request_buffered(&mut w, 0).await.unwrap();
        w.flush().await.unwrap();

        // Skip "JTPL" + varint count
        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await.unwrap();
        let _count = read_varint_u32(w.get_mut()).await.unwrap();

        // Entry: ImageID (u64) + Flags (u8) + NameLen (u16) + Filename + Size (varint)
        let got_id    = w.get_mut().read_u64().await.unwrap();
        let got_flags = w.get_mut().read_u8().await.unwrap();
        let name_len  = w.get_mut().read_u16().await.unwrap() as usize;
        let mut name  = vec![0u8; name_len];
        w.get_mut().read_exact(&mut name).await.unwrap();
        let got_size  = read_varint_u32(w.get_mut()).await.unwrap();

        assert_eq!(got_id,   id);
        assert_eq!(file_type_from_flags(got_flags), 0); // PNG
        assert!(!name.is_empty(), "filename must be non-empty");
        assert_eq!(got_size, data.len() as u32);
    }

    #[tokio::test]
    async fn list_empty_catalog() {
        let addr  = spawn_server(make_catalog(vec![])).await;
        let mut w = plain_connect(addr).await;

        write_list_request_buffered(&mut w, 0).await.unwrap();
        w.flush().await.unwrap();

        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await.unwrap();
        assert_eq!(&hdr, RESPONSE_LIST);

        let count = read_varint_u32(w.get_mut()).await.unwrap();
        assert_eq!(count, 0, "empty catalog must report count=0");
    }

    // ── GET_BY_ID (§9.2 JTPD framing) ────────────────────────────────────────

    #[tokio::test]
    async fn get_by_id_jtpd_header_present() {
        let data  = b"image_bytes".to_vec();
        let id    = compute_image_id(&data);
        let addr  = spawn_server(make_catalog(vec![data.clone()])).await;
        let mut w = plain_connect(addr).await;

        write_get_request_buffered(&mut w, 0, &[id]).await.unwrap();
        w.flush().await.unwrap();

        // §9.2: response must start with JTPD.
        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await.unwrap();
        assert_eq!(&hdr, RESPONSE_GET_BY_ID, "GET_BY_ID must respond with JTPD header");
    }

    #[tokio::test]
    async fn get_by_id_returns_correct_m_count() {
        let data  = b"pixels".to_vec();
        let id    = compute_image_id(&data);
        let addr  = spawn_server(make_catalog(vec![data])).await;
        let mut w = plain_connect(addr).await;

        write_get_request_buffered(&mut w, 0, &[id]).await.unwrap();
        w.flush().await.unwrap();

        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await.unwrap();

        let m = w.get_mut().read_u8().await.unwrap();
        assert_eq!(m, 1, "server should return M=1 for one known ID");
    }

    #[tokio::test]
    async fn get_by_id_m_less_than_n_for_unknown_ids() {
        // Request 2 IDs; only 1 exists. M must be 1, not 2.
        let data       = b"real_image".to_vec();
        let known_id   = compute_image_id(&data);
        let unknown_id = 0xDEADBEEFu64;
        let addr       = spawn_server(make_catalog(vec![data])).await;
        let mut w      = plain_connect(addr).await;

        write_get_request_buffered(&mut w, 0, &[known_id, unknown_id]).await.unwrap();
        w.flush().await.unwrap();

        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await.unwrap();
        assert_eq!(&hdr, RESPONSE_GET_BY_ID);

        let m = w.get_mut().read_u8().await.unwrap();
        assert_eq!(m, 1, "unknown IDs must be silently skipped; M should be 1");
    }

    #[tokio::test]
    async fn get_by_id_image_packet_content() {
        let data   = b"raw_pixel_data".to_vec();
        let id     = compute_image_id(&data);
        let addr   = spawn_server(make_catalog(vec![data.clone()])).await;
        let mut w  = plain_connect(addr).await;

        write_get_request_buffered(&mut w, 0, &[id]).await.unwrap();
        w.flush().await.unwrap();

        // Skip JTPD + M.
        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await.unwrap();
        let _m = w.get_mut().read_u8().await.unwrap();

        // Image packet: Flags (u8) + Length (varint) + ImageID (u64) + Data
        let flags    = w.get_mut().read_u8().await.unwrap();
        let length   = read_varint_u32(w.get_mut()).await.unwrap();
        let got_id   = w.get_mut().read_u64().await.unwrap();
        let mut body = vec![0u8; length as usize];
        w.get_mut().read_exact(&mut body).await.unwrap();

        assert_eq!(got_id, id, "ImageID in packet must match requested ID");
        assert_eq!(body,   data, "image data must match what was stored");
        assert_eq!(file_type_from_flags(flags), 0); // PNG
    }

    #[tokio::test]
    async fn get_by_id_zero_count_returns_jtpd_m0() {
        // N=0: server should still emit JTPD with M=0.
        let addr  = spawn_server(make_catalog(vec![b"img".to_vec()])).await;
        let mut w = plain_connect(addr).await;

        write_get_request_buffered(&mut w, 0, &[]).await.unwrap();
        w.flush().await.unwrap();

        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await.unwrap();
        assert_eq!(&hdr, RESPONSE_GET_BY_ID);

        let m = w.get_mut().read_u8().await.unwrap();
        assert_eq!(m, 0, "N=0 request must produce M=0 response");
    }

    // ── BATCH (delta sync) ────────────────────────────────────────────────────

    #[tokio::test]
    async fn batch_returns_jtpb_header() {
        let addr  = spawn_server(make_catalog(vec![b"x".to_vec()])).await;
        let mut w = plain_connect(addr).await;

        write_batch_request_buffered(&mut w, 0, &[]).await.unwrap();
        w.flush().await.unwrap();

        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await.unwrap();
        assert_eq!(&hdr, RESPONSE_BATCH, "BATCH must respond with JTPB");
    }

    #[tokio::test]
    async fn batch_missing_count_excludes_have_ids() {
        let data_a = b"image_a".to_vec();
        let data_b = b"image_b".to_vec();
        let id_a   = compute_image_id(&data_a);
        let id_b   = compute_image_id(&data_b);
        let addr   = spawn_server(make_catalog(vec![data_a, data_b])).await;
        let mut w  = plain_connect(addr).await;

        // Client already has id_a → server should send only image_b.
        write_batch_request_buffered(&mut w, 0, &[id_a]).await.unwrap();
        w.flush().await.unwrap();

        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await.unwrap();
        assert_eq!(&hdr, RESPONSE_BATCH);

        let missing = read_varint_u32(w.get_mut()).await.unwrap();
        assert_eq!(missing, 1, "client has 1 of 2 images; missing count must be 1");

        // The single returned packet's ImageID must be id_b.
        let _flags  = w.get_mut().read_u8().await.unwrap();
        let length  = read_varint_u32(w.get_mut()).await.unwrap();
        let got_id  = w.get_mut().read_u64().await.unwrap();
        assert_eq!(got_id, id_b);

        let mut body = vec![0u8; length as usize];
        w.get_mut().read_exact(&mut body).await.unwrap();
        assert_eq!(body, b"image_b");
    }

    #[tokio::test]
    async fn batch_all_have_sends_zero_images() {
        let data = b"only_image".to_vec();
        let id   = compute_image_id(&data);
        let addr = spawn_server(make_catalog(vec![data])).await;
        let mut w = plain_connect(addr).await;

        write_batch_request_buffered(&mut w, 0, &[id]).await.unwrap();
        w.flush().await.unwrap();

        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await.unwrap();
        assert_eq!(&hdr, RESPONSE_BATCH);

        let missing = read_varint_u32(w.get_mut()).await.unwrap();
        assert_eq!(missing, 0, "client has all images; missing count must be 0");
    }

    // ── LIST_AND_GET ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn list_and_get_header_and_varint_count() {
        let images = vec![b"p".to_vec(), b"qq".to_vec()];
        let addr   = spawn_server(make_catalog(images)).await;
        let mut w  = plain_connect(addr).await;

        write_list_and_get_request_buffered(&mut w, 0).await.unwrap();
        w.flush().await.unwrap();

        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await.unwrap();
        assert_eq!(&hdr, RESPONSE_LIST_AND_GET, "LIST_AND_GET must respond with JTPG");

        // §9.5: count is varint(u32), not u16.
        let count = read_varint_u32(w.get_mut()).await.unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn list_and_get_delivers_all_images() {
        let data_a = b"img_a".to_vec();
        let data_b = b"img_b".to_vec();
        let ids: HashSet<ImageId> = [compute_image_id(&data_a), compute_image_id(&data_b)]
            .into_iter()
            .collect();

        let addr  = spawn_server(make_catalog(vec![data_a, data_b])).await;
        let mut w = plain_connect(addr).await;

        write_list_and_get_request_buffered(&mut w, 0).await.unwrap();
        w.flush().await.unwrap();

        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await.unwrap();
        let count = read_varint_u32(w.get_mut()).await.unwrap();

        let mut received = HashSet::new();
        for _ in 0..count {
            let _flags = w.get_mut().read_u8().await.unwrap();
            let len    = read_varint_u32(w.get_mut()).await.unwrap();
            let id     = w.get_mut().read_u64().await.unwrap();
            let mut body = vec![0u8; len as usize];
            w.get_mut().read_exact(&mut body).await.unwrap();
            received.insert(id);
        }

        assert_eq!(received, ids, "LIST_AND_GET must deliver all images");
    }

    // ── Keep-alive ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn keep_alive_allows_multiple_requests() {
        let data  = b"multi".to_vec();
        let id    = compute_image_id(&data);
        let addr  = spawn_server(make_catalog(vec![data])).await;
        let mut w = plain_connect(addr).await;

        // Request 1: LIST with keep-alive.
        write_list_request_buffered(&mut w, REQUEST_FLAG_KEEP_ALIVE).await.unwrap();
        w.flush().await.unwrap();

        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await.unwrap();
        assert_eq!(&hdr, RESPONSE_LIST);
        let count = read_varint_u32(w.get_mut()).await.unwrap();
        // Drain catalog entries.
        for _ in 0..count {
            let _id      = w.get_mut().read_u64().await.unwrap();
            let _flags   = w.get_mut().read_u8().await.unwrap();
            let name_len = w.get_mut().read_u16().await.unwrap() as usize;
            let mut nb   = vec![0u8; name_len];
            w.get_mut().read_exact(&mut nb).await.unwrap();
            let _size    = read_varint_u32(w.get_mut()).await.unwrap();
        }

        // Request 2 on the same connection: GET_BY_ID.
        write_get_request_buffered(&mut w, 0, &[id]).await.unwrap();
        w.flush().await.unwrap();

        let mut hdr2 = [0u8; 4];
        w.get_mut().read_exact(&mut hdr2).await.unwrap();
        assert_eq!(&hdr2, RESPONSE_GET_BY_ID, "second request must succeed on the kept-alive connection");
    }

    // ── ImageID verification ──────────────────────────────────────────────────

    #[test]
    fn image_id_verification_detects_corruption() {
        // Simulate what a client does: verify xxHash64(data) == id.
        let data      = b"genuine_data".to_vec();
        let id        = compute_image_id(&data);
        let corrupted = b"tampered_data".to_vec();

        assert_eq!(compute_image_id(&data), id, "known-good data must verify");
        assert_ne!(
            compute_image_id(&corrupted), id,
            "tampered data must fail ImageID verification"
        );
    }
}

// ============================================================================
// § 4  CANCEL
// ============================================================================

mod cancel {
    use super::*;

    #[tokio::test]
    async fn cancel_on_non_keepalive_returns_error() {
        // CANCEL is only valid on a keep-alive connection (§8.5).
        // Sending it without a prior keep-alive GET_BY_ID should yield JTPE
        // InvalidRequest.
        let addr  = spawn_server(make_catalog(vec![b"img".to_vec()])).await;
        let mut w = plain_connect(addr).await;

        // Send CANCEL directly (no keep-alive established).
        write_cancel_request_buffered(w.get_mut()).await.unwrap();
        w.flush().await.unwrap();

        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await.unwrap();
        assert_eq!(&hdr, RESPONSE_ERROR, "CANCEL without keep-alive must return JTPE");

        let (code, _) = jtp::protocol::read_error(w.get_mut()).await.unwrap();
        assert_eq!(code, ErrorCode::InvalidRequest);
    }

    #[tokio::test]
    async fn cancel_after_keepalive_returns_jtpc() {
        // Establish keep-alive with LIST, then immediately CANCEL.
        let addr  = spawn_server(make_catalog(vec![b"img".to_vec()])).await;
        let mut w = plain_connect(addr).await;

        // LIST with keep-alive to establish the connection as a keep-alive connection.
        write_list_request_buffered(&mut w, REQUEST_FLAG_KEEP_ALIVE).await.unwrap();
        w.flush().await.unwrap();

        // Drain LIST response.
        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await.unwrap();
        assert_eq!(&hdr, RESPONSE_LIST);
        let count = read_varint_u32(w.get_mut()).await.unwrap();
        for _ in 0..count {
            let _id      = w.get_mut().read_u64().await.unwrap();
            let _fl      = w.get_mut().read_u8().await.unwrap();
            let nl       = w.get_mut().read_u16().await.unwrap() as usize;
            let mut nb   = vec![0u8; nl];
            w.get_mut().read_exact(&mut nb).await.unwrap();
            let _sz      = read_varint_u32(w.get_mut()).await.unwrap();
        }

        // Now send CANCEL.
        write_cancel_request_buffered(w.get_mut()).await.unwrap();
        w.flush().await.unwrap();

        let mut ack = [0u8; 4];
        w.get_mut().read_exact(&mut ack).await.unwrap();
        assert_eq!(&ack, RESPONSE_CANCEL, "CANCEL on keep-alive connection must return JTPC");
    }
}

// ============================================================================
// § 5  WATCH
// ============================================================================

mod watch {
    use super::*;

    #[tokio::test]
    async fn watch_unsupported_when_no_tx() {
        // Server started without --watch → must return JTPE UnsupportedFeature.
        let addr  = spawn_server(make_catalog(vec![])).await;
        let mut w = plain_connect(addr).await;

        write_watch_request_buffered(w.get_mut()).await.unwrap();
        w.flush().await.unwrap();

        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await.unwrap();
        assert_eq!(&hdr, RESPONSE_ERROR, "WATCH without --watch must return JTPE");

        let (code, _) = jtp::protocol::read_error(w.get_mut()).await.unwrap();
        assert_eq!(code, ErrorCode::UnsupportedFeature);
    }

    #[tokio::test]
    async fn watch_receives_jtpw_event_then_cancel() {
        eprintln!("[TEST START] watch_receives_jtpw_event_then_cancel");
        // Start a server with a broadcast sender, send a WatchEvent manually,
        // and verify the client receives a JTPW frame.
        let (tx, _rx) = broadcast::channel::<WatchEvent>(16);
        let tx        = Arc::new(tx);
        eprintln!("[1/8] Created broadcast channel");
        
        let addr      = spawn_server_with_watch(make_catalog(vec![]), Some(Arc::clone(&tx))).await;
        eprintln!("[2/8] Spawned server at {}", addr);
        
        let mut w     = plain_connect(addr).await;
        eprintln!("[3/8] Connected to server");

        write_watch_request_buffered(w.get_mut()).await.unwrap();
        w.flush().await.unwrap();
        eprintln!("[4/8] Sent WATCH request");
        
        // Yield to allow the spawned server task to run and subscribe to the broadcast
        // channel before we send the event. Without this, the event is sent before
        // the server calls tx.subscribe(), so the event is lost.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        eprintln!("[4.5/8] Yielded to allow server to start");

        // Inject an event from outside (simulates the rescan task).
        let event = WatchEvent {
            id:       0x1234567890ABCDEFu64,
            flags:    flags_from_file_type(2), // WebP
            filename: "new_image.webp".to_string(),
            size:     2048,
        };
        tx.send(event.clone()).unwrap();
        eprintln!("[5/8] Sent WatchEvent to broadcast channel");

        // Client should receive JTPW.
        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await.unwrap();
        assert_eq!(&hdr, RESPONSE_WATCH, "server must push JTPW frame");
        eprintln!("[6/8] Received JTPW header");

        let got_id    = w.get_mut().read_u64().await.unwrap();
        let got_flags = w.get_mut().read_u8().await.unwrap();
        let name_len  = w.get_mut().read_u16().await.unwrap() as usize;
        let mut name  = vec![0u8; name_len];
        w.get_mut().read_exact(&mut name).await.unwrap();
        let got_name  = String::from_utf8(name).unwrap();
        let got_size  = read_varint_u32(w.get_mut()).await.unwrap();

        assert_eq!(got_id,    event.id);
        assert_eq!(got_flags, event.flags);
        assert_eq!(got_name,  event.filename);
        assert_eq!(got_size,  event.size);
        eprintln!("[7/8] Verified JTPW event details");

        // CANCEL the subscription.
        write_cancel_request_buffered(w.get_mut()).await.unwrap();
        w.flush().await.unwrap();
        eprintln!("[7.5/8] Sent CANCEL request");

        let mut ack = [0u8; 4];
        w.get_mut().read_exact(&mut ack).await.unwrap();
        assert_eq!(&ack, RESPONSE_CANCEL, "WATCH CANCEL must return JTPC");
        eprintln!("[8/8] Received JTPC ack");
        
        eprintln!("[TEST END] watch_receives_jtpw_event_then_cancel completed successfully");
    }
}

// ============================================================================
// § 6  Error paths
// ============================================================================

mod error_paths {
    use super::*;

    // ── Unknown request type → UnsupportedFeature (§12) ──────────────────────

    #[tokio::test]
    async fn unknown_reqtype_returns_unsupported_feature() {
        let addr  = spawn_server(make_catalog(vec![])).await;
        let mut w = plain_connect(addr).await;

        // ReqType 0xFF is not assigned.
        w.write_all(&[0xFF, 0x00]).await.unwrap();
        w.flush().await.unwrap();

        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await.unwrap();
        assert_eq!(&hdr, RESPONSE_ERROR);

        let (code, _) = jtp::protocol::read_error(w.get_mut()).await.unwrap();
        assert_eq!(
            code, ErrorCode::UnsupportedFeature,
            "unknown ReqType must yield UnsupportedFeature, not InvalidRequest"
        );
    }

    // ── Reserved flags → InvalidRequest ──────────────────────────────────────

    #[tokio::test]
    async fn reserved_request_flags_return_invalid_request() {
        let addr  = spawn_server(make_catalog(vec![])).await;
        let mut w = plain_connect(addr).await;

        // LIST with bit 1 set (reserved).
        w.write_all(&[REQUEST_LIST, 0b0000_0010]).await.unwrap();
        w.flush().await.unwrap();

        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await.unwrap();
        assert_eq!(&hdr, RESPONSE_ERROR);

        let (code, _) = jtp::protocol::read_error(w.get_mut()).await.unwrap();
        assert_eq!(code, ErrorCode::InvalidRequest);
    }

    #[tokio::test]
    async fn all_reserved_flags_set_returns_invalid_request() {
        let addr  = spawn_server(make_catalog(vec![])).await;
        let mut w = plain_connect(addr).await;

        w.write_all(&[REQUEST_GET_BY_ID, 0xFF]).await.unwrap();
        w.flush().await.unwrap();

        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await.unwrap();
        assert_eq!(&hdr, RESPONSE_ERROR);

        let (code, _) = jtp::protocol::read_error(w.get_mut()).await.unwrap();
        assert_eq!(code, ErrorCode::InvalidRequest);
    }

    // ── BATCH have_count > 1,000,000 → InvalidRequest (§8.3, §11) ────────────

    #[tokio::test]
    async fn batch_oversized_have_count_returns_invalid_request() {
        let addr  = spawn_server(make_catalog(vec![])).await;
        let mut w = plain_connect(addr).await;

        // Build a BATCH request with HaveCount = 1,000,001.
        let too_large: u32 = 1_000_001;
        let mut buf = vec![REQUEST_BATCH, 0x00]; // ReqType + Flags
        let mut varint = [0u8; 5];
        let n = encode_varint_to_buf(too_large, &mut varint);
        buf.extend_from_slice(&varint[..n]);
        // Do NOT write any ImageIDs — server should reject before reading them.

        w.write_all(&buf).await.unwrap();
        w.flush().await.unwrap();

        let mut hdr = [0u8; 4];
        w.get_mut().read_exact(&mut hdr).await.unwrap();
        assert_eq!(&hdr, RESPONSE_ERROR);

        let (code, _) = jtp::protocol::read_error(w.get_mut()).await.unwrap();
        assert_eq!(code, ErrorCode::InvalidRequest);
    }

    // ── BATCH exactly 1,000,000 is accepted ──────────────────────────────────

    #[tokio::test]
    async fn batch_at_limit_is_accepted() {
        // 1,000,000 is the limit; it must be accepted (and return JTPB with 0
        // missing, since no images exist in the catalog that the client lacks).
        let addr  = spawn_server(make_catalog(vec![b"one".to_vec()])).await;
        let mut w = plain_connect(addr).await;

        // Build HaveCount = 1,000,000 with 1,000,000 bogus IDs.
        // That's 8 MB of data — acceptable for an integration test.
        let at_limit: u32 = 1_000_000;
        let mut buf  = vec![REQUEST_BATCH, 0x00];
        let mut vb   = [0u8; 5];
        let n        = encode_varint_to_buf(at_limit, &mut vb);
        buf.extend_from_slice(&vb[..n]);
        for i in 0u32..at_limit {
            buf.extend_from_slice(&(i as u64).to_be_bytes());
        }
        w.write_all(&buf).await.unwrap();
        w.flush().await.unwrap();

        let mut hdr = [0u8; 4];
        // Give the server 10 s to process 8 MB.
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            w.get_mut().read_exact(&mut hdr),
        ).await;
        assert!(result.is_ok(), "server timed out processing 1,000,000-entry BATCH");
        assert_eq!(&hdr, RESPONSE_BATCH, "BATCH at limit must succeed with JTPB");
    }

    // ── Unexpected EOF ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn abrupt_disconnect_does_not_panic_server() {
        let addr  = spawn_server(make_catalog(vec![])).await;
        let mut w = plain_connect(addr).await;

        // Write only half a LIST request header, then drop the connection.
        w.write_all(&[REQUEST_LIST]).await.unwrap();
        w.flush().await.unwrap();
        drop(w);

        // Give the server task a moment to observe the EOF and exit cleanly.
        tokio::time::sleep(Duration::from_millis(50)).await;
        // If we reach here without panicking, the server handled it gracefully.
    }
}

// ============================================================================
// Trait impls needed for tests
// ============================================================================
//
// ImageCatalog requires a `new_empty()` constructor and the collision check
// inside `add_image()`. If `add_image` is not exposed publicly from the crate,
// add this to protocol.rs under `#[cfg(test)]`:
//
//     impl ImageCatalog {
//         pub fn new_empty() -> Self {
//             Self { images: HashMap::new(), cached_sorted: Arc::new(Vec::new()) }
//         }
//     }
//
// handle_requests is private; expose it under cfg(test) in server.rs:
//
//     #[cfg(test)]
//     pub use crate::handle_requests as test_handle_requests;
//
// Or, equivalently, re-export it from a test-only module in server.rs:
//
//     #[cfg(test)]
//     pub async fn test_handle_requests<S: AsyncReadExt + AsyncWriteExt + Unpin>(
//         stream: BufWriter<S>,
//         catalog: Arc<RwLock<ImageCatalog>>,
//         compression_threshold: f32,
//         keep_alive_timeout: Duration,
//         verbose: bool,
//         watch_tx: Option<Arc<broadcast::Sender<WatchEvent>>>,
//     ) {
//         handle_requests(stream, catalog, compression_threshold, keep_alive_timeout, verbose, watch_tx).await
//     }
//
// unicode_normalization must be in Cargo.toml (already added for protocol.rs):
//     unicode-normalization = "0.1"
//
// tempfile must be added as a dev-dependency:
//     [dev-dependencies]
//     tempfile = "3"

// ============================================================================
// Convenience re-export of the test handle for the server integration tests
// ============================================================================

#[cfg(test)]
async fn test_handle_requests<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    stream:                BufWriter<S>,
    catalog:               Arc<RwLock<ImageCatalog>>,
    compression_threshold: f32,
    keep_alive_timeout:    Duration,
    verbose:               bool,
    watch_tx:              Option<Arc<broadcast::Sender<WatchEvent>>>,
) {
    jtp::server::handle_requests(
        stream, catalog, compression_threshold, keep_alive_timeout, verbose, watch_tx,
    )
    .await
}

#[cfg(test)]
async fn spawn_server(catalog: ImageCatalog) -> SocketAddr {
    spawn_server_with_watch(catalog, None).await
}

#[cfg(test)]
async fn spawn_server_with_watch(
    catalog:  ImageCatalog,
    watch_tx: Option<Arc<broadcast::Sender<WatchEvent>>>,
) -> SocketAddr {
    let (listener, addr) = bind_ephemeral().await;
    let catalog = Arc::new(RwLock::new(catalog));
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = BufWriter::new(stream);
        // Use very short timeout for tests so they don't hang waiting for next request
        test_handle_requests(stream, catalog, 0.0, Duration::from_millis(5), false, watch_tx).await;
    });
    addr
}