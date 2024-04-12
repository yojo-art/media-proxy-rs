# media-proxy-rs
## misskey/cherrypick用メディアプロキシのrust実装
機能的には互換性を維持しつつ、apngとavif対応に  
ほとんどの画像読み書きに[image crate v0.25](https://crates.io/crates/image/0.25.1)を使用しています  
これをビルドするにはwebp crateのコンパイルにclang、libdav1dのコンパイルにnasm meson ninja pkgconfig gitが必要です  
## target support
- [x] x86_64-unknown-linux-musl
- [ ] x86_64-unknown-linux-gnu
