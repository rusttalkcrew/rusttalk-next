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

use std::cell::Cell;

use eframe::egui::{
    self, Context, Event, Pos2, RawInput, Rect, SafeAreaInsets, Vec2, accesskit::Affine,
};
use objc2::{MainThreadMarker, msg_send, rc::Retained, runtime::NSObjectProtocol};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_quartz_core::{CALayer, CATransaction};
use objc2_ui_kit::{UIApplication, UIWindow};

thread_local! {
    /// Where the safe area starts, in egui points (from `input`, for `accessibility`).
    static OFFSET: Cell<Vec2> = const { Cell::new(Vec2::ZERO) };
}

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
    // Already inside the safe area: nothing left to keep clear.
    raw_input.safe_area_insets = Some(SafeAreaInsets::default());
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
