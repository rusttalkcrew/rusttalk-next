//! iOS: draw inside the safe area, so taps land where the app draws.
//!
//! winit 0.30 reports the window's size without the notch and home indicator
//! (`inner_size`), and egui-wgpu sizes its Metal drawable from that. The Metal layer
//! covers the whole screen and stretches that smaller drawable over it, while touches
//! and egui's screen (`outer_size`) are whole-screen points: everything below the top
//! drew further down than egui thought it was. Here the layer is moved onto the safe
//! area, where the drawable fits it exactly, and egui gets that area as its screen,
//! with touches moved into it. VoiceOver gets the same move through AccessKit's root
//! transform (docs/mobile.md, iPhone).
//!
//! The on-screen keyboard covers the bottom of the safe area: its overlap becomes
//! egui's bottom safe-area inset, and `frame` draws the app above it, as Android
//! does with its insets. The strips outside the safe area show the theme's
//! background, and the keyboard's Return reaches egui as Enter. While the recovery
//! words show, the app switcher's snapshot shows that background instead.

pub mod voice;

use std::{
    cell::{Cell, RefCell},
    ptr::NonNull,
    sync::{
        Once, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use block2::RcBlock;
use eframe::egui::{
    self, Color32, Context, Event, Key, Modifiers, Pos2, RawInput, Rect, SafeAreaInsets, Ui,
    UiBuilder, Vec2, accesskit::Affine, epaint::MarginF32,
};
use objc2::{
    MainThreadMarker, MainThreadOnly as _, msg_send,
    rc::Retained,
    runtime::{AnyClass, AnyObject, Imp, NSObjectProtocol, Sel},
    sel,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSOperationQueue, NSString};
use objc2_quartz_core::{CALayer, CATransaction};
use objc2_ui_kit::{
    UIApplication, UIApplicationDidBecomeActiveNotification,
    UIApplicationWillResignActiveNotification, UIColor, UIView, UIViewAutoresizing, UIWindow,
};
use winit::platform::ios::{StatusBarStyle, WindowExtIOS as _};

/// How long frames keep coming after the keyboard moved, while it slides.
const KEYBOARD_SETTLE: Duration = Duration::from_millis(600);
/// How often a frame looks at the keyboard while a field has focus: nothing tells
/// the app when it moves.
const KEYBOARD_POLL: Duration = Duration::from_millis(100);

thread_local! {
    /// Where the safe area starts, in egui points (from `input`, for `accessibility`).
    static OFFSET: Cell<Vec2> = const { Cell::new(Vec2::ZERO) };
    /// How far the keyboard covers the safe area, in egui points, and since when.
    static KEYBOARD: Cell<(f32, Option<Instant>)> = const { Cell::new((0.0, None)) };
    /// The background last given to the window, and whether it was dark.
    static BACKGROUND: Cell<Option<(Color32, bool)>> = const { Cell::new(None) };
    /// The recovery words show (`secure_window`).
    static SECURE: Cell<bool> = const { Cell::new(false) };
    /// What covers the window while the app is inactive and `SECURE`.
    static COVER: RefCell<Option<Retained<UIView>>> = const { RefCell::new(None) };
}

/// The keyboard's Return was typed (see `watch_return`).
static RETURN: AtomicBool = AtomicBool::new(false);
static INSERT_TEXT: OnceLock<Imp> = OnceLock::new();

/// Call from `raw_input_hook` every frame, before egui sees the input.
pub fn input(ctx: &Context, raw_input: &mut RawInput) {
    let Some(window) = key_window() else { return };
    let bounds = window.bounds();
    let insets = window.safeAreaInsets();
    // The same rectangle winit's `inner_size` measures.
    let safe = CGRect::new(
        CGPoint::new(bounds.origin.x + insets.left, bounds.origin.y + insets.top),
        CGSize::new(
            bounds.size.width - insets.left - insets.right,
            bounds.size.height - insets.top - insets.bottom,
        ),
    );
    let layer = metal_layer(&window);
    log_once(&window, layer.as_deref(), raw_input);
    if let Some(layer) = layer
        && layer.frame() != safe
    {
        // A sublayer animates frame changes unless told not to. The layer goes back to
        // the whole view when the view's bounds change (raw-window-metal keeps it in
        // sync), so this runs again on the next frame after a rotation.
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        layer.setFrame(safe);
        CATransaction::commit();
    }
    // UIKit points to egui points.
    let zoom = ctx.zoom_factor();
    let offset = Vec2::new(safe.origin.x as f32, safe.origin.y as f32) / zoom;
    let size = Vec2::new(safe.size.width as f32, safe.size.height as f32) / zoom;
    OFFSET.set(offset);
    raw_input.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, size));
    // Inside the safe area already; only the keyboard is left to keep clear.
    let covered = keyboard_overlap(&window, safe) / zoom;
    let (last, _) = KEYBOARD.get();
    if (covered - last).abs() >= 1.0 {
        tracing::info!(points = covered as u32, "iOS keyboard");
        KEYBOARD.set((covered, Some(Instant::now())));
    }
    raw_input.safe_area_insets = Some(SafeAreaInsets(MarginF32 {
        bottom: covered,
        ..MarginF32::ZERO
    }));
    watch_return();
    if RETURN.swap(false, Ordering::Relaxed) {
        for pressed in [true, false] {
            raw_input.events.push(Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: Modifiers::NONE,
            });
        }
    }
    for event in &mut raw_input.events {
        match event {
            Event::PointerMoved(pos)
            | Event::PointerButton { pos, .. }
            | Event::Touch { pos, .. }
            | Event::MiddleClickPaste { pos, .. } => *pos -= offset,
            _ => {}
        }
    }
}

