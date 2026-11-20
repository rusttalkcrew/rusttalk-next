//! Voice in the window: who is in each room, the call bar, the room view,
//! the connection panel and Settings → Voice.

use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Button, Color32, ComboBox, Event, Frame, Grid, Key, Layout, Margin, Popup,
    RichText, Role, Sense, Slider, Stroke, Ui, Vec2, Window,
};

use crate::{
    audio::{self, Audio, Settings},
    home::{Home, Tab, avatar, palette, section},
    net::Command,
    server::{Call, CallState, Id, Server, Stats},
};

const GREEN: Color32 = Color32::from_rgb(0x23, 0xa5, 0x5a);
const AMBER: Color32 = Color32::from_rgb(0xf0, 0xb2, 0x32);
const RED: Color32 = Color32::from_rgb(0xda, 0x37, 0x3c);
/// Meter range in dBFS.
const FLOOR: f32 = -70.0;
const CEILING: f32 = -10.0;
const METER_WIDTH: f32 = 360.0;

pub struct Voice {
    pub audio: Audio,
    pub call: Option<Call>,
    pub show_stats: bool,
    calibration: Option<Calibration>,
    picking_key: bool,
}

enum Calibration {
    /// Until when.
    Quiet(Instant),
    Speak(Instant, Vec<f32>),
    Done(String),
}

impl Voice {
    pub fn new(audio: Audio) -> Self {
        Self {
            audio,
            call: None,
            show_stats: false,
            calibration: None,
            picking_key: false,
        }
    }

    fn in_call(&self, server: &Server, channel: &Id) -> bool {
        self.call
            .as_ref()
            .is_some_and(|call| call.server == server.address && call.channel == *channel)
    }

    fn speaking(&self, server: &Server, person: &Id) -> bool {
        if *person == server.dir.me {
            self.audio.talking()
        } else {
            self.audio.speaking(person)
        }
    }
}

/// Join a voice channel, leaving any other room first, and show it.
pub fn join(home: &mut Home, index: usize, channel: Id) {
    home.servers[index].open = Some(channel);
    if home.voice.in_call(&home.servers[index], &channel) {
        return;
    }
    leave(home);
    let server = &home.servers[index];
    server.send(Command::JoinVoice(channel));
    home.voice.call = Some(Call {
        server: server.address.clone(),
        channel,
        state: CallState::Connecting,
        stats: Stats::default(),
    });
    home.voice.audio.set_voice(true);
}

pub fn leave(home: &mut Home) {
    let Some(call) = home.voice.call.take() else {
        return;
    };
    home.voice.audio.set_voice(false);
    home.voice.show_stats = false;
    if let Some(server) = home.servers.iter().find(|s| s.address == call.server) {
        server.send(Command::LeaveVoice);
    }
}

