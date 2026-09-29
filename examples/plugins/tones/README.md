# Tones

An example rox source plugin, written to be copied. It makes its own audio: six sine
tones and four chords, each generated as a WAV the first time it's opened. It needs no
network, no account and nothing beyond Python 3's standard library.

The protocol it speaks is in the [plugin guide](../../../README_PLUGINS.md).

## Trying it

1. Turn on Enable Plugins at the top of Settings > Plugins.
2. Press Reveal Folder on the Plugins page and copy this `tones` folder into it.
3. Switch Tones on and confirm the card.
4. Pick Add Panel > Plugins > Tones, which opens the External Sources panel on Tones.
   Tones and Chords are both collections: Keep in the library puts one in your library.

Python 3 has to be on PATH as `python3` or `python`, or as `py` on Windows.

## Talking to it without rox

The plugin reads one JSON-RPC request per line on stdin and answers on stdout, so a
shell can drive it:

```sh
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"hello","params":{"api":1,"config":{},"data_dir":"/tmp/tones","platform":"linux-x86_64"}}' \
  '{"jsonrpc":"2.0","id":2,"method":"source.browse","params":{"node":null,"cursor":null}}' \
  | python3 -B tones.py
```

`-B` keeps Python from writing `__pycache__` into the folder. rox sets the same thing
for every plugin, since any new file in the folder changes its hash and switches it off.

## Making it yours

- Turn on Developer mode, the terminal button beside the plugin's switch, while you
  work on it.
  Otherwise every save switches the plugin off until you approve it again.
- Change `id` in `plugin.json` and rename the folder to match. The id is fixed once your
  plugin has users: their rows are filed under it.
- Keep `key`s stable. A track's key becomes its row's path in the library, so changing
  how keys are built orphans every row the plugin made.
- List every program the plugin runs in `programs`, including anything those programs
  need. The Plugins page tells the user which are missing.
- Keep the thread pool. rox sends reads while a search is still running, and a plugin
  that answers one request at a time stalls its own playback.
- Keep exiting when stdin closes. It's the one signal that reaches a plugin on every OS
  when rox goes away without a shutdown.
- Write only under the `data_dir` that `hello` hands you.
