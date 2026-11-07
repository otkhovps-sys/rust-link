# Hướng dẫn triển khai và đấu nối rust-link

rust-link là một audio node nói chuẩn **Lavalink v4**. Bot Discord đang dùng Lavalink chỉ cần trỏ sang host, port và password của rust-link là chạy, không phải sửa code bot.

## 1. So sánh nhanh với Lavalink

| | Lavalink | rust-link |
|---|---|---|
| File chạy | `Lavalink.jar` | `rust-link` (Linux x86_64, Linux ARM64) hoặc `rust-link.exe` (Windows x86_64), mỗi bản là một file duy nhất |
| Cần cài thêm | Java 17+ | Không bắt buộc: yt-dlp được tự tải và tự cập nhật. Nên cài thêm Deno |
| File cấu hình | `application.yml` cạnh file chạy | `application.yml` cạnh file chạy |
| Lệnh chạy | `java -jar Lavalink.jar` | `./rust-link` hoặc bấm đúp `start.bat` |
| Port / password mặc định | `2333` / `youshallnotpass` | `2333` / `youshallnotpass` |
| WebSocket | `/v4/websocket` | `/v4/websocket` |
| Thư mục log | `./logs/` | `./logs/` |
| Plugin Java (LavaSrc, youtube-plugin...) | có | không chạy được plugin, nhưng có sẵn link Spotify, Apple Music, Deezer kiểu LavaSrc |
| Bài rất dài, live stream | phát trực tiếp | phát trực tiếp, không lưu cache (live YouTube qua HLS) |
| Filter (equalizer, timescale, karaoke...) | đủ 10 filter | đủ 10 filter |
| Cache audio + backup OneDrive | không | có |

## 2. Chuẩn bị máy chủ

- **Hệ điều hành:** Linux x86_64 hoặc ARM64 bất kỳ (ví dụ VPS Ampere miễn phí của Oracle Cloud), hoặc Windows 10/11 x86_64. Bản Linux được link tĩnh nên không phụ thuộc phiên bản glibc, đã chạy thử trên Ubuntu 18.04, Debian 11 và Alpine 3.20. Bản Windows đã có người dùng chạy thật, kể cả voice có DAVE. macOS cần tự build từ mã nguồn.
- **yt-dlp:** không cần cài tay. Với `sources.ytdlp.path: managed` (mặc định trong gói), lần chạy đầu node tự tải bản yt-dlp chính thức cho đúng hệ điều hành vào thư mục `bin/`, kiểm tra SHA-256 theo trang release của yt-dlp, rồi tự cập nhật mỗi 24 giờ. Muốn dùng yt-dlp đã cài sẵn thì ghi đường dẫn vào `path`; khi đó node gọi `yt-dlp -U` theo lịch.
- **Deno (khuyến nghị):** yt-dlp bản mới cần một JavaScript runtime để lấy đủ định dạng YouTube. Cài theo hướng dẫn tại deno.com và đặt file `deno` vào `/usr/local/bin`.
- **Mạng:** mở cổng TCP `2333` cho bot. Node cần gửi UDP ra ngoài tới voice server của Discord.

Cập nhật yt-dlp ngay, không chờ lịch:

```bash
./rust-link update-ytdlp
```

## 3. Cách 1: chạy từ file nén (giống Lavalink.jar)

Tải bản dựng sẵn tại trang Releases: https://github.com/otkhovps-sys/rust-link/releases

| File | Dành cho |
|---|---|
| `rust-link-<version>-linux-x86_64.tar.gz` | Linux x86_64 |
| `rust-link-<version>-linux-aarch64.tar.gz` | Linux ARM64 (Oracle Ampere, Raspberry Pi 4/5 chạy hệ 64-bit) |
| `rust-link-<version>-windows-x86_64.zip` | Windows x86_64 |
| `*.sha256` | Mã kiểm tra toàn vẹn của file tải về |

Kiểm tra file tải về trên Linux:

```bash
sha256sum -c rust-link-0.2.1-linux-x86_64.tar.gz.sha256
```

Giải nén gói `rust-link-<version>-linux-x86_64.tar.gz`:

```bash
tar -xzf rust-link-0.2.1-linux-x86_64.tar.gz
```

```bash
cd rust-link-0.2.1-linux-x86_64
```

Nội dung thư mục:

