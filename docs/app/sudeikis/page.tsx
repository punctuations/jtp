export default function Sudeikis() {
  const sudeikisImageIdHex = "342054a62e20cf1269029cf02779bd77";
  const sudeikisImageIdShort = sudeikisImageIdHex.slice(0, 16);

  return (
    <div className="min-h-screen bg-white text-black">
      <main className="mx-auto w-full max-w-4xl px-6 py-14 font-sans">
        <header className="mb-10">
          <div className="flex flex-row justify-between">
            <h1 className="text-3xl font-semibold tracking-tight">Sudeikis</h1>
            <a
              className="self-end mr-6 underline underline-offset-4 text-sm text-black/80"
              href="/"
            >
              Back to protocol
            </a>
          </div>

          <p className="mt-3 text-base leading-7 text-black/70">
            A tiny, real dataset bundled with this repo:{" "}
            <span className="font-mono">images/sudeikis.jpg</span>. Use it to
            sanity-check that your client can LIST and GET_BY_ID from the
            default JTP server.
          </p>

          <nav className="mt-6 flex flex-wrap gap-x-4 gap-y-2 text-sm text-black/80">
            <a className="underline underline-offset-4" href="#connect">
              Connect
            </a>
            <a className="underline underline-offset-4" href="#imageid">
              ImageID
            </a>
            <a className="underline underline-offset-4" href="#commands">
              Commands
            </a>
          </nav>
        </header>

        <section id="connect" className="mb-10">
          <h2 className="text-xl font-semibold"># Connect</h2>
          <p className="mt-3 text-sm leading-6 text-black/80">
            The “Sudeikis endpoint” is just the normal JTP server serving
            whatever is in <span className="font-mono">images/</span>
            (which includes <span className="font-mono">sudeikis.jpg</span>).
            JTP runs over TCP, optionally wrapped in TLS; the reference server
            uses TLS. Run server and client from the repo root so{" "}
            <span className="font-mono">cert.pem</span> is shared.
          </p>
        </section>

        <section id="imageid" className="mb-10">
          <h2 className="text-xl font-semibold"># Sudeikis ImageID</h2>
          <ul className="mt-3 list-disc space-y-1 pl-5 text-sm leading-6 text-black/80">
            <li>
              File: <span className="font-mono">images/sudeikis.jpg</span>
            </li>
            <li>
              ImageID (16 bytes / 32 hex):{" "}
              <span className="font-mono">{sudeikisImageIdHex}</span>
            </li>
            <li>
              Short (first 8 bytes):{" "}
              <span className="font-mono">{sudeikisImageIdShort}</span>
            </li>
          </ul>
        </section>

        <section id="commands" className="mb-10">
          <h2 className="text-xl font-semibold"># Commands</h2>

          <div className="mt-6 grid gap-6">
            <div>
              <h3 className="text-base font-semibold">
                Default server (serves images/)
              </h3>
              <pre className="mt-3 overflow-x-auto rounded-lg border border-black/10 bg-white p-4 font-mono text-xs leading-5">
                {`# Terminal 1 (server)
cargo run --bin server

# Terminal 2 (client)
cargo run --bin client`}
              </pre>
              <p className="mt-3 text-sm leading-6 text-black/80">
                This serves everything under{" "}
                <span className="font-mono">images/</span> (including{" "}
                <span className="font-mono">sudeikis.jpg</span>).
              </p>
            </div>
          </div>
        </section>

        <footer className="pt-6 text-xs text-black/60">
          Tip: if you move the client elsewhere, copy{" "}
          <span className="font-mono">cert.pem</span> alongside it.
        </footer>
      </main>
    </div>
  );
}
