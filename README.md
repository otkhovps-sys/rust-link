# rust-link

Audio Node viết bằng Rust, tương thích API **Lavalink v4**, có **cache audio cục bộ với dọn dẹp LRU** và **tự động backup lên OneDrive** (Microsoft Graph). Client Lavalink hiện có (discord.js, Shoukaku, lavalink.py, Lavalink4NET, Wavelink...) kết nối bằng cùng host, port và password.

## Tính năng

- **Lavalink v4**: WebSocket `/v4/websocket` (`ready`, `playerUpdate`, `stats`, `event`), resume session, các REST endpoint chính, định dạng lỗi chuẩn Lavalink, mã hóa track nhị phân Lavaplayer (phiên bản 1 đến 3).
- **Pipeline audio tự xây**: symphonia (WebM, Ogg, MP4/M4A, FLAC, MP3, WAV) → decoder Opus dùng libopus đăng ký vào symphonia → rubato resample về 48 kHz stereo → audiopus mã hóa khung 20 ms → RTP + AEAD → UDP.
- **Opus passthrough**: gói Opus 20 ms / 48 kHz / stereo của nguồn (YouTube itag 251) được gửi thẳng khi volume = 100 và không có filter, tiết kiệm phần lớn CPU encode.
- **Discord Voice Gateway v8**: Hello, Identify, Ready, IP discovery qua UDP, Select Protocol, Session Description, heartbeat có `seq_ack`, Resume, phân loại close code, `WebSocketClosedEvent`.
- **Mã hóa transport**: `aead_aes256_gcm_rtpsize` (ưu tiên) và `aead_xchacha20_poly1305_rtpsize`.
- **DAVE (Discord E2EE)** qua crate `davey`: opcode 11, 13, 21 đến 31, mã hóa khung Opus trước khi đóng gói RTP. Bật bằng feature `dave` (mặc định).
- **Cache cục bộ**: `/data/cache/{id_prefix}/{video_id}.opus`, chỉ mục SQLite, khử trùng lặp các lượt tải đồng thời, **phát lũy tiến** khi file còn đang tải, kiểm tra SHA-256.
- **LRU**: vượt `highWatermark` (90%) thì xóa file có `last_accessed_at` xa nhất tới khi về dưới `lowWatermark` (80%). File đang phát hoặc đang upload được pin, không bị xóa.
- **OneDrive**: OAuth2 refresh token (tự xoay vòng và lưu lại), upload session chia chunk bội số 320 KiB, resume sau lỗi mạng hoặc khởi động lại, xác minh `quickXorHash`, backup snapshot SQLite, khôi phục chỉ mục khi mất DB, kéo file về từ cold storage khi phát lại bài đã bị LRU dọn.

## Kiến trúc

```text
Client Lavalink ──REST/WS──► api/ ──► player/ (Session, Player)
                                         │
                                         ▼
                              audio/playback.rs
                ┌────────────────────────┴───────────────────────┐
                ▼                                                ▼
        cache::CacheManager::acquire                  luồng gửi 20 ms (tokio)
   ┌──────────┬──────────────┬─────────┐                         │
   ▼          ▼              ▼         ▼                         ▼
 hit đĩa   OneDrive pull   yt-dlp    HTTP            DAVE ► RTP + AEAD ► UDP ► Discord
   │      (cold storage)  (miss)   (miss/live)
   └──────────┴──────┬───────┴─────────┘
                     ▼
     thread decoder: symphonia ► rubato ► audiopus (hoặc passthrough)

 nền: cache/lru.rs (dọn LRU)   onedrive/sync_worker.rs (backup + snapshot)
```

## Cấu trúc thư mục

```text
.
├── Cargo.toml
├── config/
│   ├── application.yml          # cấu hình mẫu (cổng, password, cache, OneDrive)
│   └── onedrive.example.json    # mẫu file credentials
├── migrations/0001_init_cache.sql
├── docs/HUONG_DAN.md            # hướng dẫn triển khai và đấu nối bot
├── scripts/package.sh           # đóng gói thành file .tar.gz
├── packaging/rust-link.service  # mẫu systemd
├── Dockerfile, docker-compose.yml
├── src/
│   ├── main.rs                  # CLI: serve | onedrive-login | check-config
│   ├── lib.rs, config.rs, node.rs, stats.rs, util.rs
│   ├── api/                     # REST + WebSocket Lavalink v4
│   │   ├── mod.rs               # router, xác thực, định dạng lỗi
│   │   ├── websocket.rs, player_routes.rs, info_routes.rs
│   │   ├── models.rs            # kiểu dữ liệu Lavalink v4
│   │   └── track_codec.rs       # mã hóa track Lavaplayer
│   ├── audio/
│   │   ├── decoder.rs           # symphonia + Opus decoder + rubato
│   │   ├── encoder.rs           # audiopus, khung 20 ms
│   │   ├── crypto.rs            # RTP + AES-GCM / XChaCha20-Poly1305
│   │   ├── voice_gateway.rs     # Discord voice gateway v8 + UDP
│   │   ├── dave.rs              # máy trạng thái DAVE
│   │   └── playback.rs          # thread decoder + luồng gửi
│   ├── cache/
│   │   ├── index.rs             # tầng SQLite (sqlx)
│   │   ├── manager.rs           # hit / cold pull / miss, tải vào cache
│   │   ├── lru.rs               # worker dọn LRU
│   │   └── progressive.rs       # đọc file đang tải
│   ├── onedrive/
│   │   ├── auth.rs              # refresh token + device code login
│   │   ├── client.rs            # Graph API, upload session, download
│   │   ├── quickxor.rs          # QuickXorHash
│   │   └── sync_worker.rs       # backup định kỳ, snapshot, khôi phục
│   ├── player/                  # Session (resume) và Player
│   └── source/                  # yt-dlp, HTTP, loadtracks
└── tests/                       # test tích hợp (mock Graph, mock Discord voice)
```

