# Adobe container build: upstream Dockerfile + sccache (see adobe/DOCKER.md).
# Build context is repo root: docker buildx build -f adobe/Dockerfile ..
# syntax=docker/dockerfile:1.11
ARG BUILDER=base

FROM docker.io/library/node:23.11.0-bookworm AS node

WORKDIR /app

COPY ui .
COPY schema /schema

RUN --mount=type=cache,target=/app/npm/cache npm install

RUN --mount=type=cache,target=/app/npm/cache npm run build

FROM docker.io/library/rust:1.95.0-trixie AS musl-builder

ARG TARGETARCH

RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    rm -f /etc/apt/apt.conf.d/docker-clean && \
    echo 'Binary::apt::APT::Keep-Downloaded-Packages "true";' > /etc/apt/apt.conf.d/keep-cache && \
    apt-get update && apt-get install -y --no-install-recommends \
    make musl-tools

RUN <<EOF
mkdir /build
if [ "$TARGETARCH" = "arm64" ]; then
  rustup target add aarch64-unknown-linux-musl;
  echo aarch64-unknown-linux-musl > /build/target
else
  rustup target add x86_64-unknown-linux-musl;
  echo x86_64-unknown-linux-musl > /build/target
fi
EOF

FROM docker.io/library/rust:1.95.0-trixie AS base-builder

ARG TARGETARCH

RUN <<EOF
mkdir /build
if [ "$TARGETARCH" = "arm64" ]; then
  echo aarch64-unknown-linux-gnu > /build/target
else
  echo x86_64-unknown-linux-gnu > /build/target
fi
echo "Building $(cat /build/target)"
EOF

FROM ${BUILDER}-builder AS builder
ARG TARGETARCH
ARG PROFILE=release
ARG VERSION
ARG GIT_REVISION
ARG CARGO_FEATURES=agentgateway/ui
ARG CARGO_NO_DEFAULT_FEATURES=false

WORKDIR /app

# --- Adobe: prebuilt sccache (~10s vs cargo install ~3–8 min cold) ---
ARG SCCACHE_VERSION=0.15.0
RUN <<EOF
set -eux
if ! command -v curl >/dev/null 2>&1; then
  apt-get update
  apt-get install -y --no-install-recommends curl ca-certificates
  rm -rf /var/lib/apt/lists/*
fi
case "${TARGETARCH}" in
  amd64)
    sccache_arch=x86_64-unknown-linux-musl
    sccache_sha256=782d2b5dd7ae0a55ebe368ab258114d0928d019ac2d949ab85d5d02f3926709e
    ;;
  arm64)
    sccache_arch=aarch64-unknown-linux-musl
    sccache_sha256=3a6a3712b49da3d263bf2d30d702de4302793016019e800bfb81c0c69401d8f
    ;;
  *)
    echo "unsupported TARGETARCH for sccache: ${TARGETARCH}" >&2
    exit 1
    ;;
esac
artifact="sccache-v${SCCACHE_VERSION}-${sccache_arch}"
url="https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/${artifact}.tar.gz"
tmpdir="$(mktemp -d)"
curl -fsSL "${url}" -o "${tmpdir}/sccache.tar.gz"
echo "${sccache_sha256}  ${tmpdir}/sccache.tar.gz" | sha256sum -c -
tar -xzf "${tmpdir}/sccache.tar.gz" -C /usr/local/bin --strip-components=1 "${artifact}/sccache"
chmod +x /usr/local/bin/sccache
rm -rf "${tmpdir}"
sccache --version
EOF

ENV RUSTC_WRAPPER=sccache
ENV SCCACHE_DIR=/sccache
# --- end Adobe sccache ---

COPY Makefile Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY crates ./crates
COPY tools ./tools
COPY --from=node /app/out ./ui/out

RUN \
    --mount=type=cache,id=cargo,target=/usr/local/cargo/registry \
    --mount=type=cache,id=cargo-git,target=/usr/local/cargo/git \
    cargo fetch --locked
RUN --mount=type=cache,target=/sccache \
    --mount=type=cache,target=/app/target \
    --mount=type=cache,id=cargo,target=/usr/local/cargo/registry  \
    --mount=type=cache,id=cargo-git,target=/usr/local/cargo/git \
    <<EOF
export VERSION="${VERSION}"
export GIT_REVISION="${GIT_REVISION}"
if [ "${CARGO_NO_DEFAULT_FEATURES}" = "true" ]; then
  cargo build --no-default-features --features "${CARGO_FEATURES}" --target "$(cat /build/target)" --profile ${PROFILE} || exit 1
else
  cargo build --features "${CARGO_FEATURES}" --target "$(cat /build/target)" --profile ${PROFILE} || exit 1
fi
mkdir /out
mv /app/target/$(cat /build/target)/${PROFILE}/agentgateway /out
/out/agentgateway --version
# Fail if version is not set
if /out/agentgateway --version | grep -q '"unknown"'; then
  exit 1
fi
EOF

FROM cgr.dev/chainguard/glibc-dynamic AS runner

ARG TARGETARCH

WORKDIR /

COPY --from=builder /out/agentgateway /app/agentgateway

LABEL org.opencontainers.image.source=https://github.com/agentgateway/agentgateway
LABEL org.opencontainers.image.description="Agentgateway is an open source project that is built on AI-native protocols to connect, secure, and observe agent-to-agent and agent-to-tool communication across any agent framework and environment."

ENTRYPOINT ["/app/agentgateway"]
