# RFC: Jason Transfer Protocol (JTP)

**Status:** Internet-Draft

**Last updated:** 2026-04-03

## 1. Abstract

Jason Transfer Protocol (JTP) is a compact binary protocol for listing and
transferring images over a reliable ordered byte stream. JTP is designed to be
simple to implement and efficient to parse. Images are addressed by a
content-derived 64-bit identifier computed using xxHash64. The protocol supports
catalog enumeration, point retrieval by identifier, delta synchronization,
connection reuse via a keep-alive mechanism, in-stream cancellation, and
server-push change notification. Transport security may be provided by TLS with
an ALPN protocol identifier of `jtp/1`.

This document specifies the on-wire format of JTP version 1. It does not specify
any particular implementation.

---

## 2. Conventions and Terminology

The key words **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, and **MAY**
are to be interpreted as described in BCP 14 (RFC 2119, RFC 8174).

- **Client**: initiates connections and sends requests.
- **Server**: accepts connections and sends responses.
- **ImageID**: 64-bit content identifier derived from the image bytes.
- **Varint**: unsigned LEB128 encoding of a 32-bit integer.
- **Catalog**: the set of `(ImageID, flags, filename, size)` tuples returned by
  a LIST response.
- **Image Packet**: the wire encoding of a single image: flags, length, ImageID,
  and data.
- **Delta Sync**: the BATCH operation in which a client sends its known ImageIDs
  and the server returns only the images the client does not have.

All multi-byte fixed-width integers in this document are **big-endian** unless
otherwise specified.

---

## 3. Overview

JTP is a request/response protocol.

A typical flow:

1. Client connects and sends a `LIST` request.
2. Server returns a catalog containing `(ImageID, flags, filename, size)`
   entries.
3. Client requests images either:
   - explicitly by ID (`GET_BY_ID`), or
   - via delta sync (`BATCH`), providing IDs it already has, or
   - via combined operation (`LIST_AND_GET`) for a single round-trip.
4. Server returns image packets containing `(flags, length, ImageID, data)`.

### 3.1 Connection Reuse (Keep-Alive)

JTP supports connection reuse through a **keep-alive** mechanism. When enabled,
multiple requests can be sent over a single connection, avoiding repeated TLS
handshakes.

- If the `keep-alive` flag is set in a request, the server **SHOULD** keep the
  connection open after sending the response and await the next request.
- If the flag is not set, the server **SHOULD** close the connection after the
  response.
- Servers **MAY** implement idle timeouts to close stale keep-alive connections.
- Clients **SHOULD NOT** assume keep-alive is supported; they **MUST** handle
  server-initiated connection closes gracefully.

Legacy deployments may use **one request per connection**: the client opens a
connection, sends exactly one request, receives the response bytes, then the
connection is closed.

---

## 4. Transport

JTP requires an ordered, reliable byte stream transport.

- The default transport is **TCP**.
- JTP may be wrapped in **TLS** (RFC 8446) to provide confidentiality and
  integrity.

### 4.1 TLS and ALPN

When TLS is used, servers **MAY** advertise an ALPN protocol identifier:

- `jtp/1`

Clients that support ALPN **SHOULD** offer `jtp/1`.

JTP itself does not define certificate distribution. Deployments may use
self-signed certificates, a local CA, or public PKI. Clients **SHOULD** validate
the server certificate against a trusted trust anchor.

---

## 5. Data Types

### 5.1 `u8`, `u16`, `u32`, `u64`

Unsigned integers of the indicated width, big-endian.

### 5.2 `varint(u32)`

`varint(u32)` uses **unsigned LEB128** encoding.

- Encodes values in the range `0..=0xFFFF_FFFF`.
- Uses **1 to 5 bytes**.
- Each byte stores 7 data bits; the high bit (`0x80`) is the continuation bit.

**Canonical encoding:** Implementations **SHOULD** use the minimal (canonical)
encoding (no unnecessary leading zero groups). Receivers **MAY** reject
non-canonical encodings.

### 5.3 UTF-8 Strings

Filenames in the catalog are UTF-8 byte sequences. The protocol includes an
explicit byte length; no null terminator is used. Receivers **SHOULD** validate
that filename bytes constitute well-formed UTF-8.

