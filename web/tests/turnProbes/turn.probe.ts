// Candidate event-loop turns for an isolated Worker: their latency with the page's main thread idle
// and busy, and whether a message the page posted (and notified) before the turn has arrived.
import { test } from "@playwright/test";
import { createServer } from "node:http";

const WORKER = `
let cell = null;
const now = () => performance.now();
function stats(xs) { xs.sort((a,b)=>a-b); const q = p => xs[Math.min(xs.length-1, Math.floor(p*xs.length))]; return {n: xs.length, p50: +q(0.5).toFixed(3), p99: +q(0.99).toFixed(3), max: +xs[xs.length-1].toFixed(3)}; }
const ch = new MessageChannel(); let res = null; ch.port1.onmessage = () => { const r = res; res = null; r(); };
const mc = () => new Promise(r => { res = r; ch.port2.postMessage(0); });
const tiny = () => { const v = Atomics.load(cell, 1); const w = Atomics.waitAsync(cell, 1, v, 0.001); return w.async ? w.value : Promise.resolve(); };
const selfNotify = () => { const v = Atomics.load(cell, 1); const w = Atomics.waitAsync(cell, 1, v, 1000); Atomics.notify(cell, 1); return w.async ? w.value : Promise.resolve(); };
let delivered = 0;
async function measure(kind, n, fn) { const xs = []; for (let i = 0; i < n; i++) { const t = now(); await fn(); xs.push(now() - t); } return [kind, stats(xs)]; }
// Ordering: ask the page for N pings; for each, spin until the page's notify moves counter 0, then
// take one turn and see whether the ping's onmessage ran by then.
async function ordering(kind, fn, n) {
  let ok = 0;
  for (let i = 0; i < n; i++) {
    const before = delivered; const c = Atomics.load(cell, 0);
    postMessage({ ping: true });
    while (Atomics.load(cell, 0) === c) {}
    await fn();
    if (delivered > before) ok++; else { const t = now(); while (delivered === before && now() - t < 200) await mc(); }
  }
  return [kind + 'Ordering', { delivered: ok, of: n }];
}
self.onmessage = async (e) => {
  if (e.data.pong) { delivered++; return; }
  cell = new Int32Array(e.data.sab);
  const n = e.data.n; const out = {};
  for (const [k, v] of [
    await measure('messageChannel', n, mc),
    await measure('waitAsyncTiny', n, tiny),
    await measure('waitAsyncSelfNotify', n, selfNotify),
    await ordering('messageChannel', mc, 100),
    await ordering('waitAsyncTiny', tiny, 100),
    await ordering('waitAsyncSelfNotify', selfNotify, 100),
  ]) out[k] = v;
  postMessage({ done: out });
};
`;

const PAGE = `<!doctype html><script>
window.run = (n, blockMs) => new Promise(resolve => {
  const sab = new SharedArrayBuffer(8); const cell = new Int32Array(sab);
  const w = new Worker('/w.js');
  let iv = null;
  w.onmessage = e => {
    if (e.data.ping) { w.postMessage({ pong: true }); Atomics.add(cell, 0, 1); Atomics.notify(cell, 0); return; }
    clearInterval(iv); resolve(e.data.done);
  };
  if (blockMs > 0) iv = setInterval(() => { const end = performance.now() + blockMs; while (performance.now() < end) {} }, blockMs * 2);
  w.postMessage({ n, sab });
});
</script>`;

test("event-loop turns", async ({ page, browserName }) => {
  test.setTimeout(300_000);
  const server = createServer((req, res) => {
    const headers = { "cross-origin-opener-policy": "same-origin", "cross-origin-embedder-policy": "require-corp" };
    res.writeHead(200, { ...headers, "content-type": req.url === "/w.js" ? "text/javascript" : "text/html" });
    res.end(req.url === "/w.js" ? WORKER : PAGE);
  });
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", () => r()));
  const port = (server.address() as { port: number }).port;
  await page.goto(`http://127.0.0.1:${port}/`);
  for (const block of [0, 50]) {
    const result = await page.evaluate(([n, b]) => (window as any).run(n, b), [3000, block] as const);
    console.log(`TURN ${browserName} mainThreadBlock=${block}ms ${JSON.stringify(result)}`);
  }
  server.close();
});
