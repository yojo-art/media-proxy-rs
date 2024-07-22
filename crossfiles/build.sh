set -eu
if [ -f "/app/crossfiles/${TARGETARCH}.sh" ]; then
	source /app/crossfiles/${TARGETARCH}.sh
else
	source /app/crossfiles/${TARGETARCH}/${TARGETVARIANT}.sh
fi
cp -r /dav1d/lib /${MUSL_NAME}/dav1d/lib
mkdir ./.cargo/
echo "[target.${RUST_TARGET}]" >> ./.cargo/config.toml
echo 'rustflags = ["-C", "link-arg=-fuse-ld=/usr/bin/mold"]' >> ./.cargo/config.toml
cargo build --release --target ${RUST_TARGET}
cargo build --release --target ${RUST_TARGET} --example healthcheck
cp /app/target/${RUST_TARGET}/release/media-proxy-rs /app/media-proxy-rs
cp /app/target/${RUST_TARGET}/release/examples/healthcheck /app/healthcheck
