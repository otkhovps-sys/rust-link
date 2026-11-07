# syntax=docker/dockerfile:1
# Image rust-link build từ mã nguồn, dùng tương tự image Lavalink:
#   docker build -t rust-link .
#   docker run -d -p 2333:2333 -v rust-link-data:/data rust-link
# hoặc: docker compose up -d
# Image phát hành (amd64 + arm64) dựng từ file chạy có sẵn: packaging/Dockerfile.release.

FROM rust:1-bookworm AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release --locked \
 && install -m 0755 target/release/rust-link /usr/local/bin/rust-link

FROM debian:bookworm-slim
# Deno là JavaScript runtime yt-dlp cần cho YouTube. yt-dlp do rust-link tự tải và tự cập
# nhật vào /data/bin (sources.ytdlp.path: managed), nên không cài sẵn trong image.
COPY --from=denoland/deno:bin /deno /usr/local/bin/deno
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd -g 322 rustlink \
 && useradd -u 322 -g rustlink -d /opt/rust-link -s /usr/sbin/nologin rustlink \
 && mkdir -p /opt/rust-link /data/cache /data/bin /data/plugins \
 && chown -R rustlink:rustlink /opt/rust-link /data
COPY --from=build /usr/local/bin/rust-link /opt/rust-link/rust-link
COPY config/application.yml /opt/rust-link/application.yml
# Credentials OneDrive, yt-dlp và plugin nằm trên volume /data để còn lại sau khi tạo lại container.
RUN sed -i -e 's#credentialsFile: config/onedrive.json#credentialsFile: /data/onedrive.json#' \
           -e 's#managedDir: ./bin#managedDir: /data/bin#' \
           -e 's#directory: ./plugins#directory: /data/plugins#' /opt/rust-link/application.yml \
 && grep -q 'managedDir: /data/bin' /opt/rust-link/application.yml \
 && grep -q 'directory: /data/plugins' /opt/rust-link/application.yml
WORKDIR /opt/rust-link
USER rustlink
VOLUME ["/data"]
EXPOSE 2333
ENTRYPOINT ["/opt/rust-link/rust-link"]
