# syntax=docker/dockerfile:1

# glibc 2.28 基準 (Node.js の Linux 最低要件) の gnu と、musl (Alpine など) の
# NAPI バイナリをビルドする。x86_64 / aarch64 を同じ Dockerfile で扱い、
# stage target で切り替える。musl は amd64 (glibc) ホストからのクロスビルド。

ARG BASE=debian:buster-slim
# 再現性のため Rust の版を固定する。上げる場合は napi/ の `npm ci` で入る
# @napi-rs/cli (package-lock.json で固定) との組み合わせでビルド確認すること。
# ルートの rust-toolchain.toml の channel、.github/workflows/napi.yml の
# toolchain / build-args とそろえること (版は workflow の env が正本)。
ARG RUST_VERSION=1.97.1
ARG NODE_VERSION=22.9.0
# https://nodejs.org/dist/v${NODE_VERSION}/SHASUMS256.txt の
# node-v${NODE_VERSION}-linux-x64.tar.xz の値。NODE_VERSION を上げる際は更新すること。
ARG NODE_SHA256=1bfae9ef21ab43c92d8274f1bd032bf61f42ea004192a18d4c64477508626142
ARG DAV1D_VERSION=1.5.4
ARG LCMS2_VERSION=lcms2.16

# -----------------------------------------------------------------------------
# 共通のビルド環境
# -----------------------------------------------------------------------------
FROM ${BASE} AS build-base