**Unicode Normalization:** Implementations **SHOULD** normalize filenames to NFC
(Canonical Decomposition followed by Canonical Composition) before transmission.
Receivers **SHOULD** apply NFC normalization upon receipt before using filenames
for display or comparison. This avoids interoperability issues between platforms
that apply different normalization forms (e.g. macOS uses NFD, Linux typically
uses NFC).

---

## 6. Identifiers

### 6.1 ImageID

An ImageID is a 64-bit value computed from the raw image file bytes:

```
ImageID = xxHash64(image_bytes, seed = 0)
```

On the wire, ImageID is transmitted as `u64` big-endian.

**Textual representation:** When rendered as hex, the recommended representation
is the hex encoding of the 8 big-endian bytes.

ImageID provides content integrity verification but is **not** a cryptographic
MAC. An adversary with the ability to modify transmitted data can also forge an
ImageID. Cryptographic integrity requires TLS or an equivalent mechanism.

### 6.2 ImageID Collision Handling

Because xxHash64 is not a cryptographic hash function, the probability of a
natural collision over typical image sets is negligible. However,
implementations **MUST** define behaviour for the case where two distinct image
files produce the same ImageID:

- Servers **MUST NOT** serve two distinct images under the same ImageID. If a
  new image would collide with an existing entry, the server **SHOULD** reject
  the new image and log the collision for operator review.
- Clients that receive an image whose xxHash64 does not match its ImageID
  **MUST** treat the packet as corrupt and **SHOULD** either request the image
  again or report the discrepancy to the operator.

Note: a malicious server can intentionally construct an ImageID collision. This
is a further reason why TLS and certificate validation are **RECOMMENDED**.

---

## 7. Flags

JTP uses a one-byte `Flags` field with the following bit assignments:

| Bits | Mask          | Name       | Description                                      |
| ---- | ------------- | ---------- | ------------------------------------------------ |
| 0..2 | `0b0000_0111` | FileType   | Image file type code (see §7.1)                  |
| 3    | `0b0000_1000` | Compressed | 1 = Zstd compressed                              |
| 4    | `0b0001_0000` | Encrypted  | Reserved for future use; MUST be 0               |
| 5..7 | `0b1110_0000` | (reserved) | MUST be 0 unless specified by a future extension |

### 7.1 File Type Codes

| Code | Format          |
| ---- | --------------- |
| 0    | PNG             |
| 1    | JPEG            |
| 2    | WebP            |
| 3    | BMP             |
| 4    | GIF             |
| 5    | Reserved        |
| 6    | Reserved        |
| 7    | Unknown / Other |

If the file type is not known, senders **SHOULD** use `7`.

### 7.2 Compression (Bit 3)

When bit 3 is set, the image data is Zstd compressed (RFC 8878). Receivers
**MUST** decompress before use. Integrity verification via xxHash64 **MUST** be
performed against the **decompressed** data.

If a receiver does not support Zstd and the Compressed bit is set, the receiver
**SHOULD** fail the request rather than misinterpreting bytes.

### 7.3 Encryption (Bit 4)

Bit 4 is reserved for future encryption support. Senders **MUST** set this bit
to 0. Receivers that encounter this bit set **SHOULD** treat the packet as an
error.

---

## 8. Requests

The first byte of every request is `ReqType (u8)`. The second byte is
`RequestFlags (u8)`.

### 8.0 Request Flags

| Bit | Name       | Description                             |
| --- | ---------- | --------------------------------------- |
| 0   | keep-alive | 1 = keep connection open after response |
| 1–7 | reserved   | MUST be 0 unless specified by extension |

Servers **MUST** reject requests with reserved bits set by closing the
connection or sending an ERROR response.

### 8.1 `LIST` Request (`ReqType = 1`)

Client → Server:

| Field        | Type | Size | Description                |
| ------------ | ---- | ---- | -------------------------- |
| ReqType      | u8   | 1    | `1`                        |
| RequestFlags | u8   | 1    | Flags (bit 0 = keep-alive) |

No additional payload.

### 8.2 `GET_BY_ID` Request (`ReqType = 0`)

Client → Server:

