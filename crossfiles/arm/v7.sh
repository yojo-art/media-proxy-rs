export MUSL_NAME="armv7l-linux-musleabihf-cross"
export PATH="/musl/${MUSL_NAME}/bin:${PATH}"
export CC=armv7l-linux-musleabihf-gcc
export CXX=armv7l-linux-musleabihf-g++
export AR=armv7l-linux-musleabihf-ar
export RUSTFLAGS="-C link-args=-Wl,-lc -C linker=${CC}"
export PKG_CONFIG_SYSROOT_DIR="/musl/${MUSL_NAME}/"
export RUST_TARGET="armv7-unknown-linux-musleabihf"
