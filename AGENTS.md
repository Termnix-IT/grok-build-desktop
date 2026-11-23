# AGENTS.md

Grok Build Desktop は、外部の Grok Bot から頼まれた作業を、登録した CLI エージェントで動かして見守る Windows アプリです。Grok Bot が司令塔になり、付属の `gbd.exe` を 1 回ずつ実行して作業を頼みます。このアプリは頼まれたジョブを、登録した CLI エージェント（Claude Code、Codex、Grok、Antigravity、カスタム）で 1 つずつ非対話で動かし、結果と git の差分を短い JSON で返します。次に何をするかはこのアプリでは決めません。

## 境界

Grok 自体は再実装しません。画面は連携だけで、`grok` とのチャットは画面から外しました。ACP のホスト（`agent.rs`）はコードに残していますが、`lib.rs` の `CHAT_ENABLED` が `false` なので、起動時にチャットの子プロセスは作りません。戻すときは、そのフラグと画面の両方を戻します。チャットの子プロセスは次の引数で 1 つだけ起動するものでした。

```text
grok --permission-mode default agent --no-leader stdio
```

`--permission-mode default` は、ユーザーの `~/.grok/config.toml` が常時承認でも、この子プロセスのツール実行を確認要求にします。安全なものは画面が一度だけ許可し、危険なものは確認カードにします。その設定ファイルは編集しません。`--no-leader` は、開いているターミナルの Grok にこの画面をくっつけないためです。チャットでは `grok -p`、`grok agent serve` を使いません。xAI API の直呼びは、チャットでも連携でも使いません。

連携のジョブは ACP の接続とは別に、短命のプロセスとして起動します。作業役の CLI は、設定の「エージェント」にある引数で起動します。起動はすべて `CREATE_NO_WINDOW`（`0x0800_0000`）付きで、Windows では Job Object に入れて、止めるときは孫プロセスまで終了させます。

チャットのログインは `grok login` を Windows の `CREATE_NEW_CONSOLE`（`0x10`）で開きます。鍵や `auth.json` は読まず、保存しません。

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

この中の `orchestra/flow_tests.rs` は、Grok Bot と同じく `gbd` のコマンドからジョブを送り、パイプ、エンジン、各エージェントの種類（Claude Code、Codex、Grok、Antigravity、カスタム）を通して、結果と `gbd` での監視を確かめます。エージェントは、起動の仕方、プロンプトの渡し方、出力の形を本物どおりにした偽の CLI です。

本物の CLI で連携の流れを確かめるときは、アプリを起動し、設定の「連携」で受付をオンにしてから次を実行します。登録した各エージェントに小さなジョブを 1 つずつ送るので、それぞれの使用量を使います。確認用のリポジトリ（既定は `%LOCALAPPDATA%\GrokBuildDesktop-flow-check`）は初回に作られます。許可がないと言われたら、そのフォルダを設定の「連携」で許可に足してから流し直します。CLI が出力の形を変えると、偽の CLI のテストは通ったまま本物だけが壊れるので、CLI を更新したあとにも流します。

```powershell
pwsh -File scripts/check-flow.ps1
```

画面の確認は Tauri のウィンドウで行います。Vite は `http://localhost:1420` です。`127.0.0.1:1420` は拒否されます。OS のタイトルバーは出さず、ドラッグと最小化、最大化、閉じるは画面内の上端です。WebView の確認に CDP を使うときは、ポートの応答ではなくウィンドウタイトル「Grok Build Desktop」で見ます。

## 構成

