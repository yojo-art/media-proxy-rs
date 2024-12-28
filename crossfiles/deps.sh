set -eu
if [ -f "/app/crossfiles/${TARGETARCH}.sh" ]; then
	source /app/crossfiles/${TARGETARCH}.sh
else
	source /app/crossfiles/${TARGETARCH}/${TARGETVARIANT}.sh
fi
rustup target add ${RUST_TARGET}
mkdir /musl
curl -sSL https://musl.cc/${MUSL_NAME}.tgz | tar -zxf - -C /musl
mkdir -p /musl/${MUSL_NAME}/dav1d/