| Field           | Type | Size | Description                |
| --------------- | ---- | ---- | -------------------------- |
| ReqType         | u8   | 1    | `0`                        |
| RequestFlags    | u8   | 1    | Flags (bit 0 = keep-alive) |
| Count           | u8   | 1    | Number of IDs (`N`)        |
| ImageID[0..N-1] | u64  | 8×N  | Requested IDs (big-endian) |

Semantics:

- `N` may be zero.
- `N` **MUST NOT** exceed 255.
- Servers **MAY** ignore unknown IDs.

The server responds with a GET_BY_ID response (§9.2).

### 8.3 `BATCH` Request (delta sync) (`ReqType = 2`)

`BATCH` is used to download "missing" images.

Client → Server:

| Field           | Type        | Size | Description                  |
| --------------- | ----------- | ---- | ---------------------------- |
| ReqType         | u8          | 1    | `2`                          |
| RequestFlags    | u8          | 1    | Flags (bit 0 = keep-alive)   |
| HaveCount       | varint(u32) | 1–5  | Number of IDs provided (`N`) |
| ImageID[0..N-1] | u64         | 8×N  | IDs the client already has   |

Semantics:

- Server responds with only the images the client does not have.
- Servers **SHOULD** reject BATCH requests with `HaveCount` exceeding 1,000,000.

### 8.4 `LIST_AND_GET` Request (`ReqType = 5`)

Combined catalog listing and image transfer in a single round-trip.

Client → Server:

| Field        | Type | Size | Description                |
| ------------ | ---- | ---- | -------------------------- |
| ReqType      | u8   | 1    | `5`                        |
| RequestFlags | u8   | 1    | Flags (bit 0 = keep-alive) |

No additional payload. Server responds with all available images.

### 8.5 `CANCEL` Request (`ReqType = 3`)

`CANCEL` asks the server to abort the current in-progress response as soon as
possible. It is only meaningful on a keep-alive connection where a response
stream has not yet been fully consumed by the client.

Client → Server:

| Field        | Type | Size | Description |
| ------------ | ---- | ---- | ----------- |
| ReqType      | u8   | 1    | `3`         |
| RequestFlags | u8   | 1    | MUST be 0   |

Semantics:

- Upon receiving CANCEL, the server **SHOULD** stop transmitting at the next
  packet boundary and discard remaining queued data.
- The server **MUST NOT** close the connection in response to CANCEL on a
  keep-alive connection. The connection **MUST** remain open and ready for the
  next request.
- The server **SHOULD** acknowledge cancellation with a CANCEL acknowledgement
  response (§9.6) before awaiting the next request.
- CANCEL is only valid on a keep-alive connection. Servers **MUST** respond with
  an InvalidRequest ERROR if CANCEL is received on a non-keep-alive connection.
- Clients **MUST** be prepared to receive and discard image packets already
  in-flight before the server processed the CANCEL.

### 8.6 `WATCH` Request (`ReqType = 4`)

`WATCH` subscribes the client to server-push catalog notifications. The server
holds the connection open and streams a WATCH event frame each time a new image
becomes available, eliminating the need for the client to poll.

Client → Server:

| Field        | Type | Size | Description                        |
| ------------ | ---- | ---- | ---------------------------------- |
| ReqType      | u8   | 1    | `4`                                |
| RequestFlags | u8   | 1    | MUST be 0 (keep-alive is implicit) |

Semantics:

- A WATCH subscription is implicitly a keep-alive connection; the server **MUST
  NOT** close the connection after sending the first event.
- The server **MUST** send a WATCH event frame (§9.7) for each new image added
  to its catalog after the WATCH request is received. Images already present at
  the time of the request are **NOT** included.
- The subscription remains active until the client sends a CANCEL request or
  closes the connection.
- Servers that do not support WATCH **MUST** respond with an UnsupportedFeature
  ERROR.

---

## 9. Responses

Each response type begins with a four-octet ASCII magic header.

### 9.1 `LIST` Response (catalog)

Server → Client:

| Field         | Type        | Size | Description             |
| ------------- | ----------- | ---- | ----------------------- |
| Header        | bytes       | 4    | ASCII `"JTPL"`          |
| Count         | varint(u32) | 1–5  | Number of entries (`N`) |
| Entry[0..N-1] | —           | var  | Repeated `N` times      |

Each entry:

