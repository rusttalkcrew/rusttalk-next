//! The system's pickers behind `files::Files::ask` (docs/mobile.md, File picker).
//!
//! Photos opens `PHPickerViewController`: it runs outside the app and hands over only
//! what was picked, so it needs no photo library permission. Files opens
//! `UIDocumentPickerViewController` with copies (`asCopy`), so no security-scoped
//! access is needed. Each picked file is copied into its own temporary folder under
//! its own name (`files::keep_picked`). Never log file names here.

use std::{
    cell::OnceCell,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
};

use block2::{Block, RcBlock};
use objc2::{
    MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::{Allocated, Retained},
    runtime::{AnyClass, AnyObject, NSObject, NSObjectProtocol},
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSArray, NSError, NSItemProvider, NSOperationQueue, NSString, NSURL};
use objc2_ui_kit::UIViewController;

// The picker and UTType classes are looked up by name; nothing else links these.
#[link(name = "PhotosUI", kind = "framework")]
unsafe extern "C" {}
#[link(name = "UniformTypeIdentifiers", kind = "framework")]
unsafe extern "C" {}

/// What a pick may choose.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    /// Pictures the app can read: PNG, JPEG, GIF, WebP.
    Pictures,
    /// One .zip file.
    Zip,
    /// Any file, photo or video.
    Any,
}

/// The one pick waiting for an answer, and what it asked for; `files::Files::ask`
/// opens one at a time.
static PENDING: Mutex<Option<(mpsc::Sender<Vec<PathBuf>>, Pick)>> = Mutex::new(None);

/// Asks for files and waits for them: empty when cancelled or when no picker
/// opened. Call off the main thread, where the pickers show.
pub fn pick(what: Pick, many: bool) -> Vec<PathBuf> {
    let (sender, answer) = mpsc::channel();
    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some((sender, what));
    let labels = [
        t!("files.from_photos"),
        t!("files.from_files"),
        t!("common.cancel"),
    ];
    let show = RcBlock::new(move || {
        let opened = MainThreadMarker::new().is_some_and(|mtm| match what {
            Pick::Zip => documents(mtm, what, many),
            Pick::Pictures | Pick::Any => choose(mtm, what, many, labels),
        });
        if !opened {
            tracing::warn!("the file picker didn't open");
            finish(Vec::new());
        }
    });
    // SAFETY: the main queue runs the block on the main thread; it holds only Copy
    // values, so it may move there.
    unsafe { NSOperationQueue::mainQueue().addOperationWithBlock(&show) };
    answer.recv().unwrap_or_default()
}

