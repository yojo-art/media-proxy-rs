export MUSL_NAME="riscv64-linux-musl-cross"
export PATH="/musl/${MUSL_NAME}/bin:${PATH}"
export CC=riscv64-linux-musl-gcc
export CXX=riscv64-linux-musl-g++
export AR=riscv64-linux-musl-ar
export RUSTFLAGS="-C linker=${CC} -C target-feature=+crt-static"
export PKG_CONFIG_SYSROOT_DIR="/musl/${MUSL_NAME}/"
export RUST_TARGET="riscv64gc-unknown-linux-musl"
export BINDGEN_EXTRA_CLANG_ARGS="--sysroot ${PKG_CONFIG_SYSROOT_DIR}/riscv64-linux-musl"
