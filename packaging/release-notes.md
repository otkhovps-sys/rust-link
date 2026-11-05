Audio node tương thích **Lavalink v4**, viết bằng Rust, có cache audio cục bộ với dọn dẹp LRU và tự động backup lên OneDrive.

## Sửa lỗi

- Giữ nguyên cổng Discord voice trong endpoint (ví dụ `:2096`), tránh lỗi `4006: Session is no longer valid` do kết nối nhầm cổng.
- Gửi vị trí phát và thời gian hiện tại trước `TrackStartEvent`, tránh thanh tiến độ nhảy đến cuối bài khi client chưa nhận bản cập nhật đầu tiên.

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

- Bộ test gồm codec, cache, API, voice giả lập và OneDrive giả lập; test YouTube thật cần bật riêng.
- Đã xác nhận kết nối Discord voice với DAVE trên Windows bằng bản 0.1.0 và client giữ cổng; bản này sửa trực tiếp việc giữ cổng trong node.
- Chỉ filter `volume` có tác dụng; live stream YouTube chưa hỗ trợ.