- `src/App.tsx` と `src/App.css` が画面です。連携の状態の更新は `src/orchestra.ts` の `reduceOrchestra` に集めます。
- `src/OrchestraView.tsx` がタスクのジョブのカードとログのドロワー、`src/AgentSettings.tsx` が設定の「エージェント」、`src/GatewaySettings.tsx` が設定の「連携」、`src/MarkdownBody.tsx` がカードの要約と送信欄の下書きで使う Markdown 表示です。
- `src-tauri/src/orchestra/` が連携です。`config.rs` が CLI の登録と上限と危険なフラグの検査、`adapter.rs` が起動方法と出力の読み取り、`runner.rs` が子プロセス、`breaker.rs` が失敗の比較、`git.rs` が作業前の記録と差分、`store.rs` が記録の保存、`engine.rs` がキューと止める条件、`ipc.rs` が名前付きパイプ、`cli.rs` が `gbd.exe` の中身です。`src-tauri/src/bin/gbd.rs` は `cli.rs` を呼ぶだけです。テストは各ファイルへ足します。エンジンとパイプのテストは、`testkit.rs` の偽のエージェント（PowerShell のスクリプト）を使います。`gbd` から各エージェントの種類までを通すテストは `flow_tests.rs` に足します。`scripts/check-flow.ps1` は、起動中のアプリに本物の `gbd.exe` と本物の CLI で流れを確かめるスクリプトで、日本語を含むので BOM 付きの UTF-8 で保存します。
- `src-tauri/src/agent.rs` が ACP のホストです。公式 crate `agent-client-protocol` 3.4.0 の protocol v1、feature `process` を使います。JSON-RPC の読み取りループは自作しません。
- `src-tauri/src/usage.rs` が使用量、ファイル URI、添付ブロック、起動中セッションの判定です。テストはここへ足します。
- `src-tauri/src/permit.rs` が自動承認の判定です。テストはここへ足します。
- `src-tauri/src/lib.rs` が Tauri コマンドです。JavaScript からは camelCase で渡り、Rust 側は snake_case です。

## ACP

この節は、画面から外したチャットを戻すときのための記録です。`CHAT_ENABLED` が `false` のあいだ、ここに書いた動きはどれも起きません。

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

ジョブは FIFO のキューで 1 つずつ動かします。始められるのは、git のリポジトリで 1 つ以上のコミットがあるフォルダだけです。マージが終わっていないファイルがあるときは、エージェントを起動する前に失敗にします。各ジョブの前後で、index の写しを `GIT_INDEX_FILE` に指定して `git add -A` と `git write-tree` を実行し、二つのツリーを比べて変更したファイルと `--stat` を出します。本物の index、stash の一覧、作業ツリーには触れません。読み取りだけの git にも `GIT_OPTIONAL_LOCKS=0` を付け、本物の index を書き直させません。エージェントは `.git/config` や `.git/hooks` を書けるので、このアプリが起動する git には `core.fsmonitor=false`、`protocol.allow=never`、作らないフォルダを指す `core.hooksPath`、`GIT_NO_LAZY_FETCH=1` を付けます。部分クローンで欠けたオブジェクトを取りに行くと、リポジトリの設定の ssh コマンドが動くためです。リポジトリの設定の `protocol.<名前>.allow` は `protocol.allow=never` より優先されるので、空の `GIT_ALLOW_PROTOCOL` も付けて、どの転送方式も使わせません。git は名前ではなく、PATH から探した `git.exe` の絶対パスで起動します。PATH の前のほうに `git.cmd` があっても使いません。cmd.exe が引数を解釈し直すためです。アプリの環境から受け継いだ `GIT_CONFIG`、`GIT_CONFIG_COUNT`、`GIT_CONFIG_PARAMETERS`、`GIT_DIR`、`GIT_WORK_TREE`、`GIT_COMMON_DIR`、`GIT_INDEX_FILE`、`GIT_OBJECT_DIRECTORY`、`GIT_ALTERNATE_OBJECT_DIRECTORIES` は、別の設定やリポジトリや index やオブジェクトの置き場所を使わせるので消します。filter は、システムとユーザーの設定にあり、出どころのファイルが、プロジェクトのフォルダ、作業ツリーの最上位、git のフォルダ、共通の git のフォルダのどれの中にもないものだけを使い、それ以外は空にして止めます。プロジェクトが作業ツリーの下のフォルダのときや、リンクした worktree のときも、エージェントはそれらのフォルダを書けるためです。これらのフォルダを `git rev-parse` で読み取れないときは、信用してよい出どころを決められないので、そのフォルダでは git の操作をすべて失敗にします。ユーザーの設定の include がそれらの中のファイルを読むこともあるので、範囲だけでは決めません。git は相対の出どころを作業ツリーの最上位から表示するので、そこを基点に解決します。止める設定は `-c` ではなく `GIT_CONFIG_KEY_n` と `GIT_CONFIG_VALUE_n` で渡します。`-c` は最初の `=` で分けるので、`a=b` という名前のドライバーを止められないためです。信用しない設定に `lfs.extension.*` があるときは、Git LFS が clean のときにそのコマンドを実行するので、Git LFS も含めてすべての filter を止めます。filter は `git add` だけでなく `write-tree` や `diff` からも起動されるので、このリポジトリへのすべての git の呼び出しに付けます。filter を 1 つでも止めるときは、index の写しを使わずに `read-tree HEAD` から作り、本物の index にあるパスを `update-index --add --remove --replace -z --stdin` で渡して、すべてのファイルを止めた状態で読み直します。`--replace` は、`HEAD` でファイルだったものが index ではフォルダになっているときのためです。sparse checkout の外の項目（`ls-files -t` の `S`）は作業ツリーにファイルが無いので、本物の index の内容を `--index-info` でそのまま写し、`--skip-worktree` を付け直します。パスは作業ツリー全体のものを使うので、この 2 つは作業ツリーの最上位で、`Repo::open` が見つけた git のフォルダを `--git-dir` と `--work-tree` で指定して実行します。`core.worktree` があると、最上位から探した git が別のリポジトリを見つけることがあるためです。写しには filter が clean した内容（Git LFS のポインタ）が残っていて、読み直したファイルだけが中身そのものになるので、触っていないファイルまで変わったように見えるためです。本物の index のパスを渡すのは、ignore の対象を `--force` で追加したファイルが `HEAD` に無く、`git add -A` でも拾われず、消されても気付けないためです。作業前と作業後で止める filter が違うときは、同じ方法で比べられないので、差分を取れなかったことにします。この比べ方なら、作業前から未追跡だったファイルへの編集も差分に入ります。`git stash create` は未追跡のファイルを記録しないので使いません。作業前のツリーの ID を「作業前」の地点として返し、`git restore --source=<ID> --worktree -- .` で戻せます。

