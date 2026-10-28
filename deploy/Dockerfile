# Build:  docker build -f deploy/Dockerfile -t beldex-lws .
FROM ubuntu:24.04 AS build
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      build-essential cmake ninja-build pkg-config git ca-certificates curl \
      libboost-all-dev libsodium-dev libssl-dev libsqlite3-dev libzmq3-dev libcurl4-openssl-dev \
    && rm -rf /var/lib/apt/lists/*
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
ENV PATH=/root/.cargo/bin:$PATH
WORKDIR /src
COPY . .
RUN make dist

FROM ubuntu:24.04
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      ca-certificates libsodium23 libboost-serialization1.83.0 libboost-program-options1.83.0 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --home /var/lib/beldex-lws --create-home beldex-lws
COPY --from=build /src/dist/beldex-lws-*/bin /opt/beldex-lws/bin
COPY --from=build /src/dist/beldex-lws-*/lib /opt/beldex-lws/lib
USER beldex-lws
EXPOSE 8443 8444
ENTRYPOINT ["/opt/beldex-lws/bin/beldex-lws"]
CMD ["serve"]
