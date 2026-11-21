#!/usr/bin/env python3
"""Pack local Twemoji 72 px PNGs and Emojibase English metadata."""
import json
import re
import sys
from pathlib import Path

assets, data = map(Path, sys.argv[1:])
out = Path(__file__).resolve().parent


def key(sequence):
    # A presentation selector changes the display style, never the artwork.
    return sequence.replace("\ufe0f", "")


def read(path):
    return json.loads((data / path).read_text(encoding="utf-8"))


images = {}
with (out / "sprites.bin").open("wb") as archive:
    index = []
    for path in sorted(assets.glob("*.png")):
        sequence = "".join(chr(int(point, 16)) for point in path.stem.split("-"))
        normalized = key(sequence)
        assert normalized not in images, sequence
        images[normalized] = archive.tell()
        archive.write(path.read_bytes())
        index.append(f"{normalized}\t{archive.tell()}\n")
    assert index and archive.tell() < 5_000_000
(out / "sprites.tsv").write_text("".join(index), encoding="utf-8")

default_emoji = {
    entry["hexcode"] for entry in read("data.json")
    if entry.get("type") == 1 and "-" not in entry["hexcode"]
}


def emoji(entry):
    sequence = entry["unicode"]
    return key(sequence) if entry["hexcode"] in default_emoji else sequence


sets = [read(f"shortcodes/{source}.json") for source in ("github", "emojibase", "cldr")]
aliases = {}
rows = []
for entry in sorted(read("compact.json"), key=lambda entry: entry.get("order", 0)):
    sequence = emoji(entry)
    if key(sequence) not in images or "group" not in entry:
        continue

    same_tone = [""] * 5
    for item in [entry, *entry.get("skins", [])]:
        unicode = emoji(item)
        if key(unicode) not in images:
            continue
        tones = {ord(char) - 0x1f3fb for char in unicode if 0x1f3fb <= ord(char) <= 0x1f3ff}
        if item is not entry and len(tones) == 1:
            same_tone[tones.pop()] = unicode
        for priority, codes in enumerate(sets):
            names = codes.get(item["hexcode"], [])
            if isinstance(names, str):
                names = [names]
            for name in names:
                if re.fullmatch(r"[a-z0-9_+\-]+", name):
                    old = aliases.get(name)
                    if old is None or priority < old[0]:
                        aliases[name] = (priority, unicode)

    keywords = " ".join(tag.lower() for tag in entry.get("tags", []))
    columns = [str(entry["group"]), sequence, entry["label"], keywords, *same_tone]
    rows.append("\t".join(columns).rstrip("\t"))

assert rows and aliases
(out / "picker.tsv").write_text("\n".join(rows) + "\n", encoding="utf-8")
(out / "shortcodes.tsv").write_text(
    "".join(f"{name}\t{value[1]}\n" for name, value in sorted(aliases.items())), encoding="utf-8"
)