/// Call once per frame from `ui`. accesskit_ios places nodes and answers VoiceOver's
/// hit tests in pixels of winit's whole-screen view, through the root node's
/// transform, which egui sets to its scale alone each frame: add the safe area's
/// offset, so VoiceOver's boxes and taps line up with the moved drawing.
pub fn accessibility(ctx: &Context) {
    let scale = f64::from(ctx.pixels_per_point());
    let offset = OFFSET.get();
    let transform = Affine::translate((f64::from(offset.x) * scale, f64::from(offset.y) * scale))
        * Affine::scale(scale);
    ctx.accesskit_node_builder(egui::accesskit_root_id(), |root| {
        root.set_transform(transform)
    });
}

/// Bring the keyboard back for a tap on the focused field, if it was put away:
/// winit's view takes text input while it is the first responder.
pub fn show_keyboard() {
    let Some(view) = key_window().and_then(|window| root_view(&window)) else {
        return;
    };
    if !view.isFirstResponder() {
        let _ = view.becomeFirstResponder();
    }
}

/// How far the docked keyboard reaches into `safe`, in UIKit points (0 without it).
/// UIKit's keyboard layout guide follows it; hidden, it is the bottom safe area.
fn keyboard_overlap(window: &UIWindow, safe: CGRect) -> f32 {
    let Some(view) = root_view(window) else {
        return 0.0;
    };
    let keyboard = view.keyboardLayoutGuide().layoutFrame();
    let top = view.convertPoint_toView(keyboard.origin, Some(window)).y;
    (safe.origin.y + safe.size.height - top).max(0.0) as f32
}

/// Draw the app above the keyboard, keep the focused field in view when the
/// keyboard moves, and give the strips outside the safe area the theme's
/// background (with a matching status bar).
pub fn frame(ui: &mut Ui, window: Option<&winit::window::Window>, add: impl FnOnce(&mut Ui)) {
    let ctx = ui.ctx().clone();
    let fill = ui.visuals().panel_fill;
    let dark = ui.visuals().dark_mode;
    if BACKGROUND.get() != Some((fill, dark)) {
        BACKGROUND.set(Some((fill, dark)));
        background(fill);
        if let Some(window) = window {
            window.set_preferred_status_bar_style(if dark {
                StatusBarStyle::LightContent
            } else {
                StatusBarStyle::DarkContent
            });
        }
    }
    let (_, moved) = KEYBOARD.get();
    let settling = moved.is_some_and(|at| at.elapsed() < KEYBOARD_SETTLE);
    if settling {
        // The middle, so the button under the field shows too.
        if let Some(field) = ctx
            .memory(|memory| memory.focused())
            .and_then(|id| ctx.read_response(id))
        {
            field.scroll_to_me(Some(egui::Align::Center));
        }
    }
    if settling || ctx.egui_wants_keyboard_input() {
        ctx.request_repaint_after(KEYBOARD_POLL);
    }
    ui.painter().rect_filled(ctx.viewport_rect(), 0.0, fill);
    let free = ctx.content_rect();
    ui.scope_builder(UiBuilder::new().max_rect(free), |ui| {
        ui.set_clip_rect(free);
        add(ui);
    });
}

/// The window's and winit's view's background: what shows around the Metal layer.
fn background(fill: Color32) {
    let Some(window) = key_window() else { return };
    let [r, g, b, _] = fill.to_array();
    let channel = |value: u8| f64::from(value) / 255.0;
    let color = UIColor::colorWithRed_green_blue_alpha(channel(r), channel(g), channel(b), 1.0);
    window.setBackgroundColor(Some(&color));
    if let Some(view) = root_view(&window) {
        view.setBackgroundColor(Some(&color));
    }
}

/// While `on`, the window is covered with the theme's background whenever the app
/// stops being active (app switcher, Control Center, a call), so iOS's snapshot of it
/// never shows the recovery words. Becoming active removes the cover.
pub fn secure_window(on: bool) {
    SECURE.set(on);
    if !on {
        cover(false);
        return;
    }
    static OBSERVE: Once = Once::new();
    OBSERVE.call_once(|| {
        let center = NSNotificationCenter::defaultCenter();
        let main = NSOperationQueue::mainQueue();
        let hide = RcBlock::new(|_: NonNull<NSNotification>| cover(true));
        let show = RcBlock::new(|_: NonNull<NSNotification>| cover(false));
        // SAFETY: UIKit's notification names; the blocks run on the main queue (the
        // main thread, where the thread-locals live). The center keeps the observers.
        unsafe {
            let _ = center.addObserverForName_object_queue_usingBlock(
                Some(UIApplicationWillResignActiveNotification),
                None,
                Some(&main),
                &hide,
            );
            let _ = center.addObserverForName_object_queue_usingBlock(
                Some(UIApplicationDidBecomeActiveNotification),
                None,
                Some(&main),
                &show,
            );
        }
    });
}

