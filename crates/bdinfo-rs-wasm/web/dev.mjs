// A tiny zero-dependency static server for local demo development: serves this
// web/ package directory (index.html + the built dist/ and pkg/) over http, so
// the module Worker and the WebAssembly fetch work — neither runs over file://.
// `npm run dev`, then open the printed URL and pick a disc's BDMV folder.
//
// It only serves what is already on disk, so build the artifacts first with
// `npm run build` (so dist/ and pkg/ exist). Port via the PORT env (default 8787).
import { createReadStream, statSync } from "node:fs";
import { createServer } from "node:http";
import { dirname, extname, join, normalize, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(fileURLToPath(import.meta.url)); // crates/bdinfo-rs-wasm/web
const port = Number.parseInt(process.env.PORT ?? "8787", 10);

// A module Worker requires a JavaScript MIME type, and instantiateStreaming
// requires application/wasm; everything else is best-effort.
const types = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".wasm": "application/wasm",
  ".json": "application/json; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".map": "application/json; charset=utf-8",
  ".txt": "text/plain; charset=utf-8",
};

const server = createServer((request, response) => {
  // Drop any query/hash, default "/" to index.html, and resolve INSIDE root.
  const urlPath = decodeURIComponent((request.url ?? "/").split(/[?#]/)[0]);
  const filePath = resolve(join(root, normalize(urlPath === "/" ? "/index.html" : urlPath)));
  if (filePath !== root && !filePath.startsWith(`${root}\\`) && !filePath.startsWith(`${root}/`)) {
    response.writeHead(403, { "content-type": "text/plain" }).end("403 forbidden");
    return;
  }
  let size = 0;
  try {
    const stat = statSync(filePath);
    if (!stat.isFile()) {
      throw new Error("not a file");
    }
    size = stat.size;
  } catch {
    response.writeHead(404, { "content-type": "text/plain" }).end("404 not found");
    return;
  }
  response.writeHead(200, {
    "content-type": types[extname(filePath)] ?? "application/octet-stream",
    "content-length": size,
    "cache-control": "no-store", // always serve the freshest rebuild
  });
  createReadStream(filePath).pipe(response);
});

server.listen(port, "127.0.0.1", () => {
  console.log(`bdinfo-rs demo: http://localhost:${port}/   (serving ${root})`);
  console.log("if the page is blank, build the artifacts first: npm run build");
});
