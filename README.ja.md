# Apple Pickup Watcher 日本語版

[Apple Pickup Watcher](https://github.com/ENCHIGO/apple-pickup-watcher)のデスクトップ画面、システム通知、トレイメニュー、エラーメッセージを日本語化したフォークです。在庫判定、監視、通信、設定の保存方法は元の実装を維持しています。

## 使い方

1. アプリを開き、「追加」で地域を「日本」にします。
2. 受取店舗、モデル、容量、カラーを選び、監視対象に追加します。
3. 「設定」で確認間隔と通知音を設定し、「通知をテスト」で確認します。
4. 「開始」で監視を始めます。ウインドウを閉じても監視は続き、トレイメニューの「終了」でアプリを終了します。

アプリ内の更新機能は元のプロジェクトを参照します。元の配布物で更新すると中国語表示に戻るため、日本語版はこのフォークからビルドしてください。

## macOSでのビルド

Rust、Xcode Command Line Tools、Node.js LTS、pnpm 9を用意し、次を実行します。

```sh
pnpm install --frozen-lockfile
pnpm tauri build --bundles app --config '{"bundle":{"createUpdaterArtifacts":false}}'
```

アプリは `target/release/bundle/macos/Apple Pickup Watcher.app` に作成されます。

元の機能や対応地域については[元のREADME](README.md)を参照してください。ライセンスはGPL-3.0-or-laterです。
