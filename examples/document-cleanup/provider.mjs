/**
 * A stand-in for the model provider, so the example runs with no API key.
 *
 * It returns a typed verdict with a confidence score, as a System-1 style
 * model does. `0.86` is comfortably over the agent's bar, which is exactly
 * why nobody notices the decision is balanced on it.
 */
import { createServer } from "node:http";

const PORT = Number(process.env.PROVIDER_PORT ?? 8899);

createServer((req, res) => {
  let body = "";
  req.on("data", (c) => (body += c));
  req.on("end", () => {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ answer: "yes", confidence: 0.86 }));
  });
}).listen(PORT, "127.0.0.1", () => console.log(`provider on :${PORT}`));