止める条件は次のとおりです。数値は設定の「エージェント」の「上限」で変えられます。

- 1 つのタスクのジョブの数が `maxDispatchesPerTask`（初期値 10）に達すると、タスクを一時停止します。再開すると同じ数だけ上限を足します。
- 同じ失敗が `sameErrorLimit`（初期値 2）回続くと一時停止します。失敗が同じかどうかは、エージェント、終了コード、stderr の末尾の数字とパスを伏せて比べます。
- `pauseOnDeletion`（初期値オン）のとき、作業前からあったファイルが消えたら一時停止します。ジョブのあとの差分を取れなかったときも、削除がないことを確かめられないので一時停止します。
- Grok Bot からのジョブが同じプロジェクトで `maxConsecutivePerProject`（初期値 30）回続くと、`project_paused` で断ります。画面でそのプロジェクトのタスクを再開するか、設定の「連携」の拒否の一覧から再開すると、数が 0 に戻ります。
- 待ちのジョブは `maxQueue`（初期値 20）までです。
- 1 回のジョブは、エージェントの `timeoutSecs`、無ければ `defaultTimeoutSecs`（初期値 1200 秒）で打ち切ります。ジョブの `timeout_secs` では、10 秒以上で、それより短くすることだけができます。

一時停止したタスクは待ちのジョブを取り消し、再開するまで新しいジョブを受けません。カードの「止める」はそのジョブだけを終了し、カードの「緊急停止」はジョブを終了してタスクを一時停止します。送信欄の「緊急停止」は、1 つのロックの中で Grok Bot の受付を止め、待ちのジョブをすべて取り消し、実行中のジョブを Job Object ごと終了します。受付を止めたことは `gateway.json` に書き、書けなくてもメモリの上では止めます。

