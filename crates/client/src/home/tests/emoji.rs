//! Incoming custom emoji and stickers on an idle scrolled chat: they must load and
//! render as soon as their metadata and picture are ready, without another message,
//! user input or resize, while unavailable ones keep their `:name:` fallback.

use super::*;
use egui_kittest::{Harness, kittest::Queryable};
use image::{ImageFormat, Rgba, RgbaImage};
use rusttalk_core::client::{Attachment, BlobRef};
use std::io::Cursor;

fn picture(key: u8, rgba: [u8; 4]) -> Picture {
    let mut bytes = Cursor::new(Vec::new());
    RgbaImage::from_pixel(32, 32, Rgba(rgba))
        .write_to(&mut bytes, ImageFormat::Png)
        .unwrap();
    let bytes = bytes.into_inner();
    let image = ColorImage::from_rgba_unmultiplied([32, 32], &[rgba; 32 * 32].concat());
    Picture {
        key: [key; 32],
        frames: Arc::new(vec![(image, 0)]),
        bytes: Arc::new(bytes),
    }
}

fn msg(id: u8, person: Id, at: i64, text: impl Into<String>, files: Vec<Attachment>) -> Msg {
    Msg {
        id: [id; 32],
        person,
        at,
        body: Body::Text(text.into()),
        backfilled: false,
        reply_to: None,
        quoted_author: None,
        edit_of: None,
        files,
        live: None,
        sticker: None,
        replicas: Vec::new(),
        copy_files: Vec::new(),
    }
}

fn inline_attachment(blob_id: u8, name: &str, media_type: &str) -> Attachment {
    let mut file = BlobRef::for_tests([blob_id; 32]);
    file.size = 512;
    Attachment {
        file,
        name: name.into(),
        media_type: media_type.into(),
        thumbnail: None,
    }
}

fn history(count: u8) -> Vec<Msg> {
    (1..=count)
        .map(|i| {
            let person = if i % 2 == 0 { ANN } else { ME };
            msg(
                i,
                person,
                i64::from(i) * 10,
                format!("Earlier line {i}"),
                vec![],
            )
        })
        .collect()
}

fn sticker_commands(sent: &mut tokio::sync::mpsc::UnboundedReceiver<Command>) -> Vec<(Id, Kind)> {
    std::iter::from_fn(|| sent.try_recv().ok())
        .filter_map(|cmd| match cmd {
            Command::StickerFile(blob, kind) => Some((blob.id, kind)),
            _ => None,
        })
        .collect()
}

fn idle_channel_sized(
    dark: bool,
    size: Vec2,
) -> (
    Harness<'static, Option<Home>>,
    tokio::sync::mpsc::UnboundedReceiver<Command>,
    url::Url,
) {
    let (mut h, sent) = files_harness(dark);
    h.set_size(size);
    h.ctx.data_mut(|d| d.remove::<bool>(files_id(PROJECTS)));
    let address = h.state().as_ref().unwrap().servers[0].address.clone();
    {
        let home = h.state_mut().as_mut().unwrap();
        let chat = home.servers[0].chats.get_mut(&PROJECTS).unwrap();
        chat.messages = history(30);
    }
    h.run();
    (h, sent, address)
}

fn idle_channel(
    dark: bool,
) -> (
    Harness<'static, Option<Home>>,
    tokio::sync::mpsc::UnboundedReceiver<Command>,
    url::Url,
) {
    idle_channel_sized(dark, Vec2::new(720.0, 420.0))
}

