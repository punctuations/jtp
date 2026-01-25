import Mermaid from "@/components/Mermaid";

export default function JTP() {
  return (
    <div className="min-h-screen bg-white text-black">
      <main className="mx-auto w-full max-w-4xl px-6 py-14 font-sans">
        <header className="mb-10">
          <div className="flex flex-row justify-between">
            <h1 className="text-3xl font-semibold tracking-tight">
              Jason Transfer Protocol &mdash; JTP
              <span className="text-slate-300">/1.0</span>{" "}
            </h1>
            <p className="self-end mr-6">
              {new Date("Jan 24, 2026").toLocaleDateString("en-CA", {
                dateStyle: "long",
              })}
            </p>
          </div>

          <p className="mt-3 text-base leading-7 text-black/70">
            Jason Transfer Protocol (&quot;JTP&quot;) is a compact binary
            request/response protocol for listing and transferring images over
            TCP (optionally TLS), keyed by xxHash64-derived IDs.
          </p>
          <nav className="mt-6 flex flex-wrap gap-x-4 gap-y-2 text-sm text-black/80">
            <a className="underline underline-offset-4" href="#abstract">
              Abstract
            </a>
            <a className="underline underline-offset-4" href="#transport">
              Transport
            </a>
            <a className="underline underline-offset-4" href="#imageid">
              ImageID
            </a>
            <a className="underline underline-offset-4" href="#flags">
              Flags
            </a>
            <a className="underline underline-offset-4" href="#requests">
              Requests
            </a>
            <a className="underline underline-offset-4" href="#responses">
              Responses
            </a>
            <a className="underline underline-offset-4" href="#errors">
              Errors
            </a>
            <a className="underline underline-offset-4" href="#examples">
              Examples
            </a>
            <a className="underline underline-offset-4" href="/sudeikis">
              Live Example
            </a>
            <a
              className="underline underline-offset-4"
              href="https://github.com/punctuations/jtp"
              target="_blank"
              rel="noreferrer"
            >
              GitHub
            </a>
          </nav>
        </header>

        <section id="abstract" className="mb-10">
          <h2 className="text-xl font-semibold"># Abstract</h2>
          <p className="mt-3 max-w-3xl text-sm leading-6 text-black/80">
            JTP is a lightweight binary protocol for fast image distribution. A
            client first discovers available content with a catalog request
            (LIST), then requests one or more images by their content-derived
            identifiers (GET_BY_ID). Images are addressed by the first 8 bytes
            of xxHash64 over the file bytes, enabling deduplication and
            integrity checks; optional TLS provides confidentiality on the wire.
          </p>

          <div className="mt-6">
            <Mermaid
              caption="Request and response flow"
              chart={`sequenceDiagram
  autonumber
  participant C as Client
  participant S as Server

  Note right of C: Connect over TCP (optionally TLS)
  C->>S: TLS handshake (optional)
  S-->>C: TLS established (optional)

  C->>S: LIST (ReqType=1, RequestFlags)
  S-->>C: JTPL catalog (Header="JTPL", Count, Entries)
  Note right of C: Choose ImageID(s)

  C->>S: GET_BY_ID (ReqType=0, RequestFlags, Count, IDs)
  S-->>C: Image packet (repeated per requested ImageID)
  Note right of C: Verify ImageID == xxHash64(Data, seed=0)
`}
            />
          </div>
        </section>

        <section id="transport" className="mb-10">
          <h2 className="text-xl font-semibold"># Transport</h2>
          <p className="mt-3 text-sm leading-6 text-black/80">
            JTP requires an ordered, reliable byte stream transport.
          </p>
          <ul className="mt-3 list-disc space-y-1 pl-5 text-sm leading-6 text-black/80">
            <li>The default transport is <strong>TCP</strong>.</li>
            <li>JTP may be wrapped in <strong>TLS</strong> to provide confidentiality and integrity.</li>
            <li>
              The reference server listens on{" "}
              <span className="font-mono">0.0.0.0:8443</span>.
            </li>
          </ul>

          <h3 className="mt-6 text-base font-semibold">TLS and ALPN</h3>
          <ul className="mt-3 list-disc space-y-1 pl-5 text-sm leading-6 text-black/80">
            <li>When TLS is used, servers <strong>MAY</strong> advertise ALPN protocol identifier: <span className="font-mono">jtp/1</span></li>
            <li>Clients that support ALPN <strong>SHOULD</strong> offer <span className="font-mono">jtp/1</span>.</li>
            <li>JTP does not define certificate distribution. Deployments may use self-signed certificates, a local CA, or public PKI.</li>
          </ul>

          <h3 className="mt-6 text-base font-semibold">Keep-Alive</h3>
          <p className="mt-3 text-sm leading-6 text-black/80">
            JTP supports connection reuse through a keep-alive mechanism. When enabled,
            multiple requests can be sent over a single connection, avoiding repeated TLS handshakes.
          </p>
          <ul className="mt-3 list-disc space-y-1 pl-5 text-sm leading-6 text-black/80">
            <li>If the <span className="font-mono">keep-alive</span> flag is set in a request, the server <strong>SHOULD</strong> keep the connection open after sending the response.</li>
            <li>If the flag is not set, the server <strong>SHOULD</strong> close the connection after the response.</li>
            <li>Servers <strong>MAY</strong> implement idle timeouts to close stale keep-alive connections.</li>
            <li>Clients <strong>MUST</strong> handle server-initiated connection closes gracefully.</li>
          </ul>
        </section>

        <section id="imageid" className="mb-10">
          <h2 className="text-xl font-semibold"># ImageID</h2>
          <p className="mt-3 text-sm leading-6 text-black/80">
            <span className="font-mono">ImageID</span> is a 64-bit value computed from the raw image file bytes:
          </p>
          <pre className="mt-4 overflow-x-auto rounded-lg border border-black/10 bg-white p-4 font-mono text-xs leading-5">
            ImageID = xxHash64(image_bytes, seed = 0)
          </pre>
          <p className="mt-3 text-sm leading-6 text-black/80">
            On the wire, ImageID is transmitted as <span className="font-mono">u64</span> big-endian.
            When rendered as hex, the recommended representation is the hex encoding of the 8 big-endian bytes.
          </p>
        </section>

        <section id="flags" className="mb-10">
          <h2 className="text-xl font-semibold"># Flags</h2>
          <p className="mt-3 text-sm leading-6 text-black/80">
            JTP uses a one-byte <span className="font-mono">Flags</span> field with the following bit assignments:
          </p>
          <Table
            rows={[
              ["Bits 0..2", "0b0000_0111", "File type (0-7)"],
              ["Bit 3", "0b0000_1000", "Compressed (1 = Zstd compressed)"],
              ["Bit 4", "0b0001_0000", "Encrypted (reserved for future use)"],
              ["Bits 5..7", "0b1110_0000", "Reserved (MUST be 0)"],
            ]}
          />

          <h3 className="mt-6 text-base font-semibold">File Type Codes</h3>
          <Table
            rows={[
              ["0", "PNG", ""],
              ["1", "JPEG", "jpg/jpeg"],
              ["2", "WebP", ""],
              ["3", "BMP", ""],
              ["4", "GIF", ""],
              ["5-6", "Reserved", ""],
              ["7", "Unknown", "Use for unknown types"],
            ]}
          />

          <h3 className="mt-6 text-base font-semibold">Compression</h3>
          <p className="mt-3 text-sm leading-6 text-black/80">
            When bit 3 is set, the image data is Zstd compressed. Receivers <strong>MUST</strong> decompress before use.
            If a receiver does not support compression, it <strong>SHOULD</strong> fail the request rather than misinterpreting bytes.
          </p>
        </section>

        <section id="requests" className="mb-10">
          <h2 className="text-xl font-semibold"># Requests</h2>
          <p className="mt-3 text-sm leading-6 text-black/80">
            The first byte of every request is <span className="font-mono">ReqType (u8)</span>.
            For requests that support connection reuse, the second byte is <span className="font-mono">RequestFlags (u8)</span>.
          </p>

          <h3 className="mt-6 text-base font-semibold">Request Flags</h3>
          <Table
            rows={[
              ["Bit 0", "keep-alive", "1 = keep connection open after response"],
              ["Bits 1-7", "reserved", "MUST be 0"],
            ]}
          />

          <div className="mt-6 grid gap-6">
            <div>
              <h3 className="text-base font-semibold">
                LIST (ReqType = 1)
              </h3>
              <p className="mt-2 text-sm leading-6 text-black/80">
                Request the catalog of available images.
              </p>
              <Table
                rows={[
                  ["ReqType", "1", "1 = LIST"],
                  ["RequestFlags", "1", "Flags (bit 0 = keep-alive)"],
                ]}
              />
            </div>

            <div>
              <h3 className="text-base font-semibold">
                GET_BY_ID (ReqType = 0)
              </h3>
              <p className="mt-2 text-sm leading-6 text-black/80">
                Request specific images by their IDs. Count <strong>MUST NOT</strong> exceed 255.
              </p>
              <Table
                rows={[
                  ["ReqType", "1", "0 = GET_BY_ID"],
                  ["RequestFlags", "1", "Flags (bit 0 = keep-alive)"],
                  ["Count", "1", "Number of IDs (N), max 255"],
                  ["ImageID", "8 × N", "Requested image IDs (big-endian)"],
                ]}
              />
            </div>

            <div>
              <h3 className="text-base font-semibold">
                BATCH (ReqType = 2)
              </h3>
              <p className="mt-2 text-sm leading-6 text-black/80">
                Delta sync: client sends the IDs it already has; server returns only the missing images.
              </p>
              <Table
                rows={[
                  ["ReqType", "1", "2 = BATCH"],
                  ["RequestFlags", "1", "Flags (bit 0 = keep-alive)"],
                  ["HaveCount", "1–5", "Have ID count (varint u32)"],
                  ["ImageID", "8 × N", "IDs the client already has"],
                ]}
              />
            </div>

            <div>
              <h3 className="text-base font-semibold">
                LIST_AND_GET (ReqType = 5)
              </h3>
              <p className="mt-2 text-sm leading-6 text-black/80">
                Combined catalog listing and image transfer in a single round-trip.
                Server responds with all available images.
              </p>
              <Table
                rows={[
                  ["ReqType", "1", "5 = LIST_AND_GET"],
                  ["RequestFlags", "1", "Flags (bit 0 = keep-alive)"],
                ]}
              />
            </div>
          </div>
        </section>

        <section id="responses" className="mb-10">
          <h2 className="text-xl font-semibold"># Responses</h2>

          <div className="mt-6 grid gap-6">
            <div>
              <h3 className="text-base font-semibold">
                LIST Response (Header = JTPL)
              </h3>
              <Table
                rows={[
                  ["Header", "4", 'ASCII "JTPL"'],
                  ["Count", "2", "Number of entries (u16)"],
                  ["Entries", "variable", "Repeated Count times"],
                ]}
              />
              <p className="mt-3 text-sm leading-6 text-black/80">
                Each entry:
              </p>
              <Table
                rows={[
                  ["ImageID", "8", "Image ID (u64, big-endian)"],
                  ["Flags", "1", "File type + feature flags"],
                  ["NameLen", "2", "Filename length (u16)"],
                  ["Filename", "NameLen", "UTF-8 basename"],
                  ["Size", "1–5", "Data size (varint u32)"],
                ]}
              />
            </div>

            <div>
              <h3 className="text-base font-semibold">Image Packet</h3>
              <p className="mt-2 text-sm leading-6 text-black/80">
                Used by GET_BY_ID, BATCH, and LIST_AND_GET responses.
              </p>
              <Table
                rows={[
                  ["Flags", "1", "File type + feature flags"],
                  ["Length", "1–5", "Data length (varint u32)"],
                  ["ImageID", "8", "Image ID (u64, big-endian)"],
                  ["Data", "Length", "Raw image bytes"],
                ]}
              />
              <p className="mt-3 text-sm leading-6 text-black/80">
                Receivers <strong>SHOULD</strong> validate: <span className="font-mono">ImageID == xxHash64(Data, seed=0)</span> (after decompression if compressed).
              </p>
            </div>

            <div>
              <h3 className="text-base font-semibold">
                BATCH Response (Header = JTPB)
              </h3>
              <Table
                rows={[
                  ["Header", "4", 'ASCII "JTPB"'],
                  ["MissingCount", "1–5", "Missing image count (varint u32)"],
                  ["Images", "variable", "Repeated MissingCount times (image packet)"],
                ]}
              />
            </div>

            <div>
              <h3 className="text-base font-semibold">
                LIST_AND_GET Response (Header = JTPG)
              </h3>
              <Table
                rows={[
                  ["Header", "4", 'ASCII "JTPG"'],
                  ["Count", "2", "Number of images (u16)"],
                  ["Images", "variable", "Repeated Count times (image packet)"],
                ]}
              />
            </div>
          </div>
        </section>

        <section id="errors" className="mb-10">
          <h2 className="text-xl font-semibold"># Error Handling</h2>
          <p className="mt-3 text-sm leading-6 text-black/80">
            JTP defines an optional structured error response for servers that wish to provide detailed error information.
          </p>

          <h3 className="mt-6 text-base font-semibold">ERROR Response (Header = JTPE)</h3>
          <Table
            rows={[
              ["Header", "4", 'ASCII "JTPE"'],
              ["ErrorCode", "1", "Error code"],
              ["MessageLen", "2", "Length of message (u16)"],
              ["Message", "MessageLen", "UTF-8 error description"],
            ]}
          />

          <h3 className="mt-6 text-base font-semibold">Error Codes</h3>
          <Table
            rows={[
              ["1", "NotFound", "Requested resource not found"],
              ["2", "InvalidRequest", "Malformed or invalid request"],
              ["3", "ServerError", "Internal server error"],
              ["4", "UnsupportedFeature", "Feature not supported by server"],
              ["5", "RateLimited", "Request rate limit exceeded"],
            ]}
          />

          <p className="mt-3 text-sm leading-6 text-black/80">
            Servers may also signal errors by closing the connection or terminating the TLS session.
            Clients should treat unexpected EOF, invalid headers, or decoding errors as request failure.
          </p>
        </section>

        <section id="limits" className="mb-10">
          <h2 className="text-xl font-semibold"># Limits</h2>
          <ul className="mt-3 list-disc space-y-1 pl-5 text-sm leading-6 text-black/80">
            <li>Maximum single image size: <strong>4 GiB - 1</strong> (u32 framing)</li>
            <li>Maximum images per LIST: <strong>65,535</strong> (u16 count)</li>
            <li>Maximum GET_BY_ID count: <strong>255</strong> (u8 count)</li>
            <li>Servers <strong>SHOULD</strong> reject BATCH requests with HaveCount exceeding <strong>1,000,000</strong></li>
          </ul>
        </section>

        <section id="examples" className="mb-10">
          <h2 className="text-xl font-semibold"># Examples</h2>
          <p className="mt-3 text-sm leading-6 text-black/80">
            Hex dumps are spaced by byte. Fixed-width integers are big-endian;
            sizes/lengths use unsigned LEB128 varints.
          </p>

          <Example
            title="LIST request (with keep-alive)"
            hexLines={["01 01"]}
            notes={["01 = ReqType (LIST)", "01 = RequestFlags (keep-alive=1)"]}
          />

          <Example
            title="LIST response (Count = 1)"
            hexLines={[
              "4A 54 50 4C  00 01",
              "AA BB CC DD EE FF 00 11  01  00 09",
              "6A 61 73 6F 6E 31 2E 6A 70 67  B4 24",
            ]}
            notes={[
              "JTPL header, Count=1",
              "Entry: ImageID(8) + Flags(01=jpg) + NameLen(9)",
              "Filename 'jason1.jpg' + Size(0x00001234 varint)",
            ]}
          />

          <Example
            title="GET_BY_ID request (Count = 1)"
            hexLines={["00 00 01", "AA BB CC DD EE FF 00 11"]}
            notes={[
              "00 = ReqType (GET_BY_ID)",
              "00 = RequestFlags (no keep-alive)",
              "01 = Count",
              "8 bytes of ImageID",
            ]}
          />

          <Example
            title="Image packet response"
            hexLines={["01 04", "AA BB CC DD EE FF 00 11", "DE AD BE EF"]}
            notes={[
              "Flags(01=jpg) + Length(4 varint)",
              "ImageID echoes the requested ID (8 bytes)",
              "4 bytes of file data (example)",
            ]}
          />

          <Example
            title="ERROR response"
            hexLines={["4A 54 50 45  02  00 0F", "49 6E 76 61 6C 69 64 20 72 65 71 75 65 73 74"]}
            notes={[
              'JTPE header + ErrorCode(2=InvalidRequest) + MessageLen(15)',
              'Message: "Invalid request"',
            ]}
          />
        </section>

        <section id="security" className="mb-10">
          <h2 className="text-xl font-semibold"># Security Considerations</h2>
          <ul className="mt-3 list-disc space-y-1 pl-5 text-sm leading-6 text-black/80">
            <li>Use TLS to prevent passive observation and active tampering.</li>
            <li>ImageID is content-derived and can be used to validate integrity, but it is not a cryptographic MAC.</li>
            <li>Servers should validate and cap counts/sizes to mitigate denial-of-service.</li>
          </ul>
        </section>

        <footer className="pt-6 text-xs text-black/60">
          <a
            className="underline underline-offset-4"
            href="https://github.com/punctuations/jtp"
            target="_blank"
            rel="noreferrer"
          >
            Contribute on GitHub
          </a>
          <span className="px-2">•</span>
          <span>Issues, PRs, and protocol discussions welcome.</span>
        </footer>
      </main>
    </div>
  );
}

