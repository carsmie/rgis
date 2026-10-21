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
# Dependencies in their own layer, rebuilt only when Cargo.toml/Cargo.lock (or the GDAL base
# image, for gdal-sys) change: CI's layer cache (cache-to: gha) keeps layers, not cache mounts,
# so with mounts every release compiled all dependencies again (~5 of ~6 min).
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
 && cargo build --release --locked \
 && rm -rf src
COPY . .
# Only rgis itself compiles here; target/ is dropped so this layer stays small in the cache.
RUN touch src/main.rs \
 && cargo build --release --locked \
 && cp target/release/rgis /usr/local/bin/rgis \
 && rm -rf target

# Extra fonts for the runtime image, empty unless a build supplies them as a named build
# context, e.g. `docker build --build-context extra-fonts=<dir> .` (tnt-rgis passes
# Kartverket's map fonts this way; they are not part of this repository).
FROM scratch AS extra-fonts

FROM ghcr.io/osgeo/gdal:ubuntu-small-${GDAL_VERSION}
# The font packages of the tnt-qgis QGIS Server image (CJK left out), so labels resolve to
# the same faces; Kartverket's own fonts come only through extra-fonts above. Qt falls back to
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
# glibc keeps an arena per busy thread and hands little back after a burst of parallel
# renders (RSS stayed ~2x higher); two arenas cost no measurable latency.
ENV MALLOC_ARENA_MAX=2
# GDAL (rasters, /vsigs/): the block cache otherwise defaults to 5 % of the RAM GDAL sees,
# which can be the node's rather than the pod's limit (a GEBCO read kept ~500 MB); the rest
# matches the tnt-qgis image: HTTP/2, no sidecar-file probing on open, retries.
ENV GDAL_CACHEMAX=256 \
    GDAL_HTTP_VERSION=2 \
    GDAL_DISABLE_READDIR_ON_OPEN=EMPTY_DIR \
    GDAL_HTTP_MAX_RETRY=3 \
    GDAL_HTTP_RETRY_DELAY=1 \
    GDAL_PAM_ENABLED=NO
COPY --from=extra-fonts / /usr/local/share/fonts/rgis/
COPY --from=build /usr/local/bin/rgis /usr/local/bin/rgis
COPY demo /opt/rgis/demo
RUN useradd --uid 10001 --no-create-home rgis
USER rgis
WORKDIR /opt/rgis
EXPOSE 8000
ENTRYPOINT ["rgis"]
CMD ["-p", "demo/earth_local.qgs", "0.0.0.0:8000"]