/// Answers the waiting pick, once.
fn finish(paths: Vec<PathBuf>) {
    let pending = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some((sender, _)) = pending {
        let _ = sender.send(paths);
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and this has no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RustTalkPickerDelegate"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    // UIKit calls these on the main thread. Swiping a picker away is off
    // (`modalInPresentation`), so Cancel always comes through one of them.
    impl Delegate {
        #[unsafe(method(picker:didFinishPicking:))]
        fn photos_picked(&self, picker: &AnyObject, results: &NSArray<AnyObject>) {
            let done: Option<&Block<dyn Fn()>> = None;
            // SAFETY: PHPickerViewController leaves dismissing itself to its delegate.
            unsafe {
                let _: () = msg_send![picker, dismissViewControllerAnimated: true, completion: done];
            }
            let pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
            let what = pending.as_ref().map_or(Pick::Any, |(_, what)| *what);
            drop(pending);
            load_photos(results, what);
        }

        #[unsafe(method(documentPicker:didPickDocumentsAtURLs:))]
        fn documents_picked(&self, _picker: &AnyObject, urls: &NSArray<NSURL>) {
            // `asCopy`: the copies are the app's own, so they are removed once copied,
            // off the main thread, as big files take a while.
            let picked: Vec<PathBuf> = urls.iter().filter_map(|url| file_path(&url)).collect();
            std::thread::spawn(move || {
                finish(picked.iter().filter_map(|from| keep(from, None, true)).collect());
            });
        }

        #[unsafe(method(documentPickerWasCancelled:))]
        fn documents_cancelled(&self, _picker: &AnyObject) {
            finish(Vec::new());
        }
    }
);

/// The pickers hold their delegate weakly: this one lives as long as the app.
fn delegate(mtm: MainThreadMarker) -> Retained<Delegate> {
    thread_local! {
        static DELEGATE: OnceCell<Retained<Delegate>> = const { OnceCell::new() };
    }
    DELEGATE.with(|delegate| {
        delegate
            .get_or_init(|| {
                let this = Delegate::alloc(mtm).set_ivars(());
                // SAFETY: NSObject's init.
                unsafe { msg_send![super(this), init] }
            })
            .clone()
    })
}

/// Photos or Files, as an action sheet; Cancel answers with nothing.
fn choose(mtm: MainThreadMarker, what: Pick, many: bool, labels: [&'static str; 3]) -> bool {
    let (Some(alerts), Some(actions)) = (
        AnyClass::get(c"UIAlertController"),
        AnyClass::get(c"UIAlertAction"),
    ) else {
        return false;
    };
    let [photos_label, files_label, cancel_label] = labels;
    let none: Option<&NSString> = None;
    // SAFETY: UIKit's documented constructors and properties, on the main thread,
    // with the argument types they take (action sheet style 0; action styles 0
    // default, 1 cancel). Each action copies its handler block.
    unsafe {
        let sheet: Option<Retained<AnyObject>> = msg_send![
            alerts,
            alertControllerWithTitle: none,
            message: none,
            preferredStyle: 0isize
        ];
        let Some(sheet) = sheet else { return false };
        for (label, style, then) in [
            (photos_label, 0isize, Some(true)),
            (files_label, 0, Some(false)),
            (cancel_label, 1, None),
        ] {
            let handler = RcBlock::new(move |_: *mut AnyObject| {
                let opened = match then {
                    Some(true) => photos(mtm, what, many),
                    Some(false) => documents(mtm, what, many),
                    None => false,
                };
                if !opened {
                    finish(Vec::new());
                }
            });
            let title = NSString::from_str(label);
            let action: Option<Retained<AnyObject>> =
                msg_send![actions, actionWithTitle: &*title, style: style, handler: &*handler];
            if let Some(action) = action {
                let _: () = msg_send![&*sheet, addAction: &*action];
            }
        }
        present(&sheet)
    }
}

/// The photo library picker; `Pictures` shows only pictures.
fn photos(mtm: MainThreadMarker, what: Pick, many: bool) -> bool {
    let (Some(configs), Some(pickers)) = (
        AnyClass::get(c"PHPickerConfiguration"),
        AnyClass::get(c"PHPickerViewController"),
    ) else {
        return false;
    };
    // SAFETY: PhotosUI's documented initializers and properties, on the main thread,
    // with the argument types they take.
    unsafe {
        let config: Allocated<AnyObject> = msg_send![configs, alloc];
        let config: Option<Retained<AnyObject>> = msg_send![config, init];
        let Some(config) = config else { return false };
        // 0 is no limit.
        let _: () = msg_send![&*config, setSelectionLimit: if many { 0isize } else { 1 }];
        // Compatible: HEIC photos come as JPEG, which the app reads.
        let _: () = msg_send![&*config, setPreferredAssetRepresentationMode: 2isize];
        if what == Pick::Pictures
            && let Some(filters) = AnyClass::get(c"PHPickerFilter")
        {
            let filter: Option<Retained<AnyObject>> = msg_send![filters, imagesFilter];
            if let Some(filter) = filter {
                let _: () = msg_send![&*config, setFilter: &*filter];
            }
        }
        let picker: Allocated<AnyObject> = msg_send![pickers, alloc];
        let picker: Option<Retained<AnyObject>> =
            msg_send![picker, initWithConfiguration: &*config];
        let Some(picker) = picker else { return false };
        let _: () = msg_send![&*picker, setDelegate: &*delegate(mtm)];
        let _: () = msg_send![&*picker, setModalInPresentation: true];
        present(&picker)
    }
}

/// The Files picker, limited to the kinds of file asked for.
fn documents(mtm: MainThreadMarker, what: Pick, many: bool) -> bool {
    let (Some(pickers), Some(types)) = (
        AnyClass::get(c"UIDocumentPickerViewController"),
        AnyClass::get(c"UTType"),
    ) else {
        return false;
    };
    let identifiers: &[&str] = match what {
        Pick::Pictures => &[
            "public.png",
            "public.jpeg",
            "com.compuserve.gif",
            "org.webmproject.webp",
        ],
        Pick::Zip => &["public.zip-archive"],
        // Not folders or packages, which aren't one file.
        Pick::Any => &["public.data"],
    };
    // SAFETY: UIKit's and UniformTypeIdentifiers' documented constructors and
    // properties, on the main thread, with the argument types they take.
    unsafe {
        let types: Vec<Retained<AnyObject>> = identifiers
            .iter()
            .filter_map(|identifier| {
                let identifier = NSString::from_str(identifier);
                let kind: Option<Retained<AnyObject>> =
                    msg_send![types, typeWithIdentifier: &*identifier];
                kind
            })
            .collect();
        let types = NSArray::from_retained_slice(&types);
        let picker: Allocated<AnyObject> = msg_send![pickers, alloc];
        let picker: Option<Retained<AnyObject>> =
            msg_send![picker, initForOpeningContentTypes: &*types, asCopy: true];
        let Some(picker) = picker else { return false };
        let _: () = msg_send![&*picker, setDelegate: &*delegate(mtm)];
        let _: () = msg_send![&*picker, setAllowsMultipleSelection: many];
        let _: () = msg_send![&*picker, setModalInPresentation: true];
        present(&picker)
    }
}

/// Shows `controller` over whatever shows now; false if nothing can present it.
fn present(controller: &AnyObject) -> bool {
    let Some(mut top) = super::key_window().and_then(|window| window.rootViewController()) else {
        return false;
    };
    while let Some(next) = top.presentedViewController() {
        top = next;
    }
    let done: Option<&Block<dyn Fn()>> = None;
    // SAFETY: UIKit's documented properties and presentation, on the main thread
    // (`key_window` checked it), with the argument types they take.
    unsafe {
        // iPads show action sheets as popovers, which need a place to point from.
        let popover: Option<Retained<AnyObject>> =
            msg_send![controller, popoverPresentationController];
        if let (Some(popover), Some(view)) = (popover, top.view()) {
            let bounds = view.bounds();
            let middle = CGPoint::new(bounds.size.width / 2.0, bounds.size.height / 2.0);
            let _: () = msg_send![&*popover, setSourceView: &*view];
            let _: () = msg_send![&*popover, setSourceRect: CGRect::new(middle, CGSize::ZERO)];
            let _: () = msg_send![&*popover, setPermittedArrowDirections: 0usize];
        }
        let top: &UIViewController = &top;
        let _: () =
            msg_send![top, presentViewController: controller, animated: true, completion: done];
    }
    true
}

/// Types asked of a picked photo, best first (with compatible representations a
/// HEIC photo also offers JPEG), then of a video. Pictures stop before the video.
const PHOTO_TYPES: [&str; 6] = [
    "com.compuserve.gif",
    "org.webmproject.webp",
    "public.png",
    "public.jpeg",
    "public.image",
    "public.movie",
];

/// Copies each picked photo out of the library, then answers with all of them, in
/// the order picked. iOS loads them on its own threads.
fn load_photos(results: &NSArray<AnyObject>, what: Pick) {
    let providers: Vec<Retained<NSItemProvider>> = results
        .iter()
        .filter_map(|result| {
            // SAFETY: each result is a PHPickerResult, whose `itemProvider` is an
            // NSItemProvider.
            let provider: Option<Retained<NSItemProvider>> =
                unsafe { msg_send![&*result, itemProvider] };
            provider
        })
        .collect();
    if providers.is_empty() {
        return finish(Vec::new());
    }
    let loaded = Arc::new(Mutex::new((vec![None; providers.len()], providers.len())));
    for (at, provider) in providers.iter().enumerate() {
        let types = match what {
            Pick::Pictures => &PHOTO_TYPES[..5],
            Pick::Zip | Pick::Any => &PHOTO_TYPES[..],
        };
        let Some(kind) = types
            .iter()
            .map(|kind| NSString::from_str(kind))
            .find(|kind| provider.hasItemConformingToTypeIdentifier(kind))
        else {
            tracing::warn!("a picked photo has no file the app can use");
            loaded_one(&loaded, at, None);
            continue;
        };
        let name = provider.suggestedName().map(|name| name.to_string());
        let loaded = loaded.clone();
        // The file is deleted once this returns, so it is copied here.
        let handler = RcBlock::new(move |url: *mut NSURL, error: *mut NSError| {
            // SAFETY: Foundation passes a file URL, or nil and an error.
            let (url, error) = unsafe { (url.as_ref(), error.as_ref()) };
            if let Some(error) = error {
                tracing::warn!(domain = %error.domain(), code = error.code(), "loading a picked photo failed");
            }
            loaded_one(
                &loaded,
                at,
                url.and_then(file_path)
                    .and_then(|from| keep(&from, name.as_deref(), false)),
            );
        });
        // SAFETY: the handler holds only Send values, as it runs on another thread.
        let _ = unsafe {
            provider.loadFileRepresentationForTypeIdentifier_completionHandler(&kind, &handler)
        };
    }
}

/// One photo was copied (or not); the last one answers the pick.
fn loaded_one(loaded: &Mutex<(Vec<Option<PathBuf>>, usize)>, at: usize, path: Option<PathBuf>) {
    let mut loaded = loaded.lock().unwrap_or_else(|e| e.into_inner());
    let (paths, left) = &mut *loaded;
    paths[at] = path;
    *left -= 1;
    if *left == 0 {
        finish(paths.drain(..).flatten().collect());
    }
}

/// Copies a picked file into a temporary folder of its own. `name`: the photo's
/// own name, without the extension of the file iOS made. `ours`: the picker made the
/// file for the app, so it is removed after.
fn keep(from: &Path, name: Option<&str>, ours: bool) -> Option<PathBuf> {
    let name = match (name, from.extension()) {
        (Some(name), Some(extension)) => format!("{name}.{}", extension.to_string_lossy()),
        (Some(name), None) => name.to_owned(),
        (None, _) => from.file_name()?.to_string_lossy().into_owned(),
    };
    let kept = crate::files::keep_picked(from, &name)
        .inspect_err(|error| tracing::warn!(%error, "copying a picked file failed"))
        .ok();
    if ours && let Err(error) = fs::remove_file(from) {
        tracing::warn!(%error, "removing the picker's copy failed");
    }
    kept
}

fn file_path(url: &NSURL) -> Option<PathBuf> {
    Some(PathBuf::from(url.path()?.to_string()))
}
