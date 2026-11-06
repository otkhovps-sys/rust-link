Audio node tương thích **Lavalink v4**, viết bằng Rust, có cache audio cục bộ với dọn dẹp LRU và tự động backup lên OneDrive.

## Tính năng mới

- **Plugin viết bằng ngôn ngữ nào cũng được:** plugin là chương trình riêng (Python, Node.js, Go...) nói chuyện với node bằng JSON qua stdin/stdout. Plugin thêm được nguồn nhạc theo tiền tố hoặc mẫu URL, chọn nơi phát bài (URL kèm header, file trên máy chủ, hoặc chuyển sang bài khác), nhận sự kiện websocket, thêm route REST riêng và gọi lại REST của node. Plugin bị crash được tự chạy lại; trạng thái xem ở `GET /v4/rustlink/plugins`, plugin đang chạy hiện trong `/v4/info`.
- **Lệnh `rust-link plugin new <tên> [--lang node]`** tạo sẵn khung plugin kèm SDK Python hoặc Node.js (không cần cài thư viện); `rust-link plugin list` liệt kê plugin sẽ được chạy.
- **Hai plugin mẫu** trong `examples/plugins/`: `radio` (Python, đài radio theo tên) và `local-files` (Node.js, phát file nhạc trong thư mục trên máy chủ). Hướng dẫn đầy đủ trong `PLUGINS.md`.
- **Link YouTube Mix (`list=RD...`) thành playlist:** trước đây chỉ phát một video, nay trả về playlist 25 bài đầu với video trong link được chọn. Link radio của YouTube Music (`list=RDAMVM...`) cũng vậy. Đổi số bài bằng `sources.youtubeMixLimit` (0 = chỉ phát video trong link như cũ).

Mọi tính năng của 0.2.0 (đủ 10 filter, link Spotify/Apple Music/Deezer, live stream, tự cập nhật yt-dlp...) vẫn giữ nguyên.

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

- Nâng cấp từ 0.2.0: chỉ cần thay file chạy. Không có plugin nào được bật sẵn; node chạy plugin trong thư mục `plugins/` cạnh file chạy (Docker: `/data/plugins`).
- Plugin Python cần Python 3.9+, plugin Node.js cần Node.js 16+ trên máy chủ. Image Docker không có sẵn hai runtime này; `PLUGINS.md` có Dockerfile mẫu để thêm.
- Nâng cấp từ 0.1.x: thay file chạy, giữ nguyên thư mục `data/`. Chỉ mục SQLite tự thêm các bảng mới khi khởi động; các khóa cấu hình mới đều có giá trị mặc định.
- `application.yml` cũ có `sources.ytdlp.path: yt-dlp` vẫn chạy như trước; node gọi `yt-dlp -U` theo lịch. Đổi thành `managed` để node tự quản lý yt-dlp.
- Phần ghép YouTube cho Spotify/Apple Music/Deezer và phần live YouTube đã được test với API và yt-dlp giả lập; Deezer và link YouTube Mix đã chạy thử với dịch vụ thật.
