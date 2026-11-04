// A tiny MCP server for Heimdall's tests. Stdio by default (the server mcp.json names): every tools/list is appended
// to TINY_MCP_MARKER. With `--http <port>` it serves /mcp like Heimdall's own endpoint, and every method it receives is
// appended to TINY_MCP_HTTP_LOG.
import { appendFileSync } from 'node:fs';
import { createInterface } from 'node:readline';
import { createServer } from 'node:http';

function result(req) {
  switch (req.method) {
    case 'initialize':
      return { protocolVersion: req.params.protocolVersion, capabilities: { tools: {} }, serverInfo: { name: 'tiny', version: '1' } };
    case 'tools/list':
      return { tools: [{ name: 'ping', description: 'Replies pong', inputSchema: { type: 'object', properties: {} } }] };
    case 'tools/call':
      return { content: [{ type: 'text', text: 'pong' }] };
    case 'ping':
      return {};
    default:
      return undefined;
  }
}

// A notification (no id) gets no reply; an unknown request gets a JSON-RPC error.
function reply(req) {
  if (req.id === undefined) return undefined;
  const value = result(req);
  return value === undefined
    ? { jsonrpc: '2.0', id: req.id, error: { code: -32601, message: 'unknown method' } }
    : { jsonrpc: '2.0', id: req.id, result: value };
}

if (process.argv[2] === '--http') {
  createServer((request, response) => {
    let body = '';
    request.on('data', (chunk) => { body += chunk; });
    request.on('end', () => {
      if (request.method !== 'POST' || request.url !== '/mcp') { response.writeHead(404).end(); return; }
      const req = JSON.parse(body);
      appendFileSync(process.env.TINY_MCP_HTTP_LOG, req.method + '\n');
      const out = reply(req);
      if (!out) { response.writeHead(202).end(); return; }
      response.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify(out));
    });
  }).listen(Number(process.argv[3]), '127.0.0.1');
} else {
  createInterface({ input: process.stdin }).on('line', (line) => {
    if (!line.trim()) return;
    const req = JSON.parse(line);
    if (req.method === 'tools/list' && process.env.TINY_MCP_MARKER) appendFileSync(process.env.TINY_MCP_MARKER, 'tools/list\n');
    const out = reply(req);
    if (out) process.stdout.write(JSON.stringify(out) + '\n');
  });
}
