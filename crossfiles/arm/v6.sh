export MUSL_NAME="armv6-linux-musleabihf-cross"
export PATH="/${MUSL_NAME}/bin:${PATH}"
export CC=armv6-linux-musleabihf-gcc
export CXX=armv6-linux-musleabihf-g++
export AR=armv6-linux-musleabihf-ar
export RUSTFLAGS="-C link-args=-Wl,-lc -C linker=${CC}"
export PKG_CONFIG_SYSROOT_DIR="/${MUSL_NAME}/"
export RUST_TARGET="arm-unknown-linux-musleabihf"
