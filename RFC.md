# RFC: Jason Transfer Protocol (JTP)

**Status:** Draft

**Last updated:** 2025-12-29

## 1. Abstract

Jason Transfer Protocol (JTP) is a compact binary protocol for listing and
transferring images over a reliable byte stream. JTP is designed to be simple to
implement and efficient to parse. Images are addressed by a content-derived
64-bit identifier.

This document specifies the on-wire format of JTP. It does not specify any
particular implementation.

## 2. Conventions and Terminology

The key words **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, and **MAY**
are to be interpreted as described in RFC 2119.

- **Client**: initiates connections and sends requests.
- **Server**: accepts connections and sends responses.
- **ImageID**: 64-bit content identifier derived from the image bytes.
- **Varint**: unsigned LEB128 encoding of a 32-bit integer.

All multi-byte fixed-width integers in this document are **big-endian** unless
otherwise specified.

## 3. Overview

JTP is a request/response protocol.

A typical flow:

1. Client connects and sends a `LIST` request.
2. Server returns a catalog containing `(ImageID, flags, filename, size)`
   entries.
3. Client requests images either:
   - explicitly by ID (`GET_BY_ID`), or
   - via delta sync (`BATCH`), providing IDs it already has.
4. Server returns image packets containing `(flags, length, ImageID, data)`.

### 3.1 One request per connection

JTP deployments commonly use **one request per connection**: the client opens a
connection, sends exactly one request, receives the response bytes, then the
connection is closed.

Clients **SHOULD NOT** pipeline multiple JTP requests over a single connection
unless the server explicitly supports it.

## 4. Transport

JTP requires an ordered, reliable byte stream transport.

- The default transport is **TCP**.
- JTP may be wrapped in **TLS** to provide confidentiality and integrity.

### 4.1 TLS and ALPN

When TLS is used, servers **MAY** advertise an ALPN protocol identifier:

- `jtp/1`

Clients that support ALPN **SHOULD** offer `jtp/1`.

JTP itself does not define certificate distribution. Deployments may use
self-signed certificates, a local CA, or public PKI.

## 5. Data Types

### 5.1 `u8`, `u16`, `u32`, `u64`

Unsigned integers of the indicated width.

### 5.2 `varint(u32)`

`varint(u32)` uses **unsigned LEB128** encoding.

- Encodes values in the range `0..=0xFFFF_FFFF`.
- Uses **1 to 5 bytes**.
- Each byte stores 7 data bits; the high bit (`0x80`) is the continuation bit.

**Canonical encoding:** Implementations **SHOULD** use the minimal (canonical)
encoding (no unnecessary leading zero groups). Receivers **MAY** reject
non-canonical encodings.

### 5.3 UTF-8 strings

Filenames in the catalog are UTF-8 byte sequences. The protocol includes an
explicit byte length.

## 6. Identifiers

### 6.1 ImageID

An ImageID is a 64-bit value computed from the raw image file bytes:

- `ImageID = xxHash64(image_bytes, seed = 0)`

On the wire, ImageID is transmitted as `u64` big-endian.

**Textual representation:** When rendered as hex, the recommended representation
is the hex encoding of the 8 big-endian bytes.

## 7. Flags

JTP uses a one-byte `Flags` field with the following bit assignments:

- Bits `0..2` (mask `0b0000_0111`): **file type**
- Bit `3`: **compressed** (1 = compressed)
- Bit `4`: **encrypted** (1 = encrypted)
- Bits `5..7`: reserved (MUST be 0 unless specified by a future extension)

### 7.1 File type codes

The file type codes are:

- `0`: PNG
- `1`: JPEG (jpg/jpeg)
- `2`: WebP
- `3`: BMP
- `4`: GIF
- `5`: reserved
- `6`: reserved
- `7`: unknown/other

If the file type is not known, senders **SHOULD** use `7`.

### 7.2 Compression and encryption bits

This RFC reserves the compression and encryption bits for future extensions.

- If `compressed` or `encrypted` is set and the receiver does not support the
  corresponding feature, the receiver **SHOULD** fail the request/connection
  rather than misinterpreting bytes.

## 8. Requests

The first byte of every request is `ReqType (u8)`.

### 8.1 `LIST` request (`ReqType = 1`)

Client → Server:

| Field   | Type | Size | Description |
| ------- | ---- | ---- | ----------- |
| ReqType | u8   | 1    | `1`         |

No additional payload.

### 8.2 `GET_BY_ID` request (`ReqType = 0`)

Client → Server:

