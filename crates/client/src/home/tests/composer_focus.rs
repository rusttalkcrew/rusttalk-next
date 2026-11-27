use super::*;

const OTHER: Id = [4; 32];

/// Opening a channel puts the cursor in its message box on a computer.
#[test]
fn switching_channels_focuses_the_message_box() {
    let (mut base, _sent) = files_harness(true);
    let mut home = base.state_mut().take().unwrap();
    let mut other = home.servers[0].dir.categories[0].channels[0].clone();
    other.id = OTHER;
    other.name = "other".into();
    home.servers[0].dir.categories[0].channels.push(other);
    let mut chat = Chat::default();
    chat.opened = true;
    home.servers[0].chats.insert(OTHER, chat);
    let mut h = egui_kittest::Harness::builder()
        .with_size(Vec2::new(720.0, 420.0))
        .with_context_setup(ui::install)
        .build_ui_state(
            |ui, (home, channel): &mut (Home, Id)| chat_pane(ui, home, 0, *channel),
            (home, PROJECTS),
        );
    h.run_steps(3);
    assert!(h.ctx.memory(|m| m.has_focus(composer_id(PROJECTS))));

    // Focus elsewhere, then open another channel: its box takes the cursor.
    h.ctx
        .memory_mut(|m| m.surrender_focus(composer_id(PROJECTS)));
    h.run_steps(2);
    assert!(!h.ctx.memory(|m| m.has_focus(composer_id(PROJECTS))));
    h.state_mut().1 = OTHER;
    h.run_steps(3);
    assert!(h.ctx.memory(|m| m.has_focus(composer_id(OTHER))));

    // Staying in the channel doesn't pull focus back once it moved away.
    h.ctx.memory_mut(|m| m.surrender_focus(composer_id(OTHER)));
    h.run_steps(3);
    assert!(!h.ctx.memory(|m| m.has_focus(composer_id(OTHER))));
}
