FROM public.ecr.aws/docker/library/alpine:latest AS c_build_env
RUN apk add --no-cache make clang musl-dev meson ninja pkgconfig nasm git

FROM c_build_env AS dav1d
RUN git clone --branch 1.3.0 --depth 1 https://github.com/videolan/dav1d.git /dav1d_src
RUN cd /dav1d_src && meson build -Dprefix=/dav1d -Denable_tools=false -Denable_examples=false -Ddefault_library=static --buildtype release
RUN cd /dav1d_src && ninja -C build
RUN cd /dav1d_src && ninja -C build install

FROM c_build_env AS lcms2
RUN git clone -b lcms2.16 --depth 1 https://github.com/mm2/Little-CMS.git /lcms2_src
ENV CONFIGURE_FLAGS="--enable-static --prefix=/lcms2"
RUN cd /lcms2_src && ./configure
RUN cd /lcms2_src && make
RUN cd /lcms2_src && make DESTDIR=/lcms2 install

FROM --platform=$BUILDPLATFORM public.ecr.aws/docker/library/rust:latest AS build_app
ARG BUILDARCH
ARG TARGETARCH
ARG TARGETVARIANT
#RUN apk add --no-cache clang musl-dev curl pkgconfig nasm mold git
RUN apt-get update && apt-get install -y clang musl-dev pkg-config nasm mold git
ENV CARGO_HOME=/var/cache/cargo
ENV SYSTEM_DEPS_LINK=static
COPY crossfiles /app/crossfiles
RUN bash /app/crossfiles/deps.sh
WORKDIR /app
COPY avif-decoder_dep ./avif-decoder_dep
COPY .gitmodules ./.gitmodules
COPY --from=dav1d /dav1d /dav1d
COPY --from=lcms2 /lcms2 /lcms2
RUN cp -r /lcms2/usr/local/lib/* /dav1d/lib
ENV PKG_CONFIG_PATH=/dav1d/lib/pkgconfig
ENV LD_LIBRARY_PATH=/dav1d/lib
COPY src ./src
COPY Cargo.toml ./Cargo.toml
COPY asset ./asset
COPY examples ./examples
RUN --mount=type=cache,target=/var/cache/cargo --mount=type=cache,target=/app/target bash /app/crossfiles/build.sh

FROM public.ecr.aws/docker/library/alpine:latest
ARG UID="852"
ARG GID="852"
RUN addgroup -g "${GID}" proxy && adduser -u "${UID}" -G proxy -D -h /media-proxy-rs -s /bin/sh proxy
WORKDIR /media-proxy-rs
USER proxy
COPY --from=build_app /app/media-proxy-rs ./media-proxy-rs
COPY --from=build_app /app/healthcheck ./healthcheck
RUN sh -c "./media-proxy-rs&" && ./healthcheck 12887 http://127.0.0.1:12766/test.webp
HEALTHCHECK --interval=30s --timeout=3s CMD ./healthcheck 5555 http://127.0.0.1:12766/test.webp || exit 1
EXPOSE 12766
CMD ["./media-proxy-rs"]
