import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";

const workspace = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const normalizedWorkspace = process.platform === "win32"
  ? workspace.replaceAll("\\", "/").toLowerCase()
  : workspace;

function matches(commandLine) {
  const command = process.platform === "win32"
    ? commandLine.replaceAll("\\", "/").toLowerCase()
    : commandLine;
  return command.includes("vite/bin/vite.js") && command.includes(normalizedWorkspace);
}

if (process.platform === "win32") {
  execFileSync("powershell", [
    "-NoProfile",
    "-ExecutionPolicy",
    "Bypass",
    "-File",
    path.join(path.dirname(fileURLToPath(import.meta.url)), "stop-vellum-vite.ps1"),
  ], { stdio: "inherit" });
} else {
  const output = execFileSync("ps", ["-axo", "pid=,command="], { encoding: "utf8" });
  for (const line of output.split("\n")) {
    const match = line.trim().match(/^(\d+)\s+(.*)$/);
    if (!match || Number(match[1]) === process.pid || !matches(match[2])) continue;
    try {
      process.kill(Number(match[1]), "SIGTERM");
      console.log(`Stopped stale Vellum Vite process ${match[1]}.`);
    } catch (error) {
      if (error?.code !== "ESRCH") throw error;
    }
  }
}
