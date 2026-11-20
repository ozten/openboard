// Certificate checks, against TLS servers made here rather than a real board.
// A test certificate authority and its leaves are made with `openssl` in a
// temporary directory; without `openssl` these tests are skipped.
//
// The client trusts the test authority through `boardFetch`'s internal `ca`
// option, which only this package's tests use, handed to `board()` as its
// `fetch`. The SDK's own certificate checks run unchanged.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash, X509Certificate } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, before, describe, test } from "node:test";
import tls from "node:tls";

import { board } from "../src/index.js";
import { boardFetch } from "../src/transport.js";

function haveOpenssl() {
  try {
    execFileSync("openssl", ["version"], { stdio: "ignore" });
    return true;
  } catch {
    return false;
  }
}

const skip = haveOpenssl() ? false : "openssl is not installed";

let dir;
const pem = {};

function openssl(...args) {
  execFileSync("openssl", args, { cwd: dir, stdio: ["ignore", "ignore", "pipe"] });
}

/** A key and a certificate for `san`, signed by the test authority or by itself. */
function leaf(name, san, { selfSigned = false } = {}) {
  const ext = join(dir, `${name}.ext`);
  writeFileSync(ext, `subjectAltName=${san}\nbasicConstraints=critical,CA:FALSE\nextendedKeyUsage=serverAuth\n`);
  const key = `${name}.key`;
  const crt = `${name}.crt`;
  if (selfSigned) {
    openssl("req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-keyout", key,
      "-out", crt, "-days", "2", "-subj", `/CN=${name}`, "-addext", `subjectAltName=${san}`);
  } else {
    openssl("req", "-new", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-keyout", key,
      "-out", `${name}.csr`, "-subj", `/CN=${name}`);
    openssl("x509", "-req", "-in", `${name}.csr`, "-CA", "ca.crt", "-CAkey", "ca.key", "-CAcreateserial",
      "-out", crt, "-days", "2", "-extfile", ext);
  }
  pem[name] = { key: readFileSync(join(dir, key)), cert: readFileSync(join(dir, crt)) };
}

function fingerprintOf(cert) {
  return `sha256:${createHash("sha256").update(new X509Certificate(cert).raw).digest("hex")}`;
}

/**
 * A TLS 1.3 server that records every byte it receives and answers any
 * complete HTTP request with a small JSON body.
 */
async function serve({ key, cert }) {
  const seen = { bytes: "", handshakes: 0 };
  const server = tls.createServer({ key, cert, minVersion: "TLSv1.3" }, (socket) => {
    seen.handshakes += 1;
    let text = "";
    socket.on("data", (chunk) => {
      text += chunk.toString("latin1");
      seen.bytes += chunk.toString("latin1");
      if (text.includes("\r\n\r\n")) {
        const body = JSON.stringify({ actor: { handle: "tls/test" } });
        socket.end(
          `HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: ${body.length}\r\nconnection: close\r\n\r\n${body}`,
        );
      }
    });
    socket.on("error", () => {});
  });
  server.on("tlsClientError", () => {});
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return { seen, url: `https://127.0.0.1:${server.address().port}`, close: () => server.close() };
}

describe("certificate checks", { skip }, () => {
  before(() => {
    dir = mkdtempSync(join(tmpdir(), "openboard-sdk-tls-"));
    openssl("req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-keyout", "ca.key",
      "-out", "ca.crt", "-days", "2", "-subj", "/CN=Openboard SDK test authority",
      "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign");
    pem.ca = readFileSync(join(dir, "ca.crt"));
    leaf("trusted", "IP:127.0.0.1");
    leaf("elsewhere", "DNS:other.example");
    leaf("selfsigned", "IP:127.0.0.1", { selfSigned: true });
  });

  after(() => {
    if (dir) {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  const TOKEN = "secret-token-that-must-not-leak";
  const caClient = (url) => board({ url, token: TOKEN, fetch: boardFetch(undefined, { ca: pem.ca }) });
  const pinnedClient = (url, fingerprint) =>
    board({ url, token: TOKEN, fetch: boardFetch(fingerprint, { ca: pem.ca }) });
  // Long enough for any bytes a client wrote to have arrived.
  const settle = () => new Promise((resolve) => setTimeout(resolve, 100));

  test("without a fingerprint, a certificate from a trusted authority for the host is accepted", async () => {
    const server = await serve(pem.trusted);
    try {
      assert.equal((await caClient(server.url).me()).actor.handle, "tls/test");
      assert.match(server.seen.bytes, new RegExp(`authorization: Bearer ${TOKEN}`, "i"));
    } finally {
      server.close();
    }
  });

  test("without a fingerprint, a self-signed certificate is refused before anything is sent", async () => {
    const server = await serve(pem.selfsigned);
    try {
      await assert.rejects(caClient(server.url).me(), (error) => {
        assert.equal(error.code, "certificate_untrusted");
        assert.match(error.message, /127\.0\.0\.1/);
        assert.match(error.message, /self-signed|SELF_SIGNED/i);
        assert.match(error.hint, /fingerprint/);
        return true;
      });
      await settle();
      assert.equal(server.seen.bytes, "");
    } finally {
      server.close();
    }
  });

  test("the global certificate authorities do not vouch for the test authority", async () => {
    const server = await serve(pem.trusted);
    try {
      // The same server, without the test authority: refused, so the
      // accepting test above passes only because of the authority it trusts.
      await assert.rejects(board({ url: server.url, token: TOKEN }).me(), { code: "certificate_untrusted" });
      await settle();
      assert.equal(server.seen.bytes, "");
    } finally {
      server.close();
    }
  });

  test("without a fingerprint, a trusted authority's certificate for another host is refused", async () => {
    const server = await serve(pem.elsewhere);
    try {
      await assert.rejects(caClient(server.url).me(), (error) => {
        assert.equal(error.code, "certificate_untrusted");
        assert.match(error.message, /ERR_TLS_CERT_ALTNAME_INVALID|altnames/);
        return true;
      });
      await settle();
      assert.equal(server.seen.bytes, "");
    } finally {
      server.close();
    }
  });

  test("a pin never falls back to the authorities, even for a certificate they vouch for", async () => {
    const server = await serve(pem.trusted);
    try {
      await assert.rejects(pinnedClient(server.url, fingerprintOf(pem.selfsigned.cert)).me(), {
        code: "certificate_mismatch",
      });
      await settle();
      assert.equal(server.seen.bytes, "");
      // The right pin is accepted.
      assert.equal((await pinnedClient(server.url, fingerprintOf(pem.trusted.cert)).me()).actor.handle, "tls/test");
    } finally {
      server.close();
    }
  });

  test("a pin accepts a self-signed certificate the authorities would refuse", async () => {
    const server = await serve(pem.selfsigned);
    try {
      assert.equal((await pinnedClient(server.url, fingerprintOf(pem.selfsigned.cert)).me()).actor.handle, "tls/test");
    } finally {
      server.close();
    }
  });

  test("an https URL with no fingerprint is no longer an error at construction", () => {
    assert.doesNotThrow(() => board({ url: "https://board.example", token: "t", fingerprint: undefined }));
  });
});
