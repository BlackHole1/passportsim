# 将网页包部署到 Cloudflare Workers

[English](../../deploy-cloudflare.md) | **简体中文** | [日本語](../ja/deploy-cloudflare.md) | [Français](../fr/deploy-cloudflare.md)

网页包(`passportsim-0.1.0-web/`，见[快速入门](quickstart.md)第 5 节)是一个静态站点，`cargo xtask package` 会把它生成为现成的 Cloudflare Workers 项目。可以在它的目录中直接部署，无需修改；也可以在源码检出目录中运行 `just deploy`。

本项目自己的部署是 https://passportsim.bugs.cc：带演示固件的 Worker `passportsim`，自定义域名在控制台中绑定。

## 发布包写入的文件

页面旁边的三个文件(`xtask/src/package/cloudflare.rs`)：

| 文件 | 作用 |
|---|---|
| `wrangler.jsonc` | 一个名为 `passportsim` 的纯静态资源 Worker，提供网页包目录。没有 Worker 脚本。`workers_dev: true` 保证同时部署自定义域名时 `workers.dev` 地址仍然可用。 |
| `_headers` | 与 `passportsim serve` 相同的响应头：`Cross-Origin-Opener-Policy: same-origin` 和 `Cross-Origin-Embedder-Policy: require-corp`(`SharedArrayBuffer` 需要它们)、`Cross-Origin-Resource-Policy: same-origin` 和 `X-Content-Type-Options: nosniff`。它还设置演示 `.pebundle` 和许可证文本的内容类型。 |
| `.assetsignore` | 不让 `wrangler.jsonc` 和 `.wrangler/` 状态目录出现在站点上。 |

Workers 的响应带有 `ETag` 和 `Cache-Control: public, max-age=0, must-revalidate`，因此浏览器只在 23.5 MiB 的演示固件有变化时才重新下载。`pemu_wasm.wasm` 以 `application/wasm` 类型提供。

## 部署不保存任何数据

纯静态资源 Worker 不运行任何代码：每个请求都只是读取静态文件。访问者的一切操作都在自己的浏览器中进行。拖入的固件在本地读取并交给页面的 Web Worker(页面提到的“Worker”是这个浏览器线程，不是 Cloudflare Worker)，快照保存在页面内存中，固件历史保存在浏览器为该站点提供的 IndexedDB 中。`web/tests/local.spec.ts` 会在出现任何不是对页面自身源的 GET 请求时失败。

原生守护进程 `passportsim serve` 是另一个程序：它运行在你的机器上，会把回执和产物写入本地数据目录。

## 部署

