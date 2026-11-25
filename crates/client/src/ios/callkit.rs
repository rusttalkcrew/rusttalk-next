//! CallKit for `call::callkit`: the voice call shows as an outgoing call named after
//! the channel and server, kept out of the Phone app's Recents.
//!
//! The provider's delegate runs on the main queue, between frames. With the screen
//! locked the app draws no frames, so End and Mute from the lock screen take effect
//! right away here (leave the room, silence the microphone); the next frame brings
//! the rest of the app in line.
//!
//! The app still sets up and activates its own PlayAndRecord/VoiceChat session at
//! join (`voice::Session`), before CallKit's start; CallKit then reports that session
//! active and gives it a call's priority. Its activation callbacks are only logged.

use std::{cell::RefCell, sync::OnceLock};

use block2::RcBlock;
use eframe::egui::Context;
use objc2::{AnyThread as _, define_class, msg_send, rc::Retained, runtime::ProtocolObject};
use objc2_avf_audio::AVAudioSession;
use objc2_call_kit::{
    CXAction, CXCallController, CXCallEndedReason, CXCallUpdate, CXEndCallAction, CXHandle,
    CXHandleType, CXProvider, CXProviderConfiguration, CXProviderDelegate, CXSetHeldCallAction,
    CXSetMutedCallAction, CXStartCallAction,
};
use objc2_foundation::{NSError, NSNumber, NSObject, NSObjectProtocol, NSSet, NSString, NSUUID};
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    audio::Audio,
    call::callkit::{self, CallKit, Effect, Event, Request, System},
    home::Home,
    net::Command,
    server::Server,
};

type Call = Retained<NSUUID>;

thread_local! {
    static KIT: RefCell<Option<CallKit<Ios>>> = const { RefCell::new(None) };
    /// Events that came while `KIT` was busy, for the next frame.
    static EVENTS: RefCell<Vec<Event<Call>>> = const { RefCell::new(Vec::new()) };
    /// What the phone asked for, for the next frame.
    static EFFECTS: RefCell<Vec<Effect>> = const { RefCell::new(Vec::new()) };
    /// The call's audio and server, to act without a frame.
    static LIVE: RefCell<Option<Live>> = const { RefCell::new(None) };
}
static CTX: OnceLock<Context> = OnceLock::new();

struct Live {
    audio: Audio,
    commands: UnboundedSender<Command>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and this type has no `Drop`.
    #[unsafe(super(NSObject))]
    #[name = "RustTalkCallKitDelegate"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl CXProviderDelegate for Delegate {
        #[unsafe(method(providerDidReset:))]
        fn did_reset(&self, _provider: &CXProvider) {
            tracing::info!("CallKit reset");
            report(Event::Reset);
        }

        #[unsafe(method(provider:performStartCallAction:))]
        fn start(&self, provider: &CXProvider, action: &CXStartCallAction) {
            // SAFETY: CallKit objects used on the delegate's (main) queue.
            unsafe {
                let call = action.callUUID();
                let handle = action.handle();
                let update = CXCallUpdate::new();
                update.setRemoteHandle(Some(&handle));
                update.setLocalizedCallerName(Some(&handle.value()));
                update.setHasVideo(action.isVideo());
                // One room at a time: another phone call offers only "End & Accept".
                update.setSupportsHolding(false);
                update.setSupportsGrouping(false);
                update.setSupportsUngrouping(false);
                update.setSupportsDTMF(false);
                provider.reportCallWithUUID_updated(&call, &update);
                provider.reportOutgoingCallWithUUID_startedConnectingAtDate(&call, None);
                action.fulfill();
            }
            tracing::info!("CallKit call started");
        }

        #[unsafe(method(provider:performEndCallAction:))]
        fn end(&self, _provider: &CXProvider, action: &CXEndCallAction) {
            // SAFETY: as above.
            unsafe {
                report(Event::Ended(action.callUUID()));
                action.fulfill();
            }
        }

        #[unsafe(method(provider:performSetMutedCallAction:))]
        fn mute(&self, _provider: &CXProvider, action: &CXSetMutedCallAction) {
            // SAFETY: as above.
            unsafe {
                report(Event::Muted(action.callUUID(), action.isMuted()));
                action.fulfill();
            }
        }

        #[unsafe(method(provider:performSetHeldCallAction:))]
        fn hold(&self, _provider: &CXProvider, action: &CXSetHeldCallAction) {
            tracing::info!("CallKit hold refused");
            // SAFETY: as above.
            unsafe { action.fail() };
        }

        #[unsafe(method(provider:didActivateAudioSession:))]
        fn activated(&self, _provider: &CXProvider, _session: &AVAudioSession) {
            tracing::info!("CallKit activated the audio session");
        }

        #[unsafe(method(provider:didDeactivateAudioSession:))]
        fn deactivated(&self, _provider: &CXProvider, _session: &AVAudioSession) {
            tracing::info!("CallKit deactivated the audio session");
        }
    }
);

/// Map a CallKit event now, act on what can't wait for a frame, and ask for one.
fn report(event: Event<Call>) {
    let effect = KIT.with(|kit| match kit.try_borrow_mut() {
        Ok(mut kit) => kit.as_mut().and_then(|kit| kit.event(event)),
        Err(_) => {
            EVENTS.with_borrow_mut(|events| events.push(event));
            None
        }
    });
    if let Some(effect) = effect {
        LIVE.with_borrow(|live| {
            let Some(live) = live else { return };
            match effect {
                Effect::Leave => {
                    Server::send_to(&live.commands, Command::LeaveVoice);
                    live.audio.end_voice(None);
                }
                Effect::Mute(true) => live.audio.set_muted(true),
                // Unmuting waits for the frame, which checks a moderator's mute.
                Effect::Mute(false) => {}
            }
        });
        EFFECTS.with_borrow_mut(|effects| effects.push(effect));
    }
    if let Some(ctx) = CTX.get() {
        ctx.request_repaint();
    }
}

