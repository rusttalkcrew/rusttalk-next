use super::*;
use crate::net::Fetch;
use egui_kittest::kittest::Queryable;
use rusttalk_core::client::{BlobRef, decode_frames, prepare_file};

#[test]
fn chat_picture_reload() {
    for dm in [false, true] {
        let (mut base, mut sent) = files_harness(!dm);
        let mut home = base.state_mut().take().unwrap();
        let address = home.servers[0].address.clone();
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(640, 280)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let png = png.into_inner();
        let (file, thumbnail) = prepare_file("Synthetic.png", "image/png", &png, &png).unwrap();
        let thumb = file.thumbnail.clone().unwrap();
        let thumbnail = thumbnail.unwrap();

        let msg = Msg {
            id: [44; 32],
            person: ANN,
            at: 1,
            body: Body::Text("Synthetic photo".into()),
            backfilled: false,
            reply_to: None,
            quoted_author: None,
            edit_of: None,
            files: vec![file.clone()],
            live: None,
            sticker: None,
            replicas: vec![address.clone()],
            copy_files: vec![],
        };
        if dm {
            let mut dir = home.servers[0].dir.clone();
            dir.dms.push(crate::server::Dm {
                channel: PROJECTS,
                peer: ANN,
            });
            dir.categories.clear();
            home.apply(
                &base.ctx,
                Update::Cached(address.clone(), Box::new(dir), vec![(PROJECTS, vec![msg])]),
                0.0,
            );
        } else {
            home.servers[0]
                .chats
                .get_mut(&PROJECTS)
                .unwrap()
                .messages
                .push(msg);
        }
        let mut h = egui_kittest::Harness::builder()
            .with_size(Vec2::new(900.0, 640.0))
            .with_context_setup(ui::install)
            .build_ui_state(
                move |ui, home| {
                    if dm {
                        dm_pane(ui, home, ANN);
                    } else {
                        chat_pane(ui, home, 0, PROJECTS);
                    }
                    files::viewer(ui.ctx(), &mut home.files);
                },
                home,
            );
        h.ctx.set_theme(if dm {
            egui::Theme::Light
        } else {
            egui::Theme::Dark
        });
        let ctx = h.ctx.clone();
        let mut image_commands = || {
            let mut commands = Vec::new();
            while let Ok(command) = sent.try_recv() {
                if let Command::Image(blob, fetch) = command {
                    commands.push((blob.id, fetch));
                }
            }
            commands
        };
        let load = |home: &mut Home, blob: &BlobRef, fetch, bytes: &[u8]| {
            let frames = decode_frames(bytes, None, 64 * 1024 * 1024).unwrap();
            let texture_before = ctx.tex_manager().read().num_allocated();
            home.apply(
                &ctx,
                Update::Image(
                    address.clone(),
                    blob.clone(),
                    fetch,
                    Ok(frames.into_iter().map(crate::net::frame_image).collect()),
                ),
                0.0,
            );
            let textures = ctx.tex_manager();
            let textures = textures.read();
            assert_eq!(textures.num_allocated(), texture_before + 1);
            textures
                .allocated()
                .max_by_key(|(id, _)| *id)
                .unwrap()
                .0
                .to_owned()
        };
        let image = |h: &egui_kittest::Harness<'_, Home>, texture| {
            h.ctx
                .tessellate(h.output().shapes.clone(), h.output().pixels_per_point)
                .iter()
                .find_map(|s| match &s.primitive {
                    egui::epaint::Primitive::Mesh(mesh) if mesh.texture_id == texture => {
                        Some(mesh.calc_bounds().center())
                    }
                    _ => None,
                })
                .expect("image is painted")
        };
        let click = |h: &mut egui_kittest::Harness<'_, Home>, pos| {
            h.event(egui::Event::PointerMoved(pos));
            for pressed in [true, false] {
                h.event(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                });
                h.run_steps(1);
            }
            h.run_steps(2);
        };

        h.run_steps(4);
        assert_eq!(image_commands(), [(thumb.id, Fetch::Thumbnail)]);
        let tile = load(h.state_mut(), &thumb, Fetch::Thumbnail, &thumbnail);
        h.run_steps(4);
        let tile_pos = image(&h, tile);
        click(&mut h, tile_pos);
        assert_eq!(image_commands(), [(file.file.id, Fetch::Full)]);
        h.get_by_label(t!("common.close")).click();
        h.run_steps(4);
        let tile_pos = image(&h, tile);
        click(&mut h, tile_pos);
        let pending_reopen = image_commands();
        assert!(
            pending_reopen.is_empty(),
            "join the request already in flight"
        );

        let original = load(h.state_mut(), &file.file, Fetch::Full, &png);
        assert_eq!(
            ctx.tex_manager().read().meta(original).unwrap().size,
            [640, 280]
        );
        h.run_steps(4);
        image(&h, original);
        h.get_by_label(t!("common.close")).click();
        h.run_steps(4);
        assert_eq!(image_commands(), []);
        h.state_mut()
            .apply(&ctx, Update::Status(address.clone(), Status::Offline), 0.0);
        h.run_steps(4);
        let tile_pos = image(&h, tile);
        click(&mut h, tile_pos);
        let reopen = image_commands();
        let original_retained = ctx.tex_manager().read().meta(original).is_some();
        eprintln!(
            "dm={dm} thumbnail=1 full_first=1 pending_reopen=0 close=0 offline_reopen={} original_texture_retained={original_retained}",
            reopen.len()
        );
        assert!(reopen.is_empty());
        assert!(original_retained);
        image(&h, original);
        assert!(
            h.output()
                .textures_delta
                .set
                .iter()
                .all(|(id, _)| *id != original)
        );
    }
}
