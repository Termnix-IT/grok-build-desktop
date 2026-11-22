# AGENTS.md

Grok Build Desktop は、インストール済みの `grok` とデスクトップ画面で会話する Windows アプリです。会話の正本は `~/.grok/sessions` に残り、このアプリは写しを持ちません。

もう 1 つの使い方として「自律実行」があります。司令塔の Grok が目的を分解し、登録した CLI エージェント（Claude Code、Codex、Grok、Antigravity、カスタム）を 1 つずつ非対話で起動し、結果と git の差分を見て次の一手を決めます。

## 境界

Grok 自体は再実装しません。チャットの子プロセスは次の引数で 1 つだけ起動します。

```text
grok --permission-mode default agent --no-leader stdio
```

`--permission-mode default` は、ユーザーの `~/.grok/config.toml` が常時承認でも、この子プロセスのツール実行を確認要求にします。安全なものは画面が一度だけ許可し、危険なものは確認カードにします。その設定ファイルは編集しません。`--no-leader` は、開いているターミナルの Grok にこの画面をくっつけないためです。チャットでは `grok -p`、`grok agent serve` を使いません。xAI API の直呼びは、チャットでも自律実行でも使いません。

自律実行は ACP の接続とは別に、短命のプロセスを起動します。司令塔は 1 ターンごとに次の形で起動し、2 ターン目からは `-s` の代わりに `-r <UUID>` で同じ会話を続けます。`--permission-mode plan` は外しません。司令塔は判断だけを返し、ファイルを書き換えません。

```text
grok --prompt-file <file> --json-schema <schema> --permission-mode plan --no-subagents --no-memory --cwd <project> --rules <rules> -s <UUID>
```

作業役の CLI は、設定の「エージェント」にある引数で起動します。起動はすべて `CREATE_NO_WINDOW`（`0x0800_0000`）付きで、Windows では Job Object に入れて、止めるときは孫プロセスまで終了させます。

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

画面の確認は Tauri のウィンドウで行います。Vite は `http://localhost:1420` です。`127.0.0.1:1420` は拒否されます。OS のタイトルバーは出さず、ドラッグと最小化、最大化、閉じるは画面内の上端です。WebView の確認に CDP を使うときは、ポートの応答ではなくウィンドウタイトル「Grok Build Desktop」で見ます。

## 構成

- `src/App.tsx` と `src/App.css` が画面です。チャットの状態の更新は `src/model.ts` の `reduceDesk` に、自律実行の状態の更新は `src/orchestra.ts` の `reduceOrchestra` に集めます。
- `src/OrchestraView.tsx` が自律実行のタイムラインとログのドロワー、`src/AgentSettings.tsx` が設定の「エージェント」、`src/MarkdownBody.tsx` が会話とタイムラインで共有する Markdown 表示です。
- `src-tauri/src/orchestra/` が自律実行です。`config.rs` が CLI の登録と危険なフラグの検査、`adapter.rs` が起動方法と出力の読み取り、`runner.rs` が子プロセス、`planner.rs` が司令塔、`decision.rs` が司令塔の JSON、`breaker.rs` が止める条件、`git.rs` が差分、`store.rs` が記録の保存、`engine.rs` がループです。テストは各ファイルへ足します。
- `src-tauri/src/agent.rs` が ACP のホストです。公式 crate `agent-client-protocol` 3.4.0 の protocol v1、feature `process` を使います。JSON-RPC の読み取りループは自作しません。
- `src-tauri/src/usage.rs` が使用量、ファイル URI、添付ブロック、起動中セッションの判定です。テストはここへ足します。
- `src-tauri/src/permit.rs` が自動承認の判定です。テストはここへ足します。
- `src-tauri/src/lib.rs` が Tauri コマンドです。JavaScript からは camelCase で渡り、Rust 側は snake_case です。

## ACP

同時に接続するセッションは 1 つです。フォルダを変えると一覧の絞り込みだけが変わり、開いている会話は閉じません。

`session/delete` はありません。削除は確認のあと、開いているセッションを閉じてから `grok sessions delete <ID>` を実行します。ID は位置引数で、`--yes` は付けません。

セッション一覧は `session/new` の直後だけでは空のままになります。`_x.ai/sessions/changed` を受けたときと、各ターンのあとにも取り直します。

権限は、設定の「自動承認」がオンのときだけ、読み取り、検索、思考、プロジェクト内の編集、および内容を確認できる安全なコマンドを `allow_once` で返します。`allow_always` は自動では選びません。削除、移動、外部取得、モード変更、プロジェクト外の編集、読み取れないコマンド、そして削除、上書き、外部通信、公開、権限変更、パッケージスクリプトを含むコマンドは確認カードを出します。自動承認がオフのときは、すべて確認カードにします。初期値はオンで、`localStorage` の `gbd-auto-approve` が `"0"` のときはオフです。確認カードのボタンは `kind` から日本語にします。起動時に `session/set_mode` は送りません。選択表示は `current_mode_update` が来るまで未確認のままにします。`session/set_mode` の成功応答だけでは、モードが変わったことにしません。

