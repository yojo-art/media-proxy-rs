#!/bin/bash
set -euo pipefail

# dav1d / lcms2 を静的ライブラリとしてビルドする共通スクリプト。
# クロスコンパイルが必要な場合は --cross-file で Meson の cross file を渡す。

# インストール先。SIP で / 直下が読取専用の macOS でも書けるよう、
# 全プラットフォームで書込み可能な /tmp 配下に置く。
DEPS_PREFIX="${DEPS_PREFIX:-/tmp/napi-deps}"

CROSS_FILE=""
DAV1D_VERSION="${DAV1D_VERSION:?DAV1D_VERSION is required}"
LCMS2_VERSION="${LCMS2_VERSION:?LCMS2_VERSION is required}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --cross-file)
      CROSS_FILE="$2"
      shift 2
      ;;
    *)
      echo "Unknown argument: $1" >&2
      exit 1
      ;;
  esac
done

# macOS の /bin/bash は 3.2 であり、set -u 下での空配列の展開は
# unbound variable エラーになる (bash 4.4 未満の既知の問題)。値は制御下の
# --cross-file パスのみのため配列ではなく文字列で保持し、呼び出し側では
# unquoted で展開する (空のときは 0 引数になる)。
MESON_OPTS=""
if [[ -n "$CROSS_FILE" ]]; then
  MESON_OPTS="--cross-file $CROSS_FILE"
fi

# dav1d (static)
git clone --branch "$DAV1D_VERSION" --depth 1 https://github.com/videolan/dav1d.git /tmp/dav1d_src
cd /tmp/dav1d_src
meson setup build \
  -Dprefix="$DEPS_PREFIX/dav1d" \
  -Dlibdir=lib \
  -Denable_tools=false \
  -Denable_examples=false \
  -Ddefault_library=static \
  --buildtype release \
  ${MESON_OPTS}
ninja -C build
ninja -C build install
rm -rf /tmp/dav1d_src
cd /

# lcms2 (static)
git clone -b "$LCMS2_VERSION" --depth 1 https://github.com/mm2/Little-CMS.git /tmp/lcms2_src
cd /tmp/lcms2_src
meson setup build \
  --prefix="$DEPS_PREFIX/lcms2" \
  -Dlibdir=lib \
  -Ddefault_library=static \
  -Dfastfloat=true \
  -Dthreaded=true \
  --buildtype release \
  ${MESON_OPTS}
ninja -C build
ninja -C build install
rm -rf /tmp/lcms2_src
cd /
