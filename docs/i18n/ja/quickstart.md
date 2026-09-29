# クイックスタート

[English](../../quickstart.md) | [简体中文](../zh-CN/quickstart.md) | **日本語** | [Français](../fr/quickstart.md)

パッケージは 1 つのアーカイブに 1 つのバイナリが入ったものです。ESP-IDF も `~/.espressif` のツールも、ファームウェアコーパスも実機も、Bun、Node、Python、QEMU も不要で、ROM をダウンロードする必要もありません。

## 対応ホスト

Apple シリコンの macOS 27 以降(`aarch64-apple-darwin`)と、x64 の Windows 10 1903 以降(`x86_64-pc-windows-msvc`、8 節)です。Linux には対応していません。

## 1. パッケージを入手する

リリースをインストールするには、macOS では `curl -fsSL https://passportsim.bugs.cc/install.sh | sh`、Windows では PowerShell で `irm https://passportsim.bugs.cc/install.ps1 | iex` を実行します。ユーザーディレクトリにインストールし、`passportsim` の実行方法を表示します。チェックアウトからパッケージをビルドすることもでき、その場合インストーラーも `PATH` の編集も不要です。

```sh
cargo run -q -p xtask -- package --target aarch64-apple-darwin
```

`target/package/` に次のものが書き出されます。

| パス | 内容 |
|---|---|
| `passportsim-0.1.0-macos-arm64/` | 展開済みのパッケージ |
| `passportsim-0.1.0-macos-arm64.tar.gz` | 同じツリーを 1 つにまとめたアーカイブ |
| `passportsim-0.1.0-web/` | 静的な Web バンドル(5 節) |
| `passportsim-0.1.0-web.tar.gz` | 同じバンドルのアーカイブ |

パッケージ内のどのファイルにもビルドしたアカウント名は含まれません。ソースのパスは固定のトークンに置き換えられ、ユーザー名やホームディレクトリがどこかに残っているとパッケージ作成は失敗します。

アーカイブを好きな場所に展開し、そのディレクトリに移動します。以降のコマンドはすべてこのディレクトリで実行します。

```sh
cd passportsim-0.1.0-macos-arm64
```

パッケージ作成が途中で失敗した場合は、まずディスクの空き容量を確認してください。ツリー 2 つとアーカイブ 2 つを書き出します。`target/package/` を削除してから再実行してください。毎回最初から作り直します。

### バイナリだけを使う

パッケージを作らずにチェックアウトからエミュレーターを動かすには：

```sh
cargo build -p pemu-cli
./target/debug/passportsim status
```

以降の `./passportsim` は `./target/debug/passportsim` と読み替えてください。2 節は不要です。このビルドにはペイロードがなく(デモも埋め込まれていません)、`--version` にそう表示されます(6 節)。

## 2. macOS での初回起動

バイナリは**署名も公証もされていません**。アーカイブをブラウザ経由や別のマシンから受け取った場合は、隔離属性を一度だけ削除します。

```sh
xattr -dr com.apple.quarantine .
```

何もインストールされません。このマシンから出ていないパッケージでは何も起こりません。

## 3. 動作を確認する

```sh
./passportsim status
```

インスタンス(初回はなし)、成果物のディレクトリ、実行のレシートが表示されます。

```text
no instance is running
artifacts: ~/Library/Application Support/passportsim/artifacts
profile fast | deterministic
```

ディレクトリは何かが書き込まれるときに初めて作られます。

どのコマンドも `--help` に応答し、同じ内容が [commands/](../../commands/index.md) にあります。

```sh
./passportsim --help
```

どのコマンドも `--output json` で JSON を出力します(`--json` はコマンドが*入力*を受け取るためのものです)。

テキスト出力は長さが制限されます。同じ行はまとめられ、長い行は `...(+N chars)` で終わり、最初の 10 件と最後の 30 件だけが残って、その間に `... N lines elided ...` の印が 1 つ入ります。長いコンソール出力は、`serial read --max-bytes` と各読み出しが返す `next_cursor` を使ってページ単位で読んでください。

```sh
./passportsim status --output json
```

### デーモンと MCP

インスタンスはデーモンの中で動きます。`start` はデーモンが動いていなければバックグラウンドで起動し(`passportsim serve --headless`)、以降のコマンド(`run`、`serial`、`status`、`stop` など)はそこに送られるため、インスタンスは起動したコマンドが終わっても残ります。デーモンがなければ、コマンドは自分のプロセス内で実行されます。`--ephemeral` でこれを強制できます。相対パスで指定したファームウェアは、送る前に絶対パスに変換されます(MCP と HTTP では絶対パスが必要です)。下の `pk` はコーパス ID です(4 節)。

