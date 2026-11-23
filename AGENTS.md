# AGENTS.md

Grok Build Desktop は、インストール済みの `grok` とデスクトップ画面で会話する Windows アプリです。会話の正本は `~/.grok/sessions` に残り、このアプリは写しを持ちません。

もう 1 つの使い方として「連携」があります。外部の Grok Bot が司令塔になり、付属の `gbd.exe` を 1 回ずつ実行して作業を頼みます。このアプリは頼まれたジョブを、登録した CLI エージェント（Claude Code、Codex、Grok、Antigravity、カスタム）で 1 つずつ非対話で動かし、結果と git の差分を短い JSON で返します。次に何をするかはこのアプリでは決めません。

## 境界

Grok 自体は再実装しません。チャットの子プロセスは次の引数で 1 つだけ起動します。

```text
grok --permission-mode default agent --no-leader stdio
```

`--permission-mode default` は、ユーザーの `~/.grok/config.toml` が常時承認でも、この子プロセスのツール実行を確認要求にします。安全なものは画面が一度だけ許可し、危険なものは確認カードにします。その設定ファイルは編集しません。`--no-leader` は、開いているターミナルの Grok にこの画面をくっつけないためです。チャットでは `grok -p`、`grok agent serve` を使いません。xAI API の直呼びは、チャットでも連携でも使いません。

連携のジョブは ACP の接続とは別に、短命のプロセスとして起動します。作業役の CLI は、設定の「エージェント」にある引数で起動します。起動はすべて `CREATE_NO_WINDOW`（`0x0800_0000`）付きで、Windows では Job Object に入れて、止めるときは孫プロセスまで終了させます。

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

`npm run tauri dev` は本体の exe だけをビルドします。連携を試すときは、`gbd.exe` も同じフォルダに作ります。

```powershell
cargo build --manifest-path src-tauri/Cargo.toml --bin gbd
```

Grok を起動しない単体テストは次です。

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --lib
```

画面の確認は Tauri のウィンドウで行います。Vite は `http://localhost:1420` です。`127.0.0.1:1420` は拒否されます。OS のタイトルバーは出さず、ドラッグと最小化、最大化、閉じるは画面内の上端です。WebView の確認に CDP を使うときは、ポートの応答ではなくウィンドウタイトル「Grok Build Desktop」で見ます。

## 構成

- `src/App.tsx` と `src/App.css` が画面です。チャットの状態の更新は `src/model.ts` の `reduceDesk` に、連携の状態の更新は `src/orchestra.ts` の `reduceOrchestra` に集めます。
- `src/OrchestraView.tsx` がタスクのジョブのカードとログのドロワー、`src/AgentSettings.tsx` が設定の「エージェント」、`src/GatewaySettings.tsx` が設定の「連携」、`src/MarkdownBody.tsx` が会話とカードで共有する Markdown 表示です。
- `src-tauri/src/orchestra/` が連携です。`config.rs` が CLI の登録と上限と危険なフラグの検査、`adapter.rs` が起動方法と出力の読み取り、`runner.rs` が子プロセス、`breaker.rs` が失敗の比較、`git.rs` が作業前の記録と差分、`store.rs` が記録の保存、`engine.rs` がキューと止める条件、`ipc.rs` が名前付きパイプ、`cli.rs` が `gbd.exe` の中身です。`src-tauri/src/bin/gbd.rs` は `cli.rs` を呼ぶだけです。テストは各ファイルへ足します。エンジンとパイプのテストは、`testkit.rs` の偽のエージェント（PowerShell のスクリプト）を使います。
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

## 連携

Grok Bot は `gbd.exe` を 1 回ずつ実行します。`gbd.exe` は `\\.\pipe\grok-build-desktop-hub-<SID>` に 1 行の JSON を送り、返った 1 行の JSON を標準出力に出します。コマンドは `version`、`agents`、`submit <file|->`、`status`、`wait [--timeout N]`、`result`、`log [--tail N]`、`cancel` で、`--ascii` を付けると ASCII 以外を `\uXXXX` にします。終了コードは、成功が 0、使い方やジョブファイルの誤りが 2、それ以外の失敗が 1 です。ジョブ ID は `j`、タスク ID は `t` に 16 進 12 桁を続けたものです。外に出す JSON は snake_case、アプリの中の記録は camelCase です。

