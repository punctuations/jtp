const http = require("http");
const fs = require("fs");
const path = require("path");

const PORT = 8080;
const IMAGES_DIR = path.join(__dirname, "..", "..", "..", "images");

const server = http.createServer((req, res) => {
  // Enable CORS
  res.setHeader("Access-Control-Allow-Origin", "*");
  res.setHeader("Access-Control-Allow-Methods", "GET");

  if (req.method === "GET" && req.url.startsWith("/image/")) {
    const filename = req.url.substring("/image/".length);
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
  console.log(`HTTP Image Server listening on http://127.0.0.1:${PORT}`);
  console.log(`Serving images from: ${IMAGES_DIR}`);
  console.log(`Available endpoints:`);
  console.log(`  GET /list - List all available images`);
  console.log(`  GET /image/<filename> - Download a specific image`);
});
