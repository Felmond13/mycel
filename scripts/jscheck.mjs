// Syntax-check every frontend ES module without executing it.
// Usage: node --experimental-vm-modules scripts/jscheck.mjs [assets/js dir]
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import vm from "node:vm";

const root = process.argv[2] || join(import.meta.dirname, "..", "crates", "myc-web", "assets", "js");

function walk(dir) {
  const out = [];
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) out.push(...walk(p));
    else if (name.endsWith(".js")) out.push(p);
  }
  return out;
}

let failed = 0;
const files = walk(root);
for (const f of files) {
  try {
    // Parses as an ES module (imports/exports allowed), never runs the code.
    new vm.SourceTextModule(readFileSync(f, "utf8"), { identifier: f });
    console.log("ok      " + f);
  } catch (e) {
    failed++;
    console.error("SYNTAX  " + f + ": " + e.message);
  }
}
console.log(files.length + " modules checked, " + failed + " failed");
process.exit(failed ? 1 : 0);