/// Puts the cover over the window (when `SECURE`) or takes it away.
fn cover(on: bool) {
    COVER.with_borrow_mut(|cover| {
        if let Some(view) = cover.take() {
            view.removeFromSuperview();
        }
        if !on || !SECURE.get() {
            return;
        }
        let (Some(mtm), Some(window)) = (MainThreadMarker::new(), key_window()) else {
            return;
        };
        let view = UIView::initWithFrame(UIView::alloc(mtm), window.bounds());
        let color = window.backgroundColor().unwrap_or_else(UIColor::blackColor);
        view.setBackgroundColor(Some(&color));
        view.setAutoresizingMask(
            UIViewAutoresizing::FlexibleWidth | UIViewAutoresizing::FlexibleHeight,
        );
        window.addSubview(&view);
        *cover = Some(view);
    });
}

/// winit turns the keyboard's Return into the text "\n", which egui-winit drops as
/// a control character, so Return did nothing. Once, winit's view gets an
/// `insertText:` that notes a Return (for `input`, which sends Enter) before
/// winit's own runs.
fn watch_return() {
    if INSERT_TEXT.get().is_some() {
        return;
    }
    let Some(method) =
        AnyClass::get(c"WinitUIView").and_then(|class| class.instance_method(sel!(insertText:)))
    else {
        return;
    };
    let ours: InsertText = insert_text;
    // SAFETY: the same signature as winit's method (`-insertText:` takes an
    // NSString), on the main thread; winit's stays in `INSERT_TEXT` and is called.
    let winits = unsafe { method.set_implementation(std::mem::transmute::<InsertText, Imp>(ours)) };
    let _ = INSERT_TEXT.set(winits);
}

type InsertText = extern "C-unwind" fn(*mut AnyObject, Sel, *const NSString);

extern "C-unwind" fn insert_text(this: *mut AnyObject, cmd: Sel, text: *const NSString) {
    // SAFETY: UIKit passes the typed text, an NSString, or nil.
    if let Some(text) = unsafe { text.as_ref() }
        && text.length() == 1
        && text.to_string() == "\n"
    {
        RETURN.store(true, Ordering::Relaxed);
    }
    if let Some(winits) = INSERT_TEXT.get() {
        // SAFETY: winit's implementation of this method, with its own arguments.
        let winits = unsafe { std::mem::transmute::<Imp, InsertText>(*winits) };
        winits(this, cmd, text);
    }
}

fn root_view(window: &UIWindow) -> Option<Retained<UIView>> {
    window.rootViewController()?.view()
}

fn key_window() -> Option<Retained<UIWindow>> {
    let mtm = MainThreadMarker::new()?;
    // winit 0.30 makes its window key without a scene.
    #[expect(deprecated)]
    UIApplication::sharedApplication(mtm).keyWindow()
}

/// The `CAMetalLayer` wgpu added to winit's view (raw-window-metal).
fn metal_layer(window: &UIWindow) -> Option<Retained<CALayer>> {
    let view = window.rootViewController()?.view()?;
    // SAFETY: `sublayers` is an array of layers, read on the main thread.
    let sublayers = unsafe { view.layer().sublayers() }?;
    sublayers
        .iter()
        .find(|layer| layer.isKindOfClass(objc2::class!(CAMetalLayer)))
}

/// Once, for the device log: what winit and egui got, so a wrong size shows.
fn log_once(window: &UIWindow, layer: Option<&CALayer>, raw_input: &RawInput) {
    static LOGGED: std::sync::Once = std::sync::Once::new();
    LOGGED.call_once(|| {
        let screen = window.screen();
        let bounds = window.bounds();
        let insets = window.safeAreaInsets();
        // SAFETY: `drawableSize` is a CGSize property of CAMetalLayer.
        let drawable: Option<CGSize> = layer.map(|layer| unsafe { msg_send![layer, drawableSize] });
        let scale = window
            .rootViewController()
            .and_then(|controller| controller.view())
            .map(|view| view.contentScaleFactor());
        tracing::info!(
            outer_points = ?(bounds.size.width, bounds.size.height),
            safe_area = ?(insets.top, insets.left, insets.bottom, insets.right),
            inner_pixels = ?drawable.map(|size| (size.width, size.height)),
            layer_frame = ?layer.map(|layer| layer.frame()),
            view_scale = ?scale,
            screen_scale = screen.scale(),
            native_scale = screen.nativeScale(),
            egui_screen = ?raw_input.screen_rect,
            egui_pixels_per_point = ?raw_input
                .viewports
                .get(&raw_input.viewport_id)
                .and_then(|viewport| viewport.native_pixels_per_point),
            "iOS window"
        );
    });
}