| Field    | Type        | Size    | Description               |
| -------- | ----------- | ------- | ------------------------- |
| ImageID  | u64         | 8       | Image ID (big-endian)     |
| Flags    | u8          | 1       | File type + feature flags |
| NameLen  | u16         | 2       | Filename length in bytes  |
| Filename | bytes       | NameLen | UTF-8 basename            |
| Size     | varint(u32) | 1–5     | Size of image data        |

Notes:

- `Size` is the number of data bytes in the corresponding image packet.
- Filenames are informational; clients **SHOULD NOT** trust path components.

### 9.2 `GET_BY_ID` Response

Server → Client:

| Field          | Type  | Size | Description                            |
| -------------- | ----- | ---- | -------------------------------------- |
| Header         | bytes | 4    | ASCII `"JTPD"`                         |
| Count          | u8    | 1    | Number of image packets returned (`M`) |
| Packet[0..M-1] | —     | var  | `M` image packets (§9.3)               |

`M` MAY be less than the `N` requested if the server does not recognise some of
the provided ImageIDs. Clients **MUST** read exactly `M` image packets. If the
keep-alive flag was set, the connection remains open after the last packet.

### 9.3 Image Packet

Image packets are used by `GET_BY_ID`, `BATCH`, and `LIST_AND_GET` responses.

Server → Client:

| Field   | Type        | Size   | Description               |
| ------- | ----------- | ------ | ------------------------- |
| Flags   | u8          | 1      | File type + feature flags |
| Length  | varint(u32) | 1–5    | Data length in bytes      |
| ImageID | u64         | 8      | Image ID (big-endian)     |
| Data    | bytes       | Length | Raw image bytes           |

Receivers **SHOULD** validate after decompression (if compressed):

```
ImageID == xxHash64(Data, seed = 0)
```

If validation fails, receivers **SHOULD** treat the data as corrupt.

### 9.4 `BATCH` Response

Server → Client:

| Field          | Type        | Size | Description                       |
| -------------- | ----------- | ---- | --------------------------------- |
| Header         | bytes       | 4    | ASCII `"JTPB"`                    |
| MissingCount   | varint(u32) | 1–5  | Number of missing images (`M`)    |
| Packet[0..M-1] | —           | var  | Repeated `M` times (image packet) |

The client reads exactly `M` image packets.

### 9.5 `LIST_AND_GET` Response

Server → Client:

| Field          | Type        | Size | Description                       |
| -------------- | ----------- | ---- | --------------------------------- |
| Header         | bytes       | 4    | ASCII `"JTPG"`                    |
| Count          | varint(u32) | 1–5  | Number of images (`N`)            |
| Packet[0..N-1] | —           | var  | Repeated `N` times (image packet) |

The client reads exactly `N` image packets.

### 9.6 `CANCEL` Acknowledgement Response

Server → Client:

| Field  | Type  | Size | Description    |
| ------ | ----- | ---- | -------------- |
| Header | bytes | 4    | ASCII `"JTPC"` |

A fixed four-octet frame. Upon receiving it, the client knows the server has
finished transmitting the cancelled response and is ready for the next request.

### 9.7 `WATCH` Event Response

Server → Client:

| Field    | Type        | Size    | Description               |
| -------- | ----------- | ------- | ------------------------- |
| Header   | bytes       | 4       | ASCII `"JTPW"`            |
| ImageID  | u64         | 8       | Image ID (big-endian)     |
| Flags    | u8          | 1       | File type + feature flags |
| NameLen  | u16         | 2       | Filename length in bytes  |
| Filename | bytes       | NameLen | UTF-8 basename            |
| Size     | varint(u32) | 1–5     | Size of image data        |

A WATCH event frame carries only catalog metadata, not the image data itself.
Upon receiving a WATCH event, the client MAY retrieve the image via a subsequent
GET_BY_ID request on a separate connection, or by sending CANCEL followed by
GET_BY_ID on the same keep-alive connection.

---

## 10. Error Handling

### 10.1 Structured ERROR Response

Server → Client:

| Field      | Type  | Size       | Description             |
| ---------- | ----- | ---------- | ----------------------- |
| Header     | bytes | 4          | ASCII `"JTPE"`          |
| ErrorCode  | u8    | 1          | Error code (see below)  |
| MessageLen | u16   | 2          | Length of message       |
| Message    | bytes | MessageLen | UTF-8 error description |

