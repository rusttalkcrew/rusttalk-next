# Bundled emoji

The 72 px PNGs are [Twemoji v17.0.3](https://github.com/jdecked/twemoji/tree/v17.0.3).
Twemoji graphics are by Twitter, Inc. and other contributors and licensed under
[CC BY 4.0](TWEMOJI-LICENSE.txt). The original PNGs are unchanged. The archive
concatenates the PNGs and uses end offsets from `sprites.tsv`, so the client
can decode a single sprite as egui needs it.

The English names, tags, categories and shortcodes come from
[Emojibase Data 17.0.0](https://www.npmjs.com/package/emojibase-data/v/17.0.0)
([MIT](EMOJIBASE-LICENSE.txt)). Conflicting aliases prefer GitHub, then
Emojibase, then CLDR. Picker tone choices use the same tone for each person
in a multi-person emoji, where that variant exists.

To regenerate, unpack the Twemoji source archive and the Emojibase Data npm
tarball at those versions, then run from the repo root:

```sh
python3 crates/client/assets/emoji/generate.py twemoji-17.0.3/assets/72x72 package/en
```