```text
rust-link-0.2.1-linux-x86_64/
├── rust-link               # file chạy duy nhất
├── application.yml         # cấu hình, giống application.yml của Lavalink
├── onedrive.example.json   # mẫu credentials OneDrive
├── rust-link.service       # mẫu systemd
├── HUONG_DAN.md            # tài liệu này
├── PLUGINS.md              # hướng dẫn viết plugin
├── examples/plugins/       # plugin mẫu: radio (Python), local-files (Node.js)
├── README.md
└── LICENSE
```

Lần chạy đầu, node tạo thêm `bin/` (yt-dlp tự tải), `data/cache/` (cache audio) và `logs/`.

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
sudo cp -r rust-link-0.2.1-linux-x86_64 /opt/rust-link
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
2. Không cần tải yt-dlp: lần chạy đầu node tự tải `yt-dlp.exe` vào `bin\`. Nên cài thêm Deno.
3. Mở `application.yml` bằng Notepad, đổi `server.password`.
4. Bấm đúp `start.bat`. Cửa sổ dòng lệnh hiện dòng `rust-link is ready to accept connections` là node đã sẵn sàng.

Trong gói Windows, cache nằm ở `data\cache` và log ở `logs\`, cạnh `rust-link.exe`. Khi Windows Firewall hỏi, chọn **Allow access** để bot kết nối được vào cổng 2333.

Muốn dùng `yt-dlp.exe` có sẵn ở chỗ khác, khai báo đường dẫn đầy đủ trong `application.yml`, dùng dấu `/` hoặc đặt trong nháy đơn:

```yaml
sources:
  ytdlp:
    path: 'C:\tools\yt-dlp.exe'
```

Trên Windows, phần `stats` báo CPU và RAM bằng 0 vì phần đo hiện chỉ hỗ trợ Linux. Phát nhạc không bị ảnh hưởng.

## 4. Cách 2: chạy bằng Docker

Mỗi bản phát hành có image dựng sẵn cho amd64 và arm64 trên GitHub Container Registry. Repo đang private thì đăng nhập trước bằng một Personal Access Token có quyền `read:packages`:

```bash
docker login ghcr.io -u <github-username>
```

```bash
docker run -d --name rust-link -p 2333:2333 -v rust-link-data:/data -e RUSTLINK_SERVER_PASSWORD=doi-mat-khau ghcr.io/otkhovps-sys/rust-link:latest
```

Hoặc build từ thư mục mã nguồn:

```bash
docker compose up -d --build
```

Image có sẵn Deno, chạy bằng user không phải root (uid 322, giống image Lavalink). yt-dlp được tự tải và tự cập nhật vào `/data/bin`. Cache, chỉ mục SQLite và `onedrive.json` cũng nằm trên volume `/data`, nên còn nguyên khi tạo lại container. Đổi password qua biến `RUSTLINK_SERVER_PASSWORD`.

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

Bản Linux ARM64 được cross-compile bằng cargo-zigbuild (zig làm trình biên dịch C). Có `qemu-aarch64-static` thì script chạy thử file ARM64 bằng giả lập:

```bash
pip install ziglang cargo-zigbuild
```

```bash
rustup target add aarch64-unknown-linux-musl
```

```bash
TARGET=aarch64-unknown-linux-musl scripts/package.sh
```

### Phát hành lên GitHub Releases

Workflow `.github/workflows/release.yml` chạy test, build ba gói (Linux x86_64, Linux ARM64, Windows) rồi đăng lên Releases với tag `v<version>` lấy từ `Cargo.toml`, sau đó đẩy image Docker amd64 + arm64 lên `ghcr.io`. Có hai cách kích hoạt:

- **Thủ công:** vào tab **Actions** trên GitHub, chọn workflow **Release**, bấm **Run workflow**. Tag được tạo trên commit đã chọn.
- **Bằng tag:** push tag khớp phiên bản, ví dụ `v0.2.1`.

```bash
git tag v0.2.1
```

```bash
git push origin v0.2.1
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
| `lavalink.server.filters.*` | `lavalink.filters.*` |
| `plugins.lavasrc.providers` | `sources.mirrorProviders` |
| `plugins.lavasrc.sources.spotify`, `.applemusic`, `.deezer` | `sources.spotify.enabled`, `sources.appleMusic.enabled`, `sources.deezer.enabled` |
| `plugins.lavasrc.spotify.clientId`, `.clientSecret`, `.countryCode`, `.playlistLoadLimit`, `.albumLoadLimit` | cùng tên trong `sources.spotify` |
| `plugins.lavasrc.applemusic.mediaAPIToken`, `.countryCode` | `sources.appleMusic.mediaApiToken`, `.countryCode` |

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
- **Tìm kiếm:** dùng `ytsearch:`, `ytmsearch:` (xử lý như `ytsearch:`), `scsearch:`, `spsearch:` (Spotify), `amsearch:` (Apple Music), `dzsearch:` (Deezer), `dzisrc:` (Deezer theo mã ISRC), `cachesearch:` (chỉ tìm trong cache), hoặc dán thẳng URL.
- **Nhiều node:** có thể khai báo rust-link song song với Lavalink cũ trong danh sách node của client để chuyển dần.
- **Filter:** hỗ trợ đủ 10 filter của Lavalink v4: `volume`, `equalizer` (15 band), `karaoke`, `timescale` (speed/pitch/rate, dùng cho nightcore và vaporwave), `tremolo`, `vibrato`, `rotation` (8D), `distortion`, `channelMix`, `lowPass`. Đổi filter hoặc âm lượng có hiệu lực trong khoảng 20 ms: node bỏ phần âm thanh đang chờ trong bộ đệm và xử lý lại từ đúng vị trí đang phát, kể cả với live stream. Khi bật `timescale`, vị trí bài hát vẫn tính theo thời gian của bài, nên thanh tiến độ trên bot chạy nhanh hoặc chậm theo `speed × rate`. `pluginFilters` được lưu và trả lại nhưng không xử lý, vì rust-link không chạy plugin của Lavalink. Muốn khóa filter nào thì đặt `false` trong `lavalink.filters` (hoặc `lavalink.server.filters` nếu dùng file của Lavalink).

