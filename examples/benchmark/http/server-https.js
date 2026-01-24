const https = require("https");
const fs = require("fs");
const path = require("path");

const PORT = 8080;
const IMAGES_DIR = path.join(__dirname, "..", "..", "..", "images");

// Use the same certs as JTP server for fair comparison
const CERT_PATH = path.join(__dirname, "..", "..", "..", "cert.pem");
const KEY_PATH = path.join(__dirname, "..", "..", "..", "key.pem");

// Check if certs exist
if (!fs.existsSync(CERT_PATH) || !fs.existsSync(KEY_PATH)) {
  console.error("Error: TLS certificates not found.");
  console.error("Run the JTP server first to generate cert.pem and key.pem");
  console.error(`Expected: ${CERT_PATH}`);
  console.error(`Expected: ${KEY_PATH}`);
  process.exit(1);
}

const options = {
  key: fs.readFileSync(KEY_PATH),
  cert: fs.readFileSync(CERT_PATH),
};

const server = https.createServer(options, (req, res) => {
  // Enable CORS
  res.setHeader("Access-Control-Allow-Origin", "*");
  res.setHeader("Access-Control-Allow-Methods", "GET");

  if (req.method === "GET" && req.url.startsWith("/image/")) {
    const filename = decodeURIComponent(req.url.substring("/image/".length));
    const filepath = path.join(IMAGES_DIR, filename);

    // Security: prevent directory traversal
    if (!filepath.startsWith(IMAGES_DIR)) {
      res.writeHead(403, { "Content-Type": "text/plain" });
      res.end("Forbidden");
      return;
    }

    fs.stat(filepath, (err, stats) => {
      if (err) {
        res.writeHead(404, { "Content-Type": "text/plain" });
        res.end("Not Found");
        return;
      }

      if (!stats.isFile()) {
        res.writeHead(400, { "Content-Type": "text/plain" });
        res.end("Bad Request");
        return;
      }

      // Determine content type from extension
      const ext = path.extname(filepath).toLowerCase();
      const contentTypes = {
        ".jpg": "image/jpeg",
        ".jpeg": "image/jpeg",
        ".png": "image/png",
        ".gif": "image/gif",
        ".webp": "image/webp",
        ".bmp": "image/bmp",
      };

      const contentType = contentTypes[ext] || "application/octet-stream";

      res.writeHead(200, {
        "Content-Type": contentType,
        "Content-Length": stats.size,
        Connection: "keep-alive",
      });

      const readStream = fs.createReadStream(filepath);
      readStream.pipe(res);
    });
  } else if (req.method === "GET" && req.url === "/list") {
    // List available images
    fs.readdir(IMAGES_DIR, (err, files) => {
      if (err) {
        res.writeHead(500, { "Content-Type": "application/json" });
        res.end(JSON.stringify({ error: "Failed to list images" }));
        return;
      }

      const imageFiles = files.filter((f) => {
        const ext = path.extname(f).toLowerCase();
        return [".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp"].includes(ext);
      });

      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ images: imageFiles }));
    });
  } else {
    res.writeHead(404, { "Content-Type": "text/plain" });
    res.end("Not Found");
  }
});

server.listen(PORT, "127.0.0.1", () => {
  console.log(`HTTPS Image Server listening on https://127.0.0.1:${PORT}`);
  console.log(`Serving images from: ${IMAGES_DIR}`);
  console.log(`Using TLS certs from: ${CERT_PATH}`);
  console.log(`\nNote: Uses port 8080 so JTP can use 8443 for TLS`);
  console.log(`\nAvailable endpoints:`);
  console.log(`  GET /list - List all available images`);
  console.log(`  GET /image/<filename> - Download a specific image`);
});
