import Link from "next/link";

export default function NotFound() {
  return (
    <div className="min-h-screen bg-white text-black">
      <main className="mx-auto w-full max-w-4xl px-6 py-14 font-sans">
        <div className="flex flex-col items-center justify-center min-h-[70vh]">
          {/* Visual: broken connection */}
          <div className="mb-12 flex items-center gap-4">
            <div className="flex flex-col items-center">
              <div className="w-12 h-12 rounded-full border-2 border-black/20 flex items-center justify-center">
                <span className="text-xs font-mono text-black/40">SRC</span>
              </div>
            </div>

            <div className="flex items-center gap-1">
              <div className="w-8 h-[2px] bg-black/20" />
              <div className="w-2 h-[2px] bg-black/10" />
              <div className="w-2 h-[2px] bg-black/5" />
              <svg
                className="w-5 h-5 text-black/30"
                fill="none"
                stroke="currentColor"
                viewBox="0 0 24 24"
              >
                <path
                  strokeLinecap="round"
                  strokeLinejoin="round"
                  strokeWidth={2}
                  d="M6 18L18 6M6 6l12 12"
                />
              </svg>
              <div className="w-2 h-[2px] bg-black/5" />
              <div className="w-2 h-[2px] bg-black/10" />
              <div className="w-8 h-[2px] bg-black/20" />
            </div>

            <div className="flex flex-col items-center">
              <div className="w-12 h-12 rounded-full border-2 border-dashed border-black/10 flex items-center justify-center">
                <span className="text-xs font-mono text-black/20">DST</span>
              </div>
            </div>
          </div>

          {/* 404 */}
          <h1 className="text-[10rem] font-bold leading-none tracking-tighter bg-gradient-to-b from-black/80 to-black/20 bg-clip-text text-transparent">
            404
          </h1>

          <p className="mt-2 text-lg text-black/50">
            Route not found in catalog
          </p>

          {/* Error packet - compact */}
          <div className="mt-10 rounded-lg border border-black/10 bg-black/[0.02] px-6 py-4">
            <div className="flex items-center gap-6 text-sm font-mono">
              <div className="flex items-center gap-2">
                <span className="text-black/40">Header</span>
                <span className="text-black/80">JTPE</span>
              </div>
              <div className="w-px h-4 bg-black/10" />
              <div className="flex items-center gap-2">
                <span className="text-black/40">Code</span>
                <span className="text-black/80">0x01</span>
              </div>
              <div className="w-px h-4 bg-black/10" />
              <div className="flex items-center gap-2">
                <span className="text-black/40">NotFound</span>
              </div>
            </div>
          </div>

          {/* Navigation */}
          <nav className="mt-10 flex flex-wrap gap-x-6 gap-y-2 text-sm">
            <Link
              className="text-black/60 underline underline-offset-4 hover:text-black transition-colors"
              href="/"
            >
              Protocol
            </Link>
            <Link
              className="text-black/60 underline underline-offset-4 hover:text-black transition-colors"
              href="/sudeikis"
            >
              Live Example
            </Link>
            <a
              className="text-black/60 underline underline-offset-4 hover:text-black transition-colors"
              href="https://github.com/punctuations/jtp"
              target="_blank"
              rel="noreferrer"
            >
              GitHub
            </a>
          </nav>
        </div>
      </main>
    </div>
  );
}