#[test]
fn incoming_inline_custom_emoji_renders_on_idle_chat_after_late_async_delivery() {
    for dark in [true, false] {
        for (width, height) in [(390.0, 844.0), (820.0, 1180.0), (1100.0, 700.0)] {
            let (mut h, mut sent, address) = idle_channel_sized(dark, Vec2::new(width, height));
            let _ = sticker_commands(&mut sent);

            // An earlier custom emoji already rendered above the newest message.
            let earlier_id = [40; 32];
            let earlier_pic = picture(40, [40, 200, 120, 255]);
            {
                let ctx = h.ctx.clone();
                let home = h.state_mut().as_mut().unwrap();
                home.servers[0].dir.emoji.push(CustomEmoji {
                    id: earlier_id,
                    name: "wave_ok".into(),
                    sticker: false,
                    pack: [0; 32],
                    picture: Some(earlier_pic),
                });
                let earlier = msg(
                    41,
                    ANN,
                    400,
                    format!("hi {}", crate::emoji::token("wave_ok", &earlier_id)),
                    vec![],
                );
                home.apply(
                    &ctx,
                    Update::Messages {
                        server: address.clone(),
                        channel: PROJECTS,
                        items: vec![earlier],
                        older: false,
                        page: None,
                    },
                    1.0,
                );
            }
            h.run();
            assert!(
                h.query_by_role_and_label(Role::Image, ":wave_ok:")
                    .is_some(),
                "earlier catalog emoji must already be rendered"
            );

            // Now receive a cross-server custom emoji on the idle chat without any user
            // input, resize, or follow-up message.
            let blob_id = [99; 32];
            let incoming = msg(
                42,
                ANN,
                410,
                crate::emoji::token("32542_kuromihuh", &[98; 32]),
                vec![inline_attachment(
                    99,
                    "32542_kuromihuh.png",
                    crate::sticker_files::EMOJI_TYPE,
                )],
            );
            {
                let ctx = h.ctx.clone();
                let home = h.state_mut().as_mut().unwrap();
                home.apply(
                    &ctx,
                    Update::Messages {
                        server: address.clone(),
                        channel: PROJECTS,
                        items: vec![incoming],
                        older: false,
                        page: None,
                    },
                    2.0,
                );
            }
            h.run();

            // The newly received message must have scrolled into view and requested its
            // emoji picture automatically while still showing the readable fallback.
            let requested = sticker_commands(&mut sent);
            assert_eq!(
                requested,
                vec![(blob_id, Kind::Emoji)],
                "idle chat must request the incoming emoji picture without another message (dark={dark}, {width}x{height})"
            );
            let fallback = h
                .query_by_label(":32542_kuromihuh:")
                .expect("fallback shortcode shown while picture is in flight");
            assert!(
                fallback.rect().max.y <= h.ctx.content_rect().max.y,
                "incoming message must scroll into view while idle"
            );
            assert!(
                h.query_by_role_and_label(Role::Image, ":32542_kuromihuh:")
                    .is_none()
            );

            // Late async asset delivery arrives while the chat remains completely idle.
            let pic = picture(99, [230, 80, 180, 255]);
            {
                let ctx = h.ctx.clone();
                let home = h.state_mut().as_mut().unwrap();
                home.apply(&ctx, Update::StickerFile(blob_id, Ok(pic)), 3.0);
            }
            h.run();

            let rendered = h
                .query_by_role_and_label(Role::Image, ":32542_kuromihuh:")
                .expect("incoming custom emoji must render as an image once delivered");
            assert!(
                rendered.rect().max.y <= h.ctx.content_rect().max.y,
                "rendered custom emoji must stay within the viewport"
            );
            assert!(
                h.query_by_role_and_label(Role::Label, ":32542_kuromihuh:")
                    .is_none(),
                "literal shortcode label must be replaced by the emoji image"
            );
            // Must settle without polling.
            assert_eq!(h.run(), 1);

            if let Ok(dir) = std::env::var("RUSTTALK_EMOJI_SCREENSHOTS")
                && (width == 390.0 || width == 1100.0)
            {
                std::fs::create_dir_all(&dir).unwrap();
                let theme = if dark { "dark" } else { "light" };
                h.render()
                    .unwrap()
                    .save(format!(
                        "{dir}/emoji-rendered-{}x{}-{theme}.png",
                        width as u32, height as u32
                    ))
                    .unwrap();
            }
        }
    }
}

#[test]
fn incoming_inline_custom_emoji_in_idle_dm_renders_without_follow_up() {
    for dark in [true, false] {
        let (mut base, mut sent) = files_harness(dark);
        let mut home = base.state_mut().take().unwrap();
        let address = home.servers[0].address.clone();
        home.servers[0].dir.dms.push(crate::server::Dm {
            channel: PROJECTS,
            peer: ANN,
        });
        home.view = View::Direct(Some(ANN));
        let mut h = Harness::builder()
            .with_size(Vec2::new(720.0, 420.0))
            .with_context_setup(ui::install)
            .build_ui_state(|ui, home| dm_pane(ui, home, ANN), home);
        h.ctx.set_theme(if dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        });
        {
            let ctx = h.ctx.clone();
            h.state_mut().apply(
                &ctx,
                Update::Messages {
                    server: address.clone(),
                    channel: PROJECTS,
                    items: history(30),
                    older: false,
                    page: Some((true, Some([1; 32]))),
                },
                1.0,
            );
        }
        h.run();
        let _ = sticker_commands(&mut sent);

        let blob_id = [88; 32];
        let incoming = msg(
            42,
            ANN,
            410,
            crate::emoji::token("32542_kuromihuh", &[87; 32]),
            vec![inline_attachment(
                88,
                "32542_kuromihuh.png",
                crate::sticker_files::EMOJI_TYPE,
            )],
        );
        {
            let ctx = h.ctx.clone();
            h.state_mut().apply(
                &ctx,
                Update::Messages {
                    server: address.clone(),
                    channel: PROJECTS,
                    items: vec![incoming],
                    older: false,
                    page: None,
                },
                2.0,
            );
        }
        h.run();
        assert_eq!(sticker_commands(&mut sent), vec![(blob_id, Kind::Emoji)]);

        {
            let ctx = h.ctx.clone();
            h.state_mut().apply(
                &ctx,
                Update::StickerFile(blob_id, Ok(picture(88, [80, 180, 240, 255]))),
                3.0,
            );
        }
        h.run();
        assert!(
            h.query_by_role_and_label(Role::Image, ":32542_kuromihuh:")
                .is_some()
        );
        assert!(
            h.query_by_role_and_label(Role::Label, ":32542_kuromihuh:")
                .is_none()
        );
    }
}

