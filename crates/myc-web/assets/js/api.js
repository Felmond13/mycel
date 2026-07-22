// Single HTTP client for the JSON API. Every server error is normalized to
// a thrown `Error` whose message is the `{"error": "..."}` body (or the
// HTTP status line as a fallback) — components never see raw fetch().

async function request(path, opts) {
  const res = await fetch(path, opts);
  let body = null;
  try { body = await res.json(); } catch (_) { /* non-JSON error page */ }
  if (!res.ok) throw new Error((body && body.error) || (res.status + " " + res.statusText));
  return body;
}

function withBody(method, path, data) {
  return request(path, {
    method,
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(data || {}),
  });
}

export const api = {
  get: (path) => request(path),
  post: (path, data) => withBody("POST", path, data),
  put: (path, data) => withBody("PUT", path, data),
  del: (path) => request(path, { method: "DELETE" }),
};
