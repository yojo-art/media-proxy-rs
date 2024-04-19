set -eu
if [ -f "/app/crossfiles/${TARGETARCH}.sh" ]; then
	source /app/crossfiles/${TARGETARCH}.sh
else
	source /app/crossfiles/${TARGETARCH}/${TARGETVARIANT}.sh
fi
rustup target add ${RUST_TARGET}
curl -sSL https://musl.cc/${MUSL_NAME}.tgz | tar -zxf - -C /
mkdir -p /${MUSL_NAME}/dav1d/