function Table({
  rows,
}: {
  rows: Array<[field: string, size: string, description: string]>;
}) {
  return (
    <div className="mt-3 overflow-x-auto rounded-lg border border-black/10">
      <table className="w-full border-collapse text-left text-sm">
        <thead className="bg-black/[0.03]">
          <tr>
            <th className="whitespace-nowrap px-3 py-2 font-semibold">Field</th>
            <th className="whitespace-nowrap px-3 py-2 font-semibold">
              Size (bytes)
            </th>
            <th className="px-3 py-2 font-semibold">Description</th>
          </tr>
        </thead>
        <tbody>
          {rows.map(([field, size, description], i) => (
            <tr key={`${field}-${i}`} className="border-t border-black/10">
              <td className="whitespace-nowrap px-3 py-2 font-mono text-xs">
                {field}
              </td>
              <td className="whitespace-nowrap px-3 py-2 font-mono text-xs">
                {size}
              </td>
              <td className="px-3 py-2 text-black/80">{description}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function Example({
  title,
  hexLines,
  notes,
}: {
  title: string;
  hexLines: string[];
  notes: string[];
}) {
  return (
    <div className="mt-6">
      <h3 className="text-base font-semibold">{title}</h3>
      <pre className="mt-3 overflow-x-auto rounded-lg border border-black/10 bg-white p-4 font-mono text-xs leading-5">
        {hexLines.join("\n")}
      </pre>
      <ul className="mt-3 list-disc space-y-1 pl-5 text-sm leading-6 text-black/80">
        {notes.map((n, i) => (
          <li key={i}>{n}</li>
        ))}
      </ul>
    </div>
  );
}