パイプは、同じユーザーの SID だけに全権を与える DACL（`D:P(A;;GA;;;<SID>)`）で作り、`first_pipe_instance` と `reject_remote_clients` を付けます。`gbd.exe` は接続したあと、パイプのサーバーのプロセスが同じ SID で、自分以上の整合性レベル（自分が medium より上なら medium 以上）で動いているかを確かめてから要求を送ります。管理者のターミナルから動かした `gbd.exe` も、普通に起動したアプリにつながります。同じユーザーの低い整合性レベルのプロセス（サンドボックスの中のもの）が、アプリを閉じているあいだにパイプを作り、ジョブを読んだり偽の結果を返したりできないようにするためです。クライアントは `SECURITY_IDENTIFICATION` で接続し、サーバーに自分の権限を使わせません。要求は 1 行 1 MiB まで、読み取りは 10 秒、応答の書き込みも 10 秒で打ち切ります。同時に扱う接続は 16 までで、それを超えた接続は前の接続が終わるまで待ちます。`gbd.exe` は、`wait` では指定した秒数に 60 秒を足した時間、ほかのコマンドでは 60 秒で応答を待つのをやめます。

Grok Bot からのジョブは、設定の「連携」で受付がオンのときに、許可したフォルダの中だけで受け付けます。許可の判定、既存のタスクとの一致、連続実行の数、タスクの一覧では、フォルダを正規化してディスク上の大文字小文字にしたパスを、大文字小文字を区別して比べます。Windows でも大文字小文字を区別するフォルダでは、`Repo` と `repo` が別のフォルダになれるためです。Grok Bot が見られるのは Grok Bot が出したジョブだけで、画面から出したジョブを聞かれたら `not_found` を返します。画面からのジョブは受付と許可フォルダの検査を受けず、そのプロジェクトの連続実行の数を 0 に戻します。

ジョブは FIFO のキューで 1 つずつ動かします。始められるのは、git のリポジトリで 1 つ以上のコミットがあるフォルダだけです。マージが終わっていないファイルがあるときは、エージェントを起動する前に失敗にします。各ジョブの前後で、index の写しを `GIT_INDEX_FILE` に指定して `git add -A` と `git write-tree` を実行し、二つのツリーを比べて変更したファイルと `--stat` を出します。本物の index、stash の一覧、作業ツリーには触れません。読み取りだけの git にも `GIT_OPTIONAL_LOCKS=0` を付け、本物の index を書き直させません。エージェントは `.git/config` や `.git/hooks` を書けるので、このアプリが起動する git には `core.fsmonitor=false`、`protocol.allow=never`、作らないフォルダを指す `core.hooksPath`、`GIT_NO_LAZY_FETCH=1` を付けます。部分クローンで欠けたオブジェクトを取りに行くと、リポジトリの設定の ssh コマンドが動くためです。filter は、システムとユーザーの設定にあり、出どころのファイルがプロジェクトの外にあるものだけを使い、それ以外は空にして止めます。ユーザーの設定の include がプロジェクトの中のファイルを読むこともあるので、範囲だけでは決めません。止める設定は `-c` ではなく `GIT_CONFIG_KEY_n` と `GIT_CONFIG_VALUE_n` で渡します。`-c` は最初の `=` で分けるので、`a=b` という名前のドライバーを止められないためです。信用しない設定に `lfs.extension.*` があるときは、Git LFS が clean のときにそのコマンドを実行するので、Git LFS も含めてすべての filter を止めます。filter は `git add` だけでなく `write-tree` や `diff` からも起動されるので、このリポジトリへのすべての git の呼び出しに付けます。この比べ方なら、作業前から未追跡だったファイルへの編集も差分に入ります。`git stash create` は未追跡のファイルを記録しないので使いません。作業前のツリーの ID を「作業前」の地点として返し、`git restore --source=<ID> --worktree -- .` で戻せます。

止める条件は次のとおりです。数値は設定の「エージェント」の「上限」で変えられます。

