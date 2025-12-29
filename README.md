# Jason Transfer Protocol (JTP)

**JTP** is a high-performance, secure image transfer protocol designed for fast
delivery of Jason images (or any images) over TCP with optional TLS encryption.

## **Getting Started**

### **Prerequisites**

- Rust 1.72+
- Cargo
- On Windows, `CMake` may be required to build TLS dependencies.

### **Building**

```bash
git clone https://github.com/punctuations/jtp.git
cd jtp
cargo build --release
```

- **Server binary:** `target/release/server.exe` (Windows) or `server`
  (Linux/macOS)
- **Client binary:** `target/release/client.exe` (Windows) or `client`
  (Linux/macOS)

### **Usage**

#### **Server**

1. Start the server:

```bash
cargo run --bin server
```

- Listens on `127.0.0.1:8443` by default.
- Serves images based on `ImageID` requests.

#### **Client**

1. Run the client:

```bash
cargo run --bin client
```

- Downloads requested images and saves them locally using the filename provided
  by the LIST catalog, e.g.:

```
jason1.jpg
jason2.png
```

- If the catalog does not provide a usable filename, it falls back to:

```
output_<image_id>.jpg
```

## **Protocol Outline**

**Purpose:** Secure, fast transfer of images using a simple, hash-based
request/response protocol.

### **1. Connection**

- TCP (optionally TLS-encrypted).
- Server listens on a configurable port (default `127.0.0.1:8443`).
- Client initiates connection and negotiates TLS if enabled.

### **2. ImageID Encoding**

- **Length:** 8 bytes (64-bit)
- **Generation:**

  1. Compute `xxHash64` of the image bytes (seed = 0).
  2. Use the resulting 64-bit value as the `ImageID`.

**Example (pseudo-code):**

```text
image_bytes = read_image("jason.jpg")
image_id_u64 = xxHash64(image_bytes, seed=0)
image_id_bytes = to_be_bytes(image_id_u64) // 8 bytes
```

- Client uses `ImageID` to request images.
- Server uses `ImageID` to identify and respond with the correct image.

### **3. Client Request Packet**

JTP supports three request types.

#### **3.1 LIST request**

| Field     | Size (bytes) | Description |
| --------- | ------------ | ----------- |
| `ReqType` | 1            | `1` = LIST  |

#### **3.2 GET_BY_ID request**

| Field     | Size (bytes) | Description                        |
| --------- | ------------ | ---------------------------------- |
| `ReqType` | 1            | `0` = GET_BY_ID                    |
| `Count`   | 1            | Number of images requested (`N`)   |
| `ImageID` | 8 × N        | Hash-based IDs of requested images |

#### **3.3 BATCH request (delta sync)**

Client sends the IDs it already has; server returns only the missing images.

| Field       | Size (bytes) | Description                              |
| ----------- | ------------ | ---------------------------------------- |
| `ReqType`   | 1            | `2` = BATCH                              |
| `HaveCount` | 1–5          | Varint-encoded count of `ImageID`s (`N`) |
| `ImageID`   | 8 × N        | Hash-based IDs the client already has    |

### **4. Server Response Packets**

#### **4.1 LIST response (catalog)**

| Field     | Size (bytes) | Description             |
| --------- | ------------ | ----------------------- |
| `Header`  | 4            | `"JTPL"`                |
| `Count`   | 2            | Number of entries (`N`) |
| `Entries` | Variable     | Repeated `N` times      |

Each entry:

| Field      | Size (bytes) | Description                                          |
| ---------- | ------------ | ---------------------------------------------------- |
| `ImageID`  | 8            | Image ID (u64, big-endian)                           |
| `Flags`    | 1            | bits 0..2=file type, bit3=compressed, bit4=encrypted |
| `NameLen`  | 2            | Length of filename in bytes                          |
| `Filename` | `NameLen`    | UTF-8 filename (basename)                            |
| `Size`     | 1–5          | Varint-encoded size of image data (u32)              |

#### **4.2 Image response (per image)**