```sh
./passportsim start pk --boot none
./passportsim run 'serial:/bsp_i2c/'
./passportsim serial read --cursor 0
./passportsim stop
./passportsim serve --stop
```

デーモンは `127.0.0.1:8765` だけで待ち受けます(使用中なら空いているポート)。すべてのリクエストには、デーモンが `~/.passportsim/` の `serve.json` の隣に所有者のみ読める形で書くトークンが必要です。ヘッドレスのデーモンは `~/.passportsim/logs/serve.log` にログを書き、インスタンスがない状態が 10 分続くと終了します。

`--headless` なしの `passportsim serve` はフォアグラウンドで動き、URL、トークンファイル、Web ページ(5 節)への `ui:` リンクを表示します。リンクには `#lc=` の後に 1 回限りの起動コードが付き、60 秒間有効です。`passportsim serve --stop` は、インスタンスを停止して成果物を書き出したうえでデーモンを止めます。

`passportsim mcp` はエージェント向けの MCP サーバーです。標準入出力で MCP を話し、デーモンに中継します(必要なら起動します)。`--caps audio,nfc` でコアのツールにグループを追加します。クライアントの設定には、バイナリと引数 1 つを指定します。

```sh
./passportsim mcp
```

## 4. 組み込まれているもの

| 必要なもの | 組み込み | 任意の上書き |
|---|---|---|
| ROM | Espressif ESP32-C3 のマスク ROM ELF 2 つ。eFuse のチップリビジョンで選択 | `start` が使うものはなし(6 節) |
| eFuse | 合成イメージ：チップリビジョン v1.1、プレースホルダーの MAC `02:00:00:xx:xx:xx`、キャリブレーションワードは 0 | `--efuse-dump <dir>`(マシンが汚染済みになります。[secrets.md](secrets.md)) |
| ファームウェア | 公式 BSP デモ(ビルドホストが持っていた場合。6 節) | コーパス ID、または `idf.py` のビルドディレクトリ、マージ済みの bin、`.pebundle` のパス |
| ツール | このバイナリ、または Web バンドル | ESP-IDF はファームウェアの*ビルド*にのみ、esptool は USB Serial/JTAG のエンドポイントにのみ必要 |

`passportsim doctor` は、このマシンが解決したものを報告します。同梱 ROM のピン、ROM の上書き、埋め込まれたデモ、`corpus.toml` の各エントリ(found、missing、mismatched)です。ファイルの中身は表示しません。

```sh
./passportsim doctor
```

レポートを渡すこともできます。エージェントが MCP 経由で行うのはこちらです([commands/doctor.md](../../commands/doctor.md))。

```sh
printf '%s' '{"report":{"bundled_roms":[],"corpus":[]}}' | ./passportsim doctor --json -
```

設定ファイルもコーパスも必要ありません。

### ファームウェアコーパス

**コーパス ID** はファームウェアイメージに付けるローカルな短い名前で、`start official` のようにパスの代わりに使えます。ID は組み込まれておらず、各マシンが設定ディレクトリの `corpus.toml` で定義します。