CLI の出力は次のように読みます。Claude Code は stream-json の `result` を答えにし、`permission_denials` を拒否された操作にします。Codex は最後の `agent_message` を答えにします。Grok は `--output-format streaming-json` の `text` の断片のうち、最後のツール呼び出しのあとのものを答えにします。ツール呼び出しのあとに文章が無ければ、その前の文章は答えにせず、文章が無かったと返します。Grok の stderr の `permission required: ` は拒否された操作にし、`stopReason` が `cancelled` なら失敗にします。非対話の Grok（1.0.92）は `--permission-mode acceptEdits` でも編集と作成を拒否して `cancelled` で終わるので、既定の引数には作業フォルダの中だけを許す `--allow "Edit(./**)"` と `--allow "Write(./**)"` を足します。Antigravity は出力の末尾 1 MiB にある最後の JSON の `response` を答えにし、`denied_actions` を拒否された操作にします。その JSON を読めなければ、状態と拒否された操作が分からないので失敗にします。

危険なフラグ（`--dangerously-*`、`--yolo`、`--always-approve`、`bypassPermissions`、`danger-full-access`、`--approve all`）は、エージェントごとの「危険なフラグを許可」がオンのときだけ保存も起動もできます。`-c sandbox_mode="danger-full-access"` や `--settings` の JSON に埋めた値も、`\u0050` のようなエスケープを戻してから、引数の部分文字列として検出します。npm の `.cmd` や `.bat` は cmd.exe が引数を解釈し直すので、プロンプトを引数で渡す方式を拒否し、標準入力かファイルで渡します。Rust の `Command::new("codex")` は `.exe` しか探さないので、プログラムは `PATHEXT` を見て解決します。拡張子のない名前には `PATHEXT` の拡張子を付けたものだけを探します。npm は `.cmd` の隣に拡張子のないシェルスクリプトを置きますが、Windows はそれを起動できないためです。PATH に見つからないときは、レジストリのユーザーとシステムの PATH も探し、子プロセスにはそれを足した PATH を渡します。子プロセスはプロジェクトのフォルダで動き、エージェントはそこに `node.cmd` などを置けます。npm の `.cmd` は `node` を名前で起動し、cmd.exe と Node は PATH より先に今のフォルダを探すので、子プロセスと git には `NoDefaultCurrentDirectoryInExePath=1` を渡し、PATH からは相対の項目を除きます。

CLI の登録と上限は `app_config_dir` の `agents.json` に、`version` 付きで置きます。連携の受付と許可したフォルダは、同じ場所の `gateway.json` です。読み書きは Rust 側だけが行い、壊れていれば既定値で動きます。タスクの記録は `app_data_dir/tasks/<ID>.json` に保存します。生ログはジョブごとに末尾 2000 行、直近 50 ジョブ分をメモリに持つだけで、ファイルには残しません。アプリを閉じたときに動いていたジョブは、次に開いたとき中断にします。

イベントは `orchestrator://event`（`task`、`run`、`gateway`）と `agent://log`（ジョブごとの行のまとまり）です。画面は、イベントと一覧の取得結果を同じキューで順に反映します。古い一覧が新しいイベントを上書きしないためです。キューはアニメーションのフレームか 250 ms のタイマーで流すので、ウィンドウを最小化していても溜まり続けません。画面を読み直したあとは `orchestrator_snapshot` で、実行中と待ちのジョブのタスクと受付の状態を埋めます。

## 画面

表示文は日本語、識別子は英語です。説明のための副文は置かず、配置で役割が分かるようにします。全体は黒に近いシャープな三列で、送信欄だけ角丸 22px と円形のボタンにします。送信欄の下の選択はボタンと一覧で、項目にマウスを重ねても背景色も文字色も変えず、選んでいる項目だけに印を付けます。設定はポップアップで、左の分類を選ぶと右にその中身が出ます。分類は表示、エージェント、連携、このアプリで、中身が変わっても同じ大きさです。画面の動きは表示の「開閉の動きを出す」で切ります。切ると右の列と左のフォルダの開閉はすぐ終わります。初期値はオンで、`localStorage` の `gbd-motion` が `"0"` のときはオフです。入力欄をクリックしても青い選択枠は出しません。入力欄はテキストエリアのままで、下書きに Markdown の印があるあいだは送信前でも整形して見せます。暖色の明朝見出しや、紫を基調にした汎用チャットの見た目には戻しません。