## 8. Link Spotify, Apple Music, Deezer

rust-link xử lý các link này giống plugin LavaSrc: thông tin bài (tên, nghệ sĩ, ảnh, ISRC) lấy từ API của dịch vụ, còn âm thanh lấy từ bản tương ứng trên YouTube, tìm lúc bài bắt đầu phát. Bot vẫn thấy track Spotify gốc (`sourceName: spotify`, `pluginInfo` giống LavaSrc), nên code bot đang dùng LavaSrc không cần sửa.

Hỗ trợ: track, album, playlist, top tracks của nghệ sĩ, link `open.spotify.com` (kể cả dạng `intl-vi`), URI `spotify:track:...`, link `music.apple.com` (kể cả link album có `?i=` trỏ tới một bài), link `deezer.com` và link rút gọn `link.deezer.com`.

**Spotify:** cần client ID và client secret (miễn phí).

1. Vào https://developer.spotify.com/dashboard, tạo app, phần API chọn **Web API**.
2. Chép **Client ID** và **Client secret** vào `sources.spotify.clientId` và `clientSecret`.
3. Khởi động lại node. `/v4/info` sẽ có `spotify` trong `sourceManagers`.

Giới hạn phía Spotify với app ở **Development Mode** (mọi app tự tạo trên dashboard đều ở chế độ này; Extended Quota chỉ cấp cho doanh nghiệp):

| Link | Từ ngày 9/3/2026 |
|---|---|
| Bài (`/track/...`), `spsearch:` | Dùng được. Tìm kiếm trả tối đa 10 kết quả |
| Album (`/album/...`) | Dùng được |
| Nghệ sĩ (`/artist/...`) | Spotify bỏ API top tracks; node thay bằng 10 bài tìm được theo tên nghệ sĩ |
| Playlist (`/playlist/...`) | Chỉ đọc được playlist mà **tài khoản đã đăng nhập** (`rust-link spotify-login`, xem bên dưới) sở hữu hoặc được mời cùng sửa. Playlist của người khác: node trả lỗi ghi rõ lý do thay vì kết quả rỗng |

Ngoài ra, app ở Development Mode chỉ chạy khi chủ app có Spotify Premium, và Spotify không cho app mới đọc playlist do chính Spotify tạo (ví dụ "Today's Top Hits"). Với playlist của người khác, dùng link playlist YouTube, YouTube Music hoặc Deezer (Deezer không cần cấu hình), hoặc trong ứng dụng Spotify chép các bài sang một playlist của chính bạn.

**Đăng nhập Spotify để đọc playlist của bạn:**

1. Trên https://developer.spotify.com/dashboard, mở app, chọn **Settings** rồi **Edit**. Ở mục **Redirect URIs** thêm đúng dòng `http://127.0.0.1:8888/callback` rồi bấm **Save**. Spotify không nhận `localhost`, phải ghi `127.0.0.1`.
2. Trong thư mục có `application.yml` (đã điền `clientId`, `clientSecret`), chạy:

