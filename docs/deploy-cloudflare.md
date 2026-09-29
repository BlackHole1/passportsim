# Deploying the web bundle to Cloudflare Workers

**English** | [简体中文](i18n/zh-CN/deploy-cloudflare.md) | [日本語](i18n/ja/deploy-cloudflare.md) | [Français](i18n/fr/deploy-cloudflare.md)

The web bundle (`passportsim-0.1.0-web/`, section 5 of the [quickstart](quickstart.md)) is a static
site that `cargo xtask package` writes as a ready Cloudflare Workers project. Deploy it from its
directory with no edits, or run `just deploy` from a checkout.

The project's own deployment is https://passportsim.bugs.cc: the Worker `passportsim` with the
demo firmware, and the custom domain attached in the dashboard.

## What the package writes

Three files beside the page (`xtask/src/package/cloudflare.rs`):

| File | What it does |
|---|---|
| `wrangler.jsonc` | An assets-only Worker named `passportsim` serving the bundle directory. There is no Worker script. `workers_dev: true` keeps the `workers.dev` URL when a custom domain is also deployed. |
| `_headers` | The headers `passportsim serve` sends: `Cross-Origin-Opener-Policy: same-origin` and `Cross-Origin-Embedder-Policy: require-corp` (needed for `SharedArrayBuffer`), `Cross-Origin-Resource-Policy: same-origin` and `X-Content-Type-Options: nosniff`. It also sets the content type of the demo `.pebundle` and the licence texts. |
| `.assetsignore` | Keeps `wrangler.jsonc` and the `.wrangler/` state directory off the site. |

Workers answers with an `ETag` and `Cache-Control: public, max-age=0, must-revalidate`, so a
browser downloads the 23.5 MiB demo again only when it changed. `pemu_wasm.wasm` is served as
`application/wasm`.

## The deployment stores nothing

An assets-only Worker runs no code: every request is a static file read. Everything a visitor
does happens in their browser. Dropped firmware is read locally and passed to the page's Web
Worker (the "Worker" the page mentions is that browser thread, not a Cloudflare Worker), snapshots
stay in page memory, and the firmware history is in the browser's IndexedDB for the site.
`web/tests/local.spec.ts` fails on any request that is not a GET to the page's own origin.

The native daemon, `passportsim serve`, is a different program: it runs on your machine and writes
receipts and artifacts to its local data directory.

## Deploy