需要 [Bun](https://bun.sh)(或带 `npx` 的 Node)和一个 Cloudflare 账户。在网页包目录中执行：

```sh
cd target/package/passportsim-0.1.0-web
bunx wrangler@4 login     # 每台机器一次，会打开浏览器
bunx wrangler@4 deploy    # 上传网页包并输出 https 地址
```

`--name <name>` 以另一个 Worker 名称部署，该名称同时决定 `<name>.<account subdomain>.workers.dev` 主机名。要使用自己的域名，可在 `wrangler.jsonc` 中添加 `routes` 条目，或在控制台中绑定自定义域名。

在本地用 Workers 运行时试运行，无需账户：

```sh
bunx wrangler@4 dev --ip 127.0.0.1 --port 8787 --persist-to "$TMPDIR/passportsim-wrangler"
```

(在 PowerShell 中使用 `--persist-to "$env:TEMP\passportsim-wrangler"`)，然后打开 `http://127.0.0.1:8787/`。`--persist-to` 必须指向网页包之外：默认的 `.wrangler/state` 位于静态资源目录内，Wrangler 会因自己的缓存写入而不断重载，并截断响应，导致页面无法启动。

`web/tests/cloudflare.spec.ts` 在 Chromium 中检查正在运行的 `wrangler dev`(跨源隔离响应头、内容类型、演示固件启动、拖入固件)。它只在显式指定时运行：

```sh
cd web
PEMU_E2E_CLOUDFLARE_URL=http://127.0.0.1:8787/ PEMU_E2E_CLOUDFLARE_IMAGE=<a merged .bin> \
  bun run e2e --project=chromium tests/cloudflare.spec.ts
```

## 持续集成与发布

`.github/workflows/` 中的工作流：

| 工作流 | 触发 | 作用 |
|---|---|---|
| `pr-check.yml` | pull request、推送到 `main` | 在 macOS 和 Windows 上运行 `cargo xtask ci t0`，并在 macOS 上运行页面的类型检查和 Playwright 测试(Chromium、Firefox、WebKit)。runner 没有固件语料库，因此语料库测试报告 SKIPPED-CORPUS。 |
| `deploy-web.yml` | 手动触发，或由 `release.yml` 调用 | 在 macOS 上打包网页包，用 `wrangler deploy` 部署，并检查线上站点。 |
| `release.yml` | 在 Actions 页签手动触发 | 根据 `v*` 标签算出下一个版本(或使用指定的版本)，按该版本打包 macOS arm64 和 Windows x64，打标签，发布带有自动生成说明、压缩包和 `SHA256SUMS.txt` 的 GitHub Release，然后部署网页包。 |

仓库设置(Settings、Secrets and variables、Actions；secret 也可以放在 `production` 环境中)：

| 名称 | 类型 | 用途 |
|---|---|---|
| `CLOUDFLARE_API_TOKEN` | secret | 用“Edit Cloudflare Workers”模板创建的令牌，作用范围为该账户；设置了自定义域名时还需包含对应的 zone。 |
| `CLOUDFLARE_ACCOUNT_ID` | secret | Worker 所在的账户。 |
| `CF_CUSTOM_DOMAIN` | variable，可选 | 每次部署时声明的自定义域名。 |
| `CF_WORKER_NAME` | variable，可选 | 使用 `passportsim` 以外的 Worker 名称。 |
| `DEMO_SITE_URL` | variable，可选 | 从哪个已部署的站点下载演示固件(见下文)。 |
| `DEMO_BUNDLE_SHA256` | variable，可选 | 该站点上 `official.pebundle` 的 SHA-256。 |

**自定义域名。** 未设置时，部署不改变控制台中为 Worker 配置的域名。设置后，每次部署都会传入 `--domain <name>`，使它成为 Worker 唯一的自定义域名。fork 的仓库不设置它，只使用自己的 `workers.dev` 地址。上传完成后，任务会从线上地址获取页面、wasm 核心和演示固件，只有跨源隔离响应头齐全、且核心正是刚上传的版本时才通过。

**演示固件。** 演示固件从不提交到仓库，runner 也没有语料库，因此工作流从已经提供它的站点下载(`official.pebundle` 和两个 `licenses/official-demo.*` 文件)。第一次带演示固件的部署需要在有语料库的机器上手动完成(`just deploy`)，然后设置 `DEMO_SITE_URL` 和 `DEMO_BUNDLE_SHA256`(`shasum -a 256 official.pebundle`)。任务会校验摘要，而 `cargo xtask package --demo <dir>` 只接受与本提交从固定镜像构建出的结果完全一致的文件。下载或校验失败时，任务会在部署前失败。两个变量都未设置时，站点会在不含演示固件的情况下部署，任务摘要中会说明这一点。

## 限制

来自 Cloudflare 文档，查阅于 2026-09-27：

| 限制 | 免费版 | 付费版 | 来源 |
|---|---|---|---|
| 单个静态资源文件 | 25 MiB(26,214,400 字节) | 25 MiB | [Workers limits, static assets](https://developers.cloudflare.com/workers/platform/limits/#static-assets) |
| 每个 Worker 版本的资源文件数 | 20,000 | 100,000(Wrangler 4.34.0 或更新版本) | 同上 |
| 资源总大小 | 未说明限制 | 未说明限制 | 同上 |
| 静态资源请求 | 免费且不限量 | 免费且不限量 | [Billing and limitations](https://developers.cloudflare.com/workers/static-assets/billing-and-limitations/) |
| `_headers` 规则数 / 行长度 | 100 条规则，每行 2,000 字符 | 同左 | [Headers](https://developers.cloudflare.com/workers/static-assets/headers/) |

只要有一个文件超过 25 MiB，Wrangler 就会拒绝整个部署。带演示固件的网页包有 13 个文件；最大的是 24,690,534 字节的 `official.pebundle`(比上限少 1.45 MiB)，其次是约 6.9 MB 的 `pemu_wasm.wasm`。如果有文件超过上限，或文件数超过 20,000，`cargo xtask package` 会失败并指出是哪个文件，每次运行也会输出剩余余量。

## 需要决定的事项

- **账户**，以及谁掌握它的登录权限。
- **地址**：Worker 名称及其 `workers.dev` 主机名，或者某个路由或自定义域名。
- **是否发布演示固件。** `official.pebundle` 是 FoloToy AI Passport BSP 演示的预构建镜像，FoloToy 以 MIT 许可证发布；许可证和声明文件随附在 `licenses/` 中。如不想发布，部署前删除 `official.pebundle` 即可；页面随后会在没有机器的状态下打开，并提示拖入固件。
- **缓存。** 默认设置每次加载都会重新验证。要使用更长的缓存，需要先让文件名带上版本号，因为页面请求的是固定的文件名(`main.js`、`pemu_wasm.wasm`)。
