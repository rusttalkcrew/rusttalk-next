# Contributing to RustTalk

Thanks for helping. RustTalk is a private, end-to-end encrypted voice, video
and chat app, and it took a lot of work to get it clean and simple. These
rules keep it that way.

## How a change gets in

1. **Open an issue first.** Describe the problem or idea in plain words, with
   a screenshot if it is about something you see. You don't need to wait for
   a yes before building it: the review of your pull request decides.
   **Every issue that is being worked on must have an assignee.** If an issue
   is assigned to someone else, do not work on it. If nobody is assigned, ask
   to be assigned (or assign yourself) before you start. This stops two
   people from doing the same work.
2. **Work on a branch**, never on `main`.
3. **Open a pull request** that links the issue (`Closes #123`) and fills in
   the template: what changes, why, screenshots, how you tested it, what
   you are unsure about, and any **decisions for the maintainer** (for
   example a new dependency, a change to encryption, the protocol, privacy
   or the build). Listing them is enough; the maintainer confirms or changes
   them during review.
4. **Review.** Every pull request needs an approval from @vleeuwenmenno and
   green checks. Only the maintainer merges and makes releases.

Small pull requests get reviewed fastest. One change per pull request.

## Using AI tools

AI tools are welcome. They follow [AGENTS.md](AGENTS.md) automatically if
they support it (Claude Code, Codex, Cursor and others). You remain
responsible for what you submit:

- Read the whole change before you open the pull request, and make sure you
  can explain it in your own words.
- Say which tool you used in the pull request.
- Never let a tool push to `main`, merge or release.

## Getting started

You need Docker, Rust (installed automatically by `rust-toolchain.toml` via
[rustup](https://rustup.rs)) and, on Linux, the packages listed in
[docs/development.md](docs/development.md).

```sh
make dev      # start a local server in Docker
make client   # run the app (PROFILE=b make client for a second account)
make lint     # must pass before a pull request
make test     # must pass before a pull request
make e2e      # needed when the server, protocol or crypto changes
```

## What gets accepted

- Fixes and features that have an issue with you as assignee.
- Changes that keep everything users write end-to-end encrypted. The server
  must never see message text, names, files or other user content.
- UI that uses the shared UI kit, short friendly labels, and text in all four
  languages (English, German, Dutch, Japanese).
- Translations that a native speaker has checked.

## What gets declined

- Pull requests without an issue, without an assignee on that issue, or that
  do more than the issue asks.
- Redesigns, large refactors or new dependencies nobody asked for.
- Tracking, analytics or new connections to outside services.
- Code, images or sounds copied from other apps without a compatible license.

## Security problems

Do not open a public issue for a security problem. Use GitHub's
[private vulnerability reporting](https://github.com/rusttalkcrew/rusttalk-next/security/advisories/new)
instead.

## License

RustTalk is licensed under the [Apache License 2.0](LICENSE). By opening a
pull request you agree that your contribution is licensed under the same
terms (section 5 of the license). Anyone who redistributes RustTalk must
keep the [NOTICE](NOTICE) file, which credits the original authors.
