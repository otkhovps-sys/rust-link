# Hướng dẫn triển khai và đấu nối rust-link

rust-link là một audio node nói chuẩn **Lavalink v4**. Bot Discord đang dùng Lavalink chỉ cần trỏ sang host, port và password của rust-link là chạy, không phải sửa code bot.

## 1. So sánh nhanh với Lavalink

| | Lavalink | rust-link |
|---|---|---|
| File chạy | `Lavalink.jar` | `rust-link` (Linux x86_64) hoặc `rust-link.exe` (Windows x86_64), mỗi bản là một file duy nhất |
| Cần cài thêm | Java 17+ | `yt-dlp` (khuyến nghị thêm Deno) |
| File cấu hình | `application.yml` cạnh file chạy | `application.yml` cạnh file chạy |
| Lệnh chạy | `java -jar Lavalink.jar` | `./rust-link` hoặc bấm đúp `start.bat` |
| Port / password mặc định | `2333` / `youshallnotpass` | `2333` / `youshallnotpass` |
| WebSocket | `/v4/websocket` | `/v4/websocket` |
| Thư mục log | `./logs/` | `./logs/` |
| Plugin Java (LavaSrc, youtube-plugin...) | có | không dùng được |
| Cache audio + backup OneDrive | không | có |

## 2. Chuẩn bị máy chủ

- **Hệ điều hành:** Linux x86_64 bất kỳ, hoặc Windows 10/11 x86_64. Bản Linux được link tĩnh nên không phụ thuộc phiên bản glibc, đã chạy thử trên Ubuntu 18.04, Debian 11 và Alpine 3.20. Bản Windows mới được chạy thử bằng Wine, chưa thử trên máy Windows thật. macOS cần tự build từ mã nguồn.
- **yt-dlp:** bắt buộc để tìm và tải nhạc YouTube, SoundCloud. Bản standalone không cần Python:

```bash
sudo curl -L https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux -o /usr/local/bin/yt-dlp
```

```bash
sudo chmod a+rx /usr/local/bin/yt-dlp
```

- **Deno (khuyến nghị):** yt-dlp bản mới cần một JavaScript runtime để lấy đủ định dạng YouTube. Cài theo hướng dẫn tại deno.com và đặt file `deno` vào `/usr/local/bin`.
- **Mạng:** mở cổng TCP `2333` cho bot. Node cần gửi UDP ra ngoài tới voice server của Discord.

Cập nhật yt-dlp định kỳ, vì YouTube thay đổi thường xuyên:

```bash
sudo yt-dlp -U
```

## 3. Cách 1: chạy từ file nén (giống Lavalink.jar)

Tải bản dựng sẵn tại trang Releases: https://github.com/otkhovps-sys/rust-link/releases

| File | Dành cho |
|---|---|
| `rust-link-<version>-linux-x86_64.tar.gz` | Linux x86_64 |
| `rust-link-<version>-windows-x86_64.zip` | Windows x86_64 |
| `*.sha256` | Mã kiểm tra toàn vẹn của file tải về |

Kiểm tra file tải về trên Linux:

```bash
sha256sum -c rust-link-0.1.1-linux-x86_64.tar.gz.sha256
```

Giải nén gói `rust-link-<version>-linux-x86_64.tar.gz`:

```bash
tar -xzf rust-link-0.1.1-linux-x86_64.tar.gz
```

```bash
cd rust-link-0.1.1-linux-x86_64
```

Nội dung thư mục:

```text
rust-link-0.1.1-linux-x86_64/
├── rust-link               # file chạy duy nhất
├── application.yml         # cấu hình, giống application.yml của Lavalink
├── onedrive.example.json   # mẫu credentials OneDrive
├── rust-link.service       # mẫu systemd
├── HUONG_DAN.md            # tài liệu này
└── README.md
```

Đổi password trong `application.yml`, mục `server.password`, rồi chạy:

```bash
./rust-link
```

Khi thấy dòng `rust-link is ready to accept connections`, node đã sẵn sàng. Cache audio nằm ở `./data/cache`, log nằm ở `./logs/`, cùng thư mục với file chạy.