/// Once a frame, from `call::frame`. CallKit starts with the first call.
pub fn frame(ctx: &Context, home: &mut Home) {
    CTX.get_or_init(|| ctx.clone());
    let live = home
        .voice
        .call
        .as_ref()
        .and_then(|call| home.servers.iter().find(|s| s.address == call.server))
        .map(|server| Live {
            audio: home.voice.audio.clone(),
            commands: server.commands.clone(),
        });
    LIVE.set(live);
    KIT.with_borrow_mut(|kit| {
        if kit.is_none() && home.voice.call.is_none() {
            return;
        }
        let kit = kit.get_or_insert_with(|| CallKit::new(Ios::new()));
        let mut effects = EFFECTS.take();
        for event in EVENTS.take() {
            effects.extend(kit.event(event));
        }
        callkit::frame(home, kit, effects);
    });
}

struct Ios {
    provider: Retained<CXProvider>,
    controller: Retained<CXCallController>,
    /// The provider holds its delegate weakly.
    _delegate: Retained<Delegate>,
}

impl Ios {
    fn new() -> Self {
        // SAFETY: CallKit set up on the main thread; the delegate outlives the provider's
        // use of it (both live in `KIT` for the app's lifetime).
        unsafe {
            let config = CXProviderConfiguration::new();
            config.setSupportsVideo(true);
            config.setMaximumCallGroups(1);
            config.setMaximumCallsPerCallGroup(1);
            // Channel names stay out of the Phone app's history and its iCloud sync.
            config.setIncludesCallsInRecents(false);
            let generic = NSNumber::numberWithInteger(CXHandleType::Generic.0);
            config.setSupportedHandleTypes(&NSSet::from_slice(&[&*generic]));
            let provider = CXProvider::initWithConfiguration(CXProvider::alloc(), &config);
            let delegate: Retained<Delegate> = msg_send![Delegate::alloc(), init];
            // No queue: the main queue, where `KIT` lives.
            provider.setDelegate_queue(Some(ProtocolObject::from_ref(&*delegate)), None);
            tracing::info!("CallKit provider ready");
            Self {
                provider,
                // Its completions run on the main queue too.
                controller: CXCallController::new(),
                _delegate: delegate,
            }
        }
    }

    fn request(&self, what: &'static str, action: &CXAction, refused: impl Fn() + 'static) {
        let done = RcBlock::new(move |error: *mut NSError| {
            // SAFETY: CallKit passes nil or a valid error.
            if let Some(error) = unsafe { error.as_ref() } {
                tracing::warn!(what, error = %error.localizedDescription(), "CallKit refused");
                refused();
            }
        });
        // SAFETY: CallKit copies the block.
        unsafe {
            self.controller
                .requestTransactionWithAction_completion(action, &done)
        };
    }
}

impl System for Ios {
    type Call = Call;

    fn start(&mut self, name: &str, video: bool) -> Call {
        let call = NSUUID::new();
        // SAFETY: plain CallKit value objects.
        let action = unsafe {
            let handle = CXHandle::initWithType_value(
                CXHandle::alloc(),
                CXHandleType::Generic,
                &NSString::from_str(name),
            );
            let action = CXStartCallAction::initWithCallUUID_handle(
                CXStartCallAction::alloc(),
                &call,
                &handle,
            );
            action.setVideo(video);
            action
        };
        let id = call.clone();
        self.request("start", &action, move || {
            report(Event::Refused(id.clone(), Request::Start));
        });
        call
    }

    fn connected(&mut self, call: &Call) {
        // SAFETY: on the main thread, for a call this provider reported.
        unsafe {
            self.provider
                .reportOutgoingCallWithUUID_connectedAtDate(call, None)
        };
    }

    fn set_video(&mut self, call: &Call, on: bool) {
        // SAFETY: as above.
        unsafe {
            let update = CXCallUpdate::new();
            update.setHasVideo(on);
            self.provider.reportCallWithUUID_updated(call, &update);
        }
    }

    fn set_muted(&mut self, call: &Call, muted: bool) {
        // SAFETY: a plain CallKit value object.
        let action = unsafe {
            CXSetMutedCallAction::initWithCallUUID_muted(CXSetMutedCallAction::alloc(), call, muted)
        };
        let id = call.clone();
        self.request("mute", &action, move || {
            report(Event::Refused(id.clone(), Request::Mute));
        });
    }

    fn end(&mut self, call: &Call) {
        // SAFETY: a plain CallKit value object.
        let action = unsafe { CXEndCallAction::initWithCallUUID(CXEndCallAction::alloc(), call) };
        let (id, provider) = (call.clone(), self.provider.clone());
        // The room is left either way: take the call off the screen.
        self.request("end", &action, move || {
            // SAFETY: on the main queue, for a call this provider reported.
            unsafe {
                provider.reportCallWithUUID_endedAtDate_reason(
                    &id,
                    None,
                    CXCallEndedReason::Failed,
                );
            }
        });
    }
}