#[test]
fn late_directory_emoji_update_renders_on_idle_chat_and_unavailable_keeps_fallback() {
    for dark in [true, false] {
        let (mut h, mut sent, address) = idle_channel(dark);
        let _ = sticker_commands(&mut sent);

        let catalog_id = [77; 32];
        let missing_id = [78; 32];
        let broken_blob = [79; 32];
        let incoming = vec![
            msg(
                50,
                ANN,
                500,
                crate::emoji::token("late_catalog", &catalog_id),
                vec![],
            ),
            msg(
                51,
                ANN,
                505,
                format!(
                    "{} {}",
                    crate::emoji::token("missing_emoji", &missing_id),
                    crate::emoji::token("broken_emoji", &[76; 32])
                ),
                vec![inline_attachment(
                    79,
                    "broken_emoji.png",
                    crate::sticker_files::EMOJI_TYPE,
                )],
            ),
        ];
        {
            let ctx = h.ctx.clone();
            let home = h.state_mut().as_mut().unwrap();
            home.apply(
                &ctx,
                Update::Messages {
                    server: address.clone(),
                    channel: PROJECTS,
                    items: incoming,
                    older: false,
                    page: None,
                },
                2.0,
            );
        }
        h.run();
        assert_eq!(
            sticker_commands(&mut sent),
            vec![(broken_blob, Kind::Emoji)]
        );
        assert!(h.query_by_label(":late_catalog:").is_some());

        // Late async directory update provides the catalog emoji; the broken inline
        // attachment fails permanently and must keep its readable `:name:` fallback.
        {
            let ctx = h.ctx.clone();
            let home = h.state_mut().as_mut().unwrap();
            let mut dir = home.servers[0].dir.clone();
            dir.emoji.push(CustomEmoji {
                id: catalog_id,
                name: "late_catalog".into(),
                sticker: false,
                pack: [0; 32],
                picture: Some(picture(77, [240, 190, 60, 255])),
            });
            home.apply(&ctx, Update::Directory(address.clone(), Box::new(dir)), 3.0);
            home.apply(&ctx, Update::StickerFile(broken_blob, Err(true)), 3.0);
        }
        h.run();

        let rendered = h
            .query_by_role_and_label(Role::Image, ":late_catalog:")
            .expect("late catalog emoji must render on idle chat");
        assert!(rendered.rect().max.y <= h.ctx.content_rect().max.y);
        assert!(
            h.query_by_role_and_label(Role::Image, ":missing_emoji:")
                .is_none()
        );
        assert!(
            h.query_by_role_and_label(Role::Image, ":broken_emoji:")
                .is_none()
        );
        assert!(
            h.query_by_label(":missing_emoji: :broken_emoji:").is_some(),
            "unavailable custom emoji must preserve readable :name: fallback"
        );
    }
}

#[test]
fn incoming_continuation_sticker_loads_at_bottom_of_idle_chat() {
    let (mut h, mut sent, address) = idle_channel(true);
    let _ = sticker_commands(&mut sent);

    // Same author (ANN, since message 30 is ANN) within 300s -> continuation row with
    // no author header and no text above the sticker attachment.
    let blob_id = [66; 32];
    let incoming = msg(
        60,
        ANN,
        305,
        ":wave_hi:",
        vec![inline_attachment(
            66,
            "wave_hi.png",
            crate::sticker_files::STICKER_TYPE,
        )],
    );
    {
        let ctx = h.ctx.clone();
        let home = h.state_mut().as_mut().unwrap();
        home.apply(
            &ctx,
            Update::Messages {
                server: address.clone(),
                channel: PROJECTS,
                items: vec![incoming],
                older: false,
                page: None,
            },
            2.0,
        );
    }
    // While the sticker is downloading it shows a spinner (which animates), so run
    // only the two immediate scroll/load passes and verify the download was requested.
    h.run_steps(2);
    assert_eq!(sticker_commands(&mut sent), vec![(blob_id, Kind::Sticker)]);

    {
        let ctx = h.ctx.clone();
        let home = h.state_mut().as_mut().unwrap();
        home.apply(
            &ctx,
            Update::StickerFile(blob_id, Ok(picture(66, [100, 220, 220, 255]))),
            3.0,
        );
    }
    h.run();
    assert!(
        h.query_by_role_and_label(Role::Image, &t!("home.sticker", code = ":wave_hi:"))
            .is_some()
    );
    assert_eq!(h.run(), 1);
}
