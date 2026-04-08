FROM debian:bookworm-slim

ARG TARGETARCH
ARG TARGETVARIANT

RUN apt-get update \
    && apt-get install -y --no-install-recommends libgcc-s1 \
    && rm -rf /var/lib/apt/lists/*

COPY binaries/${TARGETARCH}${TARGETVARIANT}/haproxy-spoa-hub /usr/local/bin/

RUN mkdir -p /etc/haproxy-spoa-hub/plugins
COPY binaries/${TARGETARCH}${TARGETVARIANT}/libnoop_plugin.so /etc/haproxy-spoa-hub/plugins/
COPY binaries/${TARGETARCH}${TARGETVARIANT}/libecho_plugin.so /etc/haproxy-spoa-hub/plugins/
COPY binaries/${TARGETARCH}${TARGETVARIANT}/libdelay_plugin.so /etc/haproxy-spoa-hub/plugins/

ENTRYPOINT ["haproxy-spoa-hub"]
CMD ["--config", "/etc/haproxy-spoa-hub/config.toml"]