```bash
./rust-link spotify-login
```

3. Trình duyệt mở trang đăng nhập Spotify. Đăng nhập bằng tài khoản có playlist cần phát rồi bấm **Agree**. Lệnh in ra `Signed in to Spotify as <tên>` và lưu refresh token vào `config/spotify.json` (gói phát hành: `spotify.json` cạnh file chạy; Docker: `/data/spotify.json`).
4. Node đang chạy tự dùng tài khoản này từ lần gọi Spotify kế tiếp, không cần khởi động lại.

Node chạy trên VPS hay trong Docker thì máy chủ không có trình duyệt: thêm `--no-browser`, mở địa chỉ lệnh in ra trên máy của bạn, đăng nhập. Trình duyệt sẽ dừng ở trang lỗi `127.0.0.1:8888`; chép nguyên địa chỉ trên thanh địa chỉ và dán vào cửa sổ lệnh đang chờ.

```bash
docker exec -it rust-link /opt/rust-link/rust-link spotify-login --no-browser
```

Đăng xuất: xóa file `spotify.json` rồi khởi động lại node. Nếu bạn thu hồi quyền của app trong tài khoản Spotify, node ghi cảnh báo và quay lại chế độ không đăng nhập (bài, album, tìm kiếm vẫn chạy). File `spotify.json` chứa refresh token của tài khoản, không chia sẻ và không đưa lên git.

**Apple Music:** cần developer token MusicKit, tạo trong tài khoản Apple Developer (có phí hằng năm). Điền token vào `sources.appleMusic.mediaApiToken`. Token hết hạn tối đa sau 6 tháng, cần tạo lại.

**Deezer:** không cần cấu hình, dùng API công khai.

**Cách chọn bản YouTube:** node thử lần lượt các mẫu trong `sources.mirrorProviders`: mặc định tìm theo mã ISRC trước, sau đó theo "tên bài nghệ sĩ". Trong 5 kết quả đầu, node chọn bản có độ dài lệch không quá 10 giây (hoặc 15% với bài dài), để tránh nhầm sang bản loop 1 giờ hay bản live. Kết quả ghép được lưu trong SQLite theo bài và theo ISRC, nên mỗi bài chỉ tìm một lần, và cùng một bài trên Spotify, Apple Music hay Deezer dùng chung một file cache.

## 9. Bài rất dài và live stream

- **Bài dài:** bài dài hơn `cache.maxTrackDurationSecs` (mặc định 2 giờ), hoặc file HTTP lớn hơn `cache.maxFileSize`, được phát thẳng từ nguồn và không lưu cache. Node đọc từng đoạn 1 MiB bằng HTTP Range và tải trước đoạn kế tiếp, nên vẫn tua được và không làm đầy cache bằng các bản mix 10 giờ. Link YouTube hết hạn giữa chừng thì node tự lấy link mới.
- **Live stream:** live YouTube được phát qua HLS, ưu tiên luồng chỉ có audio. Link `.m3u8` (radio HLS) cũng dùng được. Live stream không tua được và không lưu cache.
- **Radio HTTP thường** (MP3, AAC, Ogg) vẫn phát như trước.
- **Một live stream, nhiều server:** khi nhiều server cùng phát một live stream với cùng filter và âm lượng, node chỉ tải và giải mã một lần rồi gửi cùng gói âm thanh cho tất cả (`audio.shareLiveStreams`). Server nào đổi filter hoặc âm lượng thì tự chuyển sang luồng riêng. Tạm dừng một live stream rồi tiếp tục sẽ phát tiếp ở thời điểm live hiện tại, không phát lại đoạn đã lỡ.

## 10. Tăng tốc phản hồi cho bot

- **Cache kết quả tìm kiếm:** kết quả của `ytsearch:`, `spsearch:`... và của link YouTube, SoundCloud, Spotify, Apple Music, Deezer được dùng lại trong `sources.loadCacheTtlSecs` (mặc định 6 giờ). Lệnh tìm lặp lại trả về ngay và ít gọi YouTube hơn, nên cũng ít bị chặn 429. Truy vấn không phân biệt hoa thường và khoảng trắng thừa.
- **Tải trước playlist:** khi bot load một playlist, node tải sẵn `cache.prefetchTracks` bài đầu (mặc định 3). Mỗi khi một bài trong playlist đó bắt đầu phát, node tải tiếp các bài sau nó. Bài kế tiếp vì vậy phát ngay từ đĩa. Bot dùng chế độ trộn bài (shuffle) thì phần tải trước có thể không trúng bài kế tiếp; đặt `prefetchTracks: 0` nếu muốn tắt.
- **yt-dlp khởi động nhanh hơn:** chế độ `managed` dùng bản yt-dlp dạng thư mục, không phải tự giải nén mỗi lần chạy, nên mỗi lần tìm hoặc tải bắt đầu nhanh hơn khoảng nửa giây.