| 役割 | macOS | Windows |
|---|---|---|
| 設定ディレクトリ(`corpus.toml`、`config.toml`) | `~/.config/passportsim/` | `%APPDATA%\passportsim\` |
| データルート(`corpus/`、`artifacts/`、`audio/`) | `~/Library/Application Support/passportsim/` | `%LOCALAPPDATA%\passportsim\data\` |

ID ごとに 1 つのテーブルを書きます。ファイルのキーは `bin`(マージ済みフラッシュイメージ)、`elf`(アプリの ELF)、`boot_elf`(ブートローダーの ELF)、`pt`(パーティションテーブル)で、必須なのは `bin` だけです。`sha256` で各ファイルを 64 文字の完全なダイジェストで固定します(ここでは省略しています)。

```text
[official]
bin = "corpus/official/FoloToy-AI-Passport-8MB.bin"
elf = "corpus/official/FoloToy-AI-Passport.elf"
boot_elf = "corpus/official/bootloader.elf"
sha256 = { bin = "5802...e163", elf = "dd63...a2de", boot_elf = "5fcf...17a8" }
```

パスの書き方(ここでも `config.toml` でも同じ)：

- **絶対パス**はそのまま使います(Windows でも先頭の `/` は絶対パスとして扱います)。
- **`~/...` または `~\...`** はホームディレクトリです。
- **それ以外はデータルートからの相対パス**で、カレントディレクトリからではありません。これにより 1 つの `corpus.toml` を複数のマシンで使えます。`..` でデータルートの外に出るパスは拒否されます。

ファイルがなければ `E_ASSET_MISSING`、ダイジェストが一致しなければ `E_ASSET_HASH` です。`doctor` は拒否したパスを示します。

環境変数による上書き：

- `PASSPORTSIM_CORPUS_<ID>` は 1 つのエントリのパスを置き換えます(`<ID>` は大文字にし、`-` を `_` にします。`probe-long` なら `PASSPORTSIM_CORPUS_PROBE_LONG`)。同じ名前になる 2 つの ID は拒否されます。
- `PASSPORTSIM_DATA_ROOT` はデータルートを移動します。
- `PASSPORTSIM_HOME` はすべてのディレクトリの役割を `<dir>/<role>/` に移動します。

このプロジェクト自身のテストは、ID として `official`(公式 BSP デモ)、`pk`(Passport Keys)、`goldminer`、`demo`、そして ROM とプローブのイメージ `rom0`、`probe-long`、`qemu-oracle`、`probe2`、`scan3`、`pkgatt` を使います。**新しくインストールした環境にはコーパス ID がありません**が、問題ありません。引数なしの `start` は同梱のデモを起動し、パスを渡せば自分のイメージを起動します。

```sh
./passportsim start
./passportsim start ~/esp/my-project/build
./passportsim stop
./passportsim serve --stop
```

ビルドディレクトリは `idf.py build` が書き出す `flasher_args.json` を通じて読むため、各部分は記録されたオフセットに配置されます。`cargo build` のバイナリにはデモがないため、引数なしの `start` は `cargo xtask package` を案内する `E_ASSET_MISSING` で失敗します。

## 5. Web バンドル

`passportsim-0.1.0-web/` は静的サイトです。`index.html`、スタイルシート、ページ、Worker、オーディオワークレットのスクリプト、両方の ROM を含む wasm コア、そしてビルドホストが持っていた場合はデモの `.pebundle` が入っています。任意の静的サーバーで、サイトのルートでもサブパス(例：`/emu/`)でも配信できます。ディスクから直接(`file://`)開いても動きません。

ページはデモを起動します。`idf.py` のビルドディレクトリ、マージ済みの bin、`.pebundle` をドロップすると、代わりにそれを実行します。ELF だけをドロップした場合は、`inspect` にシンボルを渡すだけです。

**シンプル**モード(既定)は、デバイス、ファームウェアカード、ページ自身の読み込み手順を含むライブログを表示します。**アドバンス**モードは、実行制御、Console、UI tree、Events、Inspect、Fidelity、Perf のタブと、Battery、USB、Audio、NFC、Wi-Fi、BLE、Snapshots のカードを加えます。ページは英語、簡体字中国語、日本語、フランス語に対応し、ブラウザの言語とシステムのテーマに従い、ヘッダーで選んだ設定を記憶します。`?mode=advanced`(または `simple`)と `?lang=ja`(または `en`、`zh-CN`、`fr`)は、その 1 回の読み込みだけ設定を上書きします。`serve` のリンクで使う場合は `#` の前に置きます：`http://127.0.0.1:8765/?mode=advanced&lang=ja#lc=<code>`。

同じバンドルがパッケージの `payload/web/` とバイナリの中にもあり、`passportsim serve` が配信するのはそれです。

他のサーバーでは、`Cross-Origin-Opener-Policy: same-origin` と `Cross-Origin-Embedder-Policy: require-corp` を送り(ないと `SharedArrayBuffer` が使えません)、`.wasm` を `application/wasm` で配信する必要があります。バンドルはそのまま Cloudflare Workers のプロジェクトとしても使えます。コマンドとファイルあたり 25 MiB の上限については [deploy-cloudflare.md](deploy-cloudflare.md) を参照してください。

## 6. 既知の制限

| 項目 | 状況 |
|---|---|
| パスで指定したファームウェアのアプリ ELF | `until_ui_settled`、`inspect`、`ui`、`--boot-cache` にはアプリ ELF の DWARF が必要です。CLI が ELF を見つけられるのは、デモと、`elf` エントリを持つコーパス ID だけです。マージ済みイメージ、ビルドディレクトリ、`.pebundle` をパスで指定した場合、`boot: until_ui_settled` は予算をすべて使い切って `unobservable` を報告し、走査コマンドは ELF がないことを示し、`--boot-cache` は `E_STATE` で失敗します。Web ページは、ドロップされたビルドディレクトリや `.pebundle` の ELF を読み込みます |
| ROM の上書き | `doctor` は `PASSPORTSIM_ROM` と `config.toml` の `rom.rev101` / `rom.rev3` キーを確認しますが、`start` は常に同梱の ROM を起動します。`--rom` フラグはありません |
| 埋め込みのデモ | ビルドホストが `official` のコーパスエントリを持つ場合だけ含まれます(イメージはコミットされません)。レシートに有無が記録されます。ない場合、引数なしの `start` は `E_ASSET_MISSING` で失敗します |

