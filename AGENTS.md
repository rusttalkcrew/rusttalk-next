# Rules for AI agents working on RustTalk

These rules apply to every AI coding tool (Claude Code, Codex, Cursor, Copilot
and others) working in this repository. Read this whole file before you change
anything. `PROMPT.md` section 1 holds the binding principles; read it too.

## 1. Who are you working for?

Run `git config user.name`, `git config user.email` and, if available,
`gh api user --jq .login`.

- **The maintainer is `vleeuwenmenno` (Menno van Leeuwen).** Working for the
  maintainer, follow the normal workflow below.
- **Everyone else gets strict mode.** Most contributors are not programmers.
  Your job is to protect the quality of this codebase as much as to help them.
  If you cannot tell who you are working for, use strict mode.

## 2. Strict mode (everyone except the maintainer)

1. **Never push to `main`, merge a pull request, create a tag or start a
   release.** Work on a branch and open a pull request. Every pull request
   needs a review from @vleeuwenmenno; GitHub enforces this, so do not try
   to get around it.
2. **An issue comes first, with an assignee** (see section 3). If no issue
   describes the change, help the person write one, assign them, and then
   build it. Nobody needs to wait for permission to start: the maintainer's
   review of the pull request is the gate.
3. **One small change per pull request.** No drive-by refactors, renames,
   reformatting, dependency bumps or "while I was here" fixes.
4. **List decisions for the maintainer instead of stopping.** If the change
   touches any of these, keep going, but name each one under
   "Decisions for the maintainer" in the pull request so it gets confirmed
   in review:
   - cryptography, keys, sealing or anything in `docs/crypto.md`;
   - the protocol or server API (`services/`, `crates/core/src/client/`,
     `docs/protocol.md`), including anything that forces older apps to update;
   - privacy guarantees (`docs/privacy.md`) or new outbound connections;
   - deployment, release or CI files (`deploy/`, `.github/`, `Makefile`);
   - `LICENSE`, `NOTICE`, `PROMPT.md`, `AGENTS.md`, `CONTRIBUTING.md`;
   - new dependencies (name, license, why);
   - any choice the issue left open.
   Pick the safest reasonable option, explain it in one line, and let the
   maintainer change it in review. A listed decision never blocks opening
   the pull request.
5. **Explain in plain language.** The person you work with may not read code.
   Tell them what you changed, why, what you did not test, and what could go
   wrong. Write the pull request the same way, using the template.
6. **Never claim something works that you did not run.** Paste the real result
   of `make lint` and `make test`. If something fails, say so.
7. **Do not invent features.** Build exactly what the issue asks. If the issue
   is unclear, pick the simplest reading, build that, and list the question
   under "Decisions for the maintainer".

## 3. Rules for everyone

### Issues and assignees (mandatory, also for the maintainer)

Two people must never build the same thing. Before you write any code:

1. **Every fix or feature needs a GitHub issue.** No issue, no work. If none
   exists, create one first (`gh issue create`).
2. **Every issue you work on must have an assignee.** Check it first:
   `gh issue view <number> --json assignees`.
   - **Assigned to someone else:** stop. Do not work on it. Tell the person
     you work for, and comment on the issue if you want to help.
   - **Nobody assigned:** assign the person you work for before starting:
     `gh issue edit <number> --add-assignee @me`.
3. **Pull requests link their issue** with `Closes #<number>`. A pull request
   whose issue has no assignee, or a different assignee, is not merged.
4. **Stopping halfway?** Say so in the issue and remove the assignee, so
   someone else can pick it up.

### Principles

- **Zero-knowledge servers.** Everything a person writes or names (messages,
  files, names, avatars, topics, role names, pokes, and so on) is end-to-end
  encrypted on their device. The server only stores and forwards ciphertext.
  Never add a plaintext field for user content, and never log keys, plaintext,
  invite codes or recovery words.
- **No telemetry.** No analytics, crash reporting or new outbound connections
  unless `docs/privacy.md` lists them.
- **Smallest complete change.** The least code that fully solves the task.
  Prefer a proven library to custom code, and small code to a framework.
  Comments explain why, not what.

### Tooling

- Use the `Makefile` targets, not raw cargo commands: `make lint`, `make test`,
  `make e2e`, `make client`, `make dev`. They carry the right flags.
- `make lint` and `make test` must pass before you open a pull request.
  Changes to the server, protocol or crypto also need `make e2e`.
- Never commit secrets, `.env` files, keys, tokens or personal data.

### UI

- Use the shared UI kit in `crates/client/src/ui/` (theme colours, text
  helpers, buttons, cards, dialogs, menus, settings rows). No hard-coded
  colours or one-off styling.
- Short, friendly, non-technical labels, in the style of Discord or Teams.
- Every user-visible text goes through `t!("key")` and must be added to all
  four files in `crates/client/locales/` (en, de, nl, ja). Mark machine
  translations in the pull request so a native speaker can check them.
- Check the change in a dark and a light theme and attach screenshots.
- Keep accessibility labels.

### Responsive UI

- Every screen must work on phones, tablets and desktops. Build it from the
  kit's responsive parts: `ui::dialog` (a full-screen sheet on phones),
  `ui::setting_row` (stacks when narrow), `ui::trailing_row`, `ui::max_width`,
  `ui::fit`, `ui::Width` and `ui::TOUCH`. Details in `docs/development.md`.
- Never hard-code a width that can be wider than the window. Let text wrap.
- Check every UI change at phone (390x844), tablet (820x1180) and desktop
  (1100x700) size, in a dark and a light theme; screenshots at 390x844 and
  1100x700.

### Pull requests

- Title: what it does, in plain words. Body: follow
  `.github/pull_request_template.md`, with `Closes #<issue>`.
- **Keep the description short.** A few bullet points: what changes for
  people using RustTalk, why, how it was tested, and what is unsure. Aim for
  under 200 words. No walls of text, no copy of the diff, no list of every
  file touched. The reviewer reads the code; the description says what to
  look at.
- **Add screenshots whenever the change is visible** (before and after, dark
  and light theme). Drag the images into the pull request description on
  GitHub. An AI agent that cannot upload images saves them and gives the
  person their full paths, so they can attach them. No screenshot for a
  visible change? Say why.
- **Changelog.** Every user-visible change adds one short line under
  `## Unreleased` in `CHANGELOG.md`, in the same pull request (plain words, what
  changes for people, no PR numbers). The app reads this file for "What's new",
  so keep its format.
- Commits end with a `Co-Authored-By:` line naming the AI model that helped.
- Keep the branch up to date with `main` by rebasing, and resolve conflicts
  so that everyone's changes survive.

### Every release

1. Before tagging, move the changes shipping in this release from
   `## Unreleased` in `CHANGELOG.md` into a section with the new version and
   release date. Use that section for the GitHub release notes.
2. Update the **Shipped** timeline in `rusttalkcrew/website` (`roadmap.html`)
   from that changelog section. Include the website PR in the release PR so
   both updates can be tracked together.
3. Before marking the release complete, verify the published roadmap and
   release notes match the changelog's version, date, and shipped changes.
   Apply this checklist to patch releases too.

## 4. Legal

RustTalk is licensed under Apache-2.0. Keep `LICENSE` and `NOTICE` intact
and shipped with every build. Do not copy code, sounds, images or text from
other projects unless their license allows it and the source is credited.
New dependencies must be compatible with Apache-2.0.
