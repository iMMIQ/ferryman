import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { api, ApiError, latestQuery } from "../web/src/lib/api";
let server: http.Server, origin: string;
before(async () => {
  server = http.createServer((req, res) => {
    if (req.url === "/slow") {
      const timer = setTimeout(() => res.end("{}"), 500);
      req.on("close", () => clearTimeout(timer));
      return;
    }
    if (req.url === "/error") {
      res.writeHead(409, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: "任务正在停止" }));
      return;
    }
    res.writeHead(200, {
      "content-type": req.url === "/html" ? "text/html" : "application/json",
    });
    res.end(
      req.url === "/html" ? "<html>login</html>" : JSON.stringify({ ok: true }),
    );
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  assert(address && typeof address !== "string");
  origin = `http://127.0.0.1:${address.port}`;
});
after(async () => {
  server.closeAllConnections();
  await new Promise<void>((resolve) => server.close(() => resolve()));
});
test("timeouts are distinguished from deliberate cancellation", async () => {
  await assert.rejects(
    api(`${origin}/slow`, {}, 20),
    (e) => e instanceof ApiError && e.kind === "timeout",
  );
  const controller = new AbortController();
  const response = api(`${origin}/slow`, { signal: controller.signal });
  controller.abort();
  await assert.rejects(
    response,
    (e) => e instanceof DOMException && e.name === "AbortError",
  );
});
test("HTTP diagnostics and invalid response contracts remain distinct", async () => {
  await assert.rejects(
    api(`${origin}/error`),
    (e) =>
      e instanceof ApiError && e.status === 409 && e.message === "任务正在停止",
  );
  await assert.rejects(
    api(`${origin}/html`),
    (e) => e instanceof ApiError && e.kind === "contract",
  );
  assert.deepEqual(await api(`${origin}/ok`), { ok: true });
});
test("a superseded query cannot commit even if a transport ignores abort", () => {
  const query = latestQuery(),
    old = query.begin(),
    current = query.begin();
  assert.equal(old.signal.aborted, true);
  assert.equal(old.current(), false);
  assert.equal(current.current(), true);
  query.cancel();
  assert.equal(current.current(), false);
  assert.equal(current.signal.aborted, true);
});
