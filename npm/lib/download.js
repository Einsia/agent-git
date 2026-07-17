'use strict';

// Dependency-free downloader built on Node's core http/https/tls. Keeping the
// npm package dependency-less means `npm install` cannot fail on a broken
// transitive dep, and there is no third-party code in the install path.
//
// Handles the things a real release download must handle:
//   • HTTPS redirects (GitHub release URLs 302 to objects.githubusercontent.com)
//   • Corporate/CI proxies via CONNECT tunnelling, honouring the standard and
//     npm_config_* proxy env vars (and NO_PROXY)
//   • request timeouts
//   • non-2xx responses surfaced as loud errors (never a silent empty file)

const http = require('http');
const https = require('https');
const tls = require('tls');

const MAX_REDIRECTS = 5;
const DEFAULT_TIMEOUT_MS = Number(process.env.AGIT_DOWNLOAD_TIMEOUT_MS) || 60000;

function env(name) {
  return process.env[name] || process.env[name.toLowerCase()] || process.env[name.toUpperCase()];
}

// Resolve the proxy URL (if any) that applies to `target`, honouring NO_PROXY.
function proxyFor(target) {
  const noProxy = env('no_proxy') || '';
  if (noProxy === '*') return null;
  const host = target.hostname;
  for (const entry of noProxy.split(',').map((s) => s.trim()).filter(Boolean)) {
    const bare = entry.replace(/^\./, '');
    if (host === bare || host.endsWith('.' + bare) || host.endsWith(bare)) return null;
  }

  const isHttps = target.protocol === 'https:';
  const candidates = isHttps
    ? ['npm_config_https_proxy', 'HTTPS_PROXY', 'npm_config_proxy', 'ALL_PROXY', 'HTTP_PROXY']
    : ['npm_config_proxy', 'HTTP_PROXY', 'ALL_PROXY'];
  for (const name of candidates) {
    const val = env(name);
    if (val) return val;
  }
  return null;
}

function baseHeaders(target) {
  return {
    Host: target.host,
    'User-Agent': 'agit-npm-installer',
    Accept: '*/*',
  };
}

function proxyAuthHeader(proxyUrl) {
  if (!proxyUrl.username && !proxyUrl.password) return {};
  const creds = `${decodeURIComponent(proxyUrl.username)}:${decodeURIComponent(proxyUrl.password)}`;
  return { 'Proxy-Authorization': 'Basic ' + Buffer.from(creds).toString('base64') };
}

// Open a response stream for a single (non-redirected) GET, choosing a direct
// connection or a proxy CONNECT tunnel. Invokes cb(err, res).
function openResponse(target, cb) {
  const proxy = proxyFor(target);

  if (!proxy) {
    const mod = target.protocol === 'https:' ? https : http;
    const req = mod.request(
      {
        protocol: target.protocol,
        hostname: target.hostname,
        port: target.port || (target.protocol === 'https:' ? 443 : 80),
        path: target.pathname + target.search,
        method: 'GET',
        headers: baseHeaders(target),
      },
      (res) => cb(null, res)
    );
    armTimeout(req, cb);
    req.on('error', cb);
    req.end();
    return;
  }

  const proxyUrl = new URL(proxy);

  // Plain HTTP through a proxy: forward the absolute-form request line.
  if (target.protocol === 'http:') {
    const req = http.request(
      {
        host: proxyUrl.hostname,
        port: Number(proxyUrl.port) || 80,
        method: 'GET',
        path: target.href,
        headers: Object.assign(baseHeaders(target), proxyAuthHeader(proxyUrl)),
      },
      (res) => cb(null, res)
    );
    armTimeout(req, cb);
    req.on('error', cb);
    req.end();
    return;
  }

  // HTTPS through a proxy: CONNECT tunnel, then TLS over the raw socket.
  const connectReq = http.request({
    host: proxyUrl.hostname,
    port: Number(proxyUrl.port) || 80,
    method: 'CONNECT',
    path: `${target.hostname}:${target.port || 443}`,
    headers: Object.assign(
      { Host: `${target.hostname}:${target.port || 443}` },
      proxyAuthHeader(proxyUrl)
    ),
  });
  armTimeout(connectReq, cb);
  connectReq.on('error', cb);
  connectReq.on('connect', (res, socket) => {
    if (res.statusCode !== 200) {
      cb(new Error(`proxy CONNECT to ${target.hostname} failed with status ${res.statusCode}`));
      socket.destroy();
      return;
    }
    const tlsSocket = tls.connect({ socket, servername: target.hostname });
    tlsSocket.on('error', cb);
    const req = https.request(
      {
        method: 'GET',
        path: target.pathname + target.search,
        headers: baseHeaders(target),
        createConnection: () => tlsSocket,
      },
      (r) => cb(null, r)
    );
    armTimeout(req, cb);
    req.on('error', cb);
    req.end();
  });
  connectReq.end();
}

function armTimeout(req, cb) {
  req.setTimeout(DEFAULT_TIMEOUT_MS, () => {
    req.destroy(new Error(`request timed out after ${DEFAULT_TIMEOUT_MS}ms`));
  });
}

// Follow one GET, transparently chasing redirects, and resolve with a Buffer of
// the response body. Rejects loudly on network error or any non-2xx status.
function fetchBuffer(urlStr, redirectsLeft) {
  const remaining = typeof redirectsLeft === 'number' ? redirectsLeft : MAX_REDIRECTS;
  return new Promise((resolve, reject) => {
    let target;
    try {
      target = new URL(urlStr);
    } catch (e) {
      reject(new Error(`invalid URL: ${urlStr}`));
      return;
    }

    openResponse(target, (err, res) => {
      if (err) {
        reject(err);
        return;
      }

      const status = res.statusCode;

      if (status >= 300 && status < 400 && res.headers.location) {
        res.resume(); // drain
        if (remaining <= 0) {
          reject(new Error(`too many redirects while fetching ${urlStr}`));
          return;
        }
        const next = new URL(res.headers.location, target).href;
        fetchBuffer(next, remaining - 1).then(resolve, reject);
        return;
      }

      if (status < 200 || status >= 300) {
        // Read a little of the body to give a useful message, then fail.
        const preview = [];
        let n = 0;
        res.on('data', (c) => {
          if (n < 512) {
            preview.push(c);
            n += c.length;
          }
        });
        res.on('end', () => {
          const snippet = Buffer.concat(preview).toString('utf8').replace(/\s+/g, ' ').trim();
          reject(
            new Error(
              `HTTP ${status} for ${urlStr}` + (snippet ? ` — ${snippet.slice(0, 200)}` : '')
            )
          );
        });
        res.on('error', reject);
        return;
      }

      const chunks = [];
      res.on('data', (c) => chunks.push(c));
      res.on('end', () => resolve(Buffer.concat(chunks)));
      res.on('error', reject);
    });
  });
}

module.exports = { fetchBuffer, proxyFor };