/// Once per frame: push-to-talk, the settings meter, and repaints for live indicators.
pub fn frame(ctx: &egui::Context, home: &mut Home) {
    let voice = &mut home.voice;
    let settings = voice.audio.settings();
    let key = Key::from_name(&settings.key);
    // shortcut: push-to-talk works only while RustTalk has focus; add a global hotkey
    // (per OS) if people ask to talk while another app is in front.
    let down = settings.push_to_talk
        && !voice.picking_key
        && !ctx.egui_wants_keyboard_input()
        && key.is_some_and(|key| ctx.input(|input| input.key_down(key)));
    voice.audio.set_key_down(down);
    let testing = home.settings == Some(Tab::Voice);
    voice.audio.set_testing(testing);
    if testing || voice.call.is_some() {
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

/// The people in a voice channel, under its name in the channel list.
pub fn room_list(ui: &mut Ui, home: &Home, server: &Server, channel: &Id) {
    let Some(peers) = server.voice.get(channel) else {
        return;
    };
    for peer in peers {
        let name = server.member_name(&peer.person);
        let row = ui.horizontal(|ui| {
            ui.add_space(22.0);
            let face = avatar(ui, &peer.person, &name, 22.0);
            if home.voice.speaking(server, &peer.person) {
                ring(ui, face.rect);
            }
            ui.label(RichText::new(&name).small());
            if peer.connecting {
                ui.label(RichText::new("Connecting…").small().weak());
            }
        });
        let row = row.response.interact(Sense::click());
        if peer.person != server.dir.me && home.voice.in_call(server, channel) {
            Popup::context_menu(&row).show(|ui| person_menu(ui, &home.voice.audio, &peer.person));
        }
    }
}

fn ring(ui: &Ui, rect: egui::Rect) {
    ui.painter().circle_stroke(
        rect.center(),
        rect.width() / 2.0 + 2.0,
        Stroke::new(2.0, GREEN),
    );
}

/// Volume and mute for one person, in their context menu.
pub fn person_menu(ui: &mut Ui, audio: &Audio, person: &Id) {
    let (mut volume, mut muted) = audio.volume(person);
    let before = (volume, muted);
    ui.label("Volume");
    ui.add(
        Slider::new(&mut volume, 0.0..=2.0)
            .custom_formatter(|value, _| format!("{:.0}%", value * 100.0))
            .custom_parser(|text| {
                let percent: f64 = text.trim().trim_end_matches('%').parse().ok()?;
                Some(percent / 100.0)
            }),
    );
    ui.checkbox(&mut muted, "Mute");
    if (volume, muted) != before {
        audio.set_volume(*person, volume, muted);
    }
}

/// The bar at the bottom of the channel list while in a voice room.
pub fn call_bar(ui: &mut Ui, home: &mut Home) {
    let Some(call) = &home.voice.call else {
        return;
    };
    let server = home.servers.iter().find(|s| s.address == call.server);
    let place = server
        .and_then(|server| {
            let (name, _) = server.chat_title(&call.channel)?;
            Some(format!("{name} / {}", server.name()))
        })
        .unwrap_or_default();
    let (text, color) = match call.state {
        CallState::Connecting => ("Connecting…", AMBER),
        CallState::Connected => ("Voice connected", GREEN),
        CallState::Reconnecting => ("Reconnecting…", AMBER),
    };
    let audio = home.voice.audio.clone();
    let mut leave_now = false;
    Frame::new()
        .fill(ui.visuals().extreme_bg_color)
        .corner_radius(6)
        .inner_margin(Margin::same(8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(text).strong().color(color));
            ui.label(RichText::new(place).small().weak());
            let off = RED.gamma_multiply(0.6);
            let selected = ui.visuals().selection.bg_fill;
            ui.horizontal(|ui| {
                let deafened = audio.deafened();
                let muted = audio.muted() || deafened;
                let mic = if muted { "🔇" } else { "🎤" };
                if toggle(
                    ui,
                    mic,
                    if muted { "Unmute" } else { "Mute" },
                    muted.then_some(off),
                )
                .clicked()
                {
                    audio.set_muted(!muted);
                    if muted {
                        audio.set_deafened(false);
                    }
                }
                let ears = if deafened { "Undeafen" } else { "Deafen" };
                if toggle(ui, "🎧", ears, deafened.then_some(off)).clicked() {
                    audio.set_deafened(!deafened);
                }
                if toggle(
                    ui,
                    "📶",
                    "Connection",
                    home.voice.show_stats.then_some(selected),
                )
                .clicked()
                {
                    home.voice.show_stats ^= true;
                }
                if toggle(ui, "⚙", "Voice settings", None).clicked() {
                    home.settings = Some(Tab::Voice);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let hang = ui
                        .add(Button::new(RichText::new("📞").color(Color32::WHITE)).fill(RED))
                        .on_hover_text("Leave");
                    hang.widget_info(|| egui::WidgetInfo::labeled(Role::Button, true, "Leave"));
                    leave_now = hang.clicked();
                });
            });
        });
    if leave_now {
        leave(home);
    }
}

/// An icon button; `on` fills it, with `fill` or the selection color.
fn toggle(ui: &mut Ui, icon: &str, label: &str, on: Option<Color32>) -> egui::Response {
    let mut button = Button::new(RichText::new(icon).size(16.0)).min_size(Vec2::splat(30.0));
    if let Some(fill) = on {
        button = button.fill(fill);
    }
    let response = ui.add(button).on_hover_text(label);
    response.widget_info(|| egui::WidgetInfo::labeled(Role::Button, true, label));
    response
}

