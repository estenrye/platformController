FROM rust:1.98-slim-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --bin platform-controller

FROM debian:bookworm-slim
ARG HELM_VERSION=v3.16.3
# Set automatically by BuildKit; helm's release tarballs use the same names.
ARG TARGETARCH
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && curl -fsSL "https://get.helm.sh/helm-${HELM_VERSION}-linux-${TARGETARCH}.tar.gz" -o /tmp/helm.tar.gz \
    && tar -xzf /tmp/helm.tar.gz -C /tmp \
    && mv "/tmp/linux-${TARGETARCH}/helm" /usr/local/bin/helm \
    && rm -rf /tmp/helm.tar.gz "/tmp/linux-${TARGETARCH}" \
    && apt-get purge -y curl \
    && apt-get autoremove -y \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/platform-controller /usr/local/bin/platform-controller
ENTRYPOINT ["/usr/local/bin/platform-controller"]
