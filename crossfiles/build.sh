set -eu
if [ -f "/app/crossfiles/${TARGETARCH}.sh" ]; then
	source /app/crossfiles/${TARGETARCH}.sh
else
	source /app/crossfiles/${TARGETARCH}/${TARGETVARIANT}.sh
fi
cp -r /dav1d/lib /${MUSL_NAME}/dav1d/lib
cargo build --release --target ${RUST_TARGET}
cp /app/target/${RUST_TARGET}/release/media-proxy-rs /app/media-proxy-rs