- 1 つのタスクのジョブの数が `maxDispatchesPerTask`（初期値 10）に達すると、タスクを一時停止します。再開すると同じ数だけ上限を足します。
- 同じ失敗が `sameErrorLimit`（初期値 2）回続くと一時停止します。失敗が同じかどうかは、エージェント、終了コード、stderr の末尾の数字とパスを伏せて比べます。
- `pauseOnDeletion`（初期値オン）のとき、作業前からあったファイルが消えたら一時停止します。ジョブのあとの差分を取れなかったときも、削除がないことを確かめられないので一時停止します。
- Grok Bot からのジョブが同じプロジェクトで `maxConsecutivePerProject`（初期値 30）回続くと、`project_paused` で断ります。画面でそのプロジェクトのタスクを再開するか、設定の「連携」の拒否の一覧から再開すると、数が 0 に戻ります。
- 待ちのジョブは `maxQueue`（初期値 20）までです。
- 1 回のジョブは、エージェントの `timeoutSecs`、無ければ `defaultTimeoutSecs`（初期値 1200 秒）で打ち切ります。ジョブの `timeout_secs` では、10 秒以上で、それより短くすることだけができます。

一時停止したタスクは待ちのジョブを取り消し、再開するまで新しいジョブを受けません。カードの「止める」はそのジョブだけを終了し、カードの「緊急停止」はジョブを終了してタスクを一時停止します。送信欄の「緊急停止」は、1 つのロックの中で Grok Bot の受付を止め、待ちのジョブをすべて取り消し、実行中のジョブを Job Object ごと終了します。受付を止めたことは `gateway.json` に書き、書けなくてもメモリの上では止めます。

CLI の出力は次のように読みます。Claude Code は stream-json の `result` を答えにし、`permission_denials` を拒否された操作にします。Codex は最後の `agent_message` を答えにします。Grok は `--output-format streaming-json` の `text` の断片のうち、最後のツール呼び出しのあとのものを答えにします。ツール呼び出しのあとに文章が無ければ、その前の文章は答えにせず、文章が無かったと返します。Grok の stderr の `permission required: ` は拒否された操作にし、`stopReason` が `cancelled` なら失敗にします。Antigravity は出力の末尾 1 MiB にある最後の JSON の `response` を答えにし、`denied_actions` を拒否された操作にします。その JSON を読めなければ、状態と拒否された操作が分からないので失敗にします。

危険なフラグ（`--dangerously-*`、`--yolo`、`--always-approve`、`bypassPermissions`、`danger-full-access`、`--approve all`）は、エージェントごとの「危険なフラグを許可」がオンのときだけ保存も起動もできます。`-c sandbox_mode="danger-full-access"` や `--settings` の JSON に埋めた値も、`\u0050` のようなエスケープを戻してから、引数の部分文字列として検出します。npm の `.cmd` や `.bat` は cmd.exe が引数を解釈し直すので、プロンプトを引数で渡す方式を拒否し、標準入力かファイルで渡します。Rust の `Command::new("codex")` は `.exe` しか探さないので、プログラムは `PATHEXT` を見て解決します。拡張子のない名前には `PATHEXT` の拡張子を付けたものだけを探します。npm は `.cmd` の隣に拡張子のないシェルスクリプトを置きますが、Windows はそれを起動できないためです。PATH に見つからないときは、レジストリのユーザーとシステムの PATH も探し、子プロセスにはそれを足した PATH を渡します。子プロセスはプロジェクトのフォルダで動き、エージェントはそこに `node.cmd` などを置けます。npm の `.cmd` は `node` を名前で起動し、cmd.exe と Node は PATH より先に今のフォルダを探すので、子プロセスと git には `NoDefaultCurrentDirectoryInExePath=1` を渡し、PATH からは相対の項目を除きます。

CLI の登録と上限は `app_config_dir` の `agents.json` に、`version` 付きで置きます。連携の受付と許可したフォルダは、同じ場所の `gateway.json` です。読み書きは Rust 側だけが行い、壊れていれば既定値で動きます。タスクの記録は `app_data_dir/tasks/<ID>.json` に保存します。生ログはジョブごとに末尾 2000 行、直近 50 ジョブ分をメモリに持つだけで、ファイルには残しません。アプリを閉じたときに動いていたジョブは、次に開いたとき中断にします。

