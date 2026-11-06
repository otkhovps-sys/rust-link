// __NAME__: a rust-link plugin.
//
// Restart rust-link after editing. Settings come from `plugins.__NAME__` in application.yml
// and arrive in `plugin.config`. Protocol and more examples: docs/PLUGINS.md.
const { Plugin, track } = require('./rustlink-plugin');

const plugin = new Plugin({ version: '0.1.0', sourceName: '__NAME__' });

plugin.onStart((config, node) => {
  plugin.log(`started on rust-link ${node.version} with settings ${JSON.stringify(config)}`);
});

// /v4/loadtracks?identifier=__NAME__:<query> comes here.
plugin.search('__NAME__', async (query) => {
  if (/^https?:\/\//.test(query)) {
    return track({ identifier: query, title: query.split('/').pop() || query, uri: query });
  }
  // Anything else: search YouTube through the node and return its results unchanged.
  return plugin.load(`ytsearch:${query}`);
});

// Where this plugin's own tracks play from; null plays the track URI.
plugin.resolver(async (info) => null);

plugin.on('TrackStartEvent', (message) => {
  plugin.log(`guild ${message.guildId} is playing ${message.track.info.title}`);
});

// GET /v4/__NAME__/hello (with the node password, like every REST call).
plugin.route('GET', '/v4/__NAME__/hello', async (request) => ({ plugin: plugin.name, query: request.query }));

plugin.run();
