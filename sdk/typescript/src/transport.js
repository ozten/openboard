// How requests reach a board. A local board is plain HTTP on loopback. A
// network board is HTTPS over TLS 1.3, checked one of two ways, fixed by
// whether a fingerprint was given and never by what the server presents:
//
// - Pinned: the SHA-256 of the certificate's DER encoding must match the
//   pin. The chain and the hostname are not consulted.
// - Certificate authorities: the certificate must chain to an authority
//   Node trusts and name the host, as a browser checks it.
//
// Either way the check happens on the handshake, before a single byte of the
// request, and so the token, is written to the connection. There is no
// fallback from one to the other.

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
 * A `fetch`-shaped function for one board.
 *
 * With a fingerprint, every request goes over a TLS 1.3 connection whose
 * certificate matches the pin, and an `http://` URL is refused. Without one,
 * an `https://` request goes over a TLS 1.3 connection verified against the
 * certificate authorities and the hostname, and an `http://` request goes
 * through the global `fetch`.
 *
 * `internal.ca` is for this package's tests only: it replaces Node's
 * certificate authorities with the given ones, so a test can trust its own
 * authority. It has no effect on a pinned connection. It is not part of the
 * public API; this module is not exported from the package.
 */
export function boardFetch(fingerprint, internal = {}) {
  if (!fingerprint) {
    const ca = internal.ca;
    return (url, init = {}) => {
      const target = new URL(url);
      if (target.protocol === "https:") {
        return tlsRequest(target, init, { ca });
      }
      return globalThis.fetch(url, init);
    };
  }
  const pin = parseFingerprint(fingerprint);
  if (!pin) {
    throw new TypeError("the certificate fingerprint is not `sha256:` and 64 hex digits");
  }
  return (url, init = {}) => {
    const target = new URL(url);
    if (target.protocol !== "https:") {
      return Promise.reject(new TypeError("a pinned board is reached over https://"));
    }
    return tlsRequest(target, init, { pin });
  };
}

/** Checks a pinned connection's certificate. Returns an error, or null. */
function checkPin(socket, pin) {
  const cert = socket.getPeerCertificate(false);
  const presented = cert?.raw ? createHash("sha256").update(cert.raw).digest("hex") : "";
  if (presented === pin) {
    return null;
  }
  const error = new Error(
    `the board presented a certificate with fingerprint ${PREFIX}${presented}, not the pinned one`,
  );
  error.code = "certificate_mismatch";
  return error;
}

/**
 * One request over a fresh TLS 1.3 connection, handed to `http.request` only
 * once the certificate has passed: `{ pin }` checks the pin, `{ ca }` (or
 * nothing) has Node verify the chain and the hostname.
 */
function tlsRequest(url, init, { pin, ca }) {
  const port = Number(url.port || 443);
  const host = url.hostname.replace(/^\[|\]$/g, "");
  return new Promise((resolve, reject) => {
    const signal = init.signal;
    if (signal?.aborted) {
      reject(signal.reason ?? new DOMException("aborted", "AbortError"));
      return;
    }
    const createConnection = (_options, done) => {
      let settled = false;
      const finish = (error, socket) => {
        if (!settled) {
          settled = true;
          done(error, socket);
        }
      };
      const socket = tls.connect({
        host,
        port,
        servername: net.isIP(host) ? undefined : host,
        minVersion: "TLSv1.3",
        ...(pin
          ? // The chain is not what is trusted here; the pin is, below.
            { rejectUnauthorized: false }
          : // Node checks the chain and the hostname, and fails the
            // handshake before `secureConnect` if either is wrong.
            { rejectUnauthorized: true, ...(ca ? { ca } : {}) }),
      });
      socket.once("secureConnect", () => {
        const error = pin ? checkPin(socket, pin) : socket.authorized ? null : untrusted(host, socket.authorizationError);
        if (error) {
          socket.destroy();
          finish(error);
          return;
        }
        finish(null, socket);
      });
      socket.once("error", (error) => {
        // A failed verification sets `authorizationError` before the socket
        // is destroyed with it; anything else is a connection failure.
        if (!pin && socket.authorizationError) {
          finish(untrusted(host, socket.authorizationError, error));
          return;
        }
        finish(error);
      });
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

function untrusted(host, reason, cause) {
  const why = cause?.message ?? String(reason ?? "unknown reason");
  const code = reason && String(reason) !== why ? ` (${reason})` : "";
  const error = new Error(`the certificate of ${host} is not trusted: ${why}${code}`);
  error.code = "certificate_untrusted";
  if (cause) {
    error.cause = cause;
  }
  return error;
}
