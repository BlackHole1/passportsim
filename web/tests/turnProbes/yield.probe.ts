// How long each way of letting a Worker's event loop run takes, per engine, with the page's main
// thread idle and busy.
import { test } from "@playwright/test";
import { createServer } from "node:http";

const WORKER = `
const cell = new Int32Array(new SharedArrayBuffer(4));
const now = () => performance.now();
function stats(xs) { xs.sort((a,b)=>a-b); const q = p => xs[Math.min(xs.length-1, Math.floor(p*xs.length))]; return {n: xs.length, p50: +q(0.5).toFixed(3), p99: +q(0.99).toFixed(3), max: +xs[xs.length-1].toFixed(3)}; }
const ch = new MessageChannel(); let res = null; ch.port1.onmessage = () => { const r = res; res = null; r(); };
const mc = () => new Promise(r => { res = r; ch.port2.postMessage(0); });
const st = () => new Promise(r => setTimeout(r, 0));
const wa = (ms) => { const w = Atomics.waitAsync ? Atomics.waitAsync(cell, 0, 0, ms) : null; return w && w.async ? w.value : Promise.resolve('sync'); };
async function measure(kind, n, fn) { const xs = []; for (let i = 0; i < n; i++) { const t = now(); await fn(); xs.push(now() - t); } return [kind, stats(xs)]; }
self.onmessage = async (e) => {
  const out = { waitAsync: typeof Atomics.waitAsync, isolated: self.crossOriginIsolated };
  const n = e.data.n;
  for (const [k, v] of [
    await measure('messageChannel', n, mc),
    await measure('setTimeout0', n, st),
    await measure('waitAsync1ms', Math.min(n, 400), () => wa(1)),
    await measure('waitAsync4ms', Math.min(n, 200), () => wa(4)),
    await measure('atomicsWait1ms', Math.min(n, 400), async () => { Atomics.wait(cell, 0, 0, 1); }),
  ]) out[k] = v;
  postMessage(out);
};
`;

const PAGE = `<!doctype html><script>
window.run = (n, blockMs) => new Promise(resolve => {
  const w = new Worker('/w.js');
  w.onmessage = e => { clearInterval(iv); resolve(e.data); };
  let iv = null;
  if (blockMs > 0) iv = setInterval(() => { const end = performance.now() + blockMs; while (performance.now() < end) {} }, blockMs * 2);
  w.postMessage({ n });
});
</script>`;

test("yield mechanisms", async ({ page, browserName }) => {
  test.setTimeout(300_000);
  const server = createServer((req, res) => {
    const headers = { "cross-origin-opener-policy": "same-origin", "cross-origin-embedder-policy": "require-corp" };
    if (req.url === "/w.js") {
      res.writeHead(200, { ...headers, "content-type": "text/javascript" });
      res.end(WORKER);
    } else {
      res.writeHead(200, { ...headers, "content-type": "text/html" });
      res.end(PAGE);
    }
  });
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", () => r()));
  const port = (server.address() as { port: number }).port;
  await page.goto(`http://127.0.0.1:${port}/`);
  for (const block of [0, 50]) {
    const result = await page.evaluate(([n, b]) => (window as any).run(n, b), [2000, block] as const);
    console.log(`PROBE ${browserName} mainThreadBlock=${block}ms ${JSON.stringify(result)}`);
  }
  server.close();
});
