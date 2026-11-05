#!/usr/bin/env bash
# Đóng gói rust-link thành một file nén chạy được ngay, giống bộ Lavalink.jar + application.yml.
#
#   scripts/package.sh                                   # Linux x86_64, binary tĩnh (musl) -> .tar.gz
#   TARGET=x86_64-pc-windows-gnu scripts/package.sh      # Windows x86_64 (MinGW)          -> .zip
#   TARGET=aarch64-unknown-linux-musl scripts/package.sh # Linux ARM64 tĩnh                -> .tar.gz
#
# Build chéo sang ARM64 từ máy x86_64 cần cargo-zigbuild và zig (pip install ziglang cargo-zigbuild).
# Có qemu-aarch64-static thì file chạy ARM64 được chạy thử bằng giả lập.
#
# Kết quả: dist/rust-link-<version>-<platform>.{tar.gz|zip} (+ .sha256)
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
TARGET=${TARGET:-x86_64-unknown-linux-musl}
EXE=rust-link
case "$TARGET" in
  x86_64-unknown-linux-musl) PLATFORM=linux-x86_64 ;;
  aarch64-unknown-linux-musl) PLATFORM=linux-aarch64 ;;
  x86_64-pc-windows-*) PLATFORM=windows-x86_64; EXE=rust-link.exe ;;
  *) PLATFORM=$TARGET ;;
esac
NAME="rust-link-${VERSION}-${PLATFORM}"
STAGE="dist/${NAME}"

BUILD=(cargo build)
case "$TARGET" in
  *-linux-musl)
    # audiopus_sys chọn kiểu link theo máy build: ép libopus tĩnh cho file chạy musl.
    export LIBOPUS_STATIC=1 LIBOPUS_NO_PKG=1
    if [[ "${TARGET%%-*}" != "$(uname -m)" ]]; then
      command -v cargo-zigbuild > /dev/null || { echo "cargo-zigbuild is required to cross-compile ${TARGET}" >&2; exit 1; }
      BUILD=(cargo zigbuild)
    fi
    ;;
esac

echo ">> build ${TARGET} (release, ${BUILD[*]})"
"${BUILD[@]}" --release --locked --target "$TARGET"
BIN="target/${TARGET}/release/${EXE}"

echo ">> stage ${STAGE}"
rm -rf "$STAGE"
mkdir -p "$STAGE"
install -m 0755 "$BIN" "$STAGE/${EXE}"

# application.yml đặt cạnh file chạy, đường dẫn tương đối để chạy được ở bất kỳ thư mục nào.
sed -e 's#directory: /data/cache #directory: ./data/cache #' \
    -e 's#database: /data/cache/index.db#database: ./data/cache/index.db#' \
    -e 's#credentialsFile: config/onedrive.json#credentialsFile: onedrive.json#' \
    config/application.yml > "$STAGE/application.yml"
grep -q 'directory: ./data/cache' "$STAGE/application.yml" || { echo "cache path rewrite failed" >&2; exit 1; }

cp config/onedrive.example.json "$STAGE/onedrive.example.json"
cp docs/HUONG_DAN.md "$STAGE/HUONG_DAN.md"
cp README.md "$STAGE/README.md"
cp LICENSE "$STAGE/LICENSE"
case "$PLATFORM" in
  windows-*)
    cp packaging/start.bat "$STAGE/start.bat"
    # Notepad trên Windows cũ cần CRLF.
    sed -i 's/$/\r/' "$STAGE/application.yml" "$STAGE/start.bat"
    ;;
  *)
    cp packaging/rust-link.service "$STAGE/rust-link.service"
    ;;
esac

echo ">> smoke test"
RUNNER=()
SKIP_SMOKE=0
if [[ "$PLATFORM" == windows-* ]]; then
  WINE=$(command -v wine64 || command -v wine || ls /usr/lib/wine/wine64 2>/dev/null || true)
  if [[ -z "$WINE" ]]; then
    echo "   (wine not found, skipping smoke test)"
    SKIP_SMOKE=1
  else
    RUNNER=("$WINE")
    export WINEDEBUG=-all
  fi
elif [[ "$TARGET" == aarch64-* && "$(uname -m)" != aarch64 ]]; then
  if command -v qemu-aarch64-static > /dev/null; then
    RUNNER=(qemu-aarch64-static)
  else
    echo "   (qemu-aarch64-static not found, skipping smoke test)"
    SKIP_SMOKE=1
  fi
fi
if [[ "$SKIP_SMOKE" == 0 ]]; then
  (cd "$STAGE" && "${RUNNER[@]}" "./${EXE}" --version && "${RUNNER[@]}" "./${EXE}" check-config > /dev/null)
  rm -rf "$STAGE/logs" "$STAGE/data"
fi

echo ">> archive"
if [[ "$PLATFORM" == windows-* ]]; then
  ARCHIVE="${NAME}.zip"
  rm -f "dist/${ARCHIVE}"
  (cd dist && zip -qr "${ARCHIVE}" "${NAME}")
else
  ARCHIVE="${NAME}.tar.gz"
  tar -C dist -czf "dist/${ARCHIVE}" "$NAME"
fi
(cd dist && sha256sum "${ARCHIVE}" > "${ARCHIVE}.sha256")
ls -lh "dist/${ARCHIVE}"
cat "dist/${ARCHIVE}.sha256"
