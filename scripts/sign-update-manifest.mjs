import {
  createPrivateKey,
  createPublicKey,
  sign,
  timingSafeEqual,
} from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";

const [manifestPath, signaturePath] = process.argv.slice(2);
const signingHex = process.env.VELLUM_UPDATE_SIGNING_KEY?.trim();
const expectedPublicHex = process.env.VELLUM_UPDATE_PUBLIC_KEY?.trim().toLowerCase();

if (!manifestPath || !signaturePath || !signingHex || !expectedPublicHex) {
  throw new Error(
    "usage: sign-update-manifest.mjs <manifest> <signature>; signing and public keys must be set",
  );
}

const seed = Buffer.from(signingHex, "hex");
const expectedPublic = Buffer.from(expectedPublicHex, "hex");
if (seed.length !== 32 || expectedPublic.length !== 32) {
  throw new Error("Ed25519 signing seed and public key must each be 32 bytes");
}

// RFC 8410 PKCS#8 prefix for a raw 32-byte Ed25519 private seed.
const pkcs8 = Buffer.concat([
  Buffer.from("302e020100300506032b657004220420", "hex"),
  seed,
]);
const privateKey = createPrivateKey({ key: pkcs8, format: "der", type: "pkcs8" });
const publicDer = createPublicKey(privateKey).export({ format: "der", type: "spki" });
const actualPublic = publicDer.subarray(publicDer.length - 32);
if (!timingSafeEqual(actualPublic, expectedPublic)) {
  throw new Error("signing key does not match the public key embedded in release builds");
}

const signature = sign(null, readFileSync(manifestPath), privateKey);
if (signature.length !== 64) {
  throw new Error(`unexpected Ed25519 signature length ${signature.length}`);
}
writeFileSync(signaturePath, signature);
