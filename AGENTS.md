# AGENTS.md

Grok Build Desktop は、インストール済みの `grok` とデスクトップ画面で会話する Windows アプリです。会話の正本は `~/.grok/sessions` に残り、このアプリは写しを持ちません。

## 境界

Grok 自体は再実装しません。子プロセスは次の引数で 1 つだけ起動します。

```text
grok --permission-mode default agent --no-leader stdio
```

`--permission-mode default` は、ユーザーの `~/.grok/config.toml` が常時承認でも、この子プロセスのツール実行だけ確認カードにします。その設定ファイルは編集しません。`--no-leader` は、開いているターミナルの Grok にこの画面をくっつけないためです。`grok -p`、`grok agent serve`、xAI API の直呼びは使いません。

ログインは `grok login` を Windows の `CREATE_NEW_CONSOLE`（`0x10`）で開きます。鍵や `auth.json` は読まず、保存しません。

## コマンド

開発時は PowerShell で次を実行します。

```powershell
npm install
npm run tauri dev
```

リリース用の Windows インストーラは NSIS だけ作ります。

```powershell
npm run tauri build -- --bundles nsis
```

Grok を起動しない単体テストは次です。

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --lib
```

画面の確認は Tauri のウィンドウで行います。Vite は `http://localhost:1420` です。`127.0.0.1:1420` は拒否されます。WebView の確認に CDP を使うときは、ポートの応答ではなくウィンドウタイトル「Grok Build Desktop」で見ます。

## 構成

- `src/App.tsx` と `src/App.css` が画面です。状態の更新は `src/model.ts` の `reduceDesk` に集めます。
- `src-tauri/src/agent.rs` が ACP のホストです。公式 crate `agent-client-protocol` 3.4.0 の protocol v1、feature `process` を使います。JSON-RPC の読み取りループは自作しません。
- `src-tauri/src/usage.rs` が使用量、ファイル URI、添付ブロック、起動中セッションの判定です。テストはここへ足します。
- `src-tauri/src/lib.rs` が Tauri コマンドです。JavaScript からは camelCase で渡り、Rust 側は snake_case です。

## ACP

同時に接続するセッションは 1 つです。フォルダを変えると一覧の絞り込みだけが変わり、開いている会話は閉じません。

`session/delete` はありません。削除は確認のあと、開いているセッションを閉じてから `grok sessions delete <ID>` を実行します。ID は位置引数で、`--yes` は付けません。

セッション一覧は `session/new` の直後だけでは空のままになります。`_x.ai/sessions/changed` を受けたときと、各ターンのあとにも取り直します。

権限は自動承認しません。ボタンの文言はエージェントが返した `name` をそのまま使います。起動時に `session/set_mode` は送りません。選択表示は `current_mode_update` が来るまで未確認のままにします。`session/set_mode` の成功応答だけでは、モードが変わったことにしません。

コンテキストの空きは、ターン終了時の `inputTokens` と、現在モデルの `availableModels[]._meta.totalContextTokens` から計算します。現行のモデル ID は `modelState.currentModelId` です。`modelState.current` や `modelState.totalContextTokens` は古い形なので、それだけを見ると空きが出ません。`/context` の結果や、ストリーム途中の `_meta.totalTokens` は使いません。金額には換算しません。

`/compact` と `/context` は、エージェントが文字も使用量も圧縮通知も返さないことがあります。そのときは ACP が結果を返さなかったと画面に出します。セッションファイルを解析して使用量を作りません。巻き戻しの RPC は未実装なので画面に出さず、ファイルも戻しません。

画像と音声は送りません。ファイル添付は `ResourceLink` で、Windows のパスは `file:///C:/...` の URI にします。

`ready` は起動時に一度だけ届きます。画面を読み直したあとは `app_snapshot` で、欠けている ready、一覧、セッション、設定、モード、使用量を埋めます。

通知ハンドラはすぐ戻します。プロンプトと権限の待ちは `ConnectionTo::spawn` に逃がし、エラーで接続を落とさないように `Ok(())` で戻します。`spawn` に渡す接続と、その async ブロックの中で使う接続は、別の変数に clone してから使います。同じ束縛を move するとコンパイルできません。

`~/.grok/active_sessions.json` に、この子プロセス以外の PID が同じセッションを持っているときは、警告を出してから開きます。ユーザーがターミナルで開いているセッションを、警告なしに load したり削除したりしません。

## 画面

表示文は日本語、識別子は英語です。全体は黒に近いシャープな三列で、送信欄だけ角丸 22px と円形のボタンにします。権限、モデル、推論の強さは送信欄の下です。コンテキストは使用割合の横棒と、空き・使用・ウィンドウの数値です。暖色の明朝見出しや、紫を基調にした汎用チャットの見た目には戻しません。

## 版と Git

機能追加とバグ修正は `claude/` で始まる作業ブランチにコミットして積みます。作業ブランチは push してよいです。`main` への merge、版番号の更新、タグ、prerelease、インストーラの公開は、ユーザーがその反映を判断したときだけ行います。一つの修正が終わるたびに `main` へ混ぜたり、版を上げたりしません。

ユーザーが反映を指示したときは、作業ブランチを `main` へ fast-forward で取り込み、その内容に対応する版へ上げます。版番号は `package.json`、`src-tauri/Cargo.toml`、`src-tauri/Cargo.lock`、`src-tauri/tauri.conf.json` で揃えます。Windows 用の NSIS インストーラを作り、タグと prerelease にその成果物を置きます。

現在の開発βは v0.0.1 です。既定ブランチは `main` です。公開先は https://github.com/Termnix-IT/grok-build-desktop です。`node_modules`、`dist`、`src-tauri/target` はコミットしません。