## Yêu cầu

- Rust 1.88 trở lên.
- `cmake` và trình biên dịch C (build libopus tĩnh nếu hệ thống không có `libopus`).
- `yt-dlp` trong `PATH` (hoặc khai báo `sources.ytdlp.path`).

## Chạy nhanh

Hướng dẫn đầy đủ về triển khai và đấu nối bot nằm trong [docs/HUONG_DAN.md](docs/HUONG_DAN.md).

Cách chạy giống Lavalink: đặt file `rust-link` và `application.yml` chung một thư mục rồi chạy `./rust-link`. Không có `--config`, node tìm lần lượt `application.yml`, `application.yaml`, `config/application.yml` trong thư mục hiện tại, sau đó tới thư mục chứa file chạy. File `application.yml` của Lavalink v4 cũng dùng được.

```bash
cargo build --release
```

```bash
./target/release/rust-link --config config/application.yml
```

Kiểm tra cấu hình mà không khởi động node:

```bash
./target/release/rust-link check-config
```

Client Lavalink trỏ tới `ws://HOST:2333/v4/websocket` với password trong `server.password`. Khi gửi voice state, client nên kèm `voice.channelId` (Lavalink 4.1+ đã làm vậy) để node bật được DAVE.

## Đóng gói và triển khai

- **File nén chạy ngay:** `scripts/package.sh` build file chạy tĩnh (musl) và tạo `dist/rust-link-<version>-linux-x86_64.tar.gz` gồm file chạy, `application.yml`, mẫu systemd và tài liệu.
- **systemd:** mẫu unit ở `packaging/rust-link.service`.
- **Docker:** `docker compose up -d --build`. Image có sẵn yt-dlp và Deno, dữ liệu nằm trên volume `/data`.

## Cấu hình

Xem chú thích trong `config/application.yml`. Các khóa quan trọng:

| Khóa | Mặc định | Ý nghĩa |
|---|---|---|
| `server.password` | `youshallnotpass` | Header `Authorization` |
| `cache.directory` | `/data/cache` | Thư mục file audio |
| `cache.maxSize` | `20GiB` | Dung lượng tối đa |
| `cache.highWatermark` / `lowWatermark` | `0.90` / `0.80` | Ngưỡng bắt đầu / dừng dọn LRU |
| `cache.evictUnsynced` | `false` | Cho phép xóa file chưa backup |
| `cache.progressivePlayback` | `true` | Phát trong lúc đang tải |
| `onedrive.enabled` | `false` | Bật backup OneDrive |
| `onedrive.chunkSize` | `10MiB` | Làm tròn xuống bội số 320 KiB |
| `audio.encryptionMode` | `auto` | Chế độ mã hóa RTP |
| `audio.dave` | `true` | Quảng bá hỗ trợ DAVE |

Biến môi trường ghi đè: `RUSTLINK_SERVER_PASSWORD` (hoặc `LAVALINK_SERVER_PASSWORD`), `RUSTLINK_SERVER_PORT` (hoặc `SERVER_PORT`), `RUSTLINK_SERVER_ADDRESS` (hoặc `SERVER_ADDRESS`), `RUSTLINK_CACHE_DIR`, `RUSTLINK_CONFIG`. `RUST_LOG` ghi đè `logging.level`. Đặt `logging.file.path` để ghi thêm log ra file xoay vòng theo ngày.

## Thiết lập OneDrive

1. Vào Azure Portal, mục **App registrations**, tạo ứng dụng mới. Chọn loại tài khoản phù hợp (tài khoản Microsoft cá nhân cho OneDrive Personal).
2. Trong **Authentication**, bật **Allow public client flows**.
3. Trong **API permissions**, thêm quyền delegated của Microsoft Graph: `Files.ReadWrite` và `offline_access`.
4. Chạy đăng nhập device code, mở URL hiện ra và nhập mã:

```bash
./target/release/rust-link onedrive-login --client-id <APPLICATION_CLIENT_ID> --tenant consumers
```

5. Đặt `onedrive.enabled: true` trong `application.yml`.