Một số lệnh hữu ích:

```bash
./rust-link check-config
```

```bash
./rust-link --config /duong/dan/khac/application.yml
```

### Chạy nền bằng systemd

```bash
sudo useradd -r -d /opt/rust-link -s /usr/sbin/nologin rustlink
```

```bash
sudo cp -r rust-link-0.1.1-linux-x86_64 /opt/rust-link
```

```bash
sudo chown -R rustlink: /opt/rust-link
```

```bash
sudo cp /opt/rust-link/rust-link.service /etc/systemd/system/
```

```bash
sudo systemctl daemon-reload
```

```bash
sudo systemctl enable --now rust-link
```

Xem log:

```bash
journalctl -u rust-link -f
```

### Chạy trên Windows

1. Giải nén `rust-link-<version>-windows-x86_64.zip`, ví dụ vào `C:\rust-link`.
2. Tải `yt-dlp.exe` từ https://github.com/yt-dlp/yt-dlp/releases và đặt cùng thư mục với `rust-link.exe`, hoặc vào một thư mục trong PATH. Nên cài thêm Deno.
3. Mở `application.yml` bằng Notepad, đổi `server.password`.
4. Bấm đúp `start.bat`. Cửa sổ dòng lệnh hiện dòng `rust-link is ready to accept connections` là node đã sẵn sàng.

Trong gói Windows, cache nằm ở `data\cache` và log ở `logs\`, cạnh `rust-link.exe`. Khi Windows Firewall hỏi, chọn **Allow access** để bot kết nối được vào cổng 2333.

Nếu `yt-dlp.exe` nằm ở chỗ khác, khai báo đường dẫn đầy đủ trong `application.yml`, dùng dấu `/` hoặc đặt trong nháy đơn:

```yaml
sources:
  ytdlp:
    path: 'C:\tools\yt-dlp.exe'
