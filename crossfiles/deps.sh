set -eu
if [ -f "/app/crossfiles/${TARGETARCH}.sh" ]; then
	source /app/crossfiles/${TARGETARCH}.sh
else
	source /app/crossfiles/${TARGETARCH}/${TARGETVARIANT}.sh
fi
rustup target add ${RUST_TARGET}
if [ -d "/musl/${MUSL_NAME}" ]; then
	:
else
	curl -sSL https://musl.cc/${MUSL_NAME}.tgz | tar -zxf - -C /musl
fi
mkdir -p /musl/${MUSL_NAME}/dav1d/
