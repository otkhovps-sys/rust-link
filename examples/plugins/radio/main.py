"""radio: internet radio stations by name.

Settings in application.yml::

    plugins:
      radio:
        stations:
          lofi:
            title: Lofi radio
            url: https://radio.example/lofi.mp3
            artwork: https://radio.example/lofi.jpg
          jazz:
            title: Smooth jazz
            url: https://radio.example/jazz/playlist.m3u8

Identifiers: ``radio:lofi`` (one station), ``radio:jazz`` (search by key or title) and
``radio:`` (every station as a playlist). ``GET /v4/radio/stations`` lists them.
"""
from rustlink_plugin import Plugin, playlist, track

# Streams are never cached; the node plays each station URL (MP3/AAC/HLS) itself.
plugin = Plugin(version="1.0.0", source_name="radio", cache=False)


def stations():
    return plugin.config.get("stations") or {}


def station_track(key, station):
    return track(
        identifier=key,
        title=station.get("title", key),
        author=station.get("author", "Internet radio"),
        uri=station["url"],
        is_stream=True,
        artwork_url=station.get("artwork"),
    )


@plugin.on_start
def start(config):
    plugin.log("{} stations configured".format(len(stations())))


@plugin.search("radio")
def search(query):
    query = query.strip().lower()
    known = stations()
    if not query:
        return playlist("Radio stations", [station_track(k, s) for k, s in known.items()])
    if query in known:
        return station_track(query, known[query])
    return [
        station_track(key, station)
        for key, station in known.items()
        if query in key.lower() or query in station.get("title", "").lower()
    ]


@plugin.route("GET", "/v4/radio/stations")
def list_stations(request):
    return [{"key": k, "title": s.get("title", k), "url": s["url"]} for k, s in stations().items()]


@plugin.on("TrackStartEvent")
def announce(message, session_id):
    info = message["track"]["info"]
    if info["sourceName"] == "radio":
        plugin.log("guild {} tuned in to {}".format(message["guildId"], info["title"]))


plugin.run()