```

Trên Windows, phần `stats` báo CPU và RAM bằng 0 vì phần đo hiện chỉ hỗ trợ Linux. Phát nhạc không bị ảnh hưởng.

## 4. Cách 2: chạy bằng Docker

Từ thư mục mã nguồn:

```bash
docker compose up -d --build
```

Image đã có sẵn yt-dlp và Deno, chạy bằng user không phải root (uid 322, giống image Lavalink). Cache, chỉ mục SQLite và `onedrive.json` nằm trên volume `/data`. Đổi password qua biến `RUSTLINK_SERVER_PASSWORD` trong `docker-compose.yml`.

Muốn dùng file cấu hình riêng, bỏ comment dòng mount `application.yml` trong `docker-compose.yml`. File đó phải để `cache.directory: /data/cache` và `onedrive.credentialsFile: /data/onedrive.json`.

Đăng nhập OneDrive trong container:

```bash
docker compose run --rm rust-link onedrive-login --client-id <APPLICATION_CLIENT_ID>
```

## 5. Cách 3: tự build và đóng gói

Cần Rust 1.88+, `cmake`, `musl-tools` và target musl:

```bash
rustup target add x86_64-unknown-linux-musl
```

```bash
scripts/package.sh
```

Kết quả nằm trong `dist/`: file `rust-link-<version>-linux-x86_64.tar.gz` và file `.sha256` đi kèm.

Bản Windows được cross-compile từ Linux bằng MinGW:

```bash
rustup target add x86_64-pc-windows-gnu
```

```bash
TARGET=x86_64-pc-windows-gnu scripts/package.sh
```

### Phát hành lên GitHub Releases

Workflow `.github/workflows/release.yml` chạy test, build hai gói Linux và Windows rồi đăng lên Releases với tag `v<version>` lấy từ `Cargo.toml`. Có hai cách kích hoạt:

- **Thủ công:** vào tab **Actions** trên GitHub, chọn workflow **Release**, bấm **Run workflow**. Tag được tạo trên commit đã chọn.
- **Bằng tag:** push tag khớp phiên bản, ví dụ `v0.1.1`.

```bash
git tag v0.1.1
```

```bash
git push origin v0.1.1
```

Workflow từ chối chạy nếu Release cho phiên bản đó đã tồn tại. Muốn phát hành bản mới, tăng `version` trong `Cargo.toml` trước.

## 6. Dùng lại application.yml của Lavalink

rust-link đọc được file `application.yml` của Lavalink v4. Khóa nào rust-link không có thì được bỏ qua, nên bạn có thể chép nguyên file cũ sang.

| Khóa Lavalink | Khóa rust-link tương ứng |
|---|---|
| `server.port`, `server.address` | giống hệt |
| `lavalink.server.password` | `server.password` |
| `lavalink.server.sources.youtube` hoặc `plugins.youtube.enabled` | `sources.youtube` |
| `lavalink.server.sources.soundcloud`, `...http` | `sources.soundcloud`, `sources.http` |
| `lavalink.server.playerUpdateInterval` | `lavalink.playerUpdateInterval` |
| `lavalink.server.trackStuckThresholdMs` | `lavalink.trackStuckThresholdMs` |
| `lavalink.server.opusEncodingQuality` | `audio.opusComplexity` |
| `lavalink.server.bufferDurationMs` | `audio.frameBufferMs` |
| `logging.level.root`, `logging.level.lavalink` | `logging.level` |
| `logging.file.path` | `logging.file.path` |

Nếu một file có cả hai kiểu khóa, khóa của rust-link được ưu tiên. File của Lavalink không có mục `cache`, nên cache sẽ nằm ở `/data/cache` mặc định. Hãy thêm mục `cache` nếu user chạy node không có quyền ghi vào đó.

Biến môi trường giống Lavalink cũng dùng được: `LAVALINK_SERVER_PASSWORD`, `SERVER_PORT`, `SERVER_ADDRESS`.

## 7. Đấu nối bot

Thông số chung cho mọi thư viện client Lavalink v4:

| Thông số | Giá trị |
|---|---|
| Host | IP hoặc tên miền của máy chạy rust-link |
| Port | `2333` |
| Password | giá trị `server.password` |
| Secure / SSL | `false` |

Các đoạn mã dưới đây chỉ minh họa phần khai báo node. Phần còn lại của bot làm theo tài liệu từng thư viện. Các đoạn mã này chưa được chạy thử với từng thư viện.

**discord.js + Shoukaku**

```js
const { Shoukaku, Connectors } = require('shoukaku');

const nodes = [{ name: 'rust-link', url: 'localhost:2333', auth: 'youshallnotpass' }];
const shoukaku = new Shoukaku(new Connectors.DiscordJS(client), nodes);

// Phát nhạc
const player = await shoukaku.joinVoiceChannel({ guildId, channelId, shardId: 0 });
const result = await player.node.rest.resolve('ytsearch:never gonna give you up');
await player.playTrack({ track: { encoded: result.data[0].encoded } });
```

**discord.js + lavalink-client**

```js
const { LavalinkManager } = require('lavalink-client');

client.lavalink = new LavalinkManager({
  nodes: [{ id: 'rust-link', host: 'localhost', port: 2333, authorization: 'youshallnotpass' }],
  sendToShard: (guildId, payload) => client.guilds.cache.get(guildId)?.shard?.send(payload),
  client: { id: process.env.CLIENT_ID, username: 'MyBot' },
});
client.on('raw', (d) => client.lavalink.sendRawData(d));
client.on('ready', () => client.lavalink.init({ ...client.user }));
```

**discord.py + Wavelink**

```python
import wavelink

async def setup_hook(self):
    node = wavelink.Node(uri="http://localhost:2333", password="youshallnotpass")
    await wavelink.Pool.connect(nodes=[node], client=self)
```

**Python + lavalink.py**

```python
import lavalink

