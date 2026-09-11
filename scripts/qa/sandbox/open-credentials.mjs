import { mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import { openCredentials } from "./credential-channel.mjs";

const secretsDir = process.argv[2];
const privateDir = process.argv[3];
const publicDir = process.argv[4];
const keyPath = process.argv[5] || "C:\\QaOnce\\once.key";
if (!secretsDir || !privateDir) {
  process.stderr.write("usage: open-credentials.mjs <secretsDir> <privateDir> [publicDir] [keyPath]\n");
  process.exit(2);
}

const key = readFileSync(keyPath, "utf8").trim();
rmSync(keyPath, { force: true });
const opened = openCredentials(secretsDir, key);
mkdirSync(privateDir, { recursive: true });
writeFileSync(path.join(privateDir, "opened.json"), `${JSON.stringify(opened)}\n`);
if (publicDir) {
  const fields = Object.keys(opened).filter((keyName) => opened[keyName] && keyName !== "fields");
  writeFileSync(
    path.join(publicDir, "credential-fields.json"),
    `${JSON.stringify({ fields, storedVia: "pending-product-ui" }, null, 2)}\n`,
  );
}
