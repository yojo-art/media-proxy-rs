export MUSL_NAME="riscv64-linux-musl-cross"
export PATH="/musl/${MUSL_NAME}/bin:${PATH}"
export CC=riscv64-linux-musl-gcc
export CXX=riscv64-linux-musl-g++
export AR=riscv64-linux-musl-ar
export RUSTFLAGS="-C linker=${CC} "
export PKG_CONFIG_SYSROOT_DIR="/musl/${MUSL_NAME}/"
export RUST_TARGET="riscv64gc-unknown-linux-musl"
