# PassportSim へのコントリビュート

[English](../../../CONTRIBUTING.md) | [简体中文](../zh-CN/CONTRIBUTING.md) | **日本語** | [Français](../fr/CONTRIBUTING.md)

ご協力ありがとうございます。PassportSim は実機とまったく同じように動くことを目指しているため、変更は根拠で判断します。すべての動作は、ドキュメント、互換ライセンスのソースコード、または実チップでの測定のいずれかにたどれる必要があります。

[行動規範](CODE_OF_CONDUCT.md)を守ってください。セキュリティ上の問題は公開の Issue ではなく、[SECURITY.md](SECURITY.md) の手順で報告してください。

最も価値のある報告は**忠実度のずれ**、つまりエミュレーターと実機で動作が異なるファームウェアです。イメージ(またはそのビルド方法)、実行したコマンド、エミュレーターと実機それぞれの動作を記載してください。実機のデータは決して添付しないでください([秘密情報](#秘密情報)を参照)。

## セットアップ

[Rust](https://rustup.rs)(`rust-toolchain.toml` のツールチェーンは自動でインストールされます)、[Bun](https://bun.sh) 1.4.1 以降(CI は `.bun-version` のバージョンを使います)、[just](https://github.com/casey/just) が必要です。開発ホストは Apple シリコンの macOS と、x64 の Windows 10 以降です。

```sh
just setup                        # wasm ターゲットと Web ページのパッケージ
cargo xtask secrets-check --init  # クローンごとに 1 回
cargo xtask hooks install         # pre-commit と pre-push のシークレットチェック
```

新しいワークツリーでも毎回 `just setup` を実行してください。`web/node_modules/` は共有されないため、これがないと Web のテストがパッケージ不足のエラーで失敗します。

`just` ですべてのコマンドを表示できます。普段使うのは `just run`(Web ページをビルドして配信)、`just test`、`just check`、`just ci` です。

## プルリクエストを出す前に

```sh
just check              # cargo fmt、clippy、TypeScript の型チェック
just test               # Rust と Web のユニットテスト
cargo xtask codegen     # テーブルとドキュメントを再生成。ツリーに差分が出ないこと
just ci                 # T0 ティア：lint、全テスト、リポジトリのチェック
```

`just ci` は `cargo xtask ci t0` を実行します。ファームウェアコーパスもデバイスデータも不要で、macOS と Windows で同じように動き、実行したマシン自体をテストします。`cargo xtask ci t1` と `t2` はコーパス、ゴールデン、ブラウザでの実行、オラクル比較、ベンチマークを追加し、macOS で実行します。GitHub Actions は両方のホストをカバーします。`pr-check.yml` はすべてのプルリクエストを macOS と Windows でチェックし、`release.yml` はリリースを公開し、`deploy-web.yml` は Web ページをデプロイします。リリースするには、Actions タブから Release ワークフローを実行します(バージョンまたは上げる桁を指定可能、既定は patch)。タグを付け、両ホストのパッケージをビルドし、自動生成のリリースノート付きで GitHub Release を公開し、Web ページをデプロイします。

チェックリスト：

- [ ] `just ci` が通ること。エミュレーションや Web ページを変更し、コーパスを持っている場合は `t1` も通ること
- [ ] 動作の変更には、変更前に失敗していたテストが付いていること
- [ ] 生成ファイルを手で編集していないこと
- [ ] 差分に実機のデータが含まれていないこと

## コミット

- タイトルは短い英語で、Conventional なプレフィックスを付けます：`feat(scope):`、`fix(scope):`、`perf(scope):`、`test(scope):`、`docs(scope):`、`ci:`、`build:`。
- 本文には理由を書き、動作の変更ではその根拠(仕様の行、プローブのキャプチャ、ドキュメントの節)を示します。
- コミットは素の `git commit` で行います。フックをスキップしたり差し替えたりしないでください(`--no-verify`、`core.hooksPath`)。

## クリーンルーム

PassportSim は MIT ライセンスで、公開情報から書かれています。これを保つために：

- **次のソースを読んだりコピーしたりしないでください：** QEMU(Espressif のフォークを含む)、esp32sim、ESP-EMU、NimBLE、Bumble、Zephyr、BlueZ、esptool、serialport-rs、その他 GPL、LGPL、AGPL またはライセンス不明のエミュレーターやスタック。コード、構造、コメントを言い換えて使うことも禁止です。
- **他のエミュレーターはブラックボックスのオラクルとしてのみ実行できます。** `tools/oracle/` や `xtask oracle` のように出力(コンソールのテキスト、レジスタ書き込みの列、トレース)を比較するのは構いませんが、内部を見てはいけません。
- **レジスタとタイミングの事実の出典にできるもの：** Espressif の公開ドキュメント(ESP32-C3 テクニカルリファレンスマニュアルとデータシート)、ESP-IDF のソース(Apache-2.0)、同梱の ROM ELF(Apache-2.0)、実チップで動かした自前のプローブファームウェア(`probes/`)。
- **出典を明記してください。** `specs/` の各行には `provenance` フィールドがあり、各モデルファイルのヘッダーには実装する仕様の行やドキュメントを記します。まだ検証していない前提には `UNVERIFIED` と付けます。T0 では `cargo xtask provenance` がこれをチェックします。

## ファイルの場所

| 内容 | 場所 |
|---|---|
| 設計 | [ARCHITECTURE.md](ARCHITECTURE.md) |
| ユニットテスト | コードの隣(`#[cfg(test)]`)と各クレートの `tests/` |
| 実イメージをブートする結合テスト | `tests/milestones/`(`t1_*` や `t2_*` という名前のテストはその CI ティアで実行) |
| ゴールデン、シナリオ、トランスクリプト | `tests/golden/`、`tests/scenarios/`、`tests/transcripts/` |
| Web のユニットテスト | `web/src/**/*.test.ts`(`bun test`) |
| ブラウザテスト | `web/tests/*.spec.ts`(`just e2e`) |
| 動作データ | `specs/`([specs/README.md](../../../specs/README.md) を参照) |

## 規約

- **生成ファイルは手で編集しません。** 元のソースを変更し、`cargo xtask codegen`(レジスタテーブル、忠実度ドキュメント)または `cargo xtask docs`(コマンドリファレンス)を実行してください。
- **レジストリはローカルです。** ペリフェラルは `periph/mod.rs` ではなく自分のファイルを変更し、コマンドは `#[command]` で自分自身を登録します。
- **凍結されたインターフェースは単独で変更します。** [ARCHITECTURE.md](ARCHITECTURE.md#凍結されたインターフェース) に挙げたコアトレイトと wasm ABI は、それを使うコードより先に、別のプルリクエストで理由を説明して変更します。
- **依存関係はレビューします。** 新しいクレートは `cargo deny check` が通る小さな単独の変更で追加します。許可されるライセンスは `deny.toml` にあるものだけです。
- **コアクレートはホストに依存しません。** `wasm32-unknown-unknown` 向けにビルドでき、時計、スレッド、ファイル、環境変数、ネットワーク、プロセスの API を使いません。ホスト固有のコードは `pemu_host::platform` に、ディレクトリの役割は `pemu_host::paths` に置きます。
- **Rust edition 2024 と `cargo fmt`。** コード、コメント、コミットメッセージは英語で書きます。コメントは短く理由を書き、何をするかはコードで示します。
- **ツリーは LF のみ**で、すべてのパスが Windows で有効である必要があります。`cargo xtask portable` が両方をチェックします。

## 秘密情報

実機のデータはリポジトリに入れません。MAC アドレス、固有 ID、キャリブレーション値、フラッシュや eFuse のダンプ、バックアップのファイル名、カードの内容はすべて対象です。プレースホルダーの MAC には `02:00:00` のプレフィックスを使います。上でインストールしたフックは、こうしたデータを含むコミットを拒否します。ポリシーは [secrets.md](secrets.md) にあります。

## ライセンス

コントリビュートすることで、あなたの貢献がこのリポジトリの MIT ライセンス([LICENSE](../../../LICENSE))で提供されることに同意したものとみなします。