# buster の通常ミラーは既に無効になっているため archive.debian.org を使う
RUN if grep -q "buster" /etc/apt/sources.list 2>/dev/null || grep -q "buster" /etc/apt/sources.list.d/*.list 2>/dev/null; then \
        echo "deb http://archive.debian.org/debian buster main contrib non-free" > /etc/apt/sources.list && \
        echo "deb http://archive.debian.org/debian-security buster/updates main contrib non-free" >> /etc/apt/sources.list && \
        echo "Acquire::Check-Valid-Until false;" > /etc/apt/apt.conf.d/99no-check-valid-until; \
    fi

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    git \
    build-essential \
    clang \
    pkg-config \
    ninja-build \
    python3-pip \
    autoconf \
    automake \
    help2man \
    texinfo \
    xz-utils \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/* \
    && pip3 install --no-cache-dir meson

# rav1e が要求する nasm >= 2.15 をソースから入れる (buster の nasm は 2.14)
# nasm の git ソースは autoconf >= 2.71 を要求するため、autotools も更新する
RUN git clone --branch v2.72 --depth 1 https://github.com/autotools-mirror/autoconf.git /tmp/autoconf-src \
    && cd /tmp/autoconf-src \
    && ./bootstrap \
    && ./configure --prefix=/usr/local \
    && make -j$(nproc) \
    && make install \
    && rm -rf /tmp/autoconf-src

RUN git clone --branch nasm-2.16.03 --depth 1 https://github.com/netwide-assembler/nasm.git /tmp/nasm-src \
    && cd /tmp/nasm-src \
    && ./autogen.sh \
    && ./configure --prefix=/usr/local \
    && make -j$(nproc) \
    && cp nasm ndisasm /usr/local/bin/ \
    && rm -rf /tmp/nasm-src \
    && nasm --version

# Rust (rustup)
ARG RUST_VERSION
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain ${RUST_VERSION}
ENV PATH="/root/.cargo/bin:${PATH}"

# Node.js (公式バイナリ。SHA256 で検証する)
ARG NODE_VERSION
ARG NODE_SHA256
RUN curl -fsSL -o /tmp/node.tar.xz "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.xz" \
    && echo "${NODE_SHA256}  /tmp/node.tar.xz" | sha256sum -c - \
    && tar -xJf /tmp/node.tar.xz -C /usr/local --strip-components=1 \
    && rm /tmp/node.tar.xz \
    && node --version && npm --version

WORKDIR /build

# napi CLI は package-lock.json で固定した版を napi/ 内に用意し、`npx napi build` で使う。
# `npm install -g @napi-rs/cli` はその時点の最新版が入るため、タグを付け直すと
# 別の版でビルドされることがある。
COPY napi/package.json napi/package-lock.json ./napi/
RUN cd napi && npm ci

COPY .github/napi/build-deps.sh /usr/local/bin/build-deps.sh
RUN chmod +x /usr/local/bin/build-deps.sh

# -----------------------------------------------------------------------------
# x86_64 用ビルド
# -----------------------------------------------------------------------------
FROM build-base AS build-x86_64

ARG DAV1D_VERSION
ARG LCMS2_VERSION
ARG TARGET=x86_64-unknown-linux-gnu

RUN rustup target add ${TARGET}

RUN DAV1D_VERSION=${DAV1D_VERSION} LCMS2_VERSION=${LCMS2_VERSION} build-deps.sh

ENV PKG_CONFIG_PATH=/tmp/napi-deps/dav1d/lib/pkgconfig:/tmp/napi-deps/lcms2/lib/pkgconfig
ENV PKG_CONFIG_ALLOW_CROSS=1

COPY src ./src
COPY Cargo.toml ./Cargo.toml
COPY Cargo.lock ./Cargo.lock
COPY asset ./asset
COPY examples ./examples
# ./napi は build-base で `npm ci` 済み。COPY はマージされるため node_modules は残る。
COPY napi ./napi
COPY avif-decoder_dep ./avif-decoder_dep

RUN cd napi && npx napi build --release --platform --target ${TARGET} \
    --manifest-path Cargo.toml --package-json-path package.json

# glibc 2.28 (buster。Node.js の Linux 最低要件) より新しいシンボル版を
# 要求していないことを確かめる
RUN MAX_GLIBC=$(readelf -V /build/napi/*.node | grep -oE 'GLIBC_[0-9.]+' | sort -Vu | tail -n 1) \
    && echo "max required GLIBC: ${MAX_GLIBC}" \
    && [ "$(printf '%s\n%s\n' "${MAX_GLIBC}" GLIBC_2.28 | sort -V | tail -n 1)" = GLIBC_2.28 ]

RUN cp /build/napi/media-proxy-napi.linux-x64-gnu.node /

FROM scratch AS out-x86_64
COPY --from=build-x86_64 /media-proxy-napi.linux-x64-gnu.node /media-proxy-napi.linux-x64-gnu.node
COPY --from=build-x86_64 /build/napi/index.js /index.js
COPY --from=build-x86_64 /build/napi/index.d.ts /index.d.ts

# -----------------------------------------------------------------------------
# aarch64 用クロスビルド
# -----------------------------------------------------------------------------
FROM build-base AS build-aarch64

ARG DAV1D_VERSION
ARG LCMS2_VERSION
ARG TARGET=aarch64-unknown-linux-gnu

RUN apt-get update && apt-get install -y --no-install-recommends \
    gcc-aarch64-linux-gnu \
    g++-aarch64-linux-gnu \
    libc6-dev-arm64-cross \
    && rm -rf /var/lib/apt/lists/*

RUN rustup target add ${TARGET}

ENV CC=aarch64-linux-gnu-gcc
ENV CXX=aarch64-linux-gnu-g++
ENV AR=aarch64-linux-gnu-ar
ENV STRIP=aarch64-linux-gnu-strip
ENV RUSTFLAGS="-C linker=aarch64-linux-gnu-gcc"
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
ENV CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
ENV CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++
ENV AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar
ENV BINDGEN_EXTRA_CLANG_ARGS="--sysroot=/usr/aarch64-linux-gnu"
ENV PKG_CONFIG_LIBDIR=/tmp/napi-deps/dav1d/lib/pkgconfig:/tmp/napi-deps/lcms2/lib/pkgconfig
ENV PKG_CONFIG_ALLOW_CROSS=1

# meson 用クロスファイル
RUN tee /tmp/cross.txt <<'EOF'
[binaries]
c = 'aarch64-linux-gnu-gcc'
cpp = 'aarch64-linux-gnu-g++'
ar = 'aarch64-linux-gnu-ar'
strip = 'aarch64-linux-gnu-strip'
pkgconfig = 'pkg-config'

[host_machine]
system = 'linux'
cpu_family = 'aarch64'
cpu = 'aarch64'
endian = 'little'
EOF

RUN DAV1D_VERSION=${DAV1D_VERSION} LCMS2_VERSION=${LCMS2_VERSION} build-deps.sh --cross-file /tmp/cross.txt

COPY src ./src
COPY Cargo.toml ./Cargo.toml
COPY Cargo.lock ./Cargo.lock
COPY asset ./asset
COPY examples ./examples
# ./napi は build-base で `npm ci` 済み。COPY はマージされるため node_modules は残る。
COPY napi ./napi
COPY avif-decoder_dep ./avif-decoder_dep

RUN cd napi && npx napi build --release --platform --target ${TARGET} \
    --manifest-path Cargo.toml --package-json-path package.json

# glibc 2.28 より新しいシンボル版を要求していないことを確かめる。
# ホストの readelf は ELF のバージョン要求節を読めるため、クロスビルド物でも使える。
RUN MAX_GLIBC=$(readelf -V /build/napi/*.node | grep -oE 'GLIBC_[0-9.]+' | sort -Vu | tail -n 1) \
    && echo "max required GLIBC: ${MAX_GLIBC}" \
    && [ "$(printf '%s\n%s\n' "${MAX_GLIBC}" GLIBC_2.28 | sort -V | tail -n 1)" = GLIBC_2.28 ]

RUN cp /build/napi/media-proxy-napi.linux-arm64-gnu.node /

FROM scratch AS out-aarch64
COPY --from=build-aarch64 /media-proxy-napi.linux-arm64-gnu.node /media-proxy-napi.linux-arm64-gnu.node
COPY --from=build-aarch64 /build/napi/index.js /index.js
COPY --from=build-aarch64 /build/napi/index.d.ts /index.d.ts

# -----------------------------------------------------------------------------
# musl クロスツールチェーン (amd64/glibc ホストから x86_64 / aarch64 musl へ)
# -----------------------------------------------------------------------------
# QEMU での arm64 ビルドは遅いため、amd64 (glibc) ホストからクロスする。
# ホストが glibc なので、build script (bindgen 等) は通常どおり動的リンクされ
# libclang を dlopen できる。
# ツールチェーンは richfelker/musl-cross-make をソースからビルドする。
# リビジョンを固定しているので、更新時は REV と動作確認をセットで行うこと。
FROM build-base AS musl-toolchain

ARG MUSL_CROSS_MAKE_REV=5e8042e63de29f8473fc3b0b831a587ba284e1a8
RUN git clone https://github.com/richfelker/musl-cross-make.git /tmp/musl-cross-make \
    && cd /tmp/musl-cross-make \
    && git checkout ${MUSL_CROSS_MAKE_REV} \
    && printf 'OUTPUT = /musl\nDL_CMD = curl -C - -L -o\n' > config.mak \
    && make -j$(nproc) TARGET=x86_64-linux-musl \
    && make TARGET=x86_64-linux-musl install \
    && make -j$(nproc) TARGET=aarch64-linux-musl \
    && make TARGET=aarch64-linux-musl install \
    && rm -rf /tmp/musl-cross-make

# インストール先を確認する。musl-cross-make はバイナリを /musl/bin、
# sysroot を /musl/<target> に置く。
RUN /musl/bin/x86_64-linux-musl-gcc --version \
    && /musl/bin/aarch64-linux-musl-gcc --version

# -----------------------------------------------------------------------------
# x86_64 musl 用クロスビルド
# -----------------------------------------------------------------------------
FROM musl-toolchain AS build-x86_64-musl

ARG DAV1D_VERSION
ARG LCMS2_VERSION
ARG TARGET=x86_64-unknown-linux-musl

RUN rustup target add ${TARGET}

ENV PATH="/musl/bin:${PATH}"
ENV CC=x86_64-linux-musl-gcc
ENV CXX=x86_64-linux-musl-g++
ENV AR=x86_64-linux-musl-ar
ENV RUSTFLAGS="-C linker=x86_64-linux-musl-gcc -C target-feature=-crt-static -C link-args=-Wl,-lc"
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-musl-gcc
ENV BINDGEN_EXTRA_CLANG_ARGS="--sysroot=/musl/x86_64-linux-musl"
ENV PKG_CONFIG_LIBDIR=/tmp/napi-deps/dav1d/lib/pkgconfig:/tmp/napi-deps/lcms2/lib/pkgconfig
ENV PKG_CONFIG_ALLOW_CROSS=1

RUN tee /tmp/cross.txt <<'EOF'
[binaries]
c = 'x86_64-linux-musl-gcc'
cpp = 'x86_64-linux-musl-g++'
ar = 'x86_64-linux-musl-ar'
pkgconfig = 'pkg-config'

[host_machine]
system = 'linux'
cpu_family = 'x86_64'
cpu = 'x86_64'
endian = 'little'
EOF

RUN DAV1D_VERSION=${DAV1D_VERSION} LCMS2_VERSION=${LCMS2_VERSION} build-deps.sh --cross-file /tmp/cross.txt

COPY src ./src
COPY Cargo.toml ./Cargo.toml
COPY Cargo.lock ./Cargo.lock
COPY asset ./asset
COPY examples ./examples
# ./napi は build-base で `npm ci` 済み。COPY はマージされるため node_modules は残る。
COPY napi ./napi
COPY avif-decoder_dep ./avif-decoder_dep

RUN cd napi && npx napi build --release --platform --target ${TARGET} \
    --manifest-path Cargo.toml --package-json-path package.json

# glibc ではなく musl に動的リンクされていることを確かめる。
# libgcc_s 由来の GLIBC_ バージョン参照は musl では無視されるため、
# NEEDED に libc.so / libgcc_s.so.1 以外の共有ライブラリが無いことで判定する。
RUN deps=$(readelf -d /build/napi/media-proxy-napi.linux-x64-musl.node \
        | sed -n 's/.*Shared library: \[\(.*\)\]/\1/p'); \
    echo "NEEDED: ${deps}"; \
    if echo "${deps}" | grep -Ev '^(libc\.so|libgcc_s\.so\.1)$' | grep -q .; then \
        echo "unexpected dynamic dependency" >&2; \
        exit 1; \
    fi

RUN cp /build/napi/media-proxy-napi.linux-x64-musl.node /

FROM scratch AS out-x86_64-musl
COPY --from=build-x86_64-musl /media-proxy-napi.linux-x64-musl.node /media-proxy-napi.linux-x64-musl.node
COPY --from=build-x86_64-musl /build/napi/index.js /index.js
COPY --from=build-x86_64-musl /build/napi/index.d.ts /index.d.ts

# -----------------------------------------------------------------------------
# aarch64 musl 用クロスビルド
# -----------------------------------------------------------------------------
FROM musl-toolchain AS build-aarch64-musl

ARG DAV1D_VERSION
ARG LCMS2_VERSION
ARG TARGET=aarch64-unknown-linux-musl

RUN rustup target add ${TARGET}

ENV PATH="/musl/bin:${PATH}"
ENV CC=aarch64-linux-musl-gcc
ENV CXX=aarch64-linux-musl-g++
ENV AR=aarch64-linux-musl-ar
ENV RUSTFLAGS="-C linker=aarch64-linux-musl-gcc -C target-feature=-crt-static -C link-args=-Wl,-lc"
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc
ENV BINDGEN_EXTRA_CLANG_ARGS="--sysroot=/musl/aarch64-linux-musl"
ENV PKG_CONFIG_LIBDIR=/tmp/napi-deps/dav1d/lib/pkgconfig:/tmp/napi-deps/lcms2/lib/pkgconfig
ENV PKG_CONFIG_ALLOW_CROSS=1

RUN tee /tmp/cross.txt <<'EOF'
[binaries]
c = 'aarch64-linux-musl-gcc'
cpp = 'aarch64-linux-musl-g++'
ar = 'aarch64-linux-musl-ar'
pkgconfig = 'pkg-config'

[host_machine]
system = 'linux'
cpu_family = 'aarch64'
cpu = 'aarch64'
endian = 'little'
EOF

RUN DAV1D_VERSION=${DAV1D_VERSION} LCMS2_VERSION=${LCMS2_VERSION} build-deps.sh --cross-file /tmp/cross.txt

COPY src ./src
COPY Cargo.toml ./Cargo.toml
COPY Cargo.lock ./Cargo.lock
COPY asset ./asset
COPY examples ./examples
# ./napi は build-base で `npm ci` 済み。COPY はマージされるため node_modules は残る。
COPY napi ./napi
COPY avif-decoder_dep ./avif-decoder_dep

RUN cd napi && npx napi build --release --platform --target ${TARGET} \
    --manifest-path Cargo.toml --package-json-path package.json

# glibc ではなく musl に動的リンクされていることを確かめる。
# libgcc_s 由来の GLIBC_ バージョン参照は musl では無視されるため、
# NEEDED に libc.so / libgcc_s.so.1 以外の共有ライブラリが無いことで判定する。
RUN deps=$(readelf -d /build/napi/media-proxy-napi.linux-arm64-musl.node \
        | sed -n 's/.*Shared library: \[\(.*\)\]/\1/p'); \
    echo "NEEDED: ${deps}"; \
    if echo "${deps}" | grep -Ev '^(libc\.so|libgcc_s\.so\.1)$' | grep -q .; then \
        echo "unexpected dynamic dependency" >&2; \
        exit 1; \
    fi

RUN cp /build/napi/media-proxy-napi.linux-arm64-musl.node /

FROM scratch AS out-aarch64-musl
COPY --from=build-aarch64-musl /media-proxy-napi.linux-arm64-musl.node /media-proxy-napi.linux-arm64-musl.node
COPY --from=build-aarch64-musl /build/napi/index.js /index.js
COPY --from=build-aarch64-musl /build/napi/index.d.ts /index.d.ts