**Error Codes:**

| Code | Name               | Description                     |
| ---- | ------------------ | ------------------------------- |
| 1    | NotFound           | Requested resource not found    |
| 2    | InvalidRequest     | Malformed or invalid request    |
| 3    | ServerError        | Internal server error           |
| 4    | UnsupportedFeature | Feature not supported by server |
| 5    | RateLimited        | Request rate limit exceeded     |

### 10.2 Legacy Error Handling

Servers may also signal errors by closing the connection or terminating the TLS
session. Clients should treat the following as request failure:

- Unexpected EOF before the response has been fully received.
- A response magic header that does not match any known value (`"JTPL"`,
  `"JTPD"`, `"JTPB"`, `"JTPG"`, `"JTPC"`, `"JTPW"`, `"JTPE"`).
- A reserved Flags or RequestFlags bit set to 1.
- A varint that is non-canonical or encodes a value exceeding `0xFFFFFFFF`.
- Any other decoding error.

---

## 11. Limits and Resource Considerations

Receivers should defend against resource exhaustion:

- `varint(u32)` values that imply huge allocations.
- Oversized `NameLen`.
- Large `Count` / `HaveCount`.

The maximum single image size supported by this framing is `4,294,967,295` bytes
(4 GiB - 1). Implementations **MAY** impose lower limits.

Servers **SHOULD** reject BATCH requests with `HaveCount` exceeding 1,000,000.

---

## 12. Extensibility

JTP is designed to evolve by adding new request types and interpreting reserved
bits.

- Unassigned `ReqType` values (currently values 6–255) are reserved. Future
  documents **MAY** define their semantics. Servers receiving an unrecognized
  `ReqType` **SHOULD** respond with an UnsupportedFeature ERROR and close the
  connection.
- Reserved `Flags` bits (`5..7`) and reserved `RequestFlags` bits (`1..7`)
  **MUST** remain 0 unless specified.
- The `Encrypted` flag (bit 4) is reserved for a future encryption layer.

A future versioning scheme may be introduced via:

- A new ALPN token (e.g. `jtp/2`).
- A new request type for capability negotiation.
- Explicit magic/version fields in a revised framing layer.

### 12.1 Version Identification on Plain TCP

When TLS is not used, ALPN is unavailable and there is no mechanism for a
receiver to identify the protocol version. A future revision of this
specification **MAY** define a fixed-length connection preface transmitted by
the client immediately upon opening a connection, before the first request:

```
+--------+---------+
| Magic  | Version |
| 3 bytes|  u8     |
+--------+---------+

Magic:   ASCII "JTP" (0x4A 0x54 0x50)
Version: 0x01 for this specification
```

Servers receiving an unrecognised Magic or Version value **SHOULD** close the
connection immediately. This mechanism is **NOT** defined for version 1; version
1 implementations on plain TCP **MUST NOT** send this preface.

---

## 13. Security Considerations

- **Transport**: Use TLS to prevent passive observation and active tampering.
  Without TLS, image content and ImageIDs are transmitted in the clear.
- **Content Integrity**: ImageID is content-derived and can detect accidental
  corruption, but it is not a cryptographic MAC. Use TLS for cryptographic
  integrity.
- **Denial of Service**: Servers should validate and cap counts/sizes before
  allocating memory or performing I/O. Enforce keep-alive idle timeouts and rate
  limiting.
- **Filename Safety**: Clients **MUST NOT** use catalog filenames as filesystem
  paths without sanitizing them. Strip path separators and `..` sequences to
  prevent path traversal.
- **Certificate Validation**: When TLS is used, clients **SHOULD** validate the
  server certificate against a trusted trust anchor.

---

## 14. Request / Response Summary

| ReqType | Name         | Response Header | Description                          |
| ------- | ------------ | --------------- | ------------------------------------ |
| 0       | GET_BY_ID    | `JTPD`          | Retrieve images by ID                |
| 1       | LIST         | `JTPL`          | Get catalog of available images      |
| 2       | BATCH        | `JTPB`          | Delta sync (send IDs you have)       |
| 3       | CANCEL       | `JTPC`          | Abort current in-progress response   |
| 4       | WATCH        | `JTPW` (stream) | Subscribe to new-image notifications |
| 5       | LIST_AND_GET | `JTPG`          | Combined catalog + all images        |
| —       | ERROR        | `JTPE`          | Structured error response            |

