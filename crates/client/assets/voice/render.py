"""Render a voice pack's lines to the Ogg files the app ships.

    python render.py en_heart      # render new or changed lines of the "en_heart" pack
    python render.py en_heart --all  # render every line again

Reads <pack>/phrases.toml and writes <pack>/<sound>.ogg (48 kHz mono Vorbis, the
rate the app mixes at). <pack>/rendered.json remembers what each file was made
from, so only lines whose words or voice changed are rendered again, and files
whose line was removed are deleted. See docs/voice-packs.md for the setup.
"""

import argparse
import hashlib
import json
import sys
import tomllib
from pathlib import Path

import numpy as np
import soundfile as sf
from kokoro import KPipeline
from scipy.signal import resample_poly

KOKORO_RATE = 24_000
APP_RATE = 48_000
# Quieter than this, relative to the loudest sample, counts as silence to trim.
TRIM_DB = -40.0
# A little air before and after the words, in seconds.
PAD = 0.03
# Loudness the lines are brought to: about as loud as the chimes at the same volume.
TARGET_RMS_DB = -24.0
PEAK_DB = -9.0
# Bump when the processing below changes, so every line is rendered again.
PROCESSING = 1


def db(value: float) -> float:
    return 10 ** (value / 20)


def process(audio: np.ndarray) -> np.ndarray:
    loud = np.flatnonzero(np.abs(audio) > np.abs(audio).max() * db(TRIM_DB))
    pad = int(PAD * KOKORO_RATE)
    audio = audio[max(loud[0] - pad, 0) : loud[-1] + pad]
    audio = resample_poly(audio, APP_RATE // KOKORO_RATE, 1)
    rms = np.sqrt(np.mean(audio**2))
    audio = audio * min(db(TARGET_RMS_DB) / rms, db(PEAK_DB) / np.abs(audio).max())
    fade = int(0.01 * APP_RATE)
    audio[:fade] *= np.linspace(0, 1, fade)
    audio[-fade:] *= np.linspace(1, 0, fade)
    return audio.astype(np.float32)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("pack", help="pack folder next to this script, e.g. en_heart")
    parser.add_argument("--all", action="store_true", help="render every line again")
    args = parser.parse_args()

    folder = Path(__file__).parent / args.pack
    config = tomllib.loads((folder / "phrases.toml").read_text(encoding="utf-8"))
    voice, lines = config["voice"], config["lines"]
    state_file = folder / "rendered.json"
    state = {} if args.all or not state_file.exists() else json.loads(state_file.read_text())

    pipeline = None
    for name, text in lines.items():
        recipe = [text, voice["voice"], voice["lang"], voice["speed"], PROCESSING]
        digest = hashlib.sha256(json.dumps(recipe).encode()).hexdigest()[:16]
        target = folder / f"{name}.ogg"
        if state.get(name) == digest and target.exists():
            continue
        pipeline = pipeline or KPipeline(lang_code=voice["lang"], repo_id="hexgrad/Kokoro-82M")
        parts = [r.audio.numpy() for r in pipeline(text, voice=voice["voice"], speed=voice["speed"])]
        sf.write(target, process(np.concatenate(parts)), APP_RATE, format="OGG", subtype="VORBIS")
        state[name] = digest
        print(f"rendered {target.name}: {text!r}")

    for name in set(state) - set(lines):
        (folder / f"{name}.ogg").unlink(missing_ok=True)
        del state[name]
        print(f"removed {name}.ogg")
    state_file.write_text(json.dumps(dict(sorted(state.items())), indent=2) + "\n")


if __name__ == "__main__":
    sys.exit(main())