| Field     | Size (bytes) | Description                                          |
| --------- | ------------ | ---------------------------------------------------- |
| `Flags`   | 1            | bits 0..2=file type, bit3=compressed, bit4=encrypted |
| `Length`  | 1–5          | Varint-encoded length of image data (u32)            |
| `ImageID` | 8            | Echoes requested `ImageID` (u64, big-endian)         |
| `Data`    | Variable     | Raw image bytes                                      |

#### **4.3 BATCH response (delta sync)**

| Field          | Size (bytes) | Description                          |
| -------------- | ------------ | ------------------------------------ |
| `Header`       | 4            | "JTPB"                               |
| `MissingCount` | 1–5          | Varint-encoded count of images (`M`) |
| `Images`       | Variable     | Repeated `M` times (image response)  |

## **Example Packets (Byte-by-Byte)**

Fixed-width integers are big-endian; sizes/lengths are unsigned LEB128 varints.
Hex dumps below are spaced by byte.

### **A) LIST request (client → server)**

Request: `ReqType = 1` (LIST)

```
01
```

| Offset | Bytes | Meaning              |
| ------ | ----- | -------------------- |
| 0      | `01`  | `ReqType = 1` (LIST) |

### **B) LIST response (server → client)**

Example: `Count = 1`, one entry for `jason1.jpg` with `Size = 0x00001234`.

```
4A 54 50 4C  00 01

AA BB CC DD EE FF 00 11  01  00 09
6A 61 73 6F 6E 31 2E 6A 70 67  B4 24
```

| Offset | Bytes                           | Meaning                               |
| ------ | ------------------------------- | ------------------------------------- |
| 0..3   | `4A 54 50 4C`                   | ASCII `"JTPL"` (LIST response header) |
| 4..5   | `00 01`                         | `Count = 1` (u16)                     |
| 6..13  | `AA .. 11`                      | `ImageID` (8 bytes)                   |
| 14     | `01`                            | `Flags` (file type=1 jpg)             |
| 15..16 | `00 09`                         | `NameLen = 9` (u16)                   |
| 17..25 | `6A 61 73 6F 6E 31 2E 6A 70 67` | UTF-8 `"jason1.jpg"`                  |
| 26..27 | `B4 24`                         | `Size = 0x00001234` (varint u32)      |

### **C) GET_BY_ID request (client → server)**

Example: request one image by ID.

```
00  01
AA BB CC DD EE FF 00 11
```

| Offset | Bytes      | Meaning                   |
| ------ | ---------- | ------------------------- |
| 0      | `00`       | `ReqType = 0` (GET_BY_ID) |
| 1      | `01`       | `Count = 1` (u8)          |
| 2..9   | `AA .. 11` | `ImageID` (8 bytes)       |

### **D) Image response (server → client, per image)**

Example: `jason1.jpg`, `Length = 4`, and `Data = DE AD BE EF`.

```
01 04
AA BB CC DD EE FF 00 11
DE AD BE EF
```

| Offset | Bytes         | Meaning                   |
| ------ | ------------- | ------------------------- |
| 0      | `01`          | `Flags` (file type=1 jpg) |
| 1      | `04`          | `Length = 4` (varint u32) |
| 2..9   | `AA .. 11`    | `ImageID` (8 bytes)       |
| 10..13 | `DE AD BE EF` | `Data` bytes              |

### **5. Security**

- TLS ensures confidentiality and integrity.
- `ImageID` prevents collisions and simplifies deduplication.
- Optional future enhancements: authentication, metadata, and compression.

### **6. Extensibility**

- Add new file types by extending the `Flags` bits 0..2 mapping.
- Include metadata (dimensions, compression/encryption parameters) in the
  header.
- Support streaming of very large images in chunks.

### **Flow Diagram**

```
Client                          Server
  |                                     |
  |   -------- LIST request -------->   |
  |  <-------- catalog (JTPL) ------    |
  |                                     |
  |  ---- GET_BY_ID request (IDs) -->   |
  |                                     |
  |<--- Flags+Len+ID+Data (per image) --|
  |         (repeated per image)        |
```

### **License**

GNU v3.0
