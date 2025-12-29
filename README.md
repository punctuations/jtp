# Jason Transfer Protocol (JTP)

**JTP** is a high-performance, secure image transfer protocol designed for fast
delivery of Jason images (or any images) over TCP with optional TLS encryption.

---

## **Getting Started**

### **Prerequisites**

- Rust 1.72+
- Cargo
- On Windows, `CMake` may be required to build TLS dependencies.

---

### **Building**

```bash
git clone https://github.com/yourusername/jtp.git
cd jtp
cargo build --release
```

- **Server binary:** `target/release/server.exe` (Windows) or `server`
  (Linux/macOS)
- **Client binary:** `target/release/client.exe` (Windows) or `client`
  (Linux/macOS)

---

### **Usage**

#### **Server**

1. Start the server:

```bash
cargo run --bin server
```

- Listens on `127.0.0.1:9999` by default.
- Serves images based on `ImageID` requests.

---

#### **Client**

1. Run the client:

```bash
cargo run --bin client
```

- Downloads requested images and saves them locally using the filename provided
  by the server, e.g.:

```
jason1.jpg
jason2.png
```

- If the server does not provide a filename, it falls back to:

```
output_<first_8_bytes_of_id>.jpg
```

---

## **Protocol Outline**

**Purpose:** Secure, fast transfer of images using a simple, hash-based
request/response protocol.

### **1. Connection**

- TCP (optionally TLS-encrypted).
- Server listens on a configurable port (default `127.0.0.1:9999`).
- Client initiates connection and negotiates TLS if enabled.

---

### **2. ImageID Encoding**

- **Length:** 16 bytes
- **Generation:**

  1. Compute SHA256 hash of the image bytes.
  2. Truncate the hash to the first 16 bytes.
  3. Use the truncated 16 bytes as the `ImageID`.

**Example (pseudo-code):**

```text
image_bytes = read_image("jason.jpg")
hash = SHA256(image_bytes)           // 32 bytes
image_id = hash[0..15]              // first 16 bytes
```

- Client uses `ImageID` to request images.
- Server uses `ImageID` to identify and respond with the correct image.

---

### **3. Client Request Packet**

JTP supports two request types.

#### **3.1 LIST request**

| Field     | Size (bytes) | Description |
| --------- | ------------ | ----------- |
| `ReqType` | 1            | `1` = LIST  |

#### **3.2 GET_BY_ID request**

| Field     | Size (bytes) | Description                        |
| --------- | ------------ | ---------------------------------- |
| `ReqType` | 1            | `0` = GET_BY_ID                    |
| `Count`   | 1            | Number of images requested (`N`)   |
| `ImageID` | 16 × N       | Hash-based IDs of requested images |

---

### **4. Server Response Packets**

#### **4.1 LIST response (catalog)**

| Field     | Size (bytes) | Description             |
| --------- | ------------ | ----------------------- |
| `Header`  | 4            | `"JTPL"`                |
| `Count`   | 2            | Number of entries (`N`) |
| `Entries` | Variable     | Repeated `N` times      |

Each entry:

| Field      | Size (bytes) | Description                                                  |
| ---------- | ------------ | ------------------------------------------------------------ |
| `ImageID`  | 16           | Image ID                                                     |
| `FileType` | 1            | Image type (`0=png, 1=jpg, 2=webp, 3=bmp, 4=gif, 255=other`) |
| `NameLen`  | 2            | Length of filename in bytes                                  |
| `Filename` | `NameLen`    | UTF-8 filename (basename)                                    |
| `Size`     | 4            | Big-endian size of image data                                |

#### **4.2 Image response (per image)**

