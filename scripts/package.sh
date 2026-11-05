#!/usr/bin/env bash
# Đóng gói rust-link thành một file nén chạy được ngay, giống bộ Lavalink.jar + application.yml.
#
#   scripts/package.sh                         # mặc định: Linux x86_64, binary tĩnh (musl)
#   TARGET=x86_64-unknown-linux-gnu scripts/package.sh
#
# Kết quả: dist/rust-link-<version>-<platform>.tar.gz (+ .sha256)
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
TARGET=${TARGET:-x86_64-unknown-linux-musl}
case "$TARGET" in
  x86_64-unknown-linux-musl) PLATFORM=linux-x86_64 ;;
  aarch64-unknown-linux-musl) PLATFORM=linux-aarch64 ;;
  *) PLATFORM=$TARGET ;;
esac
NAME="rust-link-${VERSION}-${PLATFORM}"
STAGE="dist/${NAME}"

echo ">> build ${TARGET} (release)"
cargo build --release --locked --target "$TARGET"
BIN="target/${TARGET}/release/rust-link"

echo ">> stage ${STAGE}"
rm -rf "$STAGE"
mkdir -p "$STAGE"
install -m 0755 "$BIN" "$STAGE/rust-link"

# application.yml đặt cạnh file chạy, đường dẫn tương đối để chạy được ở bất kỳ thư mục nào.
sed -e 's#directory: /data/cache #directory: ./data/cache #' \
    -e 's#database: /data/cache/index.db#database: ./data/cache/index.db#' \
    -e 's#credentialsFile: config/onedrive.json#credentialsFile: onedrive.json#' \
    config/application.yml > "$STAGE/application.yml"
grep -q 'directory: ./data/cache' "$STAGE/application.yml" || { echo "cache path rewrite failed" >&2; exit 1; }

cp config/onedrive.example.json "$STAGE/onedrive.example.json"
cp packaging/rust-link.service "$STAGE/rust-link.service"
cp docs/HUONG_DAN.md "$STAGE/HUONG_DAN.md"
cp README.md "$STAGE/README.md"

echo ">> smoke test"
"$STAGE/rust-link" --version
(cd "$STAGE" && ./rust-link check-config > /dev/null)
rm -rf "$STAGE/logs" "$STAGE/data"

echo ">> archive"
tar -C dist -czf "dist/${NAME}.tar.gz" "$NAME"
(cd dist && sha256sum "${NAME}.tar.gz" > "${NAME}.tar.gz.sha256")
ls -lh "dist/${NAME}.tar.gz"
cat "dist/${NAME}.tar.gz.sha256"
