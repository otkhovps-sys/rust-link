// local-files: play audio files from a folder on the server.
//
// Settings in application.yml:
//
//   plugins:
//     local-files:
//       folder: /srv/music
//
// Identifiers: `local:<words>` finds files whose path contains every word.
// Routes: GET /v4/local/files?limit=100, POST /v4/local/rescan.
const fs = require('fs');
const path = require('path');
const { pathToFileURL } = require('url');
const { Plugin, track } = require('./rustlink-plugin');

// Files are read straight from disk, so they skip the audio cache.
const plugin = new Plugin({ version: '1.0.0', sourceName: 'local', cache: false });
const AUDIO = new Set(['.mp3', '.flac', '.wav', '.ogg', '.oga', '.opus', '.m4a', '.aac', '.webm', '.mka']);
let folder = '';
let files = [];

function scan(dir) {
  const found = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) found.push(...scan(full));
    else if (AUDIO.has(path.extname(entry.name).toLowerCase())) found.push(path.relative(folder, full));
  }
  return found.sort();
}

function rescan() {
  files = fs.existsSync(folder) ? scan(folder) : [];
  plugin.log(`${files.length} audio files in ${folder}`);
}

function toTrack(relative) {
  const parent = path.basename(path.dirname(relative));
  return track({
    identifier: relative,
    title: path.basename(relative, path.extname(relative)),
    author: parent && parent !== '.' ? parent : 'Local file',
    uri: pathToFileURL(path.join(folder, relative)).href,
  });
}

plugin.onStart((config) => {
  folder = path.resolve(config.folder || 'music');
  rescan();
});

plugin.search('local', async (query) => {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  return files
    .filter((file) => words.every((word) => file.toLowerCase().includes(word)))
    .slice(0, 50)
    .map(toTrack);
});

plugin.resolver(async (info) => {
  const full = path.resolve(folder, info.identifier);
  if (!full.startsWith(folder + path.sep)) throw new Error('track is outside the music folder');
  return { file: full };
});

plugin.route('GET', '/v4/local/files', async (request) => files.slice(0, Number(request.query.limit || 100)));
plugin.route('POST', '/v4/local/rescan', async () => {
  rescan();
  return { files: files.length };
});

plugin.run();
