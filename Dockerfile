# syntax=docker/dockerfile:1
# Image rust-link, dùng tương tự image Lavalink:
#   docker build -t rust-link .
#   docker run -d -p 2333:2333 -v rust-link-data:/data rust-link
# hoặc: docker compose up -d

FROM rust:1-bookworm AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release --locked \
 && install -m 0755 target/release/rust-link /usr/local/bin/rust-link

FROM python:3.13-slim-bookworm
# yt-dlp tải nhạc; Deno là JavaScript runtime yt-dlp cần cho YouTube.
COPY --from=denoland/deno:bin /deno /usr/local/bin/deno
RUN pip install --no-cache-dir "yt-dlp[default]" \
 && groupadd -g 322 rustlink \
 && useradd -u 322 -g rustlink -d /opt/rust-link -s /usr/sbin/nologin rustlink \
 && mkdir -p /opt/rust-link /data/cache \
 && chown -R rustlink:rustlink /opt/rust-link /data
COPY --from=build /usr/local/bin/rust-link /opt/rust-link/rust-link
COPY config/application.yml /opt/rust-link/application.yml
# Credentials OneDrive nằm trên volume /data để token xoay vòng được ghi lại.
RUN sed -i 's#credentialsFile: config/onedrive.json#credentialsFile: /data/onedrive.json#' /opt/rust-link/application.yml
WORKDIR /opt/rust-link
USER rustlink
VOLUME ["/data"]
EXPOSE 2333
ENTRYPOINT ["/opt/rust-link/rust-link"]
