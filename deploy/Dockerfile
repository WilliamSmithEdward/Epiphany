# Build the web client, then the single embedded-UI + TLS binary, then a slim
# runtime image. Build from the repo root:
#   docker build -f deploy/Dockerfile -t epiphany .
#   docker run -p 8080:8080 -v epiphany-data:/var/lib/epiphany epiphany
#
# Note: this image is not built in CI (the project ships per-platform binaries);
# it is provided as a tested-by-you starting point.

# 1. Build the web UI (vite -> web/dist).
FROM node:24-bookworm AS web
WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ ./
RUN npm run build

# 2. Build the server with the web assets embedded and TLS available.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
COPY --from=web /web/dist ./web/dist
RUN cargo build --release -p epiphany-server --features embed-ui,tls

# 3. Minimal runtime as an unprivileged user.
FROM debian:bookworm-slim AS runtime
RUN useradd --system --home /var/lib/epiphany --create-home epiphany
COPY --from=build /src/target/release/epiphany-server /usr/local/bin/epiphany-server
USER epiphany
ENV EPIPHANY_BIND=0.0.0.0:8080 \
    EPIPHANY_DATA_DIR=/var/lib/epiphany
EXPOSE 8080
VOLUME ["/var/lib/epiphany"]
# `docker stop` sends SIGTERM, which the server drains cleanly.
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/epiphany-server"]
