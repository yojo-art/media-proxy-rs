FROM rust:alpine
RUN apk add --no-cache musl-dev curl meson ninja pkgconfig git
RUN sh -c "if [ $(uname -m) = x86_64 ]; then apk add --no-cache nasm;fi"
ENV CARGO_HOME=/var/cache/cargo
RUN mkdir /app
ENV SYSTEM_DEPS_BUILD_INTERNAL=always
ENV RUSTFLAGS="-C link-args=-Wl,-lc"
WORKDIR /app
COPY avif-decoder_dep ./avif-decoder_dep
COPY src ./src
COPY Cargo.toml ./Cargo.toml
RUN --mount=type=cache,target=/var/cache/cargo cargo build --release

FROM alpine:latest
ARG UID="852"
ARG GID="852"
RUN addgroup -g "${GID}" proxy && adduser -u "${UID}" -G proxy -D -h /media-proxy-rs -s /bin/sh proxy
WORKDIR /media-proxy-rs
USER proxy
COPY asset ./asset
COPY --from=0 /app/target/release/media-proxy-rs ./media-proxy-rs
EXPOSE 12766
CMD ["./media-proxy-rs"]