コンテキストの空きは、ターン終了時の `inputTokens` と、現在モデルの `availableModels[]._meta.totalContextTokens` から計算します。現行のモデル ID は `modelState.currentModelId` です。`modelState.current` や `modelState.totalContextTokens` は古い形なので、それだけを見ると空きが出ません。`/context` の結果や、ストリーム途中の `_meta.totalTokens` は使いません。金額には換算しません。

`/compact` と `/context` は、エージェントが文字も使用量も圧縮通知も返さないことがあります。そのときは ACP が結果を返さなかったと画面に出します。セッションファイルを解析して使用量を作りません。巻き戻しの RPC は未実装なので画面に出さず、ファイルも戻しません。

画像は、初期化で `prompt_capabilities.image` が真のときだけ送ります。偽のときは「この Grok は画像入力を告知していません」と出して、画像ブロックは作りません。png、jpeg、gif、webp で、拡張子とマジックバイトが一致し、1 枚 8MiB までです。中身は base64 の `Image` ブロックで、`file://` は画像の本体にしません。音声は送りません。画像以外のファイルは今までどおり `ResourceLink` で、Windows のパスは `file:///C:/...` の URI にします。再生時の画像は「（画像）」と出します。

`/usage`（別名 `/cost`）は Grok の使用量を聞くコマンドです。設定の「使用量を確認」から送り、返った文章を設定の中に出します。セッションのコンテキスト使用量とは別です。使えるコマンド一覧に無ければ送りません。ACP が内訳を返さないときは、その旨を設定の中に出します。`auth.json` や課金 API は読みません。

`ready` は起動時に一度だけ届きます。画面を読み直したあとは `app_snapshot` で、欠けている ready、一覧、セッション、設定、モード、使用量を埋めます。

通知ハンドラはすぐ戻します。プロンプトと権限の待ちは `ConnectionTo::spawn` に逃がし、エラーで接続を落とさないように `Ok(())` で戻します。`spawn` に渡す接続と、その async ブロックの中で使う接続は、別の変数に clone してから使います。同じ束縛を move するとコンパイルできません。

`~/.grok/active_sessions.json` に、この子プロセス以外の PID が同じセッションを持っているときは、警告を出してから開きます。ユーザーがターミナルで開いているセッションを、警告なしに load したり削除したりしません。

## 自律実行

同時に動かせるタスクは 1 つで、エージェントも 1 つずつ順に動かします。始められるのは、git のリポジトリで 1 つ以上のコミットがあるフォルダだけです。未コミットの変更があるときは確認してから始めます。各ステップの差分は、直前の `git stash create`（変更が無ければ HEAD）と作業ツリーを比べて取ります。`git stash create` は作業ツリーにも stash の一覧にも触れないので、作業前からある変更はステップの差分に混ざりません。

司令塔の返事は `decision.rs` のスキーマに従う JSON で、`next_action` は `dispatch`、`complete`、`ask_human` のどれかです。`dispatch` の `target_agent` は有効なエージェントの ID に限ります。`grok --json-schema` が成功したときの外側の形はまだ実機で確かめていないので、パーサーは JSON の中から `next_action` を持つオブジェクトを探します。失敗のときは stdout に `{"type":"error","message":…}` が出ます。

止める条件は 3 つです。判断の回数が `maxTurns`（初期値 10）に達したら一時停止し、再開すると同じ回数だけ上限を足します。同じ失敗が `sameErrorLimit`（初期値 2）回続いたら一時停止します。失敗が同じかどうかは、エージェント、終了コード、stderr の末尾の数字とパスを伏せて比べます。1 回の起動は `dispatchTimeoutSecs`（初期値 1200 秒）で打ち切ります。「ループを止める」は確認なしで、実行中のプロセスを Job Object ごと終了します。

危険なフラグ（`--dangerously-*`、`--yolo`、`--always-approve`、`bypassPermissions`、`danger-full-access`、`--approve all`）は、エージェントごとの「危険なフラグを許可」がオンのときだけ保存も起動もできます。`-c sandbox_mode="danger-full-access"` や `--settings` の JSON に埋めた値も、引数の部分文字列として検出します。司令塔の追加の引数には、危険なフラグも、アプリが付ける引数（`--permission-mode`、`--json-schema`、`-s`、`-r` など）も入れられません。npm の `.cmd` や `.bat` は cmd.exe が引数を解釈し直すので、プロンプトを引数で渡す方式を拒否し、標準入力かファイルで渡します。Rust の `Command::new("codex")` は `.exe` しか探さないので、プログラムは `PATHEXT` を見て解決します。拡張子のない名前には `PATHEXT` の拡張子を付けたものだけを探します。npm は `.cmd` の隣に拡張子のないシェルスクリプトを置きますが、Windows はそれを起動できないためです。

