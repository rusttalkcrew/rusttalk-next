# Voice packs

People can have their call sounds spoken instead of chimed: Settings → Sounds →
Voice announcements. The chimes stay the default. A voice pack is a set of short
spoken lines, one per call sound, rendered ahead of time on a contributor's
computer and committed as small Ogg files. The app only plays them; it never
generates speech or downloads anything for this.

Everything lives in `crates/client/assets/voice/`:

| File | What it is |
| --- | --- |
| `render.py` | Renders a pack's lines to Ogg files. |
| `requirements.txt` | The exact Python packages `render.py` was run with. |
| `<pack>/phrases.toml` | The voice settings and the words for each sound. |
| `<pack>/<sound>.ogg` | The rendered lines the app ships (48 kHz mono Vorbis). |
| `<pack>/rendered.json` | What each file was rendered from, so only changes are redone. |

There are two English packs, one per voice: `en_heart` (Kokoro voice `af_heart`)
and `en_michael` (`am_michael`). A sound without a line in the selected pack keeps
its chime, so nothing goes quiet while a pack is not rendered yet.

## The voice

Lines are spoken by [Kokoro-82M](https://huggingface.co/hexgrad/Kokoro-82M), an open
text-to-speech model under Apache-2.0, the same license as RustTalk. Only the
rendered files are committed; the model stays on the computer that renders. The
voice, language, speed and the model revision used are written in each pack's
`phrases.toml`.

## Setup

Once per computer. Needs about 1.5 GB of disk space (Python packages and the
330 MB model). Tested on Windows 11; Linux and macOS work the same way.

1. Install [uv](https://docs.astral.sh/uv/getting-started/installation/), which also
   fetches the right Python.
2. Create an environment outside the repository and install the packages:

   ```sh
   uv venv --python 3.12 ~/.venvs/rusttalk-voice
   uv pip install --python ~/.venvs/rusttalk-voice -r crates/client/assets/voice/requirements.txt \
     --extra-index-url https://download.pytorch.org/whl/cpu --index-strategy unsafe-best-match
   ```

   The extra index installs PyTorch for the CPU only, which is much smaller and
   plenty fast for a few short lines.
3. The first run downloads the model from Hugging Face into its cache
   (`~/.cache/huggingface`). Kokoro 0.9.4 always takes the newest model revision; if
   it differs from `model_revision` in `phrases.toml`, listen to the result and update
   that line when you commit.

Run the script with the environment's Python (`~/.venvs/rusttalk-voice/Scripts/python`
on Windows, `~/.venvs/rusttalk-voice/bin/python` elsewhere):

```sh
python crates/client/assets/voice/render.py en_heart
python crates/client/assets/voice/render.py en_michael
```

It renders only new or changed lines and removes files whose line was removed. Add
`--all` to render everything again, for example after changing the voice.

Rendering again gives lines that sound the same but are not identical to the byte:
Ogg files get a random stream number, and the model's arithmetic varies slightly
between runs. So only commit the files whose line you meant to change.

## Adding a sound

When a new call sound (a `Cue` in `crates/client/src/sounds.rs`) is added:

1. Add a line to each pack's `phrases.toml` under `[lines]`: the cue's name in
   snake_case and the words, for example `hand_raised = "Hand raised"`. Keep it short
   and generic, like the others.
2. Run `render.py` for each pack and listen to the new file.
3. Add the cue to the `every_line!` list in `sounds.rs` (all packs share it), or to
   a pack's own `lines!` list if it has one.
4. Commit the `.ogg`, `phrases.toml` and `rendered.json` changes.

`make test` checks that each pack's files and `phrases.toml` list the same sounds and
that every file decodes. If you cannot render, leave the pack alone: the new sound
keeps its chime until someone renders it.

## Adding a pack

For another voice or language:

1. Copy `en_heart/phrases.toml` to a new folder, for example `de_martin/`, and set
   `voice` and `lang`. Kokoro's
   voices and language codes are listed in its
   [VOICES.md](https://huggingface.co/hexgrad/Kokoro-82M/blob/main/VOICES.md). Not
   every language there has good quality; listen before committing.
2. Translate the lines if needed and run `render.py de_martin`.
3. In `sounds.rs`, add a `VoicePack` variant before `Off`, add it to
   `VoicePack::ALL`, give it a label, and map it to `every_line!("de_martin")` in
   `VoicePack::files`. Add the folder to the match in the
   `voice_packs_match_their_phrases` test.
4. Add the label to all four files in `crates/client/locales/`.
5. If the voice comes from another model, check that its license allows shipping the
   audio and add it to `NOTICE`.
