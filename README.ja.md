# Ferryman

[English](README.md) | [简体中文](README.zh-CN.md) | 日本語

**Ferryman** は、任意の OpenAI 互換 Chat API を使って**対訳（原文＋訳文）**または**訳文のみ**のドキュメントを生成する多形式ドキュメント翻訳ツールです。単体動作する CLI（`ferryman`）と、Docker・Lazycat MicroServer 向けの単一イメージ Web アプリケーション（`ferryman-web`）の両方を提供します。

> [!NOTE]
> 詳細な技術ドキュメント（[`docs/`](docs/cli.md)）は英語を一次情報源（Single Source of Truth）として管理しています。

## 主な特徴

- **9 種類のドキュメント形式に対応**：EPUB、DOCX、PDF、Markdown、プレーンテキスト、SRT、VTT、ASS/SSA、LRC の構造・レイアウト・非テキスト資産を保持したまま翻訳します。
- **任意の OpenAI 互換 API に接続**：クラウドまたはセルフホストの Chat Completions API を利用でき、汎用アダプター（`generic`）と Index-Translate 専用アダプター（`index-translate`）を備えます。ローカル GPU や組み込み推論ランタイムは不要です。
- **中断・再開に強い設計**：コンテンツアドレス方式の翻訳キャッシュ、Web ジョブ単位のプライベートチェックポイント、コンテキスト超過時の自動分割、中断時の部分出力保存に対応します。
- **単一イメージの Web ワークスペース**：Rust バックエンドが Vue 3 フロントエンドを直接配信し、SQLite ジョブキュー、Server-Sent Events（SSE）によるリアルタイム進捗通知、ユーザー単位のストレージ分離、保存時 API キー暗号化（任意）を提供します。
- **Lazycat MicroServer 連携（任意）**：`--features lazycat` でビルドすると、委任認証による配備済みモデル自動検出や、マウント済みドキュメント／クラウドドライブへの直接読み書きが有効になります。

## 対応フォーマット

| 形式 | 拡張子 | 対訳モード（`bilingual`） | 置換モード（`replace`） | 翻訳方式 |
| --- | --- | --- | --- | --- |
| EPUB | `.epub` | `.hy-zh` 要素として直後に追加 | マークアップを保ち本文を置換 | セグメント単位 |
| DOCX | `.docx` | 原文段落の直後に訳文段落を追加 | 書式ランを保ち本文を置換 | セグメント単位 |
| PDF | `.pdf` | 同一配置の訳文ページを交互挿入 | 段落領域を上書きして再描画 | セグメント単位 |
| Markdown | `.md`, `.markdown` | 訳文ブロックを複製して併記 | テキストノードを置換 | 文脈付きバッチ |
| プレーンテキスト | `.txt` | 原文行の次行に訳文を追加 | 行単位で置換 | 文脈付きバッチ |
| SubRip / WebVTT | `.srt`, `.vtt` | 同一キュー内に原文＋訳文を併記 | キュー本文を置換 | 文脈付きバッチ |
| ASS / SSA | `.ass`, `.ssa` | 同一 Dialogue 内で `\N` 改行併記 | 会話本文を置換 | 文脈付きバッチ |
| LRC 歌詞 | `.lrc` | 同一タイムスタンプで訳文行を追加 | 歌詞本文を置換 | 文脈付きバッチ |

各形式の仕様、PDF フォント設定、アーカイブ制限および既知の制約は [docs/formats.md](docs/formats.md) を参照してください。

## クイックスタート

### 1. CLI

ソースからビルドします（Rust ツールチェーンは [`rust-toolchain.toml`](rust-toolchain.toml) で固定されています）：

```bash
cargo build --release --bin ferryman
```

単一ファイルまたはディレクトリ全体を再帰的に翻訳します：

```bash
# 汎用 OpenAI 互換エンドポイントを使う例
export FERRYMAN_API_KEY="sk-..."
./target/release/ferryman \
  -i book.epub -o book.bilingual.epub \
  --endpoint https://api.openai.com/v1 \
  --model gpt-4o-mini \
  --target 日本語

# ローカルの Index-Translate 互換サーバーを使う例
./target/release/ferryman \
  -i ./books --mode replace --target English \
  --endpoint http://127.0.0.1:8088/v1 \
  --model index-translate-2b \
  --adapter index-translate
```

全オプション、出力ファイル名規則、キャッシュおよびリカバリー仕様は [docs/cli.md](docs/cli.md) を参照してください。

### 2. Standalone Docker（Web アプリ）

```bash
cp .env.example .env
docker compose -f docker-compose.standalone.yml up -d --build
```

ブラウザで `http://127.0.0.1:8080` を開き、**模型与 API**（モデルと API 設定）から OpenAI 互換 API を登録して翻訳を開始できます。

> [!NOTE]
> Standalone Docker はデフォルトでシングルユーザーモード（`FERRYMAN_ALLOW_LOCAL_USER=true`）で起動します。複数ユーザー環境や外部公開する場合は、`safe_uid` ヘッダーを付与する認証リバースプロキシの背後に配置してください。詳細は [docs/deployment.md](docs/deployment.md) を参照してください。

### 3. Lazycat MicroServer

Lazycat MicroServer 向けには `lazycat` feature を有効にした単一 LPK パッケージをビルドします：

```bash
bun install --frozen-lockfile
bun --bun lzc-cli project lint .
bun --bun lzc-cli project release -o dist/ferryman-web.lpk
```

前提パッケージ、ローカル Buildx ビルド、成果物検証、モデル検出の委任認証については [docs/packaging.md](docs/packaging.md) を参照してください。

## ドキュメント一覧

| ドキュメント（英語） | 内容 |
| --- | --- |
| [docs/cli.md](docs/cli.md) | CLI 全オプション、プロンプトアダプター、出力規則、キャッシュ、中断処理 |
| [docs/formats.md](docs/formats.md) | 9 形式の動作一覧、EPUB/DOCX/PDF/Markdown/字幕の仕様、フォントと制限 |
| [docs/deployment.md](docs/deployment.md) | Docker 配備、リバースプロキシ認証、全環境変数リファレンス、ストレージと運用 |
| [docs/packaging.md](docs/packaging.md) | Lazycat MicroServer LPK ビルド、検証スクリプト、権限・マウント、モデル検出 |
| [docs/development.md](docs/development.md) | Workspace 構成、ローカル開発、`ts-rs` 型同期、API 一覧、テスト、依存関係ポリシー |
| [CHANGELOG.md](CHANGELOG.md) | バージョン履歴および移行ノート |

## ライセンス

Ferryman は **LGPL-3.0-or-later** の下でライセンスされています。詳細は [COPYING.LESSER](COPYING.LESSER)（LGPL-3.0 本文）および [COPYING](COPYING)（参照される GPL-3.0 本文）を参照してください。
