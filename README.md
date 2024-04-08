# media-proxy-rs
## misskey/cherrypick用メディアプロキシのrust実装
機能的には互換性を維持しつつ、apngとavif対応に  
ほとんどの画像読み書きに[image crate v0.25](https://crates.io/crates/image/0.25.1)を使用しています  
AVIFのデコード処理はシステムライブラリの依存関係解決が難しいのでデフォルト無効にしてあります。  
Dockerfileを使用すると簡単にAVIFデコード有効なバイナリを生成できると思います  
これをビルドするにはwebp createのコンパイルの為にCコンパイラが必要です  
## target support
- [x] x86_64-unknown-linux-musl
- [ ] x86_64-unknown-linux-gnu
