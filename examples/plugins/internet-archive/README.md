# Internet Archive

An example rox source plugin that uses every optional part of the protocol. It browses and plays the Internet Archive's [netlabels](https://archive.org/details/netlabels) collection: releases their labels put out for free, most under Creative Commons. It only lists releases whose licence is Creative Commons or public domain, so everything it shows is free to stream and to show. It needs the network, no account, and nothing beyond Python 3's standard library.

Start with [tones](../tones/) for the protocol's basics. This one shows what a plugin for a real catalogue looks like, and the [plugin guide](../../../README_PLUGINS.md) has the protocol itself.

## Trying it

1. Turn on Enable Plugins at the top of Settings > Plugins.
2. Press Reveal Folder on the Plugins page and copy this `internet-archive` folder into it.
3. Switch Internet Archive on and confirm the card.
4. Pick Add Panel > Plugins > Internet Archive, which opens the External Sources panel on it.

Python 3 has to be on PATH as `python3` or `python`, or as `py` on Windows.

## What it shows

- The home lists under the panel's own Home (`home`): shelves of the most downloaded and newest releases and of the netlabels, a few tracks to start with, and genres.
- A netlabel opens as a wall of covers. A genre opens as rows with a download count and a year beside each release (`fields`), and both offer Most downloaded, Newest and Title (`views`).
- Search answers with shelves of releases and netlabels, and a view for each on its own.
- A release is a collection: Keep in the library puts it in your library and keeps it in step.
- Covers come from the release's own image when it ships one small enough, and the Archive's thumbnail otherwise (`node-art`, `source.cover`).
- Start Radio plays on through the same artist, then the same label, then the same genre (`source.radio`). Open in Browser and Copy Link go to the item's page on the Archive (`source.link`).
- A line over the roots says where the music comes from and links to the collection (`notice`, `notice-link`).

Each of these is sent only when `hello` listed it, so an older rox gets plain rows.

## How it plays

A track plays from the best file the Archive holds for it that rox decodes from a plugin, MP3 or Ogg Vorbis. A lossless original plays through one of its derivatives. Reads are range requests on a kept-alive connection to the storage node the download redirects to, so a seek in rox is a seek on the server. A file the release doesn't list is never fetched, whatever key rox sends.

## Being a good client

The plugin names itself in its User-Agent, as the Archive asks, and only reads what rox asks for. If you copy it for another catalogue, keep both habits, and check that catalogue's terms before you point a plugin at it.
