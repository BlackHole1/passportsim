# Web バンドルを Cloudflare Workers にデプロイする

[English](../../deploy-cloudflare.md) | [简体中文](../zh-CN/deploy-cloudflare.md) | **日本語** | [Français](../fr/deploy-cloudflare.md)

Web バンドル(`passportsim-0.1.0-web/`、[クイックスタート](quickstart.md)の 5 節)は静的サイトで、`cargo xtask package` がそのまま使える Cloudflare Workers プロジェクトとして書き出します。編集せずにそのディレクトリからデプロイするか、チェックアウトで `just deploy` を実行してください。

このプロジェクト自身のデプロイは https://passportsim.bugs.cc です。デモファームウェアを含む Worker `passportsim` に、ダッシュボードでカスタムドメインを割り当てています。

## パッケージが書き出すもの

ページの隣に 3 つのファイルを書き出します(`xtask/src/package/cloudflare.rs`)。

| ファイル | 役割 |
|---|---|
| `wrangler.jsonc` | バンドルのディレクトリを配信する、アセットのみの Worker `passportsim`。Worker スクリプトはありません。`workers_dev: true` により、カスタムドメインもデプロイしたときに `workers.dev` の URL が残ります。 |
| `_headers` | `passportsim serve` が送るヘッダー：`Cross-Origin-Opener-Policy: same-origin` と `Cross-Origin-Embedder-Policy: require-corp`(`SharedArrayBuffer` に必要)、`Cross-Origin-Resource-Policy: same-origin`、`X-Content-Type-Options: nosniff`。デモの `.pebundle` とライセンス文のコンテンツタイプも指定します。 |
| `.assetsignore` | `wrangler.jsonc` と `.wrangler/` の状態ディレクトリをサイトに含めません。 |

Workers は `ETag` と `Cache-Control: public, max-age=0, must-revalidate` を返すため、ブラウザが 23.5 MiB のデモを再ダウンロードするのは変更があったときだけです。`pemu_wasm.wasm` は `application/wasm` で配信されます。

## デプロイ先には何も保存されない

アセットのみの Worker はコードを実行しません。すべてのリクエストは静的ファイルの読み出しです。訪問者の操作はすべてその人のブラウザ内で完結します。ドロップしたファームウェアはローカルで読み込まれてページの Web Worker に渡され(ページに出てくる「Worker」はこのブラウザのスレッドで、Cloudflare Worker ではありません)、スナップショットはページのメモリに、ファームウェアの履歴はそのサイト用のブラウザの IndexedDB にとどまります。`web/tests/local.spec.ts` は、ページ自身のオリジンへの GET 以外のリクエストがあると失敗します。

ネイティブのデーモン `passportsim serve` は別のプログラムです。あなたのマシン上で動き、レシートと成果物をローカルのデータディレクトリに書き込みます。

## デプロイ