| Field      | Size (bytes) | Description                                                  |
| ---------- | ------------ | ------------------------------------------------------------ |
| `Header`   | 4            | `"JTP1"` ASCII identifier                                    |
| `FileType` | 1            | Image type (`0=png, 1=jpg, 2=webp, 3=bmp, 4=gif, 255=other`) |
| `ImageID`  | 16           | Matches requested `ImageID`                                  |
| `NameLen`  | 2            | Length of filename in bytes                                  |
| `Filename` | `NameLen`    | UTF-8 encoded descriptive filename                           |
| `Length`   | 4            | Big-endian length of image data                              |
| `Data`     | Variable     | Raw image bytes                                              |

---

## **Example Packets (Byte-by-Byte)**

All integers are big-endian. Hex dumps below are spaced by byte.

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

AA BB CC DD EE FF 00 11 22 33 44 55 66 77 88 99  01  00 09
6A 61 73 6F 6E 31 2E 6A 70 67  00 00 12 34
```

| Offset | Bytes                           | Meaning                               |
| ------ | ------------------------------- | ------------------------------------- |
| 0..3   | `4A 54 50 4C`                   | ASCII `"JTPL"` (LIST response header) |
| 4..5   | `00 01`                         | `Count = 1` (u16)                     |
| 6..21  | `AA .. 99`                      | `ImageID` (16 bytes)                  |
| 22     | `01`                            | `FileType = 1` (jpg)                  |
| 23..24 | `00 09`                         | `NameLen = 9` (u16)                   |
| 25..33 | `6A 61 73 6F 6E 31 2E 6A 70 67` | UTF-8 `"jason1.jpg"`                  |
| 34..37 | `00 00 12 34`                   | `Size = 0x00001234` (u32)             |

### **C) GET_BY_ID request (client → server)**

Example: request one image by ID.

```
00  01
AA BB CC DD EE FF 00 11 22 33 44 55 66 77 88 99
```

| Offset | Bytes      | Meaning                   |
| ------ | ---------- | ------------------------- |
| 0      | `00`       | `ReqType = 0` (GET_BY_ID) |
| 1      | `01`       | `Count = 1` (u8)          |
| 2..17  | `AA .. 99` | `ImageID` (16 bytes)      |

### **D) Image response (server → client, per image)**

Example: `jason1.jpg`, `Length = 4`, and `Data = DE AD BE EF`.

```
4A 54 50 31  01
AA BB CC DD EE FF 00 11 22 33 44 55 66 77 88 99
00 09
6A 61 73 6F 6E 31 2E 6A 70 67
00 00 00 04
DE AD BE EF
```

| Offset | Bytes                           | Meaning                                |
| ------ | ------------------------------- | -------------------------------------- |
| 0..3   | `4A 54 50 31`                   | ASCII `"JTP1"` (image response header) |
| 4      | `01`                            | `FileType = 1` (jpg)                   |
| 5..20  | `AA .. 99`                      | `ImageID` (16 bytes)                   |
| 21..22 | `00 09`                         | `NameLen = 9` (u16)                    |
| 23..31 | `6A 61 73 6F 6E 31 2E 6A 70 67` | UTF-8 `"jason1.jpg"`                   |
| 32..35 | `00 00 00 04`                   | `Length = 4` (u32)                     |
| 36..39 | `DE AD BE EF`                   | `Data` bytes                           |

---

### **5. Security**

- TLS ensures confidentiality and integrity.
- `ImageID` prevents collisions and simplifies deduplication.
- Optional future enhancements: authentication, metadata, and compression.

---

### **6. Extensibility**

- Add new file types in `FileType` enum.
- Include metadata (dimensions, compression) in the header.
- Support streaming of very large images in chunks.

---

### **Flow Diagram**

```
Client                          Server
  |                                     |
  |   -------- LIST request -------->   |
  |  <-------- catalog (JTPL) ------    |
  |                                     |
  |  ---- GET_BY_ID request (IDs) -->   |
  |                                     |
  |<--- Header+FileType+ID+Name+Data ---|
  |         (repeated per image)        |
```

---

### **License**

MIT License — free to use and modify for personal or commercial projects.
