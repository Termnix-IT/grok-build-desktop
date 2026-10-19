# Grok Build Desktop

インストール済みの Grok Build と、`grok agent --no-leader stdio` の ACP で会話する Windows アプリです。会話の正本は Grok のセッションに残り、このアプリはそれを写し取りません。

現在の開発βは v0.0.1 です。Windows 用インストーラは、このリポジトリの prerelease にあります。

## 起動

```powershell
npm install
npm run tauri dev
```

Grok にログインしていないときは、画面の「ログイン」から `grok login` を開きます。
