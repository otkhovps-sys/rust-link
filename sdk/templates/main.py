"""__NAME__: a rust-link plugin.

Restart rust-link after editing. Settings come from `plugins.__NAME__` in application.yml
and arrive in `plugin.config`. Protocol and more examples: docs/PLUGINS.md.
"""
from rustlink_plugin import Plugin, track

plugin = Plugin(version="0.1.0", source_name="__NAME__")


@plugin.on_start
def start(config, node):
    plugin.log("started on rust-link {} with settings {}".format(node.get("version"), config))


# /v4/loadtracks?identifier=__NAME__:<query> comes here.
@plugin.search("__NAME__")
def search(query):
    if query.startswith(("http://", "https://")):
        return track(identifier=query, title=query.rsplit("/", 1)[-1] or query, uri=query)
    # Anything else: search YouTube through the node and return its results unchanged.
    return plugin.load("ytsearch:" + query)


# Where this plugin's own tracks play from; None plays the track URI.
@plugin.resolver
def resolve(info):
    return None


@plugin.on("TrackStartEvent")
def now_playing(message, session_id):
    plugin.log("guild {} is playing {}".format(message["guildId"], message["track"]["info"]["title"]))


# GET /v4/__NAME__/hello (with the node password, like every REST call).
@plugin.route("GET", "/v4/__NAME__/hello")
def hello(request):
    return {"plugin": plugin.name, "query": request.query}


plugin.run()