/// The main pane for a voice channel: everyone in it, with speaking rings.
pub fn room(ui: &mut Ui, home: &mut Home, index: usize, channel: Id) {
    let server = &home.servers[index];
    let title = server
        .chat_title(&channel)
        .map(|(name, _)| name)
        .unwrap_or_default();
    let joined = home.voice.in_call(server, &channel);
    ui.label(RichText::new(format!("🔊 {title}")).strong().size(17.0));
    ui.separator();
    let peers = server.voice.get(&channel).cloned().unwrap_or_default();
    if peers.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.3);
            ui.weak(if joined {
                "Connecting…"
            } else {
                "No one is here yet."
            });
        });
    }
    ui.horizontal_wrapped(|ui| {
        for peer in &peers {
            let name = server.member_name(&peer.person);
            let me = peer.person == server.dir.me;
            let tile = Frame::new()
                .fill(palette(ui).side)
                .corner_radius(8)
                .inner_margin(Margin::same(12))
                .show(ui, |ui| {
                    ui.set_width(150.0);
                    ui.vertical_centered(|ui| {
                        let face = avatar(ui, &peer.person, &name, 64.0);
                        if home.voice.speaking(server, &peer.person) {
                            ring(ui, face.rect);
                        }
                        ui.label(RichText::new(&name).strong());
                        if peer.connecting {
                            ui.weak("Connecting…");
                        } else if me && home.voice.audio.deafened() {
                            ui.weak("Deafened");
                        } else if me && home.voice.audio.muted() {
                            ui.weak("Muted");
                        } else if !me && home.voice.audio.volume(&peer.person).1 {
                            ui.weak("Muted by you");
                        } else {
                            ui.label(" ");
                        }
                    });
                });
            let tile = tile.response.interact(Sense::click());
            if !me && joined {
                Popup::context_menu(&tile)
                    .show(|ui| person_menu(ui, &home.voice.audio, &peer.person));
            }
        }
    });
    if !joined {
        ui.add_space(12.0);
        if crate::app::primary(ui, "Join voice").clicked() {
            join(home, index, channel);
        }
    }
}

/// The connection panel, opened from the call bar.
pub fn stats_window(ctx: &egui::Context, home: &mut Home) {
    let Some(call) = &home.voice.call else {
        return;
    };
    let mut open = home.voice.show_stats;
    let server = home.servers.iter().find(|s| s.address == call.server);
    Window::new("Connection")
        .open(&mut open)
        .resizable(false)
        .collapsible(false)
        .default_width(240.0)
        // Above the call bar, beside the server rail.
        .pivot(egui::Align2::LEFT_BOTTOM)
        .default_pos(ctx.content_rect().left_bottom() + Vec2::new(84.0, -150.0))
        .show(ctx, |ui| {
            let stats = &call.stats;
            let ms = |value: Option<f64>| value.map_or("—".into(), |v| format!("{v:.0} ms"));
            let count = |value: Option<i64>| value.map_or("—".into(), |v| v.to_string());
            Grid::new("connection").num_columns(2).show(ui, |ui| {
                ui.label("Ping");
                ui.label(ms(stats.ping_ms));
                ui.end_row();
                ui.label("Jitter");
                ui.label(ms(stats.jitter_ms));
                ui.end_row();
                ui.label("Lost packets in");
                ui.label(count(stats.lost_in));
                ui.end_row();
                ui.label("Lost packets out");
                ui.label(count(stats.lost_out));
                ui.end_row();
            });
            section(ui, "Bitrate");
            if stats.tracks.is_empty() {
                ui.weak("No audio yet.");
            }
            Grid::new("bitrates").num_columns(2).show(ui, |ui| {
                for track in &stats.tracks {
                    let name = match (track.mine, server) {
                        (true, _) => "You".to_owned(),
                        (false, Some(server)) => server.member_name(&track.person),
                        (false, None) => String::new(),
                    };
                    ui.label(name);
                    ui.label(format!("{:.0} kbps", track.kbps));
                    ui.end_row();
                }
            });
        });
    home.voice.show_stats &= open;
}

/// Settings → Voice.
pub fn settings_tab(ui: &mut Ui, voice: &mut Voice) {
    let audio = voice.audio.clone();
    let mut s = audio.settings();
    let (inputs, outputs) = audio.devices();
    device_picker(ui, "Microphone", &mut s.input, &inputs);
    device_picker(ui, "Speakers", &mut s.output, &outputs);
    if let Some(problem) = audio.problem() {
        ui.colored_label(ui.visuals().error_fg_color, problem);
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.selectable_value(&mut s.push_to_talk, false, "Voice activity");
        ui.selectable_value(&mut s.push_to_talk, true, "Push to talk");
    });
    meter(ui, &audio, &s);
    if s.push_to_talk {
        key_picker(ui, voice, &mut s);
    } else {
        // As wide as the meter, so the handle sits on the line.
        ui.spacing_mut().slider_width = METER_WIDTH;
        let slider = ui.add(Slider::new(&mut s.threshold, FLOOR..=CEILING).show_value(false));
        slider.widget_info(|| egui::WidgetInfo::labeled(Role::Slider, true, "Sensitivity"));
        ui.weak("Your mic turns on when the level passes the line. Drag to change it.");
        calibrate(ui, voice, &mut s);
    }
    ui.add_space(6.0);
    ui.checkbox(&mut s.echo_cancellation, "Echo cancellation");
    ui.checkbox(&mut s.noise_suppression, "Noise suppression");
    ui.checkbox(&mut s.auto_gain, "Automatic gain");
    audio.set_settings(s);
}

