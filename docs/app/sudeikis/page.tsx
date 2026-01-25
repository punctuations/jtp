"use client";

import { useEffect, useMemo, useState } from "react";

export default function Sudeikis() {
  const [imageIdHex, setImageIdHex] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const res = await fetch("/api/sudeikis", { cache: "no-store" });
        const header = res.headers.get("X-JTP-ImageID");
        if (!cancelled) {
          if (!header) {
            setError("Missing X-JTP-ImageID header from /api/sudeikis");
          } else {
            setImageIdHex(header);
          }
        }
      } catch (e) {
        if (!cancelled) {
          setError(String(e));
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const imageIdShort = useMemo(() => {
    if (!imageIdHex) return null;
    return imageIdHex.slice(0, 8);
  }, [imageIdHex]);

  return (
    <div className="min-h-screen bg-white text-black">
      <main className="mx-auto w-full max-w-4xl px-6 py-14 font-sans">
        <header className="mb-10">
          <div className="flex flex-row justify-between">
            <h1 className="text-3xl font-semibold tracking-tight">Live Example</h1>
            <a
              className="self-end mr-6 underline underline-offset-4 text-sm text-black/80"
              href="/"
            >
              Back to protocol
            </a>
          </div>

          <p className="mt-3 text-base leading-7 text-black/70">
            A live JTP server is running at{" "}
            <span className="font-mono font-semibold">jtp.mattt.space:8443</span>{" "}
            serving 7 images. Use this to test your client implementation against
            a real server with valid TLS certificates.
          </p>

          <nav className="mt-6 flex flex-wrap gap-x-4 gap-y-2 text-sm text-black/80">
            <a className="underline underline-offset-4" href="#server">
              Server
            </a>
            <a className="underline underline-offset-4" href="#connect">
              Connect
            </a>
            <a className="underline underline-offset-4" href="#catalog">
              Catalog
            </a>
            <a className="underline underline-offset-4" href="#examples">
              Examples
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

        <section id="server" className="mb-10">
          <h2 className="text-xl font-semibold"># Live Server</h2>
          <div className="mt-3 overflow-x-auto rounded-lg border border-black/10">
            <table className="w-full border-collapse text-left text-sm">
              <tbody>
                <tr className="border-b border-black/10">
                  <td className="whitespace-nowrap px-3 py-2 font-semibold">Host</td>
                  <td className="px-3 py-2 font-mono">jtp.mattt.space</td>
                </tr>
                <tr className="border-b border-black/10">
                  <td className="whitespace-nowrap px-3 py-2 font-semibold">Port</td>
                  <td className="px-3 py-2 font-mono">8443</td>
                </tr>
                <tr className="border-b border-black/10">
                  <td className="whitespace-nowrap px-3 py-2 font-semibold">TLS</td>
                  <td className="px-3 py-2">Enabled (Let&apos;s Encrypt)</td>
                </tr>
                <tr>
                  <td className="whitespace-nowrap px-3 py-2 font-semibold">Images</td>
                  <td className="px-3 py-2">7 files (~47 MB total)</td>
                </tr>
              </tbody>
            </table>
          </div>
          <p className="mt-3 text-sm leading-6 text-black/80">
            The server uses a valid Let&apos;s Encrypt certificate, so the client
            can use standard CA roots (no custom <span className="font-mono">--cert</span> needed).
          </p>
        </section>

        <section id="connect" className="mb-10">
          <h2 className="text-xl font-semibold"># Connect</h2>
          <p className="mt-3 text-sm leading-6 text-black/80">
            Clone the repository and build the client:
          </p>
          <pre className="mt-3 overflow-x-auto rounded-lg border border-black/10 bg-white p-4 font-mono text-xs leading-5">
{`git clone https://github.com/punctuations/jtp
cd jtp
cargo build --release`}
          </pre>

          <p className="mt-4 text-sm leading-6 text-black/80">
            Connect to the live server:
          </p>
          <pre className="mt-3 overflow-x-auto rounded-lg border border-black/10 bg-white p-4 font-mono text-xs leading-5">
{`# List available images
./target/release/client jtp://jtp.mattt.space

# Download all images to ./output
./target/release/client jtp://jtp.mattt.space --out ./output

# With keep-alive for faster downloads
./target/release/client jtp://jtp.mattt.space -k

# Parallel download (4 workers)
./target/release/client jtp://jtp.mattt.space -p 4

# Delta sync (only download missing images)
./target/release/client jtp://jtp.mattt.space --batch`}
          </pre>
        </section>

        <section id="catalog" className="mb-10">
          <h2 className="text-xl font-semibold"># Image Catalog</h2>
          <p className="mt-3 text-sm leading-6 text-black/80">
            The server hosts these 7 images:
          </p>
          <div className="mt-3 overflow-x-auto rounded-lg border border-black/10">
            <table className="w-full border-collapse text-left text-sm">
              <thead className="bg-black/[0.03]">
                <tr>
                  <th className="whitespace-nowrap px-3 py-2 font-semibold">Filename</th>
                  <th className="whitespace-nowrap px-3 py-2 font-semibold">Type</th>
                  <th className="whitespace-nowrap px-3 py-2 font-semibold">Size</th>
                  <th className="px-3 py-2 font-semibold">Compressed</th>
                </tr>
              </thead>
              <tbody>
                <tr className="border-t border-black/10">
                  <td className="whitespace-nowrap px-3 py-2 font-mono text-xs">bateman.jpg</td>
                  <td className="px-3 py-2">JPEG</td>
                  <td className="px-3 py-2 font-mono text-xs">178 KB</td>
                  <td className="px-3 py-2">No</td>
                </tr>
                <tr className="border-t border-black/10">
                  <td className="whitespace-nowrap px-3 py-2 font-mono text-xs">derulo.png</td>
                  <td className="px-3 py-2">PNG</td>
                  <td className="px-3 py-2 font-mono text-xs">1.4 MB</td>
                  <td className="px-3 py-2">No</td>
                </tr>
                <tr className="border-t border-black/10">
                  <td className="whitespace-nowrap px-3 py-2 font-mono text-xs">momoa.bmp</td>
                  <td className="px-3 py-2">BMP</td>
                  <td className="px-3 py-2 font-mono text-xs">43 MB</td>
                  <td className="px-3 py-2">Yes (Zstd)</td>
                </tr>
                <tr className="border-t border-black/10">
                  <td className="whitespace-nowrap px-3 py-2 font-mono text-xs">segel.jpeg</td>
                  <td className="px-3 py-2">JPEG</td>
                  <td className="px-3 py-2 font-mono text-xs">217 KB</td>
                  <td className="px-3 py-2">No</td>
                </tr>
                <tr className="border-t border-black/10">
                  <td className="whitespace-nowrap px-3 py-2 font-mono text-xs">statham.webp</td>
                  <td className="px-3 py-2">WebP</td>
                  <td className="px-3 py-2 font-mono text-xs">174 KB</td>
                  <td className="px-3 py-2">No</td>
                </tr>
                <tr className="border-t border-black/10">
                  <td className="whitespace-nowrap px-3 py-2 font-mono text-xs">sudeikis.jpg</td>
                  <td className="px-3 py-2">JPEG</td>
                  <td className="px-3 py-2 font-mono text-xs">54 KB</td>
                  <td className="px-3 py-2">No</td>
                </tr>
                <tr className="border-t border-black/10">
                  <td className="whitespace-nowrap px-3 py-2 font-mono text-xs">voorhees.gif</td>
                  <td className="px-3 py-2">GIF</td>
                  <td className="px-3 py-2 font-mono text-xs">2.4 MB</td>
                  <td className="px-3 py-2">No</td>
                </tr>
              </tbody>
            </table>
          </div>

          <h3 className="mt-6 text-base font-semibold">Sudeikis ImageID</h3>
          <p className="mt-2 text-sm leading-6 text-black/80">
            The <span className="font-mono">sudeikis.jpg</span> file has ImageID:
          </p>
          <ul className="mt-2 list-disc space-y-1 pl-5 text-sm leading-6 text-black/80">
            <li>
              Full (16 hex):{" "}
              <span className="font-mono">
                {imageIdHex ?? (error ? "(error)" : "(loading...)")}
              </span>
            </li>
            <li>
              Short (8 hex):{" "}
              <span className="font-mono">
                {imageIdShort ?? (error ? "(error)" : "(loading...)")}
              </span>
            </li>
            {error ? (
              <li>
                Error: <span className="font-mono text-red-600">{error}</span>
              </li>
            ) : null}
          </ul>
        </section>

        <section id="examples" className="mb-10">
          <h2 className="text-xl font-semibold"># Protocol Examples</h2>

          <div className="mt-6">
            <h3 className="text-base font-semibold">LIST Request</h3>
            <p className="mt-2 text-sm leading-6 text-black/80">
              Send bytes <span className="font-mono">01 00</span> (ReqType=LIST, no keep-alive)
              to receive the catalog of 7 images.
            </p>
            <pre className="mt-3 overflow-x-auto rounded-lg border border-black/10 bg-white p-4 font-mono text-xs leading-5">
{`# Response starts with:
4A 54 50 4C  # "JTPL" header
00 07        # Count = 7 images

# Followed by 7 catalog entries...`}
            </pre>
          </div>

          <div className="mt-6">
            <h3 className="text-base font-semibold">Expected Output</h3>
            <pre className="mt-3 overflow-x-auto rounded-lg border border-black/10 bg-white p-4 font-mono text-xs leading-5">
{`$ ./target/release/client jtp://jtp.mattt.space
Server catalog:
- a1b2c3d4e5f67890  bateman.jpg  178234 bytes
- b2c3d4e5f6789012  derulo.png   1468921 bytes
- c3d4e5f678901234  momoa.bmp    45088054 bytes
- d4e5f67890123456  segel.jpeg   222187 bytes
- e5f6789012345678  statham.webp 178432 bytes
- f67890123456789a  sudeikis.jpg 55291 bytes
- 7890123456789abc  voorhees.gif 2516482 bytes
Downloaded 7 images in 3.2s (2.2 images/sec)`}
            </pre>
            <p className="mt-3 text-sm leading-6 text-black/80 italic">
              Note: ImageIDs shown above are examples. Actual IDs are computed from file contents.
            </p>
          </div>

          <div className="mt-6">
            <h3 className="text-base font-semibold">Verbose Mode</h3>
            <pre className="mt-3 overflow-x-auto rounded-lg border border-black/10 bg-white p-4 font-mono text-xs leading-5">
{`$ ./target/release/client jtp://jtp.mattt.space -v
Client args: addr=jtp.mattt.space:8443, server_name=jtp.mattt.space, cert=None, ...
Using system root certificates...
Connecting TCP to jtp.mattt.space:8443...
TLS handshake complete
TLS connected; sending LIST request
LIST response header OK (JTPL)
LIST count=7
...`}
            </pre>
          </div>
        </section>

        <section id="local" className="mb-10">
          <h2 className="text-xl font-semibold"># Run Locally</h2>
          <p className="mt-3 text-sm leading-6 text-black/80">
            To run your own server with the same images:
          </p>
          <pre className="mt-3 overflow-x-auto rounded-lg border border-black/10 bg-white p-4 font-mono text-xs leading-5">
{`# Terminal 1: Start the server
cargo run --release --bin server -- --images images --verbose

# Terminal 2: Connect with client (plain TCP for local testing)
cargo run --release --bin client -- --no-tls --addr 127.0.0.1:8443`}
          </pre>
          <p className="mt-3 text-sm leading-6 text-black/80">
            For TLS locally, the server auto-generates self-signed certificates
            (<span className="font-mono">cert.pem</span>, <span className="font-mono">key.pem</span>).
            Pass <span className="font-mono">--cert cert.pem</span> to the client.
          </p>
        </section>

        <footer className="pt-6 text-xs text-black/60">
          <a
            className="underline underline-offset-4"
            href="https://github.com/punctuations/jtp"
            target="_blank"
            rel="noreferrer"
          >
            View source on GitHub
          </a>
          <span className="px-2">•</span>
          <span>Server uptime not guaranteed &mdash; this is a demo endpoint.</span>
        </footer>
      </main>
    </div>
  );
}
