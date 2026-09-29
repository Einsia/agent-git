// The fixture has a deliberately public test key; never use this script with real private data.
import assert from 'node:assert/strict';
import { createHash, webcrypto } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { resolve } from 'node:path';

const [fixturePath, sodiumPackage, outputPath] = process.argv.slice(2);
if (!fixturePath || !sodiumPackage) {
  throw new Error('Usage: node scripts/privacy-envelope-interop.mjs FIXTURE SODIUM_PACKAGE_JSON [BROWSER_FIXTURE]');
}
const sodium = createRequire(resolve(sodiumPackage))('libsodium-wrappers-sumo');
await sodium.ready;
const fixture = JSON.parse(readFileSync(fixturePath, 'utf8'));
const decode = (text) => Buffer.from(text, 'base64');
const encode = (bytes) => Buffer.from(bytes).toString('base64');

// JSON.stringify(object) reorders integer-like keys even after insertion into a sorted object.
function canonical(value) {
  if (value === null || typeof value === 'boolean' || typeof value === 'string') return JSON.stringify(value);
  if (typeof value === 'number') {
    if (!Number.isSafeInteger(value)) throw new Error('Privacy numbers must be safe integers');
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`;
  const keys = Object.keys(value).sort((a, b) => Buffer.compare(Buffer.from(a), Buffer.from(b)));
  return `{${keys.map((key) => `${JSON.stringify(key)}:${canonical(value[key])}`).join(',')}}`;
}

function associatedData(envelope) {
  const { format_version, policy_digest, snapshot_digest, public_projection, attachments, private_payload } = envelope;
  const { algorithm, wrapped_keys } = private_payload;
  const digest = createHash('sha256').update(canonical({
    algorithm, attachments, format_version, policy_digest, public_projection, snapshot_digest, wrapped_keys,
  })).digest('hex');
  return Buffer.from(`agit-privacy-envelope-v1\0sha256:${digest}`);
}

const envelope = fixture.envelope;
const publicKey = decode(fixture.viewing_public_key);
const privateKey = decode(fixture.test_only_viewing_private_key);
const objectKey = sodium.crypto_box_seal_open(
  decode(envelope.private_payload.wrapped_keys[0].ciphertext), publicKey, privateKey,
);
const aad = associatedData(envelope);
assert.equal(encode(aad), fixture.aad_base64);
const key = await webcrypto.subtle.importKey('raw', objectKey, 'AES-GCM', false, ['decrypt']);
const plaintext = await webcrypto.subtle.decrypt({
  name: 'AES-GCM', iv: decode(envelope.private_payload.nonce), additionalData: aad,
}, key, decode(envelope.private_payload.ciphertext));
assert.equal(encode(plaintext), fixture.private_payload_base64);

if (outputPath) {
  const browserKey = sodium.randombytes_buf(32);
  const nonce = sodium.randombytes_buf(12);
  envelope.private_payload.wrapped_keys[0].ciphertext = encode(sodium.crypto_box_seal(browserKey, publicKey));
  envelope.private_payload.nonce = encode(nonce);
  const newAad = associatedData(envelope);
  const aes = await webcrypto.subtle.importKey('raw', browserKey, 'AES-GCM', false, ['encrypt']);
  envelope.private_payload.ciphertext = encode(await webcrypto.subtle.encrypt({
    name: 'AES-GCM', iv: nonce, additionalData: newAad,
  }, aes, decode(fixture.private_payload_base64)));
  fixture.aad_base64 = encode(newAad);
  writeFileSync(outputPath, `${JSON.stringify(fixture, null, 2)}\n`);
  sodium.memzero(browserKey);
}
sodium.memzero(objectKey);
sodium.memzero(privateKey);
console.log('CLI envelope decrypted with libsodium and WebCrypto; canonical AAD matches.');