fn device_picker(
    ui: &mut Ui,
    label: &str,
    chosen: &mut Option<String>,
    devices: &[audio::DeviceInfo],
) {
    let current = match chosen {
        None => "System default",
        Some(id) => devices
            .iter()
            .find(|d| d.id == *id)
            .map_or("Not connected", |d| d.name.as_str()),
    };
    ComboBox::from_label(label)
        .width(280.0)
        .selected_text(current)
        .show_ui(ui, |ui| {
            ui.selectable_value(chosen, None, "System default");
            for device in devices {
                ui.selectable_value(chosen, Some(device.id.clone()), &device.name);
            }
        });
}

/// The live input level, with the voice activation line.
fn meter(ui: &mut Ui, audio: &Audio, s: &Settings) {
    let fraction = |db: f32| ((db - FLOOR) / (CEILING - FLOOR)).clamp(0.0, 1.0);
    let level = audio.level();
    let width = METER_WIDTH;
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 10.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
    let open = if s.push_to_talk {
        audio.talking()
    } else {
        level > s.threshold
    };
    let mut filled = rect;
    filled.set_width(rect.width() * fraction(level));
    painter.rect_filled(filled, 4.0, if open { GREEN } else { Color32::GRAY });
    if !s.push_to_talk {
        let x = rect.left() + rect.width() * fraction(s.threshold);
        painter.vline(
            x,
            rect.y_range(),
            Stroke::new(2.0, ui.visuals().strong_text_color()),
        );
    }
    response.widget_info(|| egui::WidgetInfo::labeled(Role::Meter, true, "Mic level"));
}

fn key_picker(ui: &mut Ui, voice: &mut Voice, s: &mut Settings) {
    ui.horizontal(|ui| {
        if voice.picking_key {
            ui.label("Press a key…");
            let pressed = ui.input(|input| {
                input.events.iter().find_map(|event| match event {
                    Event::Key {
                        key, pressed: true, ..
                    } => Some(*key),
                    _ => None,
                })
            });
            match pressed {
                Some(Key::Escape) => voice.picking_key = false,
                Some(key) => {
                    s.key = key.name().to_owned();
                    voice.picking_key = false;
                }
                None => {}
            }
        } else {
            ui.label(format!("Talk key: {}", s.key));
            if ui.button("Change").clicked() {
                voice.picking_key = true;
            }
        }
    });
    ui.weak("Hold it to talk while RustTalk is the active window and you're not typing.");
}

fn calibrate(ui: &mut Ui, voice: &mut Voice, s: &mut Settings) {
    let now = Instant::now();
    let audio = &voice.audio;
    voice.calibration = match voice.calibration.take() {
        Some(Calibration::Quiet(until)) if now >= until => {
            let quiet = audio.record(true).unwrap_or_default();
            Some(Calibration::Speak(now + Duration::from_secs(4), quiet))
        }
        Some(Calibration::Speak(until, quiet)) if now >= until => {
            let speech = audio.record(false).unwrap_or_default();
            Some(Calibration::Done(match audio::calibrate(&quiet, &speech) {
                Some(threshold) => {
                    s.threshold = threshold.clamp(FLOOR, CEILING);
                    "Done. Your mic now opens when you talk.".into()
                }
                None => "Couldn't hear you clearly. Check your mic and try again.".into(),
            }))
        }
        other => other,
    };
    match &voice.calibration {
        Some(Calibration::Quiet(_)) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Stay quiet for a moment…");
            });
        }
        Some(Calibration::Speak(..)) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Say something, like counting to five.").strong());
            });
        }
        done => {
            if let Some(Calibration::Done(text)) = done {
                ui.label(text.as_str());
            }
            if ui.button("Calibrate").clicked() {
                audio.record(true);
                voice.calibration = Some(Calibration::Quiet(now + Duration::from_secs(2)));
            }
        }
    }
}
