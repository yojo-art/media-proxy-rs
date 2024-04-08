FROM rust:alpine
RUN apk add --no-cache musl-dev curl nasm meson ninja pkgconfig git
RUN curl -sSL https://github.com/mozilla/sccache/releases/download/v0.7.7/sccache-v0.7.7-x86_64-unknown-linux-musl.tar.gz | tar -zxf - -C /tmp && mv /tmp/sccache*/sccache /usr/local/bin && rm -rf /tmp/sccache*
ENV CARGO_HOME=/var/cache/cargo
RUN mkdir /app && mkdir /dav1d
RUN git clone --branch 1.3.0 --depth 1 https://code.videolan.org/videolan/dav1d.git /dav1d
WORKDIR /dav1d
RUN meson build -Dprefix=/app/dav1d -Denable_tools=false -Denable_examples=false --buildtype release
RUN ninja -C build && ninja -C build install
ENV PKG_CONFIG_PATH=/app/dav1d/lib/pkgconfig
ENV LD_LIBRARY_PATH=/app/dav1d/lib
WORKDIR /app
COPY src ./src
COPY Cargo.toml ./Cargo.toml
COPY Cargo.lock ./Cargo.lock
RUN --mount=type=cache,target=/var/cache/cargo cargo fetch --locked
ENV RUSTC_WRAPPER=/usr/local/bin/sccache
ENV SCCACHE_DIR=/var/cache/sccache
RUN --mount=type=cache,target=/var/cache/cargo --mount=type=cache,target=/var/cache/sccache cargo build --target x86_64-unknown-linux-musl --release --offline --features "image/avif-native"

FROM alpine:latest
ARG UID="852"
ARG GID="852"
RUN addgroup -g "${GID}" proxy && adduser -u "${UID}" -G proxy -D -h /media-proxy-rs -s /bin/sh proxy
WORKDIR /media-proxy-rs
USER proxy
COPY --from=0 /app/target/x86_64-unknown-linux-musl/release/media-proxy-rs ./media-proxy-rs
EXPOSE 12766
CMD ["./media-proxy-rs"]
