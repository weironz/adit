// Read-only, dependency-free request of NetBird 0.81.2 public peer-key metadata.
// Official schema: client/proto/daemon.proto; only GetPeerSSHHostKey is called.
// No key bytes, credentials, authentication prompts or gRPC bodies are saved.
const http2 = require('node:http2');
const { Duplex } = require('node:stream');
const { spawn } = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const privateDir = process.argv[2];
if (!privateDir || !fs.statSync(privateDir).isDirectory()) {
  throw new Error('Existing owned evidence directory required');
}
const summary = {
  utc: new Date().toISOString(),
  transport: 'protected Windows named pipe with Identification',
  rpc: '/daemon.DaemonService/GetPeerSSHHostKey',
  read_only: true,
  raw_key_saved: false,
  results: [],
};
const bridge = spawn('pwsh.exe', [
  '-NoLogo', '-NoProfile', '-NonInteractive', '-File',
  path.join(__dirname, 'netbird-peer-key-transport.ps1'),
], { stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true });
const stderr = [];
bridge.stderr.on('data', chunk => stderr.push(chunk));
bridge.on('error', error => { summary.bridge_error = error.code || error.name; });
bridge.on('exit', code => { summary.bridge_exit = code; });
const duplex = Duplex.from({ readable: bridge.stdout, writable: bridge.stdin });
const client = http2.connect('http://localhost', { createConnection: () => duplex });
client.on('error', error => { summary.session_error = error.code || error.name; });

function varint(value) {
  const bytes = [];
  do {
    let byte = value & 127;
    value = Math.floor(value / 128);
    if (value) byte |= 128;
    bytes.push(byte);
  } while (value);
  return Buffer.from(bytes);
}
function fieldsFrom(buffer) {
  let position = 0;
  const fields = new Map();
  function readVarint() {
    let value = 0, multiplier = 1, count = 0;
    while (position < buffer.length && count++ < 10) {
      const byte = buffer[position++];
      value += (byte & 127) * multiplier;
      if (!(byte & 128)) return value;
      multiplier *= 128;
    }
    throw new Error('invalid_varint');
  }
  while (position < buffer.length) {
    const tag = readVarint(), field = Math.floor(tag / 8), wire = tag & 7;
    if (wire === 0) fields.set(field, readVarint());
    else if (wire === 2) {
      const size = readVarint();
      if (size > buffer.length - position) throw new Error('invalid_length');
      fields.set(field, buffer.subarray(position, position + size));
      position += size;
    } else throw new Error('unexpected_wire_type');
  }
  return fields;
}
async function lookup(peer) {
  return new Promise(resolve => {
    const result = { peer_address: peer };
    const address = Buffer.from(peer, 'utf8');
    const protobuf = Buffer.concat([Buffer.from([10]), varint(address.length), address]);
    const framed = Buffer.alloc(5 + protobuf.length);
    framed.writeUInt32BE(protobuf.length, 1);
    protobuf.copy(framed, 5);
    let request;
    try {
      request = client.request({
        ':method': 'POST', ':path': summary.rpc, ':authority': 'localhost',
        'content-type': 'application/grpc', te: 'trailers', 'grpc-timeout': '5S',
      });
    } catch (error) {
      result.error = error.code || error.name;
      resolve(result);
      return;
    }
    const chunks = [];
    let headers = {}, trailers = {}, done = false;
    const timer = setTimeout(() => {
      result.error = 'local_rpc_timeout';
      request.close();
      finish();
    }, 6000);
    function finish() {
      if (done) return;
      done = true;
      clearTimeout(timer);
      const data = Buffer.concat(chunks);
      result.http_status = headers[':status'] ?? null;
      result.grpc_status = String(trailers['grpc-status'] ?? headers['grpc-status'] ?? 'missing');
      result.response_bytes = data.length;
      try {
        if (result.grpc_status === '0') {
          if (data.length < 5 || data[0] !== 0 || data.readUInt32BE(1) !== data.length - 5) {
            throw new Error('invalid_grpc_frame');
          }
          const fields = fieldsFrom(data.subarray(5));
          const key = fields.get(1) || Buffer.alloc(0);
          result.found = fields.get(4) === 1;
          result.key_bytes = key.length;
          result.key_sha256 = crypto.createHash('sha256').update(key).digest('hex');
          const authorized = /^(ssh-[a-z0-9-]+|ecdsa-sha2-[a-z0-9-]+)\s+([A-Za-z0-9+/=]+)/.exec(key.toString('ascii'));
          result.key_algorithm = authorized ? authorized[1] : null;
          result.key_parseable_authorized_form = Boolean(authorized);
          if (authorized) {
            result.key_blob_sha256 = crypto.createHash('sha256')
              .update(Buffer.from(authorized[2], 'base64')).digest('hex');
          }
          result.peer_ip = fields.get(2)?.toString('utf8') ?? '';
          result.peer_fqdn = fields.get(3)?.toString('utf8') ?? '';
        }
      } catch (error) { result.decode_error = error.message; }
      resolve(result);
    }
    request.on('response', value => { headers = value; });
    request.on('trailers', value => { trailers = value; });
    request.on('data', chunk => chunks.push(chunk));
    request.on('end', finish);
    request.on('error', error => { result.error = error.code || error.name; finish(); });
    request.end(framed);
  });
}
const watchdog = setTimeout(() => {
  summary.watchdog = true;
  client.destroy();
  bridge.kill();
}, 16000);
(async () => {
  try {
    // Match the official callback's hostname-with-port then remote-IP lookup.
    for (const peer of ['willoptpc-172-11.netbird.selfhosted:22', '100.70.172.11']) {
      summary.results.push(await lookup(peer));
    }
  } catch (error) { summary.error = error.code || error.name; }
  finally {
    clearTimeout(watchdog);
    client.close();
    client.destroy();
    bridge.stdin.end();
    await new Promise(resolve => {
      if (bridge.exitCode !== null) resolve();
      else {
        bridge.once('exit', resolve);
        setTimeout(() => { bridge.kill(); resolve(); }, 1500).unref();
      }
    });
    const bridgeStderr = Buffer.concat(stderr);
    fs.writeFileSync(path.join(privateDir, 'identified-rpc-bridge.stderr.private.txt'), bridgeStderr);
    summary.bridge_stderr_bytes = bridgeStderr.length;
    summary.bridge_stderr_sha256 = crypto.createHash('sha256').update(bridgeStderr).digest('hex');
    fs.writeFileSync(path.join(privateDir, 'identified-peer-key-rpc-summary.json'), JSON.stringify(summary, null, 2) + '\n');
    console.log(JSON.stringify(summary, null, 2));
  }
})();