[Bun](https://bun.sh)(または `npx` が使える Node)と Cloudflare のアカウントが必要です。バンドルのディレクトリで：

```sh
cd target/package/passportsim-0.1.0-web
bunx wrangler@4 login     # マシンごとに 1 回。ブラウザが開きます
bunx wrangler@4 deploy    # バンドルをアップロードし、https の URL を表示します
```

`--name <name>` を付けると別の Worker 名でデプロイでき、それが `<name>.<account subdomain>.workers.dev` のホスト名にもなります。独自のドメインで配信するには、`wrangler.jsonc` に `routes` のエントリを追加するか、ダッシュボードでカスタムドメインを割り当ててください。

アカウントなしで、Workers のランタイム上でローカルに試すには：

```sh
bunx wrangler@4 dev --ip 127.0.0.1 --port 8787 --persist-to "$TMPDIR/passportsim-wrangler"
```

(PowerShell では `--persist-to "$env:TEMP\passportsim-wrangler"`)を実行し、`http://127.0.0.1:8787/` を開きます。`--persist-to` はバンドルの外を指すようにしてください。既定の `.wrangler/state` はアセットのディレクトリ内にあり、Wrangler が自身のキャッシュ書き込みでリロードを繰り返してレスポンスを途中で切るため、ページが起動しません。

`web/tests/cloudflare.spec.ts` は、動作中の `wrangler dev` を Chromium で確認します(分離ヘッダー、コンテンツタイプ、デモの起動、ファームウェアのドロップ)。名前を指定したときだけ実行されます。

```sh
cd web
PEMU_E2E_CLOUDFLARE_URL=http://127.0.0.1:8787/ PEMU_E2E_CLOUDFLARE_IMAGE=<a merged .bin> \
  bun run e2e --project=chromium tests/cloudflare.spec.ts
```

## 継続的インテグレーションとリリース

`.github/workflows/` のワークフロー：

| ワークフロー | 実行のきっかけ | 内容 |
|---|---|---|
| `pr-check.yml` | プルリクエスト、`main` へのプッシュ | macOS と Windows で `cargo xtask ci t0`(`--group` で並列のジョブに分割)、macOS でページの型チェックと Playwright のテスト(Chromium、Firefox、WebKit)。ランナーにはファームウェアコーパスがないため、コーパスのテストは SKIPPED-CORPUS と報告されます。 |
| `deploy-web.yml` | 手動、または `release.yml` から | macOS で Web バンドルをパッケージし、`wrangler deploy` でデプロイして、公開中のサイトを確認します。 |
| `release.yml` | Actions タブから手動 | `v*` タグから次のバージョンを算出し(指定があればそれを使い)、そのバージョンで macOS arm64 と Windows x64 のパッケージを作り、タグを付け、自動生成のリリースノート、アーカイブ、`SHA256SUMS.txt` を含む GitHub Release を公開してから、Web バンドルをデプロイします。 |

リポジトリの設定(Settings、Secrets and variables、Actions。シークレットはデプロイジョブが使う `production` 環境に置いても構いません)：

| 名前 | 種類 | 用途 |
|---|---|---|
| `CLOUDFLARE_API_TOKEN` | secret | 「Edit Cloudflare Workers」テンプレートから作るトークン。アカウントに、カスタムドメインを使う場合はゾーンにもスコープを設定します。 |
| `CLOUDFLARE_ACCOUNT_ID` | secret | Worker が属するアカウント。 |
| `CF_CUSTOM_DOMAIN` | variable、任意 | デプロイのたびに宣言するカスタムドメイン。 |
| `CF_WORKER_NAME` | variable、任意 | `passportsim` 以外の Worker 名。 |
| `DEMO_SITE_URL` | variable、任意 | デモファームウェアをダウンロードする、デプロイ済みのサイト(後述)。 |
| `DEMO_BUNDLE_SHA256` | variable、任意 | そのサイトの `official.pebundle` を展開した内容の SHA-256(サイトは gzip で圧縮して配信します)。 |

**カスタムドメイン。** 未設定なら、デプロイは Worker のドメインをダッシュボードの設定のまま残します。設定すると、毎回のデプロイで `--domain <name>` を渡し、それが Worker の唯一のカスタムドメインになります。フォークでは未設定のままにすれば、`workers.dev` の URL が得られます。アップロード後、ジョブは公開中の URL からページ、wasm コア、デモを取得し、分離ヘッダーがあり、コアが今アップロードしたものと同じでなければ失敗します。

**デモファームウェア。** デモはコミットされず、ランナーにはコーパスがないため、ワークフローはすでにデモを配信しているサイトからダウンロードします(`official.pebundle` と 2 つの `licenses/official-demo.*` ファイル)。最初のデモ付きデプロイは、コーパスのあるマシンから手動で行い(`just deploy`)、その後 `DEMO_SITE_URL` と `DEMO_BUNDLE_SHA256`(Web バンドルの gzip 圧縮されたコピーに対して `gzip -dc official.pebundle | shasum -a 256`)を設定してください。ジョブはダイジェストを確認し、`cargo xtask package --demo <dir>` は、ファイルがこのコミットで固定のイメージからビルドされるものとまったく同じ場合だけ受け入れます。ダウンロードや確認に失敗すると、何もデプロイする前にジョブが失敗します。両方の変数が未設定なら、デモなしでサイトをデプロイし、ジョブのサマリーにその旨を記載します。

## 上限

Cloudflare のドキュメントより(2026-09-27 時点)：

| 上限 | Free | Paid | 出典 |
|---|---|---|---|
| 静的アセット 1 ファイル | 25 MiB(26,214,400 バイト) | 25 MiB | [Workers limits, static assets](https://developers.cloudflare.com/workers/platform/limits/#static-assets) |
| Worker のバージョンあたりのアセットファイル数 | 20,000 | 100,000(Wrangler 4.34.0 以降) | 同上 |
| アセットの合計サイズ | 記載なし | 記載なし | 同上 |
| 静的アセットへのリクエスト | 無料・無制限 | 無料・無制限 | [Billing and limitations](https://developers.cloudflare.com/workers/static-assets/billing-and-limitations/) |
| `_headers` のルール数 / 行の長さ | 100 ルール、1 行 2,000 文字 | 同じ | [Headers](https://developers.cloudflare.com/workers/static-assets/headers/) |

25 MiB を超えるファイルが 1 つでもあると、Wrangler はデプロイ全体を拒否します。デモ付きのバンドルは 18 ファイルで、最大は約 7.0 MB の `pemu_wasm.wasm`、次が約 6.1 MB の `official.pebundle` です。これは 24.7 MB のデモを gzip で圧縮したもので、Cloudflare は `application/octet-stream` のファイルをそのまま送るため、ページ側で展開します。`cargo xtask package` は、上限を超えるファイルがあるか 20,000 ファイルを超えるとファイル名を示して失敗し、毎回余裕の大きさを表示します。

## 決めておくこと

- **アカウント**と、そのログイン情報を持つ人。
- **アドレス**：Worker 名とその `workers.dev` のホスト名、またはルートやカスタムドメイン。
- **デモを公開するかどうか。** `official.pebundle` は FoloToy の AI Passport BSP デモをビルドしたイメージで、FoloToy が MIT ライセンスで公開しているものです。ライセンスと告知文は `licenses/` に同梱されます。含めない場合は、デプロイ前に `official.pebundle` を削除してください。ページはマシンなしで開き、ファームウェアを求めます。
- **キャッシュ。** 既定では毎回の読み込みで再検証します。より長くキャッシュするには、ページが固定の名前(`main.js`、`pemu_wasm.wasm`)を要求するため、先にファイル名にバージョンを付ける必要があります。
