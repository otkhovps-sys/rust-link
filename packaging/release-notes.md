Audio node tương thích **Lavalink v4**, viết bằng Rust, có cache audio cục bộ với dọn dẹp LRU và tự động backup lên OneDrive.

## Bản vá 0.2.3

Bản này sửa lỗi yt-dlp tự quản lý (`sources.ytdlp.path: managed`), không thêm tính năng mới.

- **Tự sửa yt-dlp hỏng:** mỗi lần khởi động và mỗi lần kiểm tra cập nhật, node chạy thử `yt-dlp --version`. Bản cài không khởi động được (antivirus xóa file trong `_internal`, thư mục copy lỗi) sẽ được tải lại. Trước đây YouTube lỗi mãi cho đến khi xóa tay thư mục `bin/`.
- **Bản mới hỏng thì quay lại bản cũ:** bản yt-dlp mới tải về mà không chạy được sẽ bị bỏ, node giữ bản đang dùng và ghi lý do vào log.
- **Không còn mất yt-dlp trên Windows khi quay lại bản cũ:** bản cũ chỉ bị xóa khi đã được đưa về chỗ thành công. Nếu Windows đang khóa file, bản cũ được giữ trong `bin/yt-dlp-bundle.old` và node tự khôi phục ở lần khởi động sau, kể cả khi không có mạng.
- **Lần chạy đầu chậm không bị coi là hỏng:** nếu `--version` chạy quá 60 giây (antivirus đang quét), node giữ nguyên bản đang cài thay vì tải lại.

Mọi tính năng của 0.2.2 (đăng nhập Spotify, theo kịp Web API Spotify 2026), 0.2.1 (plugin, YouTube Mix thành playlist) và 0.2.0 vẫn giữ nguyên.

## Tải về

| File | Nền tảng | Cách chạy |
|---|---|---|
| `rust-link-{{VERSION}}-linux-x86_64.tar.gz` | Linux x86_64, file chạy tĩnh | giải nén rồi chạy `./rust-link` |
| `rust-link-{{VERSION}}-linux-aarch64.tar.gz` | Linux ARM64, file chạy tĩnh | giải nén rồi chạy `./rust-link` |
| `rust-link-{{VERSION}}-windows-x86_64.zip` | Windows x86_64 | giải nén rồi bấm đúp `start.bat` |
| `*.sha256` | | mã kiểm tra toàn vẹn |

Image Docker: `ghcr.io/otkhovps-sys/rust-link:v{{VERSION}}` (hoặc `:latest`).

## Bắt đầu nhanh

1. Đổi `server.password` trong `application.yml` nằm cạnh file chạy (khuyến nghị cài thêm Deno cho YouTube).
2. Chạy node. Lần đầu node tự tải yt-dlp vào `bin/`, rồi hiện dòng `rust-link is ready to accept connections`.
3. Trong bot, khai báo node Lavalink với host của máy chủ, cổng `2333` và password vừa đặt.

Hướng dẫn đầy đủ nằm trong `HUONG_DAN.md` đi kèm mỗi gói.

## Lưu ý

- Nâng cấp từ 0.2.0, 0.2.1 hoặc 0.2.2: chỉ cần thay file chạy, giữ nguyên thư mục `bin/` và `data/`, các khóa cấu hình mới đều có giá trị mặc định.
- Đọc playlist Spotify: trên Spotify dashboard, thêm Redirect URI `http://127.0.0.1:8888/callback` cho app, rồi chạy `rust-link spotify-login` trong thư mục có `application.yml`. Playlist của người khác vẫn không đọc được, đó là giới hạn của Spotify. Hướng dẫn chi tiết ở mục 8 của `HUONG_DAN.md`.
- `spotify.json` chứa refresh token của tài khoản Spotify: không chia sẻ file này.
- Không có plugin nào được bật sẵn; node chạy plugin trong thư mục `plugins/` cạnh file chạy (Docker: `/data/plugins`).
- Plugin Python cần Python 3.9+, plugin Node.js cần Node.js 16+ trên máy chủ. Image Docker không có sẵn hai runtime này; `PLUGINS.md` có Dockerfile mẫu để thêm.
- Nâng cấp từ 0.1.x: thay file chạy, giữ nguyên thư mục `data/`. Chỉ mục SQLite tự thêm các bảng mới khi khởi động; các khóa cấu hình mới đều có giá trị mặc định.
- `application.yml` cũ có `sources.ytdlp.path: yt-dlp` vẫn chạy như trước; node gọi `yt-dlp -U` theo lịch. Đổi thành `managed` để node tự quản lý yt-dlp.
- Phần Spotify (kể cả đăng nhập), ghép YouTube cho Spotify/Apple Music/Deezer và live YouTube đã được test với API và yt-dlp giả lập; Deezer và link YouTube Mix đã chạy thử với dịch vụ thật.
