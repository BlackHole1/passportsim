<div align="center">

# PassportSim

**実機なしで FoloToy AI Passport のファームウェアを開発・デバッグ。**<br>
改変していない ESP-IDF ファームウェアを、ブラウザやデスクトップでそのまま動かせます。

**オンラインで試す：[passportsim.bugs.cc](https://passportsim.bugs.cc)**(インストール不要)

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](../../../LICENSE)
![Hosts](https://img.shields.io/badge/hosts-macOS%20%7C%20Windows-lightgrey.svg)
![Web](https://img.shields.io/badge/web-WebAssembly-654ff0.svg)
![MCP](https://img.shields.io/badge/agents-MCP-black.svg)

[English](../../../README.md) | [简体中文](../zh-CN/README.md) | **日本語** | [Français](../fr/README.md)

<img src="../../images/web-simple.png" alt="ブラウザ上の PassportSim：公式デモを動かすエミュレートされたデバイス、ファームウェアカード、ログ" width="900">

</div>

## できること

- **デバイスなしでファームウェアを実行。** ビルドをページにドロップするだけで、画面、ボタン、音、シリアル出力が実機と同じように動きます。
- **デバッグ。** 一時停止、ステップ実行、状態の保存と復元、シリアルログ、UI ツリー、タスク、メモリの確認ができます。
- **他の人のファームウェアを試す。** マージ済みの `.bin`、`idf.py` のビルドフォルダ、`.pebundle` をそのまま実行できます。データはすべてブラウザ内にとどまり、アップロードされません。
- **自動化。** スクリプト、CI、AI エージェント向けにコマンドラインと MCP サーバーを用意しています。

## クイックスタート

いちばん手軽なのは[オンライン版](https://passportsim.bugs.cc)です。開いてファームウェアをドロップするだけです。

最新リリースから CLI をインストールするには：

```sh
curl -fsSL https://passportsim.bugs.cc/install.sh | sh      # macOS(Apple シリコン)
```

```powershell
irm https://passportsim.bugs.cc/install.ps1 | iex           # Windows x64(PowerShell)
```

ソースからビルドして動かすには [Rust](https://rustup.rs)、[Bun](https://bun.sh)、[just](https://github.com/casey/just)(または `make`)が必要です。

```sh
just setup    # 初回のみ
just run      # ビルドして Web UI を http://127.0.0.1:4173/ で開く
```

その他のコマンド：

| コマンド | 内容 |
|---|---|
| `just cli start --fw official` | 任意の引数でコマンドラインを実行 |
| `just test` | ユニットテスト |
| `just package` | このコンピューター向けのリリースパッケージを `target/package/` に作成 |
| `just deploy` | Web UI を Cloudflare Workers に公開 |
| `just` | すべてのコマンドを表示 |

`make` では引数を変数で渡します：`make run PORT=8080`、`make cli ARGS="start"`。

## ブラウザで使う

- **シンプルモード**が最初に開きます：デバイス、ファームウェアのドロップ先、ログ。
- **アドバンスモード**では、実行制御、シリアルコンソール、UI ツリー、イベント記録、バッテリー、USB、オーディオ、NFC、Wi-Fi、Bluetooth のカードが加わります。

日本語、英語、中国語、フランス語に対応し、ライトテーマとダークテーマを選べます。

<table>
  <tr>
    <td width="50%"><img src="../../images/web-simple-dark.png" alt="中国語・ダークテーマのシンプルモード、ファームウェアを読み込んだ直後"></td>
    <td width="50%"><img src="../../images/web-advanced-console.png" alt="アドバンスモードとシリアルコンソール"></td>
  </tr>
  <tr>
    <td align="center"><sub>自分のファームウェアを読み込む</sub></td>
    <td align="center"><sub>アドバンスモードとシリアルコンソール</sub></td>
  </tr>
</table>

## コマンドラインで使う

```sh
just package
cd target/package/passportsim-*-macos-arm64
./passportsim start                  # デモを起動
./passportsim start path/to/firmware.bin
./passportsim screenshot             # 画面を PNG で保存
./passportsim serial read            # シリアル出力を読む
./passportsim stop
```

Windows では実行ファイル名が `passportsim.exe` です。各 OS での初回起動は[クイックスタート](quickstart.md)を参照してください。

## AI エージェントと使う

`passportsim mcp` は MCP サーバーです。MCP クライアントに追加します：

```json
{ "mcpServers": { "passportsim": { "command": "/path/to/passportsim", "args": ["mcp"] } } }
```

エージェントはファームウェアの起動、ボタン操作、シリアル出力の待機、UI ツリーの読み取り、スクリーンショットができます。[エージェント用スキル](../../../skills/passportsim/SKILL.md)は `npx skills add BlackHole1/passportsim` で追加するか、`skills/passportsim/` をエージェントのスキルディレクトリにコピーしてください。

<div align="center">
<img src="../../images/web-advanced-ui-tree.png" alt="UI ツリータブ：エージェントが読むウィジェットツリー。ホバー中のウィジェットが画面上で枠表示される" width="900">
<br><sub>エージェントが読む UI ツリー。ホバー中のウィジェットが画面上で枠表示されます</sub>
</div>

## 対応環境

| | |
|---|---|
| macOS(Apple シリコン) | 対応 |
| Windows 10 以降、x64 | 対応 |
| ブラウザ | Chrome、Edge、Firefox、Safari |
| Linux | 非対応 |

## 詳しく知る

| | |
|---|---|
| [仕組み](overview.md) | エミュレートの対象、アーキテクチャ、決定性と忠実度 |
| [クイックスタート](quickstart.md) | パッケージ、初回起動、デーモン、Web バンドル、Windows |
| [コマンド](../../commands/) | 各コマンドの引数とエラー |
| [Cloudflare へのデプロイ](deploy-cloudflare.md) | Web UI を静的サイトとして公開 |
| [アーキテクチャ](ARCHITECTURE.md) | 設計の全体 |

自動生成されるリファレンス(コマンドリファレンス、エラーコード、fidelity.md、schema)と THIRD_PARTY.md、LICENSE は英語版のみです。

## コントリビュートとライセンス

コントリビューションを歓迎します。[CONTRIBUTING.md](CONTRIBUTING.md) と[行動規範](CODE_OF_CONDUCT.md)をお読みください。セキュリティ上の問題は [SECURITY.md](SECURITY.md) の手順で報告してください。

MIT ライセンスです([LICENSE](../../../LICENSE))。サードパーティの素材は [THIRD_PARTY.md](../../../THIRD_PARTY.md) に記載しています。FoloToy、AI Passport、Espressif、ESP32 はそれぞれの所有者の名称です。
