Audio node tương thích **Lavalink v4**, viết bằng Rust, có cache audio cục bộ với dọn dẹp LRU và tự động backup lên OneDrive.

## Tải về

| File | Nền tảng | Cách chạy |
|---|---|---|
| `rust-link-{{VERSION}}-linux-x86_64.tar.gz` | Linux x86_64, file chạy tĩnh | giải nén rồi chạy `./rust-link` |
| `rust-link-{{VERSION}}-windows-x86_64.zip` | Windows x86_64 | giải nén rồi bấm đúp `start.bat` |
| `*.sha256` | | mã kiểm tra toàn vẹn |

## Bắt đầu nhanh

1. Cài `yt-dlp` (khuyến nghị cài thêm Deno).
2. Đổi `server.password` trong `application.yml` nằm cạnh file chạy.
3. Chạy node, đợi dòng `rust-link is ready to accept connections`.
4. Trong bot, khai báo node Lavalink với host của máy chủ, cổng `2333` và password vừa đặt.

Hướng dẫn đầy đủ nằm trong `HUONG_DAN.md` đi kèm mỗi gói.

## Lưu ý

- Bản Windows mới được chạy thử bằng Wine, chưa thử trên máy Windows thật.
- Chưa kiểm thử với voice server Discord và OneDrive thật; các bài test dùng server giả lập.
- Chỉ filter `volume` có tác dụng; live stream YouTube chưa hỗ trợ.
