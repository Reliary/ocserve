// R1 logging CONNECT proxy: proves whether the opencode ELF honors
// https_proxy env for its LLM path, and logs every destination it dials.
// The completion still succeeds (tunnel), so "no CONNECT lines" cleanly
// distinguishes "proxy ignored" from "proxy honored".
const http = require("http");
const net = require("net");

const server = http.createServer((_req, res) => {
  res.writeHead(405);
  res.end();
});

server.on("connect", (req, clientSocket, head) => {
  const t = new Date().toISOString();
  const [host, port] = req.url.split(":");
  console.log(`CONNECT ${req.url} ${t}`);
  const upstream = net.connect(Number(port) || 443, host, () => {
    clientSocket.write("HTTP/1.1 200 Connection Established\r\n\r\n");
    if (head && head.length) upstream.write(head);
    let up = 0, down = 0;
    upstream.on("data", (c) => (up += c.length));
    clientSocket.on("data", (c) => (down += c.length));
    upstream.on("end", () => console.log(`CLOSE ${req.url} up=${up} down=${down}`));
    upstream.pipe(clientSocket);
    clientSocket.pipe(upstream);
  });
  upstream.on("error", (e) => {
    console.log(`UP_ERR ${req.url} ${e.message}`);
    clientSocket.destroy();
  });
  clientSocket.on("error", () => upstream.destroy());
});

server.listen(8888, "127.0.0.1", () => console.log("proxy-log on 127.0.0.1:8888"));
