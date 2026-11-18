# One build of the node for both images. Build context: backend/.
#   docker build -f deploy/Dockerfile --target net -t acrux-net .
#   docker build -f deploy/Dockerfile --target api -t acrux-api .
FROM rust:1-bookworm AS build
WORKDIR /src
COPY node/ node/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/node/target \
    cd node && cargo build --release --locked -p acrux-node && cp target/release/acrux /acrux

# Acrux Net: twelve validators in three hubs behind one gateway.
FROM debian:bookworm-slim AS net
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /acrux /usr/local/bin/acrux
COPY chain/genesis/ /app/chain/genesis/
COPY chain/placements/ /app/chain/placements/
COPY chain/target/deploy/demo_pool.so chain/target/deploy/hub_registry.so /app/chain/target/deploy/
WORKDIR /app/node
ENV ACRUX_BIND=0.0.0.0
EXPOSE 4930 4931 4989
CMD ["acrux", "net", "up", "--store", "/var/lib/acrux/net"]

# The API, with the node binary it runs for the schedule preview and the Fault Lab sandboxes.
FROM node:22-bookworm-slim AS api
WORKDIR /app
COPY contract/ contract/
RUN cd contract && npm ci && npm run build
COPY spec/ spec/
COPY chain/genesis/ chain/genesis/
COPY chain/placements/ chain/placements/
COPY chain/target/deploy/demo_pool.so chain/target/deploy/hub_registry.so chain/target/deploy/
COPY api/package.json api/package-lock.json api/
RUN cd api && npm ci
COPY api/ api/
RUN cd api && npm run build
COPY --from=build /acrux /app/node/acrux
ENV HOST=0.0.0.0 PORT=4920 ACRUX_BIN=/app/node/acrux ACRUX_NODE_DIR=/app/node LAB_DIR=/tmp/acrux-lab
EXPOSE 4920
CMD ["node", "api/scripts/start-service.mjs", "api"]
