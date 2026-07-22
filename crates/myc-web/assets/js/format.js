// Pure formatting helpers — no state, no DOM. Unit-tested by scripts/jstest.mjs.

/** 1536 -> "1.5 KiB" */
export function fmtBytes(n) {
  n = Number(n) || 0;
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let u = 0;
  while (n >= 1024 && u < units.length - 1) { n /= 1024; u++; }
  return u === 0 ? n + " B" : n.toFixed(1) + " " + units[u];
}

/** "myc1-abcdef0123456789…" -> "myc1-abcdef012345" */
export function shortId(id) {
  id = String(id || "");
  return id.startsWith("myc1-") ? "myc1-" + id.slice(5, 17) : id.slice(0, 17);
}

/** 4000 (seconds) -> "1h 6m" */
export function fmtDur(secs) {
  secs = Math.max(0, Math.floor(secs));
  if (secs < 60) return secs + "s";
  if (secs < 3600) return Math.floor(secs / 60) + "m " + (secs % 60) + "s";
  if (secs < 86400) return Math.floor(secs / 3600) + "h " + Math.floor((secs % 3600) / 60) + "m";
  return Math.floor(secs / 86400) + "d " + Math.floor((secs % 86400) / 3600) + "h";
}

/** `nginx` -> `docker.io/library/nginx:latest` (matches server-side naming). */
export function canonicalImage(ref) {
  let name = ref, tag = "latest";
  const slash = ref.indexOf("/");
  const colon = ref.lastIndexOf(":");
  if (colon > slash) { name = ref.slice(0, colon); tag = ref.slice(colon + 1); }
  const parts = name.split("/");
  if (parts.length === 1) name = "docker.io/library/" + name;
  else if (parts.length === 2 && !parts[0].includes(".") && !parts[0].includes(":")) name = "docker.io/" + name;
  return name + ":" + tag;
}

/**
 * Split any reference into `{registry, namespace, shortName, tag, digest}`.
 *
 *   "docker.io/library/alpine:3.20"   -> docker.io / library  / alpine / 3.20
 *   "nginxinc/nginx-unprivileged"     -> docker.io / nginxinc / nginx-unprivileged / latest
 *   "ghcr.io/acme/sub/tool:v1"        -> ghcr.io   / acme/sub / tool   / v1
 *   "localhost:5000/app"              -> localhost:5000 / ""  / app    / latest
 *   "alpine@sha256:ab…"               -> tag "" and digest "sha256:ab…"
 *   "myc1-…" ids                      -> shortName is the shortened id, everything else ""
 */
export function parseRef(ref) {
  ref = String(ref || "").trim();
  const p = { registry: "docker.io", namespace: "library", shortName: "", tag: "latest", digest: "" };
  if (!ref) return { ...p, registry: "", namespace: "", tag: "" };
  if (ref.startsWith("myc1-")) return { registry: "", namespace: "", shortName: shortId(ref), tag: "", digest: "" };

  let rest = ref;
  const at = rest.indexOf("@");
  if (at !== -1) { p.digest = rest.slice(at + 1); rest = rest.slice(0, at); p.tag = ""; }
  const slash = rest.lastIndexOf("/");
  const colon = rest.lastIndexOf(":");
  if (colon > slash) { p.tag = rest.slice(colon + 1); rest = rest.slice(0, colon); }

  const parts = rest.split("/").filter(Boolean);
  p.shortName = parts[parts.length - 1] || "";
  if (parts.length > 1) {
    const first = parts[0];
    // A first segment with a dot or port (or "localhost") is a registry host.
    if (first.includes(".") || first.includes(":") || first === "localhost") {
      p.registry = first;
      p.namespace = parts.slice(1, -1).join("/");
      if (p.registry === "docker.io" && !p.namespace) p.namespace = "library";
    } else {
      p.namespace = parts.slice(0, -1).join("/");
    }
  }
  return p;
}

/** Muted prefix shown before the short name; "" for docker.io/library. */
export function refPrefix(p) {
  if (p.registry === "docker.io" || !p.registry) return p.namespace === "library" ? "" : p.namespace;
  return p.registry + (p.namespace ? "/" + p.namespace : "");
}

/** Compact plain-text form for toasts and options: "alpine:3.20", "nginxinc/nginx-unprivileged". */
export function shortRef(ref) {
  const p = parseRef(ref);
  if (!p.shortName) return String(ref || "");
  const prefix = refPrefix(p);
  let out = (prefix ? prefix + "/" : "") + p.shortName;
  if (p.digest) return out + "@" + p.digest.slice(0, 15) + "…";
  if (p.tag && p.tag !== "latest") out += ":" + p.tag;
  return out;
}

/** Deterministic hue (0-359) from a reference — used by the identicon avatars. */
export function refHue(ref) {
  const p = parseRef(ref);
  const key = refPrefix(p) + "/" + (p.shortName || String(ref || ""));
  let h = 5381;
  for (let i = 0; i < key.length; i++) h = ((h * 33) ^ key.charCodeAt(i)) >>> 0;
  return h % 360;
}
