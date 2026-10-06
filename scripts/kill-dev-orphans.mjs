#!/usr/bin/env node
// Kills stray dev processes left over from a force-closed terminal or a
// killed debugger session: a Vite dev server still bound to port 1420, and
// any orphaned uia/uia-app binary. Run before `tauri dev` so it never fails
// with "port already in use".

import { execSync } from "node:child_process";

const PORT = 1420;
const PROCESS_NAMES = ["uia", "uia-app"];

function run(cmd) {
  try {
    return execSync(cmd, { stdio: ["ignore", "pipe", "ignore"] }).toString();
  } catch {
    return "";
  }
}

function findWindowsPids() {
  const pids = new Set();

  for (const line of run("netstat -ano").split("\n")) {
    if (line.includes(`:${PORT}`) && line.includes("LISTENING")) {
      const pid = line.trim().split(/\s+/).pop();
      if (pid && pid !== "0") pids.add(pid);
    }
  }

  for (const name of PROCESS_NAMES) {
    const out = run(`tasklist /FI "IMAGENAME eq ${name}.exe" /FO CSV /NH`);
    for (const line of out.split("\n")) {
      const match = line.match(/"[^"]+","(\d+)"/);
      if (match) pids.add(match[1]);
    }
  }

  return pids;
}

function findUnixPids() {
  const pids = new Set();

  for (const pid of run(`lsof -ti tcp:${PORT}`).split("\n")) {
    if (pid.trim()) pids.add(pid.trim());
  }

  for (const name of PROCESS_NAMES) {
    for (const pid of run(`pgrep -f "${name}"`).split("\n")) {
      if (pid.trim()) pids.add(pid.trim());
    }
  }

  return pids;
}

const pids = process.platform === "win32" ? findWindowsPids() : findUnixPids();

if (pids.size === 0) {
  console.log("No lingering dev processes found.");
  process.exit(0);
}

for (const pid of pids) {
  console.log(`Killing orphaned dev process PID ${pid}`);
  if (process.platform === "win32") {
    run(`taskkill /PID ${pid} /F /T`);
  } else {
    run(`kill -9 ${pid}`);
  }
}
