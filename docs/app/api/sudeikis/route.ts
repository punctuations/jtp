import { readFile } from "fs/promises";
import path from "path";

export const runtime = "nodejs";

type JtpWasmExports = {
    memory: WebAssembly.Memory;
    alloc: (len: number) => number;
    dealloc: (ptr: number, cap: number) => void;
    image_id_hex: (ptr: number, len: number) => number;
    image_id_hex_len: () => number;
};

let wasmExportsPromise: Promise<JtpWasmExports> | null = null;

async function getWasm(): Promise<JtpWasmExports> {
    if (!wasmExportsPromise) {
        wasmExportsPromise = (async () => {
            const wasmUrl = new URL("./jtp_imageid.wasm", import.meta.url);
            const wasmBytes = await readFile(wasmUrl);
            const { instance } = await WebAssembly.instantiate(wasmBytes, {});
            return instance.exports as unknown as JtpWasmExports;
        })();
    }
    return wasmExportsPromise;
}

function toImageIdHexWasm(wasm: JtpWasmExports, fileBytes: Buffer): string {
    const inLen = fileBytes.length;
    const inPtr = wasm.alloc(inLen);
    const mem = new Uint8Array(wasm.memory.buffer);
    mem.set(fileBytes, inPtr);

    const outPtr = wasm.image_id_hex(inPtr, inLen);
    const outLen = wasm.image_id_hex_len();
    const outView = new Uint8Array(wasm.memory.buffer, outPtr, outLen);
    const out = new TextDecoder().decode(outView);

    wasm.dealloc(inPtr, inLen);
    wasm.dealloc(outPtr, outLen);
    return out;
}

export async function GET() {
    // When running from `docs/`, the repo-root `images/` folder is one level up.
    const filePath = path.join(process.cwd(), "..", "images", "sudeikis.jpg");

    try {
        const bytes = await readFile(filePath);
        const wasm = await getWasm();
        const imageIdHex = toImageIdHexWasm(wasm, bytes);

        // Return the image bytes so hitting /api/sudeikis displays the JPG.
        // Keep the computed ImageID available via headers.
        return new Response(bytes, {
            headers: {
                "Content-Type": "image/jpeg",
                "Content-Length": String(bytes.length),
                "Cache-Control": "public, max-age=3600",
                "X-JTP-ImageID": imageIdHex,
            },
        });
    } catch (err) {
        return new Response(`Failed to read ${filePath}: ${String(err)}`, {
            status: 500,
            headers: { "Content-Type": "text/plain; charset=utf-8" },
        });
    }
}
