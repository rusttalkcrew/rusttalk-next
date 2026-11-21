//! Files dropped on the window on Wayland, where winit 0.30 delivers no drops. The
//! patched clipboard (crates/smithay-clipboard) takes the drag, and this hands it to
//! egui the way winit does on Windows, macOS and X11, so the drop zone and the
//! attach path are the same everywhere.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use smithay_clipboard::Drag;

#[derive(Default)]
struct Drops {
    hovering: bool,
    dropped: Vec<PathBuf>,
}

static DROPS: Mutex<Drops> = Mutex::new(Drops {
    hovering: false,
    dropped: Vec::new(),
});

/// Take file drags over this window. Not a Wayland window (X11, which winit
/// handles itself): the clipboard never reports drags, so nothing is delivered twice.
pub fn install(cc: &eframe::CreationContext) {
    let Ok(RawWindowHandle::Wayland(window)) = cc.window_handle().map(|handle| handle.as_raw())
    else {
        return;
    };
    let ours = window.surface.as_ptr() as usize;
    let ctx = cc.egui_ctx.clone();
    smithay_clipboard::on_drag(move |drag| {
        update(drag, ours);
        ctx.request_repaint();
    });
}

/// More files than anyone drops at once; the rest of a list is ignored.
const MAX_FILES: usize = 100;

fn update(drag: Drag, ours: usize) {
    let files = match &drag {
        Drag::Drop(surface, list) if *surface == ours => {
            let mut files = crate::paste::file_paths(list);
            files.truncate(MAX_FILES);
            tracing::info!(files = files.len(), "files dropped on the window");
            files
        }
        _ => Vec::new(),
    };
    let mut drops = DROPS.lock().unwrap_or_else(|poison| poison.into_inner());
    drops.hovering = matches!(drag, Drag::Enter(surface) if surface == ours);
    drops.dropped.extend(files);
}

/// Add the drag to this frame's input, as winit would.
pub fn input(raw: &mut egui::RawInput) {
    let mut drops = DROPS.lock().unwrap_or_else(|poison| poison.into_inner());
    if drops.hovering && raw.hovered_files.is_empty() {
        raw.hovered_files.push(egui::HoveredFile::default());
    }
    let dropped = drops.dropped.drain(..);
    (raw.dropped_files).extend(dropped.map(|path| Arc::new(File(path)) as egui::DroppedFileHandle));
}

#[derive(Debug)]
struct File(PathBuf);

impl egui::DroppedFile for File {
    fn path(&self) -> &Path {
        &self.0
    }

    fn bytes(&self) -> Result<Vec<u8>, String> {
        std::fs::read(&self.0).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drags_become_hovered_then_dropped_files() {
        let (ours, other) = (1, 2);
        let frame = || {
            let mut raw = egui::RawInput::default();
            input(&mut raw);
            let dropped: Vec<_> = raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_owned())
                .collect();
            (raw.hovered_files.len(), dropped)
        };
        update(Drag::Enter(other), ours);
        assert_eq!(frame(), (0, vec![]));
        update(Drag::Enter(ours), ours);
        assert_eq!(frame(), (1, vec![]));
        assert_eq!(frame(), (1, vec![]), "hover lasts until the drag ends");
        update(
            Drag::Drop(ours, b"file:///tmp/my%20file.png\r\n".to_vec()),
            ours,
        );
        update(Drag::Leave, ours);
        assert_eq!(frame(), (0, vec![PathBuf::from("/tmp/my file.png")]));
        assert_eq!(frame(), (0, vec![]), "a drop is delivered once");
        update(Drag::Drop(other, b"file:///tmp/a\r\n".to_vec()), ours);
        assert_eq!(frame(), (0, vec![]), "drops on other surfaces are ignored");
        let many: String = (0..150).map(|i| format!("file:///tmp/{i}\r\n")).collect();
        update(Drag::Drop(ours, many.into_bytes()), ours);
        assert_eq!(frame().1.len(), MAX_FILES);
    }
}
