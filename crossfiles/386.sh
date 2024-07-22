export MUSL_NAME="i686-linux-musl-cross"
export PATH="/musl/${MUSL_NAME}/bin:${PATH}"
export CC=i686-linux-musl-gcc
export CXX=i686-linux-musl-g++
export AR=i686-linux-musl-ar
#現時点ではringがsse2を必須としている
#https://github.com/briansmith/ring/blob/main/src/cpu/intel.rs#L23
#https://github.com/briansmith/ring/issues/1793#issuecomment-1793243725
#https://github.com/briansmith/ring/issues/1832
#https://github.com/briansmith/ring/issues/1833.
export RUSTFLAGS="-C target-feature=+sse -C target-feature=+sse2 -C linker=${CC}"
export PKG_CONFIG_SYSROOT_DIR="/musl/${MUSL_NAME}/"
export RUST_TARGET="i686-unknown-linux-musl"
