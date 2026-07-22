// Unit tests for the pure helpers in assets/js/format.js.
// Usage: node scripts/jstest.mjs   (runs in the gate, no browser needed)
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const mod = join(import.meta.dirname, "..", "crates", "myc-web", "assets", "js", "format.js");
const { parseRef, refPrefix, shortRef, refHue, fmtBytes, shortId, canonicalImage } =
  await import(pathToFileURL(mod));

let failed = 0;
function eq(what, got, want) {
  const g = JSON.stringify(got), w = JSON.stringify(want);
  if (g === w) { console.log("ok      " + what); }
  else { failed++; console.error("FAIL    " + what + "\n        got  " + g + "\n        want " + w); }
}

/* ---- parseRef ---- */
eq("parseRef canonical docker.io/library",
  parseRef("docker.io/library/alpine:3.20"),
  { registry: "docker.io", namespace: "library", shortName: "alpine", tag: "3.20", digest: "" });
eq("parseRef bare name",
  parseRef("nginx"),
  { registry: "docker.io", namespace: "library", shortName: "nginx", tag: "latest", digest: "" });
eq("parseRef hub namespace",
  parseRef("nginxinc/nginx-unprivileged:latest"),
  { registry: "docker.io", namespace: "nginxinc", shortName: "nginx-unprivileged", tag: "latest", digest: "" });
eq("parseRef ghcr nested namespace",
  parseRef("ghcr.io/acme/sub/tool:v1"),
  { registry: "ghcr.io", namespace: "acme/sub", shortName: "tool", tag: "v1", digest: "" });
eq("parseRef registry port",
  parseRef("localhost:5000/app"),
  { registry: "localhost:5000", namespace: "", shortName: "app", tag: "latest", digest: "" });
eq("parseRef registry port with tag",
  parseRef("registry.example.com:8443/team/app:2.1"),
  { registry: "registry.example.com:8443", namespace: "team", shortName: "app", tag: "2.1", digest: "" });
eq("parseRef digest",
  parseRef("alpine@sha256:0123456789abcdef"),
  { registry: "docker.io", namespace: "library", shortName: "alpine", tag: "", digest: "sha256:0123456789abcdef" });
eq("parseRef digest with registry",
  parseRef("ghcr.io/acme/tool@sha256:ff00"),
  { registry: "ghcr.io", namespace: "acme", shortName: "tool", tag: "", digest: "sha256:ff00" });
eq("parseRef docker.io two segments implies library",
  parseRef("docker.io/alpine:edge"),
  { registry: "docker.io", namespace: "library", shortName: "alpine", tag: "edge", digest: "" });
eq("parseRef manifest id",
  parseRef("myc1-d5f20fd353800e5cd1d7224adc44efc7"),
  { registry: "", namespace: "", shortName: "myc1-d5f20fd35380", tag: "", digest: "" });
eq("parseRef empty", parseRef(""),
  { registry: "", namespace: "", shortName: "", tag: "", digest: "" });

/* ---- refPrefix / shortRef ---- */
eq("refPrefix hides docker.io/library", refPrefix(parseRef("docker.io/library/alpine:3.20")), "");
eq("refPrefix keeps hub namespace", refPrefix(parseRef("nginxinc/nginx-unprivileged")), "nginxinc");
eq("refPrefix keeps foreign registry", refPrefix(parseRef("ghcr.io/acme/tool:v1")), "ghcr.io/acme");
eq("shortRef canonical", shortRef("docker.io/library/alpine:3.20"), "alpine:3.20");
eq("shortRef latest tag hidden", shortRef("docker.io/nginxinc/nginx-unprivileged:latest"), "nginxinc/nginx-unprivileged");
eq("shortRef registry port", shortRef("localhost:5000/app:dev"), "localhost:5000/app:dev");
eq("shortRef digest", shortRef("alpine@sha256:0123456789abcdef"), "alpine@sha256:01234567…");
eq("shortRef passthrough id", shortRef("myc1-d5f20fd353800e5cd1d7224adc44efc7"), "myc1-d5f20fd35380");

/* ---- refHue: deterministic, in range, tag-independent ---- */
eq("refHue deterministic", refHue("alpine:3.20") === refHue("docker.io/library/alpine:3.21"), true);
eq("refHue range", refHue("postgres") >= 0 && refHue("postgres") < 360, true);
eq("refHue differs by namespace", refHue("nginx") !== refHue("nginxinc/nginx-unprivileged"), true);

/* ---- existing helpers stay stable ---- */
eq("fmtBytes", fmtBytes(1536), "1.5 KiB");
eq("shortId", shortId("myc1-d5f20fd353800e5cd1d7224a"), "myc1-d5f20fd35380");
eq("canonicalImage", canonicalImage("nginx"), "docker.io/library/nginx:latest");
eq("canonicalImage namespace", canonicalImage("nginxinc/nginx-unprivileged"), "docker.io/nginxinc/nginx-unprivileged:latest");

console.log(failed ? failed + " test(s) FAILED" : "all format.js tests passed");
process.exit(failed ? 1 : 0);
