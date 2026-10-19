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

画像は、初期化で `prompt_capabilities.image` が真のときだけ送ります。偽のときは「この Grok は画像入力を告知していません」と出して、画像ブロックは作りません。png、jpeg、gif、webp で、拡張子とマジックバイトが一致し、1 枚 8MiB までです。中身は base64 の `Image` ブロックで、`file://` は画像の本体にしません。音声は送りません。画像以外のファイルは今までどおり `ResourceLink` で、Windows のパスは `file:///C:/...` の URI にします。再生時の画像は「（画像）」と出します。

`/usage`（別名 `/cost`）は Grok の使用量を聞くコマンドです。設定の「使用量を確認」から送り、返った文章を設定の中に出します。セッションのコンテキスト使用量とは別です。使えるコマンド一覧に無ければ送りません。ACP が内訳を返さないときは、その旨を設定の中に出します。`auth.json` や課金 API は読みません。

`ready` は起動時に一度だけ届きます。画面を読み直したあとは `app_snapshot` で、欠けている ready、一覧、セッション、設定、モード、使用量を埋めます。

通知ハンドラはすぐ戻します。プロンプトと権限の待ちは `ConnectionTo::spawn` に逃がし、エラーで接続を落とさないように `Ok(())` で戻します。`spawn` に渡す接続と、その async ブロックの中で使う接続は、別の変数に clone してから使います。同じ束縛を move するとコンパイルできません。

`~/.grok/active_sessions.json` に、この子プロセス以外の PID が同じセッションを持っているときは、警告を出してから開きます。ユーザーがターミナルで開いているセッションを、警告なしに load したり削除したりしません。

## 画面

表示文は日本語、識別子は英語です。全体は黒に近いシャープな三列で、送信欄だけ角丸 22px と円形のボタンにします。権限、モデル、推論の強さは送信欄の下です。これらはボタンと一覧で、項目にマウスを重ねても背景色も文字色も変えず、選んでいる項目だけに印を付けます。コンテキストは、モデルの左に使用割合の円を出し、右の列には今までどおり横棒と空き・使用・ウィンドウの数値を残します。使えるコマンドは左下の設定にまとめ、右の列には置きません。設定では Grok の使用量と、このアプリ自身の新しい版の確認もします。会話の Markdown は送信後に描画し、入力欄はテキストエリアのままです。暖色の明朝見出しや、紫を基調にした汎用チャットの見た目には戻しません。

## アプリの更新

起動時には更新を確認しません。左下の設定から確認し、適用は確認ダイアログのあとだけ行います。開発中の `npm run tauri dev` では確認できても適用しません。Windows の `installMode` は `passive` で、適用するとアプリが終了してからインストーラが動きます。会話は Grok のセッションに残ります。

確認先は `https://raw.githubusercontent.com/Termnix-IT/grok-build-desktop/main/updates/latest.json` です。GitHub の `/releases/latest` は使いません。公開済みの v0.0.1 にはこの更新機能が入っていないので、そのインストーラからアプリ内では更新できません。v0.0.2 を手で入れた版から、設定の適用が使えます。

署名の公開鍵は `src-tauri/tauri.conf.json` の `plugins.updater.pubkey` にあります。秘密鍵とパスワードはリポジトリの外に置きます。ビルド時は `TAURI_SIGNING_PRIVATE_KEY` に秘密鍵のパスか中身を渡し、パスワードがあるときは `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` も渡します。`TAURI_SIGNING_PRIVATE_KEY_PATH` だけでは署名されません。別の公開鍵に差し替えると、すでに配った版は新しい署名を検証できません。

公開するときは、次の順にします。

1. `TAURI_SIGNING_PRIVATE_KEY` と `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` を設定する。
2. `bundle.createUpdaterArtifacts` は `true` のまま、NSIS のインストーラを作る。`.exe` と `.exe.sig` が出ます。
3. その 2 つを prerelease にアップロードする。
4. `updates/latest.json` を `main` に置く。`version` は semver です。`platforms["windows-x86_64"].url` はインストーラの URL、`signature` は `.sig` ファイルの中身です。パスは書きません。

`updates/latest.json` は、公開を頼まれるまで作りません。

## 版と Git

機能追加とバグ修正は `claude/` で始まる作業ブランチにコミットして積みます。作業ブランチは push してよいです。`main` への merge、版番号の更新、タグ、prerelease、インストーラの公開は、ユーザーがその反映を判断したときだけ行います。一つの修正が終わるたびに `main` へ混ぜたり、版を上げたりしません。

ユーザーが反映を指示したときは、作業ブランチを `main` へ fast-forward で取り込み、その内容に対応する版へ上げます。版番号は `package.json`、`src-tauri/Cargo.toml`、`src-tauri/Cargo.lock`、`src-tauri/tauri.conf.json` で揃えます。Windows 用の NSIS インストーラを作り、タグと prerelease にその成果物を置きます。

現在の開発βは v0.0.2 です。既定ブランチは `main` です。公開先は https://github.com/Termnix-IT/grok-build-desktop です。`node_modules`、`dist`、`src-tauri/target` はコミットしません。
