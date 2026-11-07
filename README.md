<div align="center">

# rust-link

**Audio node viết bằng Rust thay thế trực tiếp Lavalink v4 cho bot Discord, giữ nhạc đã phát trong cache cục bộ có backup OneDrive để lần sau không phải tải lại.**

[![Build Status](https://github.com/otkhovps-sys/rust-link/actions/workflows/ci.yml/badge.svg)](https://github.com/otkhovps-sys/rust-link/actions/workflows/ci.yml)
[![Release](https://github.com/otkhovps-sys/rust-link/actions/workflows/release.yml/badge.svg)](https://github.com/otkhovps-sys/rust-link/actions/workflows/release.yml)
[![Version](https://img.shields.io/github/v/release/otkhovps-sys/rust-link?sort=semver&label=version)](https://github.com/otkhovps-sys/rust-link/releases/latest)
[![License](https://img.shields.io/github/license/otkhovps-sys/rust-link)](LICENSE)
[![Rust](https://img.shields.io/badge/dynamic/toml?url=https%3A%2F%2Fraw.githubusercontent.com%2Fotkhovps-sys%2Frust-link%2FHEAD%2FCargo.toml&query=%24.package%5B%27rust-version%27%5D&label=rust&logo=rust&color=orange&suffix=%2B)](https://www.rust-lang.org)
[![Lavalink API](https://img.shields.io/badge/Lavalink_API-v4-5865F2?logo=discord&logoColor=white)](https://lavalink.dev/api/)
[![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20Windows%20%7C%20Docker-lightgrey)](#-cài-đặt)

[Tính năng](#-tính-năng-chính) ·
[Kiến trúc](#-kiến-trúc) ·
[Cài đặt](#-cài-đặt) ·
[Chạy nhanh](#-chạy-nhanh-và-cấu-hình) ·
[Cấu trúc mã](#-cấu-trúc-thư-mục) ·
[Đóng góp](#-roadmap-đóng-góp-và-giấy-phép) ·
[Hướng dẫn đầy đủ](docs/HUONG_DAN.md)

</div>

---

## ✨ Tính năng chính

- **Thay thế trực tiếp Lavalink v4:** cùng REST, WebSocket, định dạng lỗi, resume session và mã hóa track Lavaplayer. Bot đang chạy Shoukaku, lavalink-client, Wavelink, lavalink.py hay Lavalink4NET chỉ cần đổi host.
- **Một file chạy, không cần JVM:** file tĩnh (musl) nén còn khoảng 8 MB, RAM lúc rảnh khoảng 17 MB, có bản Linux x86_64, Linux ARM64 và Windows, cùng image Docker amd64 + arm64.
- **yt-dlp tự quản lý:** node tự tải bản yt-dlp chính thức (kiểm SHA-256) và tự cập nhật theo lịch, vì YouTube đổi liên tục và yt-dlp cũ là hỏng phát nhạc.
- **Bất đồng bộ từ đầu đến cuối:** tokio và axum xử lý mạng; mỗi bài có thread decode riêng, luồng gửi UDP giữ đúng nhịp 20 ms mà không chặn runtime.
- **Opus passthrough:** gói Opus 48 kHz của YouTube được gửi thẳng tới Discord, không decode và không encode lại khi volume 100 và không bật filter.
- **Link Spotify, Apple Music, Deezer kiểu LavaSrc:** track, album, playlist, nghệ sĩ và `spsearch:`, `amsearch:`, `dzsearch:`, `dzisrc:`. Lúc phát, node tìm bản YouTube theo ISRC rồi theo tên bài, có so độ dài, và nhớ kết quả trong SQLite. Từ tháng 3/2026, Spotify không còn trả danh sách bài của playlist cho app ở Development Mode; bài, album và tìm kiếm Spotify vẫn dùng được (chi tiết trong [HUONG_DAN.md](docs/HUONG_DAN.md#8-link-spotify-apple-music-deezer)).
- **Phát trực tiếp khi không nên cache:** live YouTube và radio `.m3u8` qua HLS (tự tách audio từ MPEG-TS); bài dài hơn 2 giờ hoặc file quá lớn đọc bằng HTTP Range, vẫn tua được.
- **Nhanh hơn ở những chỗ bot hay chờ:** kết quả tìm kiếm được nhớ 6 giờ, playlist được tải trước vài bài kế tiếp, filter và âm lượng có hiệu lực trong khoảng 20 ms thay vì chờ bộ đệm 0,4 giây, yt-dlp dùng bản thư mục nên mỗi lần gọi nhanh gấp đôi (đo được 0,45 s so với 1 s).
- **Một luồng live, nhiều server:** các server phát cùng một live stream với cùng filter và âm lượng dùng chung một lần tải, giải mã và mã hóa, nên bot radio 24/7 không tốn thêm CPU khi có thêm server.
- **Đủ 10 filter Lavalink:** volume, equalizer 15 band, karaoke, timescale (nightcore, vaporwave), tremolo, vibrato, rotation (8D), distortion, channelMix, lowPass. DSP viết thuần Rust, bật cả 10 filter tốn khoảng 1% một nhân CPU cho mỗi player.
- **Voice Discord đời mới:** Voice Gateway v8, mã hóa `aead_aes256_gcm_rtpsize` hoặc `aead_xchacha20_poly1305_rtpsize`, mã hóa đầu cuối DAVE.
- **Cache tiết kiệm băng thông:** phát ngay khi file còn đang tải, gộp các lượt tải trùng nhau, dọn LRU theo ngưỡng 90% và 80%, không xóa file đang phát.
- **Backup OneDrive chịu lỗi:** upload chia chunk 320 KiB và tiếp tục được sau khi mất mạng hay khởi động lại, kiểm `quickXorHash`, snapshot SQLite bằng `VACUUM INTO`, tự kéo file về khi bài đã bị dọn khỏi đĩa.
- **Plugin viết bằng ngôn ngữ nào cũng được:** plugin là chương trình riêng (Python, Node.js, Go...) nói chuyện với node qua JSON trên stdin/stdout. Plugin thêm được nguồn nhạc (tiền tố, URL), chọn nơi phát (URL, file, bài khác), nhận sự kiện websocket, thêm route REST và gọi lại REST của node. Node tự chạy lại plugin bị crash; `rust-link plugin new <tên>` tạo sẵn khung kèm SDK. Xem [docs/PLUGINS.md](docs/PLUGINS.md).
- **Dùng lại cấu hình cũ:** đọc được nguyên file `application.yml` của Lavalink v4.

## 🧭 Kiến trúc

Luồng dữ liệu từ lệnh phát của bot tới gói UDP gửi Discord:

```mermaid
flowchart LR
    Bot["Bot Discord<br/>client Lavalink v4"] -- "REST + WebSocket /v4" --> API["api/<br/>axum"]
    API --> Player["player/<br/>Session, Player"]
    Player --> Playback["audio/playback"]
    Playback --> Cache{"cache/<br/>CacheManager"}
    Index[("SQLite<br/>index.db")] --- Cache
    Cache -- "hit" --> Disk[("Đĩa cục bộ<br/>/data/cache")]
    Cache -- "cold" --> OneDrive[("OneDrive<br/>AudioCache/")]
    Cache -- "miss" --> Sources["source/<br/>yt-dlp, HTTP"]
    Mirror["source/mirror<br/>Spotify, Apple Music, Deezer"] -- "ISRC / tên bài" --> Sources
    Plugins["plugin/<br/>tiến trình Python, Node..."] -- "loadItem, resolve" --- Sources
    API -- "route của plugin" --> Plugins
    Player -. "sự kiện" .-> Plugins
    Playback -- "Spotify, Apple, Deezer" --> Mirror
    Sources --> Disk
    OneDrive --> Disk
    Disk --> Decoder
    Playback -- "live, bài dài" --> Direct["source/hls, remote<br/>HLS, HTTP Range"]
    Direct --> Decoder

    subgraph Worker["Thread decoder"]
        Decoder["symphonia<br/>+ libopus"] --> Resample["rubato<br/>48 kHz stereo"]
        Resample --> Filters["audio/filters<br/>10 filter Lavalink"]
        Filters --> Encoder["audiopus<br/>Opus 20 ms"]
    end

    Encoder --> Sender["Luồng gửi 20 ms<br/>tokio"]
    Decoder -. "Opus passthrough" .-> Sender
    Sender --> Crypto["DAVE E2EE<br/>RTP + AEAD"]
    Crypto -- "UDP" --> Discord(("Discord Voice"))

    LRU["cache/lru"] -. "dọn file cũ" .-> Disk
    Sync["onedrive/sync_worker"] -. "backup + snapshot" .-> OneDrive
```

Vòng đời của một file trong cache:

```mermaid
stateDiagram-v2
    direction LR
    [*] --> Downloading: miss, tải qua yt-dlp hoặc HTTP
    Downloading --> Local: tải xong, ghi index
    Local --> Local: phát lại từ đĩa
    Local --> Synced: sync_worker upload lên OneDrive
    Synced --> Cold: LRU xóa bản cục bộ
    Cold --> Synced: phát lại, kéo từ OneDrive về
```

## 📦 Cài đặt

### Yêu cầu tối thiểu

| Thành phần | Yêu cầu | Ghi chú |
|---|---|---|
| Hệ điều hành | Linux x86_64 hoặc ARM64 (mọi distro), Windows 10/11 x86_64 | Nền tảng khác thì tự build |
| [yt-dlp](https://github.com/yt-dlp/yt-dlp) | không cần cài | Node tự tải và tự cập nhật (`sources.ytdlp.path: managed`) |
| [Deno](https://deno.com) | 2.x | JavaScript runtime mà yt-dlp cần cho YouTube |
| Docker | Engine 20.10+, Compose v2 | Chỉ khi chạy bằng Docker; image có sẵn Deno |
| Rust | 1.88+, kèm `cmake` và trình biên dịch C | Chỉ khi build từ mã nguồn |
| Client bot | Thư viện hỗ trợ Lavalink v4 | Nên gửi `voice.channelId` (Lavalink 4.1+) để bật DAVE |

### Cách 1: file chạy dựng sẵn

Linux, tải gói từ [Releases](https://github.com/otkhovps-sys/rust-link/releases) bằng GitHub CLI (máy ARM64 thay `linux-x86_64` bằng `linux-aarch64`):

```bash
gh release download --repo otkhovps-sys/rust-link --pattern '*linux-x86_64.tar.gz'
```

```bash
tar -xzf rust-link-*-linux-x86_64.tar.gz && cd rust-link-*-linux-x86_64
```

Windows: tải `rust-link-<version>-windows-x86_64.zip`, giải nén rồi bấm đúp `start.bat`. Lần chạy đầu node tự tải `yt-dlp.exe`.

Mỗi gói có kèm file `.sha256` để kiểm tra toàn vẹn.

### Cách 2: Docker

Image dựng sẵn (amd64 + arm64). Repo private thì `docker login ghcr.io` trước:

```bash
docker run -d --name rust-link -p 2333:2333 -v rust-link-data:/data ghcr.io/otkhovps-sys/rust-link:latest
```

Hoặc build từ mã nguồn:

```bash
git clone https://github.com/otkhovps-sys/rust-link.git && cd rust-link
```

```bash
docker compose up -d --build
```

Cache, chỉ mục SQLite, `onedrive.json` và yt-dlp nằm trên volume `/data`.

### Cách 3: Cargo

```bash
cargo install --git https://github.com/otkhovps-sys/rust-link.git --locked
```

Lệnh trên cài `rust-link` vào `~/.cargo/bin`. Hãy chép `config/application.yml` về máy và sửa `cache.directory` sang thư mục có quyền ghi.

### Cách 4: build từ mã nguồn

```bash
cargo build --release --locked
```

```bash
scripts/package.sh
```

`scripts/package.sh` tạo gói `dist/rust-link-<version>-linux-x86_64.tar.gz` giống bản trên Releases. Đặt `TARGET=x86_64-pc-windows-gnu` để tạo gói Windows, `TARGET=aarch64-unknown-linux-musl` để tạo gói ARM64 (cần cargo-zigbuild).

## 🚀 Chạy nhanh và cấu hình

### Chạy thử trong 30 giây

1. Chạy node ngay trong thư mục giải nén. Giống Lavalink, node tự đọc `application.yml` nằm cạnh file chạy; lần đầu nó tải yt-dlp vào `bin/` trong vài giây:

```bash
./rust-link
```

2. Kiểm tra node đã nhận kết nối (password mặc định là `youshallnotpass`):

```bash
curl -H "Authorization: youshallnotpass" http://localhost:2333/version
```

3. Thử tìm một bài trên YouTube:

```bash
curl -G -H "Authorization: youshallnotpass" --data-urlencode "identifier=ytsearch:lofi hip hop" http://localhost:2333/v4/loadtracks
```

4. Trỏ bot vào node, ví dụ với discord.js và Shoukaku:

```js
const { Shoukaku, Connectors } = require('shoukaku');

const shoukaku = new Shoukaku(new Connectors.DiscordJS(client), [
  { name: 'rust-link', url: 'localhost:2333', auth: 'youshallnotpass' },
]);
```

Bật filter vẫn theo API Lavalink, ví dụ hiệu ứng nightcore:

```js
await player.setFilters({ timescale: { speed: 1.2, pitch: 1.2, rate: 1.0 } });
```

Cách khai báo node cho lavalink-client, Wavelink, lavalink.py và các bước chạy bằng systemd nằm trong [docs/HUONG_DAN.md](docs/HUONG_DAN.md).

### Biến môi trường

| Biến | Ghi đè | Ví dụ |
|---|---|---|
| `RUSTLINK_CONFIG` | Đường dẫn file cấu hình (tương đương `--config`) | `/etc/rust-link/application.yml` |
| `RUSTLINK_SERVER_PASSWORD` hoặc `LAVALINK_SERVER_PASSWORD` | `server.password` | `doi-mat-khau-di` |
| `RUSTLINK_SERVER_PORT` hoặc `SERVER_PORT` | `server.port` | `2333` |
| `RUSTLINK_SERVER_ADDRESS` hoặc `SERVER_ADDRESS` | `server.address` | `0.0.0.0` |
| `RUSTLINK_CACHE_DIR` | `cache.directory` (và `cache.database` nếu để mặc định) | `/srv/rust-link/cache` |
| `RUST_LOG` | `logging.level` | `info,rust_link=debug` |

### File `application.yml`

Node tìm file cấu hình theo thứ tự: `--config`, `application.yml` hoặc `application.yaml` trong thư mục hiện tại, `config/application.yml`, rồi thư mục chứa file chạy. Mọi khóa đều có giá trị mặc định; [config/application.yml](config/application.yml) có chú thích đầy đủ.

| Khóa | Mặc định | Ý nghĩa |
|---|---|---|
| `server.port` | `2333` | Cổng REST và WebSocket |
| `server.password` | `youshallnotpass` | Giá trị header `Authorization` |
| `lavalink.playerUpdateInterval` | `5` | Số giây giữa các op `playerUpdate` |
| `lavalink.filters.<tên>` | `true` | Tắt một filter; client gửi filter đã tắt sẽ nhận lỗi 400 |
| `audio.opusPassthrough` | `true` | Gửi thẳng gói Opus của nguồn khi không cần xử lý |
| `audio.frameBufferMs` | `400` | Bộ đệm giữa decoder và luồng gửi (đổi filter, âm lượng vẫn có hiệu lực ngay) |
| `audio.shareLiveStreams` | `true` | Server cùng phát một live stream dùng chung một lần giải mã |
| `audio.encryptionMode` | `auto` | `auto`, `aead_aes256_gcm_rtpsize` hoặc `aead_xchacha20_poly1305_rtpsize` |
| `audio.dave` | `true` | Hỗ trợ mã hóa đầu cuối DAVE |
| `sources.ytdlp.path` | `managed` (gói và Docker) | `managed`: node tự tải yt-dlp; hoặc đường dẫn tới yt-dlp có sẵn |
| `sources.ytdlp.autoUpdate` / `updateIntervalHours` | `true` / `24` | Tự cập nhật yt-dlp theo lịch |
| `sources.spotify.clientId` / `clientSecret` | trống | Bật link Spotify (app miễn phí tại developer.spotify.com) |
| `sources.appleMusic.mediaApiToken` | trống | Bật link Apple Music (MusicKit developer token) |
| `sources.deezer.enabled` | `true` | Link Deezer, không cần tài khoản |
| `sources.mirrorProviders` | ISRC rồi tên bài | Cách tìm bản YouTube cho Spotify, Apple Music, Deezer |
| `cache.directory` | `/data/cache` | Thư mục chứa file audio |
| `cache.maxSize` | `20GiB` | Dung lượng tối đa của cache |
| `cache.highWatermark` / `cache.lowWatermark` | `0.90` / `0.80` | Ngưỡng bắt đầu và dừng dọn LRU |
| `cache.evictUnsynced` | `false` | Cho phép xóa file chưa backup |
| `cache.progressivePlayback` | `true` | Phát trong lúc file còn đang tải |
| `cache.maxTrackDurationSecs` | `7200` | Bài dài hơn phát thẳng từ nguồn, không lưu cache (0 = không giới hạn) |
| `cache.prefetchTracks` | `3` | Số bài tải trước khi load playlist và khi phát một bài trong playlist (0 = tắt) |
| `sources.loadCacheTtlSecs` | `21600` | Thời gian dùng lại kết quả tìm kiếm và link nhạc (0 = tắt) |
| `sources.youtubeMixLimit` | `25` | Số bài lấy từ link YouTube Mix `list=RD...` (0 = chỉ phát video trong link) |
| `onedrive.enabled` | `false` | Bật backup OneDrive |
| `onedrive.remoteFolder` | `AudioCache` | Thư mục trên OneDrive |
| `onedrive.chunkSize` | `10MiB` | Kích thước chunk upload, làm tròn xuống bội số 320 KiB |
| `plugins.directory` | `./plugins` | Thư mục plugin, mỗi thư mục con có `plugin.yml` là một plugin |
| `plugins.<tên>` | trống | Cấu hình riêng gửi cho plugin cùng tên |
| `plugins.requestTimeoutSecs` | `10` | Thời gian tối đa cho một lần gọi plugin |
| `logging.file.path` | `./logs/` | Ghi thêm log ra file xoay vòng theo ngày |

<details>
<summary><b>Bật backup OneDrive</b></summary>

1. Trong Azure Portal, mục **App registrations**, tạo ứng dụng mới (chọn tài khoản Microsoft cá nhân nếu dùng OneDrive Personal).
2. Mục **Authentication**: bật **Allow public client flows**.
3. Mục **API permissions**: thêm quyền delegated `Files.ReadWrite` và `offline_access` của Microsoft Graph.
4. Đăng nhập bằng device code, mở URL hiện ra và nhập mã:

```bash
./rust-link onedrive-login --client-id <APPLICATION_CLIENT_ID> --tenant consumers
```

5. Đặt `onedrive.enabled: true` trong `application.yml` rồi khởi động lại node.

Lệnh đăng nhập ghi `onedrive.json` với quyền 0600, và refresh token mới được lưu lại mỗi lần Microsoft xoay vòng. Tài khoản công ty dùng `--tenant organizations` hoặc GUID tenant.

</details>

<details>
<summary><b>API</b></summary>

Chuẩn Lavalink v4, mọi route yêu cầu header `Authorization`:

| Method | Path |
|---|---|
| GET | `/version`, `/v4/info`, `/v4/stats` |
| GET | `/v4/websocket` (header `User-Id`, `Client-Name`, `Session-Id` để resume) |
| GET | `/v4/loadtracks?identifier=...` |
| GET, POST | `/v4/decodetrack?encodedTrack=...`, `/v4/decodetracks` |
| PATCH | `/v4/sessions/{sessionId}` |
| GET | `/v4/sessions/{sessionId}/players` |
| GET, PATCH, DELETE | `/v4/sessions/{sessionId}/players/{guildId}` (PATCH hỗ trợ `?noReplace=true`) |

Identifier hỗ trợ: `ytsearch:`, `ytmsearch:`, `scsearch:`, `spsearch:`, `amsearch:`, `dzsearch:`, `dzisrc:`, `cachesearch:` (tìm trong cache), URL YouTube (video, shorts, playlist, Mix, live), Spotify, Apple Music, Deezer, SoundCloud, file audio, radio HTTP và `.m3u8`, URL khác qua extractor của yt-dlp, và video ID YouTube 11 ký tự.

Endpoint mở rộng cho vận hành:

| Method | Path | Tác dụng |
|---|---|---|
| GET | `/v4/rustlink/cache` | Thống kê cache và backup |
| POST | `/v4/rustlink/cache/evict` | Chạy ngay một lượt LRU |
| POST | `/v4/rustlink/cache/sync` | Kích hoạt đồng bộ OneDrive |
| GET | `/v4/rustlink/plugins` | Trạng thái và khả năng của từng plugin |

Plugin có thể thêm tiền tố identifier và route REST riêng, ví dụ `radio:lofi` và `GET /v4/radio/stations` của plugin mẫu.

</details>

## 🗂 Cấu trúc thư mục

```text
.
├── src/
│   ├── main.rs              # CLI: serve | onedrive-login | check-config | update-ytdlp | plugin new/list
│   ├── config.rs            # application.yml, khóa tương thích Lavalink, biến môi trường
│   ├── node.rs, stats.rs    # khởi tạo node, số liệu op "stats"
│   ├── api/                 # REST + WebSocket Lavalink v4, định dạng lỗi, codec track Lavaplayer
│   ├── player/              # Session (resume, backlog event) và Player theo guild
│   ├── audio/
│   │   ├── decoder.rs       # symphonia + decoder Opus (libopus) + resample rubato
│   │   ├── filters/         # 10 filter Lavalink: effects.rs, timescale.rs (WSOLA)
│   │   ├── encoder.rs       # Opus 20 ms
│   │   ├── playback.rs      # thread decoder và luồng gửi 20 ms
│   │   ├── voice_gateway.rs # Discord Voice Gateway v8 + UDP
│   │   ├── crypto.rs        # RTP + AES-GCM / XChaCha20-Poly1305
│   │   └── dave.rs          # mã hóa đầu cuối DAVE
│   ├── cache/               # chỉ mục SQLite, hit/cold/miss, phát thẳng, worker LRU
│   ├── onedrive/            # OAuth2, Graph API, upload session, QuickXorHash, worker backup
│   ├── plugin/              # chạy plugin (JSON-RPC qua stdin/stdout), tự chạy lại, route, sự kiện
│   └── source/
│       ├── mod.rs           # loadtracks, chọn nguồn, phát thẳng (open_direct), ghép mirror
│       ├── ytdlp.rs         # gọi yt-dlp; ytdlp_update.rs: tự tải và cập nhật yt-dlp
│       ├── http.rs          # file audio, radio HTTP
│       ├── hls.rs, mpegts.rs # HLS (live, .m3u8) và tách audio MPEG-TS
│       ├── remote.rs        # đọc HTTP Range cho bài phát thẳng
│       └── mirror/          # Spotify, Apple Music, Deezer kiểu LavaSrc
├── sdk/                     # SDK plugin Python và Node.js, mẫu cho `rust-link plugin new`
├── examples/plugins/        # plugin mẫu: radio (Python), local-files (Node.js)
├── tests/                   # test tích hợp: mock Discord voice, Microsoft Graph, Spotify/Apple/Deezer, HLS, plugin
├── migrations/              # schema SQLite (nhúng vào file chạy)
├── config/application.yml   # cấu hình mẫu có chú thích
├── docs/HUONG_DAN.md        # hướng dẫn triển khai và đấu nối bot
├── docs/PLUGINS.md          # viết plugin: SDK, giao thức, vòng đời
├── packaging/               # start.bat, unit systemd, Dockerfile.release, mẫu ghi chú phát hành
├── scripts/package.sh       # đóng gói tar.gz (Linux x86_64, ARM64) và zip (Windows)
├── Dockerfile, docker-compose.yml
└── .github/workflows/       # ci.yml (lint, test, MSRV), release.yml (Releases + image ghcr.io)
```

## 🛣 Roadmap, đóng góp và giấy phép

### Roadmap

- [x] REST và WebSocket Lavalink v4, resume session
- [x] Discord Voice Gateway v8, mã hóa rtpsize, DAVE
- [x] Cache cục bộ với LRU, backup và khôi phục qua OneDrive
- [x] Đủ 10 filter của Lavalink
- [x] Gói Linux, Windows, Docker và phát hành tự động lên Releases
- [x] Live stream YouTube (HLS / MPEG-TS), phát thẳng bài quá dài
- [x] Gói Linux ARM64, image Docker amd64 + arm64 trên ghcr.io
- [x] Tự tải và tự cập nhật yt-dlp
- [x] Link Spotify, Apple Music, Deezer kiểu LavaSrc
- [ ] Route planner (xoay vòng IPv6)
- [ ] Endpoint Prometheus `/metrics`
- [ ] Số liệu CPU và RAM trong `stats` trên Windows
- [x] Filter và âm lượng có hiệu lực ngay, không chờ bộ đệm
- [x] Cache kết quả tìm kiếm, tải trước playlist, live stream dùng chung, yt-dlp bản thư mục
- [ ] Script đo so sánh CPU, RAM, độ trễ với Lavalink
- [x] Plugin ngoài tiến trình, viết bằng ngôn ngữ bất kỳ (nguồn nhạc, route, sự kiện)
- [ ] Plugin xử lý âm thanh (`pluginFilters`)

Hiện `pluginFilters` chỉ được lưu và trả lại: rust-link không chạy plugin JVM của Lavalink, và plugin của rust-link chưa xử lý âm thanh trực tiếp.

### Quy chuẩn commit

Commit theo [Conventional Commits](https://www.conventionalcommits.org/):

| Loại | Dùng khi | Ví dụ |
|---|---|---|
| `feat` | Thêm tính năng | `feat(filters): add echo plugin filter` |
| `fix` | Sửa lỗi | `fix(voice): preserve Discord voice endpoint ports` |
| `perf` | Tăng hiệu năng | `perf(cache): batch LRU deletions` |
| `refactor` | Đổi cấu trúc mã, không đổi hành vi | `refactor(player): split voice handling` |
| `docs` | Tài liệu | `docs: describe OneDrive setup` |
| `test` | Thêm hoặc sửa test | `test(api): cover filter validation` |
| `build`, `ci`, `chore` | Build, workflow, việc lặt vặt | `ci: cache cargo registry` |

Thay đổi phá vỡ tương thích thêm `!` sau loại (`feat!:`) và ghi `BREAKING CHANGE:` ở chân commit.

### Quy trình Pull Request

1. Tạo nhánh từ nhánh mặc định, đặt tên theo loại thay đổi: `feat/...`, `fix/...`, `docs/...`.
2. Chạy đủ ba bước kiểm tra mà CI cũng chạy (CI dùng clippy của một bản Rust cố định, xem `LINT_TOOLCHAIN` trong `ci.yml`):

```bash
cargo fmt --all -- --check
```

```bash
cargo clippy --all-targets --locked -- -D warnings
```

```bash
cargo test --locked
```

3. Mỗi PR một chủ đề. Mô tả thay đổi, lý do và cách đã kiểm thử.
4. Thêm khóa cấu hình thì cập nhật cả `config/application.yml` và `docs/HUONG_DAN.md`. Sửa hành vi thì kèm test, ưu tiên test tích hợp với mock sẵn có trong `tests/common`.
5. Không commit bí mật: `onedrive.json`, cookie của yt-dlp, token bot.

Phát hành: tăng `version` trong `Cargo.toml` theo [SemVer](https://semver.org/), cập nhật `packaging/release-notes.md`, rồi push tag `v<version>` hoặc chạy workflow **Release** trong tab Actions.

Test YouTube thật mặc định bị bỏ qua, chạy riêng bằng:

```bash
RUSTLINK_TEST_YTDLP=$(which yt-dlp) cargo test --test live_youtube -- --ignored --nocapture
```

### Giấy phép

Phát hành theo giấy phép [MIT](LICENSE). Các thư viện phụ thuộc giữ giấy phép riêng của chúng (ví dụ libopus dùng BSD-3-Clause).