司令塔の 1 回目の呼び出しが失敗したときは、次の呼び出しで新しいセッション ID を使います。失敗した呼び出しがセッションを作っていると、`-s` に同じ ID を渡せない可能性があるからです（実機では未確認）。司令塔が正常に終了したら、返事が読めなくても次からは `-r` で続けます。

CLI の登録は `app_config_dir` の `agents.json` に、`version` 付きで置きます。読み書きは Rust 側だけが行い、壊れていれば既定値で動きます。タスクの記録は `app_data_dir/tasks/<ID>.json` に保存します。生ログはノードごとに末尾 2000 行をメモリに持つだけで、ファイルには残しません。アプリを閉じたときに動いていたタスクは、次に開いたとき停止済みにします。

イベントは `orchestrator://event`（`task`、`planning`、`decision`、`node-started`、`node-finished`、`state`）と `agent://log`（ノードごとの行のまとまり）です。司令塔の Grok セッションはチャットの一覧に出さず、タスクとして見せます。

## 画面

表示文は日本語、識別子は英語です。説明のための副文は置かず、配置で役割が分かるようにします。全体は黒に近いシャープな三列で、送信欄だけ角丸 22px と円形のボタンにします。権限、モデル、推論の強さは送信欄の下です。これらはボタンと一覧で、項目にマウスを重ねても背景色も文字色も変えず、選んでいる項目だけに印を付けます。コンテキストは、モデルの左に使用割合の円を出し、右の列には今までどおり横棒と空き・使用・ウィンドウの数値を残します。使えるコマンドは左下の設定にまとめ、右の列には置きません。設定はポップアップで、左の分類を選ぶと右にその中身が出ます。分類は表示、自動承認、エージェント、使用量、コマンド一覧、このアプリです。画面の動きは表示の「開閉の動きを出す」で切ります。切るとコンテキストと左のフォルダの開閉はすぐ終わります。初期値はオンで、`localStorage` の `gbd-motion` が `"0"` のときはオフです。会話の表示は「会話と結果」（思考を隠す）、「思考」（発言・返事・思考を出し、ツールとプランを隠す）、「すべて」から選びます。左の作成は「新規チャット」と「新規プロジェクト」で、印は別の SVG です。登録フォルダの下には、選んでいるフォルダの会話だけを罫線で出し、畳めます。開閉の印は各フォルダに付けます。セッションが開いていないときは、会話欄の中央でプロジェクトを選び、選んだ状態で送るとそのフォルダの新しいセッションになります。設定のポップアップは中身が変わっても同じ大きさです。右のコンテキストは上端の同じボタンで、幅と矢印を動かして畳みます。セッションを閉じる操作はコンテキストのペインに置きます。入力欄をクリックしても青い選択枠は出しません。入力欄はテキストエリアのままで、下書きに Markdown の印があるあいだは送信前でも整形して見せます。会話の Markdown は見えている範囲だけを描きます。ツールは閉じて出し、開いたものだけ中身を描きます。画像のドロップは、Grok が画像入力を告知しているときは画像として送り、告知していないか画像として送れないときは絶対パスを入力欄へ入れます。暖色の明朝見出しや、紫を基調にした汎用チャットの見た目には戻しません。

送信欄の下の左端で「チャット」と「自律実行」を切り替えます。初期値はチャットで、`localStorage` の `gbd-flow-mode` が `"1"` のときは自律実行です。自律実行のあいだは、権限の位置に「自律実行中」「承認待ち」「一時停止」「完了確認」などの状態を出し、モデルの位置にターン数を出します。左の作成は「新規タスク」になります。タスクが動いているあいだは、送信欄に「ループを止める」を出します。添付は使わず、ドロップしたパスや `@` で選んだパスは入力欄の文字にします。左の木では、選んでいるフォルダのタスクを会話より上に出し、その下にステップを `[Claude] 実装` の形で並べ、動作中・完了・エラー・要確認・停止の印を付けます。タスクを選ぶと会話欄が Grok の判断のタイムラインになります。カードには理由、送り先のバッジ、畳んだプロンプト、結果の要約と差分の統計を出し、「ログ」でその CLI の stdout と stderr を会話欄の下のドロワーに出します。ドロワーは末尾に追従し、上へ戻すと追従をやめます。

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

現在の開発βは v0.0.5 です。既定ブランチは `main` です。公開先は https://github.com/Termnix-IT/grok-build-desktop です。`node_modules`、`dist`、`src-tauri/target` はコミットしません。
