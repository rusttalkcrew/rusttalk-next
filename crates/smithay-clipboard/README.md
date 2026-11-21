> **Vendored for RustTalk.** This is smithay-clipboard 0.7.3 from crates.io
> (MIT, see LICENSE), used through `[patch.crates-io]` in the workspace
> `Cargo.toml`. The only change: it accepts file drags (`text/uri-list`) and
> hands them to a callback set with `on_drag` (marked "RustTalk patch").
> winit 0.30 has no Wayland drag and drop, and compositors such as Hyprland
> offer a drag to only one data device per app, so a second one can't take
> them. When egui moves to a newer smithay-clipboard, this patch stops being
> used (Cargo warns): update or remove it then.

[![crates.io](https://img.shields.io/crates/v/smithay-clipboard.svg)](https://crates.io/crates/smithay-clipboard)


# Smithay Clipboard

This crate provides access to the Wayland clipboard for applications
already using some sort of GUI toolkit or a windowing library, like
[winit](https://github.com/rust-windowing/winit), since you should
have some surface around to receive keyboard/pointer events.

If you want to access clipboard from the CLI or to write clipboard manager,
this is not what you're looking for.

## Documentation

The documentation for the master branch is [available online](https://smithay.github.io/smithay-clipboard/).

The documentation for the releases can be found on [docs.rs](https://docs.rs/smithay-clipboard).

## Contact Us

If you have questions or want to discuss the project with us, join our chatroom on matrix:
[#sctk:matrix.org](https://matrix.to/#/#sctk:matrix.org).
