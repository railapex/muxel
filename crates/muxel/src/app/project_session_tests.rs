//! A project's context menu → New session starts the chosen preset in that
//! project, not in the one that's active. The menu is driven as a person
//! drives it: a right-click on the project's sidebar row, then the menu's own
//! keys.

use super::MuxelApp;
use crate::theme;
use gpui::{
    AppContext as _, Entity, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, TestAppContext,
    VisualTestContext,
};
use muxel_core::{AgentPreset, Instance, PaneNode, Project};
use std::path::PathBuf;
use uuid::Uuid;

struct RemoveDir(PathBuf);

impl Drop for RemoveDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn new_session_starts_the_chosen_preset_in_the_right_clicked_project() {
    let root = std::env::temp_dir().join(format!("muxel-project-session-{}", Uuid::new_v4()));
    for dir in ["active", "other"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    let root = root.canonicalize().unwrap();
    let _storage = muxel_store::test_support::set_root(root.clone());
    let _root = RemoveDir(root.clone());

    let mut cx = TestAppContext::single();
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::register_bundled_themes(cx);
        cx.set_global(theme::UiScale(1.0));
        cx.set_global(theme::UiFontSize(14.0));
        theme::apply_initial_theme("", cx);
        super::install_keybindings(&muxel_core::Settings::default(), cx);
    });
    let mut app: Option<Entity<MuxelApp>> = None;
    let window = cx.add_window(|window, cx| {
        let view = cx.new(|cx| MuxelApp::new(window, cx));
        app = Some(view.clone());
        gpui_component::Root::new(view, window, cx)
    });
    let app = app.unwrap();
    let mut visual = VisualTestContext::from_window(window.into(), &cx);

    // Two presets that need no program on the test host; the second is the
    // one to pick. Project `active` is shown and has a pane; `other`, below
    // it in the sidebar, has none.
    let first = AgentPreset {
        id: Uuid::new_v4(),
        name: "First".into(),
        ..AgentPreset::shell()
    };
    let second = AgentPreset {
        id: Uuid::new_v4(),
        name: "Second".into(),
        ..AgentPreset::shell()
    };
    let chosen = second.id;
    let (active, other, existing) = visual.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.show_terms = false;
            app.show_workspace_selector = false;
            app.current_workspace = Some(Uuid::new_v4());
            app.use_tmux = false;
            app.use_worktree = false;
            app.presets = vec![first, second];
            app.current_preset = 0;
            let active = app
                .workspace
                .add_project(Project::new("active", root.join("active")));
            let other = app
                .workspace
                .add_project(Project::new("other", root.join("other")));
            let existing = Instance::shell(active);
            let existing_id = existing.id;
            app.workspace.add_instance(existing);
            app.workspace.project_mut(active).unwrap().layout = Some(PaneNode::leaf(existing_id));
            // No child process for the fixture's pane.
            app.failed_launches
                .insert(existing_id, "fixture has no child".into());
            app.select_project(active, window, cx);
            (active, other, existing_id)
        })
    });
    draw(&mut visual);

    let row = visual
        .debug_bounds("proj-row-1")
        .expect("the second project's sidebar row is drawn");
    let at = row.center();
    visual.simulate_event(MouseDownEvent {
        position: at,
        modifiers: Modifiers::default(),
        button: MouseButton::Right,
        click_count: 1,
        first_mouse: false,
    });
    visual.simulate_event(MouseUpEvent {
        position: at,
        modifiers: Modifiers::default(),
        button: MouseButton::Right,
        click_count: 1,
    });
    draw(&mut visual);
    // New session, into its submenu, down to the second preset, and pick it.
    for key in ["down", "right", "down", "enter"] {
        visual.simulate_keystrokes(key);
        draw(&mut visual);
    }

    visual.update(|_, cx| {
        let app = app.read(cx);
        let started = app.workspace.project(other).unwrap().instances();
        assert_eq!(started.len(), 1, "the right-clicked project got no session");
        assert_eq!(
            app.workspace.instance(started[0]).unwrap().preset_id,
            Some(chosen),
            "the session isn't the preset that was picked"
        );
        assert_eq!(
            app.workspace.project(active).unwrap().instances(),
            vec![existing],
            "the active project changed"
        );
        assert_eq!(
            app.workspace.active_project,
            Some(other),
            "the session's project isn't shown"
        );
    });

    drop(app);
    drop(visual);
    // gpui-component's context menu keeps the last menu it opened. The menu's
    // dismiss subscription holds the state that holds the menu and the
    // subscription, until the next right-click on the row replaces both.
    // Dropping the app would fail GPUI's leak check on that menu, so the test
    // leaves the app to the process. Its timers never ran, so nothing of it
    // is still working.
    std::mem::forget(cx);
}

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
}
