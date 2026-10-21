import { test } from "node:test";
import assert from "node:assert/strict";
import { createHash, randomBytes } from "node:crypto";

import { sha256, fingerprintFromSdp, pairingCode } from "../../client/pairing.mjs";

const HOST_FP = "AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99";
const VIEWER_FP = "01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef";

test("sha256 matches Node's at every padding boundary", () => {
  for (let length = 0; length < 200; length++) {
    const bytes = randomBytes(length);
    const expected = createHash("sha256").update(bytes).digest("hex");
    assert.equal(Buffer.from(sha256(bytes)).toString("hex"), expected, "length " + length);
  }
});

// Same vector as pairing_code_matches_shared_vector in host/src/pairing.rs.
test("pairing code matches the host's", () => {
  assert.equal(pairingCode(HOST_FP, VIEWER_FP), "3ES8-F2PT-1MEY");
});

test("pairing code depends on order and both keys, not case", () => {
  const code = pairingCode(HOST_FP, VIEWER_FP);
  assert.notEqual(code, pairingCode(VIEWER_FP, HOST_FP));
  assert.notEqual(code, pairingCode(HOST_FP, HOST_FP));
  assert.equal(code, pairingCode(HOST_FP.toLowerCase(), VIEWER_FP));
});

test("fingerprintFromSdp reads and checks every line", () => {
  const sdp = `v=0\r\na=fingerprint:sha-256 ${HOST_FP}\r\nm=video 9\r\na=fingerprint:sha-256 ${HOST_FP}\r\n`;
  assert.equal(fingerprintFromSdp(sdp), HOST_FP.toLowerCase());
  assert.equal(
    fingerprintFromSdp(`a=fingerprint:sha-256 ${HOST_FP}\r\na=fingerprint:sha-256 ${VIEWER_FP}\r\n`),
    null,
  );
  assert.equal(fingerprintFromSdp("a=fingerprint:sha-1 AB:CD\r\n"), null);
  assert.equal(fingerprintFromSdp("v=0\r\n"), null);
});