## 11. Kiểm tra node hoạt động

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

## 12. Sự cố thường gặp

| Hiện tượng | Nguyên nhân và cách xử lý |
|---|---|
| HTTP 401 `Unauthorized` | Password trong bot khác `server.password`. |
| `failed to start yt-dlp` | Với `path: managed`: node chưa tải được yt-dlp (xem log lúc khởi động, kiểm tra mạng tới github.com). Với đường dẫn tự khai báo: yt-dlp không nằm ở đó. |
| `loadType: error` với YouTube | yt-dlp đã cũ hoặc thiếu Deno. Chạy `./rust-link update-ytdlp` và cài Deno. |
| `yt-dlp was installed with pip` trong log | yt-dlp cài qua pip không tự cập nhật bằng `-U`. Chạy `pip install -U yt-dlp`, hoặc chuyển sang `sources.ytdlp.path: managed`. |
| Link Spotify báo `needs sources.spotify.clientId` | Chưa cấu hình Spotify, xem mục 8. |
| Playlist Spotify báo "not found" | Playlist do Spotify tạo, app mới không đọc được. Dùng playlist của người dùng. |
| Bài Spotify phát sai bản (remix, cover) | Bài không có ISRC hoặc ISRC không tìm ra trên YouTube. Thêm mẫu chính xác hơn vào `sources.mirrorProviders`, ví dụ `'ytsearch:%QUERY% official audio'`. |
| `HTTP Error 429` hoặc "Sign in to confirm you're not a bot" | YouTube đang giới hạn IP của máy chủ. Xuất cookies từ trình duyệt và thêm `extraArgs: ["--cookies", "/duong/dan/cookies.txt"]` vào mục `sources.ytdlp`. |
| `cannot create cache directory` | User chạy node không có quyền ghi thư mục cache. Đổi `cache.directory` hoặc cấp quyền. |
| `WebSocketClosedEvent` mã 4006 ngay khi vào voice | Bản 0.1.0 bỏ cổng của endpoint `host:port`, có thể kết nối nhầm cổng Discord. Bản sửa giữ nguyên cổng (ví dụ `:2096`); với binary 0.1.0 cũ, client có thể gửi `wss://host:port` để tránh lỗi này. |
| `WebSocketClosedEvent` mã 4014 | Bot bị kick hoặc kênh voice bị xóa. Bot cần vào lại kênh. |
| `WebSocketClosedEvent` mã 4017 | Client không gửi `voice.channelId`, không bật được DAVE. Cập nhật client. |
| Bot vào kênh nhưng không có tiếng | Tường lửa chặn UDP gửi ra ngoài. Mở UDP outbound. |
| Bài phát lần đầu chậm vài giây | Bài đang được tải qua yt-dlp. Từ lần thứ hai bài được phát thẳng từ cache. |

## 13. Bật backup OneDrive

1. Tạo app trong Azure Portal, mục App registrations, bật **Allow public client flows** và cấp quyền `Files.ReadWrite`, `offline_access`.
2. Đăng nhập một lần bằng lệnh dưới đây; credentials được ghi ra file theo `onedrive.credentialsFile`.
3. Đặt `onedrive.enabled: true` trong `application.yml` và khởi động lại node.

```bash
./rust-link onedrive-login --client-id <APPLICATION_CLIENT_ID>
```

Chi tiết về cơ chế cache, backup và khôi phục nằm trong `README.md`.

## 14. Plugin

Plugin là chương trình riêng (Python, Node.js hay ngôn ngữ khác) mở rộng node: thêm nguồn nhạc, route REST, nhận sự kiện của player. Tạo khung plugin bằng:

```bash
./rust-link plugin new ten-plugin
```

Khởi động lại node, plugin trong thư mục `plugins/` sẽ tự chạy. Gói phát hành có hai plugin mẫu trong `examples/plugins/` (radio bằng Python, phát file nhạc trên máy chủ bằng Node.js). Hướng dẫn đầy đủ, gồm SDK và giao thức: [PLUGINS.md](PLUGINS.md).