Lệnh trên ghi `config/onedrive.json` (quyền 0600). Refresh token mới do Microsoft xoay vòng được lưu lại tự động. Với tài khoản công ty, dùng `--tenant organizations` hoặc GUID tenant.

## Cơ chế cache, backup và khôi phục

- **Khóa cache**: YouTube video ID; nguồn khác dùng SHA-256 của URL. File lưu tại `{directory}/{2 ký tự đầu}/{khóa}.opus`, container thật (WebM, Ogg, MP4...) được nhận diện theo magic bytes và ghi ở cột `container`.
- **Bảng `audio_cache`**: `video_id, file_path, file_size, duration_ms, last_accessed_at, onedrive_synced` cùng metadata track, `is_local`, `sha256`, trạng thái và lỗi đồng bộ.
- **Phát một bài**: có trên đĩa thì đọc trực tiếp; bị LRU dọn nhưng đã backup thì tải về từ OneDrive (kiểm tra SHA-256, lỗi thì dự phòng tải từ nguồn gốc); chưa có thì tải qua yt-dlp hoặc HTTP.
- **Worker backup**: quét các bản ghi `onedrive_synced = 0`, upload (file nhỏ dùng một lệnh PUT, file lớn dùng upload session), so `size` và `quickXorHash`, rồi đặt `onedrive_synced = 1`. Lỗi được thử lại với backoff lũy thừa. URL upload session lưu trong SQLite để resume sau khi khởi động lại.
- **Snapshot**: `VACUUM INTO` tạo bản sao nhất quán của DB, upload lên `AudioCache/_meta/cache_index.sqlite`.
- **Khôi phục thảm họa**: mất DB cục bộ và `restoreIndexOnStartup: true` thì node tải snapshot về lúc khởi động. Các bài trong đó trở thành "cold" và được kéo về khi có người phát.

## API

Chuẩn Lavalink v4 (mọi route yêu cầu header `Authorization`):

| Method | Path |
|---|---|
| GET | `/version`, `/v4/info`, `/v4/stats` |
| GET | `/v4/websocket` (header `User-Id`, `Client-Name`, `Session-Id` để resume) |
| GET | `/v4/loadtracks?identifier=...` |
| GET / POST | `/v4/decodetrack?encodedTrack=...`, `/v4/decodetracks` |
| PATCH | `/v4/sessions/{sessionId}` |
| GET | `/v4/sessions/{sessionId}/players` |
| GET / PATCH / DELETE | `/v4/sessions/{sessionId}/players/{guildId}` (PATCH hỗ trợ `?noReplace=true`) |

Identifier hỗ trợ: `ytsearch:`, `ytmsearch:`, `scsearch:`, `cachesearch:` (tìm theo tiêu đề trong cache), URL YouTube (video, shorts, playlist), SoundCloud, URL file audio hoặc radio HTTP, các URL khác qua extractor của yt-dlp, và video ID YouTube 11 ký tự.

Endpoint mở rộng cho vận hành:

| Method | Path | Tác dụng |
|---|---|---|
| GET | `/v4/rustlink/cache` | Thống kê cache và backup |
| POST | `/v4/rustlink/cache/evict` | Chạy ngay một lượt LRU |
| POST | `/v4/rustlink/cache/sync` | Kích hoạt đồng bộ OneDrive |

## Kiểm thử

```bash
cargo test
```

- Unit test: codec track (gồm chuỗi mẫu trong tài liệu Lavalink), QuickXorHash (so với cài đặt tham chiếu cấp bit), RTP/AEAD hai chế độ, resample, seek, passthrough Opus (cần `ffmpeg` có libopus, tự bỏ qua nếu thiếu), đọc lũy tiến, máy trạng thái DAVE.
- `tests/onedrive.rs`: mock Microsoft Graph; upload chunk có lỗi 500 giữa chừng, session hết hạn, hash sai, rồi trọn vòng backup, dọn LRU, kéo về từ cold storage, snapshot và khôi phục.
- `tests/voice.rs`: mock Discord voice server (WebSocket v8 + UDP); phát một bài từ cache, giải mã từng gói RTP và kiểm tra khung Opus, opcode Speaking, heartbeat, sự kiện TrackStart và TrackEnd.
- `tests/api.rs`: xác thực, định dạng lỗi, loadtracks từ cache, WebSocket, resume session.
- `tests/live_youtube.rs` (mặc định bỏ qua): YouTube thật qua yt-dlp.

```bash
RUSTLINK_TEST_YTDLP=$(which yt-dlp) cargo test --test live_youtube -- --ignored --nocapture
```

## Giới hạn hiện tại

- Chỉ áp dụng filter `volume`; các filter khác (equalizer, timescale...) được nhận, lưu và trả lại nhưng không xử lý âm thanh.
- Live stream YouTube (HLS / MPEG-TS) chưa hỗ trợ vì symphonia không demux MPEG-TS. Radio HTTP (MP3, AAC, Ogg) vẫn phát được.
- Luồng DAVE mới được kiểm thử bằng unit test với `davey`, chưa chạy với server Discord thật.
- Route planner của Lavalink chưa được triển khai.
