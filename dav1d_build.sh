set -eu
if [ -d /dav1d_bin/lib ]; then \
	mkdir /dav1d/
	cp -r /dav1d_bin/lib /dav1d/lib
	exit 0
fi
apk add --no-cache clang musl-dev meson ninja pkgconfig nasm git
git clone --branch 1.3.0 --depth 1 https://code.videolan.org/videolan/dav1d.git /dav1d_src
cd /dav1d_src
meson build -Dprefix=/dav1d -Denable_tools=false -Denable_examples=false -Ddefault_library=static --buildtype release
ninja -C build
ninja -C build install
rm -r /dav1d_src
cp -r /dav1d/lib /dav1d_bin/lib
