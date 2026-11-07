Audio node tương thích **Lavalink v4**, viết bằng Rust, có cache audio cục bộ với dọn dẹp LRU và tự động backup lên OneDrive.

## Tính năng mới

- **Đăng nhập Spotify để đọc playlist:** từ tháng 3/2026, Spotify chỉ trả danh sách bài của playlist thuộc tài khoản đã đăng nhập. Lệnh mới `rust-link spotify-login` đăng nhập tài khoản Spotify một lần (mở trình duyệt, hoặc dán địa chỉ khi node chạy trên VPS/Docker) và lưu refresh token vào `spotify.json`. Node đang chạy tự dùng tài khoản này, không cần khởi động lại, và đọc được playlist mà tài khoản đó sở hữu hoặc được mời cùng sửa.
- **Theo kịp thay đổi Web API Spotify 2026:**
  - Playlist Spotify không trả danh sách bài: node báo lỗi ghi rõ lý do thay vì `empty`, nên bot không còn báo nhầm "không tìm thấy".
  - Đọc được định dạng playlist mới (`items`, `item`) lẫn định dạng cũ.
  - Link nghệ sĩ: Spotify đã bỏ API top tracks, node thay bằng 10 bài tìm theo tên nghệ sĩ.
  - `spsearch:` tự giới hạn 10 kết quả, mức tối đa mới của Spotify (trước đây đặt cao hơn sẽ bị lỗi 400).
  - Lỗi 403 ghi rõ app ở Development Mode cần chủ app có Spotify Premium.

Mọi tính năng của 0.2.1 (plugin, YouTube Mix thành playlist...) và 0.2.0 vẫn giữ nguyên.

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

- Nâng cấp từ 0.2.0 hoặc 0.2.1: chỉ cần thay file chạy, các khóa cấu hình mới đều có giá trị mặc định.
- Đọc playlist Spotify: trên Spotify dashboard, thêm Redirect URI `http://127.0.0.1:8888/callback` cho app, rồi chạy `rust-link spotify-login` trong thư mục có `application.yml`. Playlist của người khác vẫn không đọc được, đó là giới hạn của Spotify. Hướng dẫn chi tiết ở mục 8 của `HUONG_DAN.md`.
- `spotify.json` chứa refresh token của tài khoản Spotify: không chia sẻ file này.
- Không có plugin nào được bật sẵn; node chạy plugin trong thư mục `plugins/` cạnh file chạy (Docker: `/data/plugins`).
- Plugin Python cần Python 3.9+, plugin Node.js cần Node.js 16+ trên máy chủ. Image Docker không có sẵn hai runtime này; `PLUGINS.md` có Dockerfile mẫu để thêm.
- Nâng cấp từ 0.1.x: thay file chạy, giữ nguyên thư mục `data/`. Chỉ mục SQLite tự thêm các bảng mới khi khởi động; các khóa cấu hình mới đều có giá trị mặc định.
- `application.yml` cũ có `sources.ytdlp.path: yt-dlp` vẫn chạy như trước; node gọi `yt-dlp -U` theo lịch. Đổi thành `managed` để node tự quản lý yt-dlp.
- Phần Spotify (kể cả đăng nhập), ghép YouTube cho Spotify/Apple Music/Deezer và live YouTube đã được test với API và yt-dlp giả lập; Deezer và link YouTube Mix đã chạy thử với dịch vụ thật.
