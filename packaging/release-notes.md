Audio node tương thích **Lavalink v4**, viết bằng Rust, có cache audio cục bộ với dọn dẹp LRU và tự động backup lên OneDrive.

## Tính năng mới

- **Đủ 10 filter của Lavalink v4:** volume, equalizer, karaoke, timescale (nightcore, vaporwave), tremolo, vibrato, rotation (8D), distortion, channelMix, lowPass.
- **Link Spotify, Apple Music, Deezer kiểu LavaSrc:** track, album, playlist, top tracks của nghệ sĩ, `spsearch:`, `amsearch:`, `dzsearch:`, `dzisrc:`. Âm thanh lấy từ bản YouTube khớp ISRC hoặc tên bài, có so độ dài, kết quả được nhớ lại. Spotify cần client ID/secret, Apple Music cần developer token, Deezer không cần cấu hình.
- **Live stream YouTube và radio `.m3u8`** qua HLS.
- **Bài dài hơn 2 giờ hoặc file quá lớn** phát thẳng từ nguồn bằng HTTP Range, vẫn tua được, không làm đầy cache.
- **yt-dlp tự tải và tự cập nhật** (`sources.ytdlp.path: managed`), thêm lệnh `rust-link update-ytdlp`.
- **Gói Linux ARM64** và **image Docker amd64 + arm64** trên `ghcr.io`.
- **Filter và âm lượng có hiệu lực ngay** (khoảng 20 ms), không chờ bộ đệm.
- **Cache kết quả tìm kiếm** 6 giờ và **tải trước playlist** (3 bài kế tiếp), nên bài kế phát ngay từ đĩa.
- **Live stream dùng chung:** nhiều server phát cùng một live stream chỉ tốn một lần tải và giải mã.
- **yt-dlp bản thư mục:** mỗi lần gọi yt-dlp nhanh gấp đôi.

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

- Nâng cấp từ 0.1.x: thay file chạy, giữ nguyên thư mục `data/`. Chỉ mục SQLite tự thêm các bảng mới khi khởi động; các khóa cấu hình mới đều có giá trị mặc định.
- `application.yml` cũ có `sources.ytdlp.path: yt-dlp` vẫn chạy như trước; node gọi `yt-dlp -U` theo lịch. Đổi thành `managed` để node tự quản lý yt-dlp.
- Phần ghép YouTube cho Spotify/Apple Music/Deezer và phần live YouTube đã được test với API và yt-dlp giả lập; Deezer đã chạy thử với API thật.