イベントは `orchestrator://event`（`task`、`run`、`gateway`）と `agent://log`（ジョブごとの行のまとまり）です。画面は、イベントと一覧の取得結果を同じキューで順に反映します。古い一覧が新しいイベントを上書きしないためです。キューはアニメーションのフレームか 250 ms のタイマーで流すので、ウィンドウを最小化していても溜まり続けません。画面を読み直したあとは `orchestrator_snapshot` で、実行中と待ちのジョブのタスクと受付の状態を埋めます。

## 画面

表示文は日本語、識別子は英語です。説明のための副文は置かず、配置で役割が分かるようにします。全体は黒に近いシャープな三列で、送信欄だけ角丸 22px と円形のボタンにします。権限、モデル、推論の強さは送信欄の下です。これらはボタンと一覧で、項目にマウスを重ねても背景色も文字色も変えず、選んでいる項目だけに印を付けます。コンテキストは、モデルの左に使用割合の円を出し、右の列には今までどおり横棒と空き・使用・ウィンドウの数値を残します。使えるコマンドは左下の設定にまとめ、右の列には置きません。設定はポップアップで、左の分類を選ぶと右にその中身が出ます。分類は表示、自動承認、エージェント、連携、使用量、コマンド一覧、このアプリです。画面の動きは表示の「開閉の動きを出す」で切ります。切るとコンテキストと左のフォルダの開閉はすぐ終わります。初期値はオンで、`localStorage` の `gbd-motion` が `"0"` のときはオフです。会話の表示は「会話と結果」（思考を隠す）、「思考」（発言・返事・思考を出し、ツールとプランを隠す）、「すべて」から選びます。左の作成は「新規チャット」と「新規プロジェクト」で、印は別の SVG です。登録フォルダの下には、選んでいるフォルダの会話だけを罫線で出し、畳めます。開閉の印は各フォルダに付けます。セッションが開いていないときは、会話欄の中央でプロジェクトを選び、選んだ状態で送るとそのフォルダの新しいセッションになります。設定のポップアップは中身が変わっても同じ大きさです。右のコンテキストは上端の同じボタンで、幅と矢印を動かして畳みます。セッションを閉じる操作はコンテキストのペインに置きます。入力欄をクリックしても青い選択枠は出しません。入力欄はテキストエリアのままで、下書きに Markdown の印があるあいだは送信前でも整形して見せます。会話の Markdown は見えている範囲だけを描きます。ツールは閉じて出し、開いたものだけ中身を描きます。画像のドロップは、Grok が画像入力を告知しているときは画像として送り、告知していないか画像として送れないときは絶対パスを入力欄へ入れます。暖色の明朝見出しや、紫を基調にした汎用チャットの見た目には戻しません。

送信欄の下の左端で「チャット」と「連携」を切り替えます。初期値はチャットで、`localStorage` の `gbd-flow-mode` が `"1"` のときは連携です。送り先は表示しているもので決まるので、左の木でタスクを選ぶと連携に、会話を開くとチャットに切り替わります。連携のあいだは、権限の位置に Grok Bot の受付（受付中・受付停止）と実行中のエージェントを出し、モデルの位置にタスクのジョブの数と上限、送り先のエージェントの選択を出します。選んだエージェントは `gbd-flow-agent` に覚えます。左の作成は「新規タスク」になり、何も選ばずに送ると新しいタスクになります。一時停止や終了のタスクを表示しているあいだは送れません。ジョブが実行中か待ちのあいだは、送信欄に「緊急停止」を出します。添付は使わず、ドロップしたパスや `@` で選んだパスは入力欄の文字にします。左の木では、選んでいるフォルダのタスクを会話より上に出し、その下にジョブを `[Claude] 実装` の形で並べ、実行中・待ち・完了・エラー・要確認・停止の印を付けます。タスクを選ぶと、会話欄がジョブのカードの並びになります。カードには出した側（Grok Bot か画面）、エージェント、状態、終了コード、時間、畳んだプロンプト、要約、拒否された操作、変更したファイル、作業前の地点を出し、「ログ」でその CLI の stdout と stderr を会話欄の下のドロワーに出します。ドロワーは末尾に追従し、上へ戻すと追従をやめます。

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