### ペイロードと `--version`

バイナリはペイロード全体(Web のアセット、wasm コア、スキーマ、ドキュメント、スキル、デモ)を内蔵しています。バイナリだけを別の場所に移しても何も失われません。`--version`(または `-V`)はペイロードのダイジェストを表示し、その場で再検証します。この値は `receipt.json` の `payload.sha256` と一致します。

```sh
./passportsim --version
```

```text
passportsim 0.1.0
payload: embedded, sha256 <the payload.sha256 of receipt.json>
```

| `payload:` の行 | 意味 |
|---|---|
| `embedded, sha256 <digest>` | パッケージ版のバイナリで、改変されていない |
| `package directory beside the binary, sha256 <digest>` | 埋め込みはなく、隣にある検証済みの `payload/` ディレクトリを使っている |
| `none: development build ...` | 素の `cargo build`。行の残りにペイロードを得る方法が書かれている |
| `embedded, damaged: ...` | 実行ファイルが改変されている。置き換えてください |

## 7. レシート

`receipt.json` には、バージョン、コミット、ターゲット、ペイロードの各ファイルの SHA-256 とペイロード全体のダイジェスト、デモを埋め込んだかどうか(埋め込まなかった理由)、各 ROM ELF が各成果物に含まれているか、どのシークレットガードのルールを実行したかが記録されます。ホストのパス、ユーザー名、デバイスの識別情報は含みません。

パッケージ作成では、すべてのファイルにシークレットガード([secrets.md](secrets.md))をかけ、検出があれば失敗します。ハッシュルールには、そのホスト自身の `~/.config/passportsim/secrets-check.toml` が必要です。ない場合、レシートには `secrets.hashed_rules: false` と記録されます。

パッケージは署名されておらず、Homebrew、winget、Scoop のパッケージもありません。代わりに 2 節と 8 節の初回起動の手順を行ってください。

## 8. Windows

Windows のパッケージは `passportsim.exe` を含む `passportsim-0.1.0-windows-x64.zip` です。MSVC ツールを入れた Windows ホスト上で、PowerShell からビルドします。

```powershell
cargo run -q -p xtask -- package --target x86_64-pc-windows-msvc --payload-from <macOS package directory>
```

1 節と同じ 4 つの成果物が、`.zip` のアーカイブとして書き出されます。実行ファイルは C ランタイムを静的リンクしているため、実行するマシンに何もインストールする必要はありません。また、長いパスへの対応と UTF-8 コードページを宣言しています。どちらかのチェックが通らなければパッケージ作成は失敗し、両方の結果が `receipt.json` の `windows` ブロックに記録されます。

**両ホストで同じペイロード。** デモは macOS のビルドホストにしかなく、wasm コアのバイト列はホストによって異なります。`--payload-from` は同じコミットの macOS パッケージからこの 2 つを取り込み、デモを検証し、残りを自分でビルドして、ペイロード全体が macOS パッケージとファイル単位で一致しない限り拒否します。これにより、両方のレシートが同じ `payload.sha256` を持ちます。指定しない場合、Windows パッケージは独自の wasm コアを持ち、デモは含みません。Windows on Arm 向けのパッケージはありません。

**Windows での初回起動。** `.exe` は署名されていないため、SmartScreen が「Windows によって PC が保護されました」と表示します。**詳細情報**を選び、**実行**を選んでください。または PowerShell でダウンロードの印を外します。

```powershell
Get-ChildItem -Recurse | Unblock-File
```

その後、3 節と 4 節の確認が同じように使えます。

```powershell
.\passportsim.exe --version
```

```powershell
.\passportsim.exe status
```

```powershell
.\passportsim.exe doctor
```

その他も `./passportsim` を `.\passportsim.exe` に読み替えれば同じように動きます。バックグラウンドのデーモンは起動したコンソールを閉じても残り、`passportsim serve --stop` で終了します。

ディレクトリの役割は Windows の既知のフォルダーから決まり、`%USERPROFILE%` や `%LOCALAPPDATA%` は使いません。すべてを移動するには `PASSPORTSIM_HOME=<dir>` を設定してください。

## 9. 次に読むもの

- [commands/index.md](../../commands/index.md)：すべてのコマンドと、その MCP ツール、HTTP ルート、シナリオのステップ(自動生成、英語)。
- [errors.md](../../errors.md)：すべてのエラーコード(自動生成、英語)。
- [SKILL.md](../../../skills/passportsim/SKILL.md)：エージェント用スキルと、デバイス保護のための拒否リスト(英語)。
- [secrets.md](secrets.md)：シークレットポリシー。
- リポジトリの `docs/ARCHITECTURE.md`：設計(パッケージには含まれません)。
