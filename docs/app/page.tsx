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
              {new Date("Dec 28, 2025").toLocaleDateString("en-CA", {
                dateStyle: "long",
              })}
            </p>
          </div>

          <p className="mt-3 text-base leading-7 text-black/70">
            Jason Transfer Protocol ("JTP") is a compact request/response
            protocol for listing and transferring images over TCP (optionally
            TLS), keyed by xxHash64-derived IDs.
          </p>
          <nav className="mt-6 flex flex-wrap gap-x-4 gap-y-2 text-sm text-black/80">
            <a className="underline underline-offset-4" href="#abstract">
              Abstract
            </a>
            <a className="underline underline-offset-4" href="#connection">
              Connection
            </a>
            <a className="underline underline-offset-4" href="#imageid">
              ImageID
            </a>
            <a className="underline underline-offset-4" href="#requests">
              Requests
            </a>
            <a className="underline underline-offset-4" href="#responses">
              Responses
            </a>
            <a className="underline underline-offset-4" href="#examples">
              Examples
            </a>
            <a className="underline underline-offset-4" href="/sudeikis">
              Sudeikis
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

  C->>S: LIST (ReqType=1)
  S-->>C: JTPL catalog (Header="JTPL", Count, Entries)
  Note right of C: Choose ImageID(s)

  C->>S: GET_BY_ID (ReqType=0, Count, ImageID x N)
  S-->>C: Image packet (repeated per requested ImageID)
  Note right of C: Verify ImageID == xxHash64(Data, seed=0)
`}
            />
          </div>
        </section>

        <section id="connection" className="mb-10">
          <h2 className="text-xl font-semibold"># Connection</h2>
          <ul className="mt-3 list-disc space-y-1 pl-5 text-sm leading-6 text-black/80">
            <li>Transport is TCP, optionally wrapped in TLS.</li>
            <li>
              The reference server listens on{" "}
              <span className="font-mono">127.0.0.1:8443</span>.
            </li>
          </ul>
        </section>

        <section id="imageid" className="mb-10">
          <h2 className="text-xl font-semibold"># ImageID encoding</h2>
          <p className="mt-3 text-sm leading-6 text-black/80">
            <span className="font-mono">ImageID</span> is 8 bytes (64-bit): the
            output of xxHash64 over the raw file bytes (seed = 0).
          </p>
          <pre className="mt-4 overflow-x-auto rounded-lg border border-black/10 bg-white p-4 font-mono text-xs leading-5">
            image_bytes = read_file("jason.jpg") {"\n"}
            image_id_u64 = xxHash64(image_bytes, seed=0) {"\n"}
            image_id_bytes = to_be_bytes(image_id_u64) // 8 bytes
          </pre>
        </section>

        <section id="requests" className="mb-10">
          <h2 className="text-xl font-semibold"># Client request packets</h2>
          <p className="mt-3 text-sm leading-6 text-black/80">
            The first byte is <span className="font-mono">ReqType</span>.
          </p>

          <div className="mt-6 grid gap-6">
            <div>
              <h3 className="text-base font-semibold">
                3.1 LIST (ReqType = 1)
              </h3>
              <Table rows={[["ReqType", "1", "1 = LIST"]]} />
            </div>

            <div>
              <h3 className="text-base font-semibold">
                3.2 GET_BY_ID (ReqType = 0)
              </h3>
              <Table
                rows={[
                  ["ReqType", "1", "0 = GET_BY_ID"],
                  ["Count", "1", "Number of IDs (N)"],
                  ["ImageID", "8 × N", "Requested image IDs"],
                ]}
              />
            </div>

            <div>
              <h3 className="text-base font-semibold">
                3.3 BATCH (ReqType = 2, delta sync)
              </h3>
              <p className="mt-2 text-sm leading-6 text-black/80">
                Client sends the IDs it already has; server returns only the
                missing images.
              </p>
              <Table
                rows={[
                  ["ReqType", "1", "2 = BATCH"],
                  ["HaveCount", "1–5", "Have ID count (u32 varint)"],
                  ["ImageID", "8 × N", "IDs the client already has"],
                ]}
              />
            </div>
          </div>
        </section>

        <section id="responses" className="mb-10">
          <h2 className="text-xl font-semibold"># Server response packets</h2>

          <div className="mt-6 grid gap-6">
            <div>
              <h3 className="text-base font-semibold">
                4.1 LIST response (Header = JTPL)
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
                  [
                    "Flags",
                    "1",
                    "bits 0..2=file type, bit3=compressed, bit4=encrypted",
                  ],
                  ["NameLen", "2", "Filename length (u16)"],
                  ["Filename", "NameLen", "UTF-8 basename"],
                  ["Size", "1–5", "Data size (u32 varint)"],
                ]}
              />
            </div>

            <div>
              <h3 className="text-base font-semibold">4.2 Image response</h3>
              <Table
                rows={[
                  [
                    "Flags",
                    "1",
                    "bits 0..2=file type, bit3=compressed, bit4=encrypted",
                  ],
                  ["Length", "1–5", "Data length (u32 varint)"],
                  ["ImageID", "8", "Echoes requested ImageID (u64)"],
                  ["Data", "variable", "Raw file bytes"],
                ]}
              />
            </div>

            <div>
              <h3 className="text-base font-semibold">
                4.3 BATCH response (Header = JTPB)
              </h3>
              <Table
                rows={[
                  ["Header", "4", 'ASCII "JTPB"'],
                  ["MissingCount", "1–5", "Missing image count (u32 varint)"],
                  [
                    "Images",
                    "variable",
                    "Repeated MissingCount times (image response)",
                  ],
                ]}
              />
            </div>
          </div>
        </section>

        <section id="examples" className="mb-10">
          <h2 className="text-xl font-semibold"># Example packets</h2>
          <p className="mt-3 text-sm leading-6 text-black/80">
            Hex dumps are spaced by byte. Fixed-width integers are big-endian;
            sizes/lengths are unsigned LEB128 varints.
          </p>

          <Example
            title="1.1. LIST request"
            hexLines={["01"]}
            notes={["01 = ReqType (LIST)"]}
          />

          <Example
            title="1.2. LIST response (Count = 1)"
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
            title="2.1. GET_BY_ID request (Count = 1)"
            hexLines={["00  01", "AA BB CC DD EE FF 00 11"]}
            notes={[
              "00 = ReqType (GET_BY_ID)",
              "01 = Count",
              "8 bytes of ImageID",
            ]}
          />

          <Example
            title="2.2. Response: image packet"
            hexLines={["01 04", "AA BB CC DD EE FF 00 11", "DE AD BE EF"]}
            notes={[
              "Flags(01=jpg) + Length(4 varint)",
              "ImageID echoes the requested ID (8 bytes)",
              "4 bytes of file data (example)",
            ]}
          />
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
          {rows.map(([field, size, description]) => (
            <tr key={field} className="border-t border-black/10">
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
        {notes.map((n) => (
          <li key={n}>{n}</li>
        ))}
      </ul>
    </div>
  );
}
