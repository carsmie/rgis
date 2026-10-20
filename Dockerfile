# syntax=docker/dockerfile:1
# Build: docker build -t rgis .            (GDAL version: --build-arg GDAL_VERSION=3.13.3)
# Run:   docker run --rm -p 8000:8000 rgis            (serves demo/earth_local.qgs)
#        docker run --rm -p 8000:8000 -v /data:/data rgis -p /data/project.qgs 0.0.0.0:8000
#        GCS: -e GS_ACCESS_KEY_ID -e GS_SECRET_ACCESS_KEY … rgis --projects-dir /vsigs/bucket/prefix/ 0.0.0.0:8000

# Official GDAL images carry headers + gdal.pc, so build and runtime share one GDAL.
ARG GDAL_VERSION=3.13.3
FROM ghcr.io/osgeo/gdal:ubuntu-small-${GDAL_VERSION} AS build
ARG RUST_VERSION=1.98.1
# libclang-dev: bindgen for gdal-sys needs clang's builtin headers (stddef.h).
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential libclang-dev libssl-dev pkg-config curl ca-certificates \
 && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain ${RUST_VERSION}
WORKDIR /src
COPY . .
# Cache mounts keep the registry and incremental target dir across builds; the binary is
# copied out because the target mount is not part of the image. Target cache is keyed by
# GDAL version: gdal-sys doesn't re-run bindgen when only the installed GDAL changes.
ARG GDAL_VERSION
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=rgis-target-gdal${GDAL_VERSION},target=/src/target \
    cargo build --release --locked && cp target/release/rgis /usr/local/bin/rgis

FROM ghcr.io/osgeo/gdal:ubuntu-small-${GDAL_VERSION}
# The font packages of the tnt-qgis QGIS Server image (CJK left out), so labels resolve to
# the same faces; Kartverket's own fonts are not redistributed here. Qt falls back to
# fontconfig's sans-serif there (Noto Sans), rgis to RGIS_LABEL_FONT. The upgrade picks up
# OS security fixes released after the GDAL base image was built (Pharos/Trivy findings).
RUN apt-get update \
 && apt-get upgrade -y --no-install-recommends \
 && apt-get install -y --no-install-recommends fonts-dejavu fonts-dejavu-core fonts-dejavu-extra \
      fonts-liberation fonts-liberation2 fonts-open-sans fonts-noto-core fonts-noto-extra \
      fonts-noto-mono fonts-noto-ui-core fonts-noto-ui-extra fonts-roboto fonts-vollkorn fonts-ebgaramond \
 && rm -rf /var/lib/apt/lists/* \
 # Canonical's Pebble service manager ships in the Ubuntu base image (not a package, unused
 # here: the entrypoint is rgis) and carries Go stdlib CVEs.
 && rm -rf /usr/bin/pebble /var/lib/pebble
ENV RGIS_LABEL_FONT="Noto Sans"
COPY --from=build /usr/local/bin/rgis /usr/local/bin/rgis
COPY demo /opt/rgis/demo
RUN useradd --uid 10001 --no-create-home rgis
USER rgis
WORKDIR /opt/rgis
EXPOSE 8000
ENTRYPOINT ["rgis"]
CMD ["-p", "demo/earth_local.qgs", "0.0.0.0:8000"]