左の列に並べるフォルダは、Grok Bot に許可したフォルダ（`gateway.json` の順）と、許可から外したあともタスクが残っているフォルダです。Grok Bot はどの許可フォルダにもジョブを送ってくるので、チャットで開いた順の一覧は使いません。並びは固定で、選んでも先頭へ動かしません。左の作成は「新規タスク」と「新規プロジェクト」で、印は別の SVG です。「新規プロジェクト」で選んだフォルダは Grok Bot の許可にも足します。足すのは `gateway_add_project` で、ロックの中で今の設定にフォルダだけを足すので、そのあいだに緊急停止で止めた受付を古い値で戻しません。どのフォルダも開いて中を見られ、開閉の印は各フォルダに付けます。フォルダの行には、そのフォルダのタスクで実行中・待ち・一時停止のものがあるとき、いちばん強い印を出します。フォルダの下にはタスクを、その下にジョブを `[Claude] 実装` の形で並べ、実行中・待ち・完了・エラー・要確認・停止の印を付けます。タスクを開くと、左の選択もそのタスクのフォルダへ移します。送り先は表示しているタスクのフォルダで決まるので、左の選択とずれないようにするためです。フォルダを選んでいないときは、中央でプロジェクトを選びます。

タスクを選ぶと、中央がジョブのカードの並びになります。カードには出した側（Grok Bot か画面）、エージェント、状態、終了コード、時間、畳んだプロンプト、要約、拒否された操作、変更したファイル、作業前の地点を出し、作業前の地点は ID と `git restore --source=<ID> --worktree -- .` をそれぞれコピーできます。「ログ」でその CLI の stdout と stderr を中央の下のドロワーに出します。ドロワーは末尾に追従し、上へ戻すと追従をやめます。

送信欄の下の左には Grok Bot の受付（受付中・受付停止・パイプ失敗）と実行中のエージェントを出し、表示しているフォルダが許可に無いときは「受付中（未許可）」にします。右にはタスクのジョブの数と上限、送り先のエージェントの選択を出します。選んだエージェントは `gbd-flow-agent` に覚え、覚えたものが使えないときは有効な最初のエージェントにします。何も選ばずに送ると新しいタスクになります。送る前に `orchestrator_preflight` でフォルダを確かめ、git のリポジトリでないかコミットが無いときは、入力欄にそれを出して送れなくします。ウィンドウに戻ったときにも確かめ直します。一時停止や終了のタスクを表示しているあいだも送れません。ジョブが実行中か待ちのあいだは、送信欄に「緊急停止」を出します。添付は使わず、ドロップしたパスや `@` で選んだパスは入力欄の文字にします。`@` の候補は表示しているフォルダから探します。

右の列は上端の同じボタンで、幅と矢印を動かして畳みます。中身は、表示しているプロジェクトのパス、git のブランチ、Grok Bot の許可、受付の状態と、タスクを開いているときはその状態、出した側、ジョブの数と上限、待ちの数、更新の時刻です。このプロジェクトで Grok Bot のジョブを断ったときは「最近の拒否」を出し、連続実行の上限で断ったものはそこから再開できます。許可していないフォルダでは「許可を設定」から設定の「連携」を開きます。

## アプリの更新

起動時には更新を確認しません。左下の設定から確認し、適用は確認ダイアログのあとだけ行います。開発中の `npm run tauri dev` では確認できても適用しません。Windows の `installMode` は `passive` で、適用するとアプリが終了してからインストーラが動きます。タスクの記録は残ります。

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

現在の開発βは v0.0.6 です。既定ブランチは `main` です。公開先は https://github.com/Termnix-IT/grok-build-desktop です。`node_modules`、`dist`、`src-tauri/target` はコミットしません。