bot.lavalink = lavalink.Client(bot.user.id)
bot.lavalink.add_node('localhost', 2333, 'youshallnotpass', 'eu', 'rust-link')
```

Lavalink4NET, Lavalink-Client cho JDA, DisGo và các thư viện khác khai báo cùng bốn thông số trên.

### Lưu ý khi đấu nối

- **Discord E2EE (DAVE):** Discord yêu cầu mã hóa đầu-cuối cho voice. rust-link chỉ bật DAVE khi client gửi kèm `voice.channelId`, việc mà các client hỗ trợ Lavalink 4.1+ đã làm. Client quá cũ không gửi trường này thì Discord có thể đóng kết nối với mã 4017, bot sẽ nhận `WebSocketClosedEvent`.
- **Tìm kiếm:** dùng `ytsearch:`, `ytmsearch:` (xử lý như `ytsearch:`), `scsearch:`, `cachesearch:` (chỉ tìm trong cache), hoặc dán thẳng URL.
- **Nhiều node:** có thể khai báo rust-link song song với Lavalink cũ trong danh sách node của client để chuyển dần.
- **Filter:** chỉ filter `volume` có tác dụng. Equalizer, timescale và các filter khác được chấp nhận nhưng không thay đổi âm thanh.

## 8. Kiểm tra node hoạt động

```bash
curl -H "Authorization: youshallnotpass" http://localhost:2333/version
```

```bash
curl -H "Authorization: youshallnotpass" "http://localhost:2333/v4/loadtracks?identifier=ytsearch:never%20gonna%20give%20you%20up"
```

```bash
curl -H "Authorization: youshallnotpass" http://localhost:2333/v4/rustlink/cache
```

Lệnh đầu trả về `4.1.1`. Lệnh thứ hai trả về JSON có `"loadType":"search"`. Lệnh cuối cho biết số bài đã cache, dung lượng đang dùng và số bài chờ backup.

## 9. Sự cố thường gặp

| Hiện tượng | Nguyên nhân và cách xử lý |
|---|---|
| HTTP 401 `Unauthorized` | Password trong bot khác `server.password`. |
| `failed to start yt-dlp` | yt-dlp chưa cài hoặc không nằm trong PATH. Cài lại, hoặc khai báo `sources.ytdlp.path`. |
| `loadType: error` với YouTube | yt-dlp đã cũ hoặc thiếu Deno. Chạy `yt-dlp -U` và cài Deno. |
| `HTTP Error 429` hoặc "Sign in to confirm you're not a bot" | YouTube đang giới hạn IP của máy chủ. Xuất cookies từ trình duyệt và thêm `extraArgs: ["--cookies", "/duong/dan/cookies.txt"]` vào mục `sources.ytdlp`. |
| `cannot create cache directory` | User chạy node không có quyền ghi thư mục cache. Đổi `cache.directory` hoặc cấp quyền. |
| `WebSocketClosedEvent` mã 4006 ngay khi vào voice | Bản 0.1.0 bỏ cổng của endpoint `host:port`, có thể kết nối nhầm cổng Discord. Bản sửa giữ nguyên cổng (ví dụ `:2096`); với binary 0.1.0 cũ, client có thể gửi `wss://host:port` để tránh lỗi này. |
| `WebSocketClosedEvent` mã 4014 | Bot bị kick hoặc kênh voice bị xóa. Bot cần vào lại kênh. |
| `WebSocketClosedEvent` mã 4017 | Client không gửi `voice.channelId`, không bật được DAVE. Cập nhật client. |
| Bot vào kênh nhưng không có tiếng | Tường lửa chặn UDP gửi ra ngoài. Mở UDP outbound. |
| Bài phát lần đầu chậm vài giây | Bài đang được tải qua yt-dlp. Từ lần thứ hai bài được phát thẳng từ cache. |

## 10. Bật backup OneDrive

1. Tạo app trong Azure Portal, mục App registrations, bật **Allow public client flows** và cấp quyền `Files.ReadWrite`, `offline_access`.
2. Đăng nhập một lần bằng lệnh dưới đây; credentials được ghi ra file theo `onedrive.credentialsFile`.
3. Đặt `onedrive.enabled: true` trong `application.yml` và khởi động lại node.

```bash
./rust-link onedrive-login --client-id <APPLICATION_CLIENT_ID>
```

Chi tiết về cơ chế cache, backup và khôi phục nằm trong `README.md`.
