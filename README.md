# media-proxy-rs
## misskey/cherrypick用メディアプロキシのrust実装
機能的には互換性を維持しつつ、様々な画像形式のデコードに対応  
ほとんどの画像読み書きに[image crate v0.25](https://crates.io/crates/image/0.25.5)を使用しています

## 実行(Docker)
```
docker run -itd -p 12766:12766 ghcr.io/yojo-art/media-proxy-rs:main
```

## 実行(Linux)
例(x86_64/amd64)
```
curl -L https://github.com/yojo-art/media-proxy-rs/releases/download/nightly/media-proxy-rs_linux-amd64.gz | gzip -d > ./media-proxy-rs
chmod u+x ./media-proxy-rs
./media-proxy-rs
```
利用するプラットフォームに応じて適切なバイナリを選択してください。ファイル名のリストを示します
```
media-proxy-rs_linux-386.gz (i686+sse2)
media-proxy-rs_linux-amd64.gz (x86-64-v3)
media-proxy-rs_linux-arm-v6.gz
media-proxy-rs_linux-arm-v7.gz
media-proxy-rs_linux-arm64.gz
media-proxy-rs_linux-riscv64.gz
```

## 設定ファイル
環境変数`MEDIA_PROXY_CONFIG_PATH`を設定する事でファイルの場所を指定できます  
デフォルト値は`$(pwd)/config.json`です  
十分に強力なマシンでは`encode_avif`を`true`に変更することでAVIFエンコードを利用する事ができます

## target support
- [x] x86_64-unknown-linux-musl
- [x] aarch64-unknown-linux-musl
- [x] armv7-unknown-linux-musleabihf
- [x] arm-unknown-linux-musleabihf
- [x] i686-unknown-linux-musl
- [x] riscv64gc-unknown-linux-musl

## ビルド(x64 Docker)
Dockerを使用する場合はbuildxとqemuによるクロスコンパイルが利用できます  
ビルド対象プラットフォームはtarget supportの項目を参照してください
1. `git clone https://github.com/yojo-art/media-proxy-rs && cd media-proxy-rs`
2. `docker build -t media-proxy-rs .`

## ビルド(Docker aarch64等その他)
./crosstiles/arm64.shのMUSL_NAMEと./crossfiles/deps.shのmuslをダウンロードする処理を調整する必要があります

## プラットフォーム最適化
amd64ではデフォルトでx86-64-v3向けにビルドしますが、x86-64-v3未満の環境やx86-64-v4向け最適化利用したい場合./crosstiles/amd64.shのRUSTFLAGSを編集してください
他プラットフォームであればarm64.shやriscv64.shの編集でRUSTFLAGSを変更してください
最も簡単なのはtarget-cpu=nativeを指定し、実行環境と同じCPUでビルドする方法です

## ビルド(x64 Debian系)
この方法では`x86_64-unknown-linux-gnu`向けにビルドします  
すべてを静的に組み込むmusl系とは異なる共有ライブラリを必要とする場合があります
1. https://www.rust-lang.org/ja/tools/install に従ってrustをインストール
1. `apt-get install -y meson ninja-build pkg-config nasm git`
2. `git clone https://github.com/yojo-art/media-proxy-rs && cd media-proxy-rs`
3. `cargo build --release`

## 対応する画像形式
- AVIF(dav1d)
- BMP
- DDS
- Farbfeld
- GIF
- HDR
- ICO(png+rgba not support)
- JPEG
- EXR
- PNG
- PNM
- QOI
- TGA
- TIFF
- WebP
- JPEG XL(jxl-oxide)
- JPEG 2000(openjp2)
- JPEG XR(jxrlib)
