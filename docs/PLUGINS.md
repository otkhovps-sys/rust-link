# Plugin cho rust-link

Plugin là một chương trình chạy riêng, viết bằng ngôn ngữ nào cũng được (Python, Node.js, Go, Rust, C#...). rust-link khởi động plugin như tiến trình con và trao đổi với nó bằng JSON qua stdin/stdout. Plugin không cần biết gì về Rust hay về cách node phát nhạc.

Plugin làm được:

| Việc | Ví dụ |
|---|---|
| Thêm nguồn nhạc theo tiền tố | `radio:lofi`, `local:son tung`, `nct:...` |
| Nhận URL theo mẫu regex | `https://music.example.com/...` |
| Quyết định bài phát từ đâu | URL (kèm header), file trên máy chủ, hoặc chuyển sang bài khác (`ytsearch:...`) |
| Nhận sự kiện websocket | `TrackStartEvent`, `TrackEndEvent`, `playerUpdate`, `ready`... để ghi log, thống kê, scrobble |
| Thêm route REST | `GET /v4/lyrics/{guildId}`, `POST /v4/radio/reload` |
| Gọi lại REST của node | tìm bài, điều khiển player (autoplay, gợi ý bài kế tiếp) |

Mục lục:

1. [Tạo plugin đầu tiên](#1-tạo-plugin-đầu-tiên)
2. [Khai báo và cấu hình](#2-khai-báo-và-cấu-hình)
3. [SDK Python](#3-sdk-python)
4. [SDK Node.js](#4-sdk-nodejs)
5. [Plugin mẫu](#5-plugin-mẫu)
6. [Giao thức (cho ngôn ngữ khác)](#6-giao-thức-cho-ngôn-ngữ-khác)
7. [Vòng đời và xử lý lỗi](#7-vòng-đời-và-xử-lý-lỗi)
8. [Lưu ý khi triển khai](#8-lưu-ý-khi-triển-khai)

## 1. Tạo plugin đầu tiên

Cần Python 3.9+ (hoặc Node.js 16+ nếu chọn `--lang node`) trên máy chạy node.

```bash
./rust-link plugin new hello
```

```bash
./rust-link plugin new hello-js --lang node
```

Lệnh tạo thư mục `plugins/hello/` gồm `plugin.yml`, `main.py` (plugin mẫu có chú thích) và `rustlink_plugin.py` (SDK, không cần cài gì thêm). Khởi động lại node rồi thử:

```bash
curl -H "Authorization: youshallnotpass" "http://localhost:2333/v4/hello/hello?x=1"
```

```bash
curl -H "Authorization: youshallnotpass" "http://localhost:2333/v4/loadtracks?identifier=hello:https://example.com/song.mp3"
```

Xem trạng thái các plugin:

```bash
curl -H "Authorization: youshallnotpass" http://localhost:2333/v4/rustlink/plugins
```

```bash
./rust-link plugin list
```

Plugin đang chạy cũng xuất hiện trong `plugins` và `sourceManagers` của `/v4/info`, nên bot nào kiểm tra plugin qua `/v4/info` cũng thấy.

## 2. Khai báo và cấu hình

### Thư mục plugin

Mỗi thư mục con của `plugins.directory` (mặc định `./plugins`, Docker là `/data/plugins`) có file `plugin.yml` (hoặc `plugin.yaml`, `plugin.json`) là một plugin. Lệnh được chạy ngay trong thư mục đó.

```yaml
name: radio                       # mặc định: tên thư mục
command: ["python3", "main.py"]   # hoặc một chuỗi: "python3 main.py"
enabled: true
env:                              # biến môi trường thêm cho plugin
  API_TOKEN: "..."
workingDir: src                   # tùy chọn, tính từ thư mục plugin
```

Chương trình ghi đường dẫn tương đối (`./bin/plugin`, `bin/plugin.exe`) được tìm trong thư mục plugin; tên trần (`python3`, `node`, `java`) được tìm trong `PATH`. Trên Windows dùng `python` thay cho `python3`.

File `plugin.yml` hỏng chỉ làm plugin đó bị bỏ qua (có cảnh báo trong log), node vẫn chạy.

### Trong application.yml

```yaml
plugins:
  enabled: true
  directory: ./plugins
  requestTimeoutSecs: 10      # mỗi lần gọi plugin (loadItem, resolve, route)
  startupTimeoutSecs: 15      # chờ plugin trả lời initialize
  entries:                    # khai báo plugin ngay tại đây, không cần plugin.yml
    - name: lyrics
      command: ["node", "index.js"]
      workingDir: /opt/lyrics
      env: { GENIUS_TOKEN: "..." }
      config: { language: vi }  # thay cho plugins.lyrics bên dưới
  # Cấu hình riêng cho từng plugin, giống khối plugins: của Lavalink:
  radio:
    stations:
      lofi: { title: "Lofi radio", url: "https://radio.example/lofi.mp3" }
```

Plugin nhận `plugins.<tên>` (hoặc `config` của entry) trong lệnh `initialize`; SDK để sẵn trong `plugin.config`. Các tên `enabled`, `directory`, `entries`, `requestTimeoutSecs`, `startupTimeoutSecs` dành cho node, không đặt làm tên plugin.

Khóa `plugins.youtube`, `plugins.lavasrc` trong file cấu hình Lavalink cũ không ảnh hưởng gì, trừ khi có plugin trùng tên.

## 3. SDK Python

File `rustlink_plugin.py` chỉ dùng thư viện chuẩn. `print()` được chuyển sang stderr và hiện trong log của node, nên in thoải mái.

```python
from rustlink_plugin import Plugin, Response, track, playlist, error

plugin = Plugin(version="1.0.0", source_name="radio", cache=False)


@plugin.on_start
def start(config, node):
    plugin.log("settings: {}".format(config))


@plugin.search("radio", "fm")            # radio:<query> và fm:<query>
def search(query, identifier):
    return [track(identifier="lofi", title="Lofi", uri="https://radio.example/lofi.mp3", is_stream=True)]


@plugin.url(r"^https?://music\.example\.com/")
def by_url(url, match):
    return track(identifier=url, title="Bài từ music.example.com", uri=url, length=215000)


@plugin.resolver
def resolve(info):
    return {"url": "https://cdn.example.com/signed.mp3", "headers": {"Referer": "https://music.example.com"}}


@plugin.on("TrackStartEvent", "TrackEndEvent")
def on_track(message, session_id):
    plugin.log("{} {}".format(message["type"], message["guildId"]))


@plugin.route("GET", "/v4/radio/{station}")
def station(request):
    if request.params["station"] not in plugin.config.get("stations", {}):
        return Response({"error": "unknown station"}, status=404)
    return {"station": request.params["station"], "query": request.query}


plugin.run()
```

| Thành phần | Tác dụng |
|---|---|
| `Plugin(name, version, source_name, cache)` | `source_name`: `sourceName` cho bài plugin tạo ra. `cache=False`: bài của plugin luôn phát thẳng, không lưu cache (radio, file cục bộ) |
| `@plugin.search(*prefixes)` | Hàm nhận `(query, identifier)` |
| `@plugin.url(regex)` | Hàm nhận `(url, match)`. Regex theo cú pháp `regex` của Rust, giống Python với mẫu thông thường |
| `@plugin.resolver` | Hàm nhận thông tin bài, trả về nơi phát (xem [resolve](#resolve-phát-bài-của-plugin)) |
| `@plugin.on(*names)` | Tên sự kiện (`TrackStartEvent`...), tên op (`ready`, `playerUpdate`, `stats`) hoặc `*` (mọi thứ trừ `playerUpdate` và `stats`). Hàm nhận `(message, session_id)`; sự kiện được xử lý lần lượt theo thứ tự |
| `@plugin.route(method, path)` | `{tên}` lấy một đoạn đường dẫn, `{*tên}` ở cuối lấy phần còn lại. Hàm nhận `Request` (`params`, `query`, `headers`, `body`, `method`, `path`) và trả về body (chuỗi được gửi dạng text), `(status, body)` hoặc `Response(body, status, headers)` |
| `@plugin.on_start` / `@plugin.on_stop` | Chạy lúc khởi động (nhận `config`, `node`) và trước khi dừng |
| `track(...)` | Tạo bài: `identifier`, `title`, `author`, `length` (ms), `uri`, `is_stream`, `is_seekable`, `artwork_url`, `isrc`, `source_name`, `plugin_info`, `user_data` |
| `playlist(name, tracks, selected=-1)` / `error(message)` | Kết quả playlist / báo lỗi cho bot |
| `plugin.rest(method, path, body=None, query=None)` | Gọi REST của node, không cần mật khẩu, trả về `(status, body)` |
| `plugin.load(identifier)` | `/v4/loadtracks` qua node: YouTube, Spotify, plugin khác... |
| `plugin.log(message, level)` | Ghi log ở mức `debug`, `info`, `warn`, `error` |
| `raise PluginError(message)` | Báo lỗi; với `loadItem` bot nhận `loadType: error` kèm thông báo |

Mỗi lệnh từ node chạy trên một thread riêng (tối đa 16 cùng lúc), nên handler được phép gọi `plugin.rest()` hay chờ mạng mà không chặn plugin. Riêng `on_start` chạy trước khi node sẵn sàng nên chưa gọi được `plugin.rest()`.

## 4. SDK Node.js

File `rustlink-plugin.js` không cần package nào. `console.log` được chuyển sang stderr.

```js
const { Plugin, track, playlist, error, reply } = require('./rustlink-plugin');

const plugin = new Plugin({ version: '1.0.0', sourceName: 'local', cache: false });

plugin.onStart((config, node) => plugin.log(`folder: ${config.folder}`));
plugin.search('local', async (query, identifier) => [track({ identifier: 'a.mp3', title: 'A' })]);
plugin.url(/^https?:\/\/music\.example\.com\//, async (url, match) => track({ identifier: url, uri: url }));
plugin.resolver(async (info) => ({ file: `/srv/music/${info.identifier}` }));
plugin.on(['TrackStartEvent', 'TrackEndEvent'], (message, sessionId) => {});
plugin.route('GET', '/v4/local/{*path}', async (req) => reply({ path: req.params.path }, { status: 200 }));

plugin.run();
```

Các hàm tương ứng với SDK Python: `search`, `url`, `resolver`, `on`, `route`, `onStart`, `onStop`, `log`, `rest(method, path, { body, query })` (trả về Promise `{status, body}`), `load(identifier)`; tham số của `track()` viết kiểu camelCase (`isStream`, `artworkUrl`, `pluginInfo`). Handler là hàm async, các lệnh được xử lý song song.

## 5. Plugin mẫu

Thư mục `examples/plugins/` (có trong gói phát hành) chứa hai plugin dùng được ngay, chép vào `plugins/` là chạy:

| Plugin | Ngôn ngữ | Tác dụng |
|---|---|---|
| `radio` | Python | Đài radio theo tên: `radio:lofi`, `radio:` (mọi đài, dạng playlist), `radio:jazz` (tìm theo tên). Route `GET /v4/radio/stations`. Ghi log khi một server bắt đầu nghe đài |
| `local-files` | Node.js | Phát file nhạc trong một thư mục trên máy chủ: `local:<từ khóa>` tìm theo tên file và thư mục. Route `GET /v4/local/files`, `POST /v4/local/rescan` |

```yaml
plugins:
  radio:
    stations:
      lofi: { title: "Lofi radio", url: "https://radio.example/lofi.mp3" }
      jazz: { title: "Jazz", url: "https://radio.example/jazz/playlist.m3u8" }
  local-files:
    folder: /srv/music
```

Bot dùng các bài nhạc từ plugin như bài bình thường: load bằng `/v4/loadtracks`, phát bằng `encoded` hoặc `identifier`.

## 6. Giao thức (cho ngôn ngữ khác)

Hai SDK trên chỉ là lớp mỏng; ngôn ngữ nào đọc ghi được stdin/stdout đều viết plugin được.

- Mỗi dòng là một object JSON-RPC 2.0, kết thúc bằng `\n`, mã hóa UTF-8.
- Node gửi lệnh vào stdin của plugin, plugin trả lời ra stdout. Plugin cũng gửi lệnh ngược lại cho node theo cùng cách.
- stderr và các dòng không phải JSON trên stdout được ghi vào log của node.
- Lệnh có `id` cần trả lời `{"jsonrpc":"2.0","id":<id>,"result":...}` hoặc `{"jsonrpc":"2.0","id":<id>,"error":{"code":-32000,"message":"..."}}`. Thông báo (không có `id`) không cần trả lời.
- Node có thể gửi nhiều lệnh liền nhau trước khi nhận trả lời; trả lời theo thứ tự nào cũng được.
- Biến môi trường có sẵn: `RUSTLINK_PLUGIN` (tên plugin), `RUSTLINK_VERSION`.

### initialize

Lệnh đầu tiên sau khi khởi động.

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
  "protocol":1,"name":"radio",
  "config":{"stations":{"lofi":{"url":"https://radio.example/lofi.mp3"}}},
  "node":{"name":"rust-link","version":"0.2.1","lavalinkApi":"4.1.1","address":"0.0.0.0","port":2333}}}
```

Kết quả là bảng khai báo khả năng (mọi trường đều tùy chọn):

| Trường | Kiểu | Ý nghĩa |
|---|---|---|
| `version` | chuỗi | Hiện trong `/v4/info` |
| `sourceName` / `sourceNames` | chuỗi / mảng | `sourceName` của bài plugin tạo ra; bài mang tên đó được gửi về plugin khi phát |
| `searchPrefixes` | mảng | Tiền tố plugin nhận, không phân biệt hoa thường (`"radio"` nhận `radio:...`) |
| `urlPatterns` | mảng | Regex (cú pháp Rust) cho identifier plugin nhận |
| `events` | mảng | Sự kiện muốn nhận, xem [event](#event) |
| `routes` | mảng | `[{"method":"GET","path":"/v4/lyrics/{guildId}"}]`; method `*` nhận mọi method |
| `resolve` | bool | `true`: node hỏi `resolve` trước khi phát bài của plugin |
| `cache` | bool | Mặc định `true`. `false`: bài của plugin luôn phát thẳng |

Plugin được kiểm tra trước các nguồn có sẵn, nên có thể nhận cả tiền tố hay URL mà node đã biết (ví dụ tự xử lý link YouTube). Route của plugin chỉ được dùng khi không trùng route có sẵn của node, và vẫn cần header `Authorization`.

### loadItem

```json
{"jsonrpc":"2.0","id":2,"method":"loadItem","params":{"identifier":"radio:lofi","prefix":"radio","query":"lofi"}}
{"jsonrpc":"2.0","id":3,"method":"loadItem","params":{"identifier":"https://music.example.com/x","url":"https://music.example.com/x"}}
```

Kết quả nhận các dạng sau. Node tự tạo chuỗi `encoded` và điền trường còn thiếu (`sourceName` lấy theo plugin, `title` lấy theo `identifier`, bài `isStream` có `length` bằng giá trị Lavalink dùng cho live).

| Kết quả | Hiểu là |
|---|---|
| `null` | Không phải của plugin; node thử plugin khác rồi tới nguồn có sẵn |
| `{"loadType":"track"\|"search"\|"playlist"\|"empty"\|"error","data":...}` | Đúng định dạng Lavalink v4 |
| `[track, ...]` | Kết quả tìm kiếm (mảng rỗng là `empty`) |
| `{"name":"...","selectedTrack":0,"tracks":[...]}` | Playlist |
| `{"info":{...},"pluginInfo":{...}}` hoặc chỉ `{...info}` | Một bài |

Một bài cần ít nhất `identifier` hoặc `uri`; các trường khác giống `info` của Lavalink. Bài có `sourceName` của nguồn có sẵn (ví dụ `youtube`) được node tự phát như bài YouTube thường.

### resolve (phát bài của plugin)

Gửi khi cần âm thanh của bài có `sourceName` thuộc plugin và plugin khai báo `resolve: true`. Với bài đã nằm trong cache, node không hỏi lại.

```json
{"jsonrpc":"2.0","id":4,"method":"resolve","params":{"track":{"identifier":"lofi","sourceName":"radio","uri":"...","isStream":true}}}
```

| Kết quả | Node làm gì |
|---|---|
| `null` | Phát `uri` của bài (http/https), nếu không có thì thử yt-dlp với `uri` |
| `{"url":"https://...","headers":{...},"format":"mp3"}` | Phát URL đó (HLS `.m3u8` cũng được). File thường đọc bằng HTTP Range, tua được; link hết hạn (403/410) thì node gọi `resolve` lần nữa để lấy link mới |
| `{"file":"/srv/music/a.flac"}` | Phát file trên máy chủ |
| `{"identifier":"ytsearch:artist - title"}` | Load identifier đó rồi phát bài đầu tiên (kiểu LavaSrc) |
| `{"track":{...}}` | Phát bài đó (ví dụ một bài YouTube đã biết ID) |
| chuỗi | URL nếu bắt đầu bằng http(s), file nếu là đường dẫn tuyệt đối, còn lại là identifier |

Plugin không khai báo `resolve` thì node phát `uri` của bài như trường hợp `null`.

### event

Thông báo, không cần trả lời:

```json
{"jsonrpc":"2.0","method":"event","params":{"sessionId":"abc","guildId":"123","message":{"op":"event","type":"TrackStartEvent","guildId":"123","track":{...}}}}
```

`message` giống hệt tin nhắn websocket node gửi cho bot. Trong `events`, mỗi phần tử khớp với `type` (`TrackStartEvent`, `TrackEndEvent`, `TrackExceptionEvent`, `TrackStuckEvent`, `WebSocketClosedEvent`) hoặc `op` (`ready`, `event`, `playerUpdate`, `stats`). `*` nhận tất cả trừ `playerUpdate` và `stats`, hai loại gửi định kỳ vài giây một lần nên phải ghi tên rõ.

### route

```json
{"jsonrpc":"2.0","id":5,"method":"route","params":{
  "route":"GET /v4/lyrics/{guildId}","method":"GET","path":"/v4/lyrics/123",
  "params":{"guildId":"123"},"query":{"lang":"vi"},"headers":{"user-agent":"..."},"body":null}}
```

`route` cho biết plugin khai báo route nào đã khớp. `body` là JSON nếu yêu cầu gửi JSON, chuỗi nếu là dạng khác, `null` nếu trống. Header `Authorization` không được chuyển cho plugin.

Kết quả dạng object luôn được hiểu là `{"status":200,"headers":{...},"body":...}` (mọi trường tùy chọn; thiếu `status` thì là 200, hoặc 204 khi không có body). Kết quả không phải object (chuỗi, mảng, số, `null`) được dùng làm body. Body dạng chuỗi được gửi dạng `text/plain`, dạng khác gửi dạng JSON.

### shutdown

Thông báo `{"jsonrpc":"2.0","method":"shutdown","params":{}}` khi node dừng; plugin có 2 giây để thoát trước khi bị kết thúc. stdin bị đóng cũng có nghĩa là phải thoát.

### Plugin gọi node

| Method | Params | Kết quả |
|---|---|---|
| `rest` | `{"method":"PATCH","path":"/v4/sessions/abc/players/123","body":{...}}` | `{"status":200,"body":...}` |
| `log` (thông báo) | `{"level":"info","message":"..."}` | không có |

`rest` chạy route của node ngay bên trong tiến trình (không qua mạng, không cần mật khẩu), gồm cả route của các plugin khác. `path` có thể kèm query string đã mã hóa URL.

## 7. Vòng đời và xử lý lỗi

- Node chạy mọi plugin khi khởi động và chờ chúng trả lời `initialize` (tối đa `startupTimeoutSecs`) trước khi nhận kết nối của bot.
- Plugin thoát hay bị crash được chạy lại sau 1 giây, rồi 2, 4... tối đa 30 giây giữa các lần. Plugin chạy ổn hơn 60 giây thì thời gian chờ quay về 1 giây. Lệnh đang chờ trả lời lúc crash nhận lỗi ngay.
- Trong lúc plugin đang khởi động lại, identifier của nó trả về `loadType: error` với thông báo plugin chưa chạy, route trả 503.
- Plugin không trả lời trong `requestTimeoutSecs`: `loadItem` trả lỗi cho bot, route trả 504, bài đang chờ phát nhận `TrackExceptionEvent`.
- Lệnh trong `command` không chạy được (sai tên chương trình, thiếu thư mục) thì plugin chuyển sang `failed` và không thử lại; sửa cấu hình rồi khởi động lại node.
- `GET /v4/rustlink/plugins` cho biết `state` (`starting`, `running`, `restarting`, `failed`, `stopped`), số lần chạy lại, lỗi gần nhất và mọi khả năng plugin đã khai báo.

## 8. Lưu ý khi triển khai

- **Quyền hạn:** plugin chạy với quyền của tiến trình node, đọc được file và mạng như node. Chỉ cài plugin có mã nguồn mình tin.
- **Không ghi rác ra stdout:** stdout dành cho giao thức. Hai SDK đã chuyển `print` và `console.log` sang stderr; ngôn ngữ khác nên ghi log ra stderr.
- **Docker:** image chính thức không có Python hay Node.js. Dựng image riêng từ image của rust-link rồi gắn thư mục plugin vào `/data/plugins`:

```dockerfile
FROM ghcr.io/otkhovps-sys/rust-link:latest
USER root
RUN apt-get update \
 && apt-get install -y --no-install-recommends python3 nodejs \
 && rm -rf /var/lib/apt/lists/*
USER rustlink
```

```bash
docker run -d -p 2333:2333 -v "$PWD/data:/data" -v "$PWD/plugins:/data/plugins" my-rust-link
```

- **Hiệu năng:** mỗi lệnh đi qua pipe nội bộ. Đo trên bản debug, một route của plugin mất trung vị 0,66 đến 0,73 ms so với 0,15 ms của route có sẵn, tức khoảng 0,5 ms cho một lượt qua plugin (gồm cả SDK Python hoặc Node), chưa kể thời gian plugin tự xử lý. Bài từ plugin vẫn được cache như bài thường (trừ khi `cache: false`), nên lần phát sau không cần gọi plugin.
- **Khác plugin Lavalink:** rust-link không chạy được file JAR của Lavalink. Phần lớn plugin Lavalink phổ biến đã có sẵn trong node (LavaSrc cho Spotify, Apple Music, Deezer; youtube-source qua yt-dlp); phần còn lại viết lại thành plugin như trên khá nhanh. `pluginFilters` hiện chỉ được lưu và trả lại, plugin chưa xử lý âm thanh trực tiếp được.