| Field   | Type | Size | Description                |
| ------- | ---- | ---- | -------------------------- |
| ReqType | u8   | 1    | `0`                        |
| Count   | u8   | 1    | Number of IDs (`N`)        |
| ImageID | u64  | 8×N  | Requested IDs (big-endian) |

Semantics:

- `N` may be zero.
- Servers **MAY** ignore unknown IDs.

**Response framing note:** The `GET_BY_ID` response has no explicit top-level
count. Clients **SHOULD** treat the response as a stream of image packets until
the connection closes.

### 8.3 `BATCH` request (delta sync) (`ReqType = 2`)

`BATCH` is used to download “missing” images.

Client → Server:

| Field     | Type        | Size | Description                  |
| --------- | ----------- | ---- | ---------------------------- |
| ReqType   | u8          | 1    | `2`                          |
| HaveCount | varint(u32) | 1–5  | Number of IDs provided (`N`) |
| ImageID   | u64         | 8×N  | IDs the client already has   |

Semantics:

- Server compares provided IDs against its catalog.
- Server responds with only the images the client does not have.

## 9. Responses

### 9.1 `LIST` response (catalog)

Server → Client:

| Field   | Type  | Size | Description             |
| ------- | ----- | ---- | ----------------------- |
| Header  | bytes | 4    | ASCII `"JTPL"`          |
| Count   | u16   | 2    | Number of entries (`N`) |
| Entries | —     | var  | Repeated `N` times      |

Each entry:

| Field    | Type        | Size    | Description                 |
| -------- | ----------- | ------- | --------------------------- |
| ImageID  | u64         | 8       | Image ID (big-endian)       |
| Flags    | u8          | 1       | File type + feature flags   |
| NameLen  | u16         | 2       | Filename length in bytes    |
| Filename | bytes       | NameLen | UTF-8 basename              |
| Size     | varint(u32) | 1–5     | Size of image data in bytes |

Notes:

- `Size` is the number of data bytes that will appear in an image packet for
  that ImageID.
- Filenames are informational; clients **SHOULD NOT** trust path components.

### 9.2 Image packet

Image packets are used by multiple responses (e.g., `GET_BY_ID`, `BATCH`).

Server → Client:

| Field   | Type        | Size   | Description               |
| ------- | ----------- | ------ | ------------------------- |
| Flags   | u8          | 1      | File type + feature flags |
| Length  | varint(u32) | 1–5    | Data length in bytes      |
| ImageID | u64         | 8      | Image ID (big-endian)     |
| Data    | bytes       | Length | Raw image bytes           |

Receivers **SHOULD** validate:

- `ImageID == xxHash64(Data, seed=0)`

If validation fails, receivers **SHOULD** treat the data as corrupt.

### 9.3 `BATCH` response

Server → Client:

| Field        | Type        | Size | Description                       |
| ------------ | ----------- | ---- | --------------------------------- |
| Header       | bytes       | 4    | ASCII `"JTPB"`                    |
| MissingCount | varint(u32) | 1–5  | Number of missing images (`M`)    |
| Images       | —           | var  | Repeated `M` times (image packet) |

The client reads exactly `M` image packets.

## 10. Error Handling

JTP does not define a structured error frame.

Servers may signal errors by:

- closing the connection
- terminating the TLS session

Clients should treat unexpected EOF, invalid headers, unsupported flags, or
decoding errors as request failure.

## 11. Limits and Resource Considerations

Receivers should defend against resource exhaustion:

- `varint(u32)` values that imply huge allocations
- oversized `NameLen`
- large `Count/HaveCount`

Because `Length` and `Size` are `u32`, the maximum single image size supported
by this framing is `4,294,967,295` bytes (4 GiB − 1). Implementations **MAY**
impose lower limits.

## 12. Extensibility

JTP is designed to evolve by adding new request types and interpreting reserved
bits.

- Reserved `ReqType` values may be defined by future RFCs.
- Reserved `Flags` bits (`5..7`) must remain 0 unless specified.
- The compression and encryption flags reserve space for future
  negotiation/parameterization.

A future versioning scheme may be introduced via:

- a new ALPN token (e.g., `jtp/2`),
- a new request type for capability negotiation, or
- explicit magic/version fields.

## 13. Security Considerations

- Use TLS to prevent passive observation and active tampering.
- ImageID is content-derived and can be used to validate integrity, but it is
  not a cryptographic MAC.
- Servers should validate and cap counts/sizes to mitigate denial-of-service.

## 14. Appendix: Varint (unsigned LEB128) example

`0x0000_1234` (4660) encodes as:

- `0xB4 0x24`

Explanation:

- `0x1234` in binary is split into 7-bit groups from least significant to most.
- Continuation bit is set on all but the final byte.