Needs [Bun](https://bun.sh) (or Node with `npx`) and a Cloudflare account. From the bundle
directory:

```sh
cd target/package/passportsim-0.1.0-web
bunx wrangler@4 login     # once per machine; opens the browser
bunx wrangler@4 deploy    # uploads the bundle and prints the https URL
```

`--name <name>` deploys under another Worker name, which is also the
`<name>.<account subdomain>.workers.dev` host. For your own domain, add a `routes` entry to
`wrangler.jsonc` or attach a custom domain in the dashboard.

To try it locally under the Workers runtime, with no account:

```sh
bunx wrangler@4 dev --ip 127.0.0.1 --port 8787 --persist-to "$TMPDIR/passportsim-wrangler"
```

(in PowerShell, `--persist-to "$env:TEMP\passportsim-wrangler"`), then open
`http://127.0.0.1:8787/`. Keep `--persist-to` outside the bundle: the default `.wrangler/state` is
inside the assets directory, and Wrangler then reloads on its own cache writes and cuts responses
short, so the page never boots.

`web/tests/cloudflare.spec.ts` checks a running `wrangler dev` in Chromium (isolation headers,
content types, demo boot, firmware drop). It runs only when named:

```sh
cd web
PEMU_E2E_CLOUDFLARE_URL=http://127.0.0.1:8787/ PEMU_E2E_CLOUDFLARE_IMAGE=<a merged .bin> \
  bun run e2e --project=chromium tests/cloudflare.spec.ts
```

## Continuous integration and releases

Workflows in `.github/workflows/`:

| Workflow | Runs on | What it does |
|---|---|---|
| `pr-check.yml` | pull requests, pushes to `main` | `cargo xtask ci t0` on macOS and Windows, and the page's type checks and Playwright tests (Chromium, Firefox, WebKit) on macOS. Runners have no firmware corpus, so corpus tests report SKIPPED-CORPUS. |
| `deploy-web.yml` | by hand, or from `release.yml` | Packages the web bundle on macOS, deploys it with `wrangler deploy` and checks the live site. |
| `release.yml` | by hand, from the Actions tab | Computes the next version from the `v*` tags (or takes the one given), packages macOS arm64 and Windows x64 as that version, tags the commit, publishes a GitHub Release with generated notes, the archives and `SHA256SUMS.txt`, then deploys the web bundle. |

Repository settings (Settings, Secrets and variables, Actions; the secrets may live on the
`production` environment instead):

| Name | Kind | Purpose |
|---|---|---|
| `CLOUDFLARE_API_TOKEN` | secret | A token from the "Edit Cloudflare Workers" template, scoped to the account, and to the zone when a custom domain is set. |
| `CLOUDFLARE_ACCOUNT_ID` | secret | The account the Worker lives in. |
| `CF_CUSTOM_DOMAIN` | variable, optional | A custom domain to declare on every deploy. |
| `CF_WORKER_NAME` | variable, optional | A Worker name other than `passportsim`. |
| `DEMO_SITE_URL` | variable, optional | A deployed site to download the demo firmware from (below). |
| `DEMO_BUNDLE_SHA256` | variable, optional | The SHA-256 of that site's `official.pebundle`. |

**Custom domain.** Unset, a deploy leaves the Worker's domains as the dashboard set them. Set, every
deploy passes `--domain <name>`, which makes that the Worker's only custom domain. A fork leaves it
unset and gets its `workers.dev` URL. After uploading, the job fetches the page, the wasm core and
the demo from the live URLs and fails unless the isolation headers are present and the core is the
one just uploaded.

**Demo firmware.** The demo is never committed and runners have no corpus, so a workflow downloads
it from a site that already serves it (`official.pebundle` and the two `licenses/official-demo.*`
files). Make the first deploy with the demo by hand (`just deploy`) from a machine with the corpus,
then set `DEMO_SITE_URL` and `DEMO_BUNDLE_SHA256` (`shasum -a 256 official.pebundle`). The job
checks the digest, and `cargo xtask package --demo <dir>` accepts the files only if they are
exactly what this commit would build from the pinned image. A failed download or check fails the
job before anything is deployed. With both variables unset, the site is deployed without the demo
and the job summary says so.

## Limits

From Cloudflare's documentation, read on 2026-09-27:

| Limit | Free | Paid | Source |
|---|---|---|---|
| One static asset file | 25 MiB (26,214,400 bytes) | 25 MiB | [Workers limits, static assets](https://developers.cloudflare.com/workers/platform/limits/#static-assets) |
| Asset files per Worker version | 20,000 | 100,000 (Wrangler 4.34.0 or newer) | same |
| Total size of the assets | no limit stated | no limit stated | same |
| Requests to static assets | free and unlimited | free and unlimited | [Billing and limitations](https://developers.cloudflare.com/workers/static-assets/billing-and-limitations/) |
| `_headers` rules / line length | 100 rules, 2,000 characters a line | same | [Headers](https://developers.cloudflare.com/workers/static-assets/headers/) |

Wrangler refuses the whole deploy if one file is over 25 MiB. A bundle with the demo has 13 files;
the largest is `official.pebundle` at 24,690,534 bytes (1.45 MiB under the limit), then
`pemu_wasm.wasm` at about 6.9 MB. `cargo xtask package` fails, naming the file, if one is over the
limit or there are more than 20,000, and prints the margin on every run.

## What to decide

- **The account**, and who holds its login.
- **The address**: the Worker name and its `workers.dev` host, or a route or custom domain.
- **Whether to publish the demo.** `official.pebundle` is a prebuilt image of FoloToy's AI
  Passport BSP demo, published by FoloToy under the MIT License; the licence and notice ship beside
  it in `licenses/`. To leave it out, delete `official.pebundle` before deploying; the page then
  opens with no machine and asks for a firmware.
- **Caching.** The defaults revalidate on every load. Longer caching needs versioned file names
  first, because the page requests fixed names (`main.js`, `pemu_wasm.wasm`).
