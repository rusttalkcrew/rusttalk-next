//! macOS notifications through `UNUserNotificationCenter`, so a click opens the chat.
//!
//! notify-rust's macOS backend waits for the click on the main run loop, which a worker
//! thread cannot do. The system calls our delegate instead, and it sends the click down
//! the same channel as Linux clicks. The center only works for an app bundle (the DMG's
//! ad-hoc signed `RustTalk.app` is enough); a bare `cargo run` binary makes `show`
//! return false and the caller keeps the old pop-up.

use std::{
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc::Sender,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use block2::{DynBlock, RcBlock};
use objc2::{AnyThread, define_class, msg_send, rc::Retained, runtime::ProtocolObject};
use objc2_foundation::{NSBundle, NSError, NSObject, NSObjectProtocol, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification,
    UNNotificationDefaultActionIdentifier, UNNotificationPresentationOptions,
    UNNotificationRequest, UNNotificationResponse, UNNotificationSound, UNUserNotificationCenter,
    UNUserNotificationCenterDelegate,
};

use super::{Pending, Target, deliver_click};

struct State {
    pending: Mutex<Pending>,
    clicks: Sender<Target>,
    ctx: eframe::egui::Context,
}

/// `None` when this is not an app bundle.
static STATE: OnceLock<Option<State>> = OnceLock::new();
static COUNTER: AtomicU64 = AtomicU64::new(0);

define_class!(
    // SAFETY: NSObject has no subclassing requirements and this type has no `Drop`.
    #[unsafe(super(NSObject))]
    #[name = "RustTalkNotificationDelegate"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl UNUserNotificationCenterDelegate for Delegate {
        // Without this the system hides a notification while RustTalk is in front.
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            done: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            done.call((UNNotificationPresentationOptions::Banner
                | UNNotificationPresentationOptions::List
                | UNNotificationPresentationOptions::Sound,));
        }

        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            done: &DynBlock<dyn Fn()>,
        ) {
            // SAFETY: a constant string of the framework.
            let default = unsafe { UNNotificationDefaultActionIdentifier };
            if response.actionIdentifier().isEqualToString(default) {
                let id = response.notification().request().identifier().to_string();
                if let Some(Some(state)) = STATE.get() {
                    let target = state.pending.lock().ok().and_then(|mut p| p.take(&id));
                    if let Some(target) = target {
                        deliver_click(&state.clicks, &state.ctx, target);
                    }
                }
            }
            done.call(());
        }
    }
);

fn init(clicks: &Sender<Target>, ctx: &eframe::egui::Context) -> Option<&'static State> {
    STATE
        .get_or_init(|| {
            // The center raises an Objective-C exception without a bundle.
            NSBundle::mainBundle().bundleIdentifier()?;
            let center = UNUserNotificationCenter::currentNotificationCenter();
            // SAFETY: `init` of a freshly allocated NSObject subclass.
            let delegate: Retained<Delegate> = unsafe { msg_send![Delegate::alloc(), init] };
            center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            // The center holds its delegate weakly: keep this one for the app's lifetime.
            std::mem::forget(delegate);
            // Asked once; the system shows its prompt the first time only.
            center.requestAuthorizationWithOptions_completionHandler(
                UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
                &RcBlock::new(|granted: objc2::runtime::Bool, error: *mut NSError| {
                    // SAFETY: the system passes nil or a valid error.
                    if let Some(error) = unsafe { error.as_ref() } {
                        tracing::warn!(error = %error.localizedDescription(), "asking for notification permission failed");
                    } else if !granted.as_bool() {
                        tracing::info!("notifications are off for RustTalk in System Settings");
                    }
                }),
            );
            Some(State {
                pending: Mutex::new(Pending::default()),
                clicks: clicks.clone(),
                ctx: ctx.clone(),
            })
        })
        .as_ref()
}

/// Post a notification. False when the center is unavailable (not an app bundle).
pub fn show(
    ctx: &eframe::egui::Context,
    clicks: &Sender<Target>,
    title: &str,
    body: &str,
    sound: bool,
    target: &Target,
) -> bool {
    let Some(state) = init(clicks, ctx) else {
        return false;
    };
    // Unique across restarts too: a repeated identifier replaces the earlier notification.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let id = format!("{nanos:x}-{}", COUNTER.fetch_add(1, Ordering::Relaxed));

    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setBody(&NSString::from_str(body));
    if sound {
        content.setSound(Some(&UNNotificationSound::defaultSound()));
    }
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
        &NSString::from_str(&id),
        &content,
        None,
    );
    if let Ok(mut pending) = state.pending.lock() {
        pending.add(id, target.clone());
    }
    UNUserNotificationCenter::currentNotificationCenter()
        .addNotificationRequest_withCompletionHandler(
            &request,
            Some(&RcBlock::new(|error: *mut NSError| {
                // SAFETY: the system passes nil or a valid error.
                if let Some(error) = unsafe { error.as_ref() } {
                    tracing::warn!(error = %error.localizedDescription(), "showing a desktop notification failed");
                }
            })),
        );
    true
}
