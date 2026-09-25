const http = require("http");

const port = Number(process.env.PORT || 3000);
const server = http.createServer((_request, response) => {
  response.writeHead(200, { "content-type": "text/plain" });
  response.end("Hello from Compute\n");
});

server.listen(port, "0.0.0.0", () => {
  console.log(`compute-demo listening on ${port}`);
});
