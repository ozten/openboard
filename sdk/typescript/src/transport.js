// How requests reach a board. A local board is plain HTTP on loopback. A
// network board is HTTPS with its certificate pinned: the SHA-256 of the
// certificate's DER encoding must match the pin before a single byte of the
// request, and so the token, is written to the connection.

import { createHash } from "node:crypto";
import http from "node:http";
import net from "node:net";
import tls from "node:tls";

const PREFIX = "sha256:";

/** Parses `sha256:<64 hex>`. Returns the lowercase hex, or null. */
export function parseFingerprint(text) {
  if (typeof text !== "string" || !text.startsWith(PREFIX)) {
    return null;
  }
  const hex = text.slice(PREFIX.length).toLowerCase();
  return /^[0-9a-f]{64}$/.test(hex) ? hex : null;
}

/**
 * A `fetch`-shaped function for one board. With a fingerprint it opens a TLS
 * 1.3 connection, checks the pin on the handshake, and only then sends the
 * request over it. Without one it is the global `fetch`.
 */
export function boardFetch(fingerprint) {
  if (!fingerprint) {
    return (url, init) => globalThis.fetch(url, init);
  }
  const pin = parseFingerprint(fingerprint);
  if (!pin) {
    throw new TypeError("the certificate fingerprint is not `sha256:` and 64 hex digits");
  }
  return (url, init = {}) => pinnedRequest(new URL(url), init, pin);
}

function pinnedRequest(url, init, pin) {
  if (url.protocol !== "https:") {
    return Promise.reject(new TypeError("a pinned board is reached over https://"));
  }
  const port = Number(url.port || 443);
  const host = url.hostname.replace(/^\[|\]$/g, "");
  return new Promise((resolve, reject) => {
    const signal = init.signal;
    if (signal?.aborted) {
      reject(signal.reason ?? new DOMException("aborted", "AbortError"));
      return;
    }
    const createConnection = (_options, done) => {
      const socket = tls.connect({
        host,
        port,
        servername: net.isIP(host) ? undefined : host,
        minVersion: "TLSv1.3",
        // The chain is not what is trusted here; the pin is, below.
        rejectUnauthorized: false,
      });
      socket.once("secureConnect", () => {
        const cert = socket.getPeerCertificate(false);
        const presented = cert?.raw ? createHash("sha256").update(cert.raw).digest("hex") : "";
        if (presented !== pin) {
          const error = new Error(
            `the board presented a certificate with fingerprint ${PREFIX}${presented}, not the pinned one`,
          );
          error.code = "certificate_mismatch";
          socket.destroy();
          done(error);
          return;
        }
        done(null, socket);
      });
      socket.once("error", (error) => done(error));
    };
    const headers = { ...(init.headers || {}), host: url.host };
    const request = http.request(
      {
        method: init.method || "GET",
        path: url.pathname + url.search,
        headers,
        // With no agent, the request goes over the connection made above.
        createConnection,
      },
      (response) => {
        const chunks = [];
        response.on("data", (chunk) => chunks.push(chunk));
        response.on("end", () => {
          const body = Buffer.concat(chunks);
          const flat = {};
          for (const [name, value] of Object.entries(response.headers)) {
            flat[name] = Array.isArray(value) ? value.join(", ") : value;
          }
          resolve(new Response(body.length ? body : null, { status: response.statusCode, headers: flat }));
        });
        response.on("error", reject);
      },
    );
    const onAbort = () => request.destroy(signal.reason ?? new DOMException("aborted", "AbortError"));
    signal?.addEventListener("abort", onAbort, { once: true });
    request.on("close", () => signal?.removeEventListener("abort", onAbort));
    request.on("error", reject);
    if (init.body != null) {
      request.write(init.body);
    }
    request.end();
  });
}