---

## Appendix A: Varint (unsigned LEB128) Example

`0x0000_1234` (4660) encodes as `0xB4 0x24`.

Derivation:

- `4660` in binary: `0001 0010 0011 0100`
- Split into 7-bit groups (LSB first): `011 0100` (0x34), `010 0100` (0x24)
- Set continuation bit on all but the last byte: `0xB4`, `0x24`

Decoding: `36 × 128 + 52 = 4660`.

---

## Appendix B: Wire Format Summary

### Requests

```
LIST (ReqType = 1):
  +----------+----------+
  | ReqType  | ReqFlags |
  |  (0x01)  |  (u8)    |
  +----------+----------+

GET_BY_ID (ReqType = 0):
  +----------+----------+-------+-------- - - --------+
  | ReqType  | ReqFlags | Count |  ImageID[0..N-1]    |
  |  (0x00)  |  (u8)    | (u8)  |  N × u64 big-endian |
  +----------+----------+-------+-------- - - --------+

BATCH (ReqType = 2):
  +----------+----------+-------------+-------- - - --------+
  | ReqType  | ReqFlags |  HaveCount  |  ImageID[0..N-1]    |
  |  (0x02)  |  (u8)    | varint(u32) |  N × u64 big-endian |
  +----------+----------+-------------+-------- - - --------+

LIST_AND_GET (ReqType = 5):
  +----------+----------+
  | ReqType  | ReqFlags |
  |  (0x05)  |  (u8)    |
  +----------+----------+

CANCEL (ReqType = 3):
  +----------+----------+
  | ReqType  | ReqFlags |
  |  (0x03)  | (0x00)   |
  +----------+----------+

WATCH (ReqType = 4):
  +----------+----------+
  | ReqType  | ReqFlags |
  |  (0x04)  | (0x00)   |
  +----------+----------+
```

### Responses

```
LIST response ("JTPL"):
  +--------+-------------+------ - - ------+
  | "JTPL" |    Count    |     Entries     |
  | 4 bytes| varint(u32) | N × entry (var) |
  +--------+-------------+------ - - ------+

  Catalog entry:
  +---------+-------+---------+----------+------+
  | ImageID | Flags | NameLen | Filename | Size |
  | (u64)   | (u8)  | (u16)   | NameLen  | var  |
  +---------+-------+---------+----------+------+

GET_BY_ID response ("JTPD"):
  +--------+-------+------ - - ------+
  | "JTPD" | Count |    Images       |
  | 4 bytes| (u8)  | M × image pkt   |
  +--------+-------+------ - - ------+

Image packet (used in GET_BY_ID, BATCH, LIST_AND_GET responses):
  +-------+--------+---------+------ - ------+
  | Flags | Length | ImageID |     Data       |
  | (u8)  | (var)  | (u64)   | Length bytes   |
  +-------+--------+---------+------ - ------+

BATCH response ("JTPB"):
  +--------+--------------+------ - - ------+
  | "JTPB" | MissingCount |    Images       |
  | 4 bytes| varint(u32)  | M × image pkt   |
  +--------+--------------+------ - - ------+

LIST_AND_GET response ("JTPG"):
  +--------+-------------+------ - - ------+
  | "JTPG" |    Count    |    Images       |
  | 4 bytes| varint(u32) | N × image pkt   |
  +--------+-------------+------ - - ------+

CANCEL acknowledgement ("JTPC"):
  +--------+
  | "JTPC" |
  | 4 bytes|
  +--------+

WATCH event ("JTPW"):
  +--------+---------+-------+---------+----------+------+
  | "JTPW" | ImageID | Flags | NameLen | Filename | Size |
  | 4 bytes| (u64)   | (u8)  | (u16)   | NameLen  | var  |
  +--------+---------+-------+---------+----------+------+

ERROR response ("JTPE"):
  +--------+-----------+------------+-- - - --+
  | "JTPE" | ErrorCode | MessageLen | Message |
  | 4 bytes|   (u8)    |   (u16)    |  var    |
  +--------+-----------+------------+-- - - --+
```
