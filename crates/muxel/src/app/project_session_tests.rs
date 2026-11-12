//! A project's context menu → New session starts the chosen preset in that
//! project, not in the one that's active. The menu is driven as a person
//! drives it: a right-click on the project's sidebar row, then the menu's own
//! keys.

use super::{MuxelApp, NotifKind, Notification};
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

/// Muxel's window with two projects in its sidebar: `active`, shown, with one
/// pane, and `other` below it, which no window shows. Two presets need no
/// program on the test host; the menu picks the second.
struct Sidebar {
    app: Entity<MuxelApp>,
    visual: VisualTestContext,
    cx: Option<TestAppContext>,
    active: Uuid,
    other: Uuid,
    /// The pane `active` shows.
    existing: Uuid,
    /// The pane `other` last showed, when it has one.
    other_pane: Option<Uuid>,
    chosen: Uuid,
    _storage: muxel_store::test_support::TestRoot,
    _root: RemoveDir,
}

impl Sidebar {
    fn new(other_has_a_pane: bool) -> Self {
        let root = std::env::temp_dir().join(format!("muxel-project-session-{}", Uuid::new_v4()));
        for dir in ["active", "other"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let root = root.canonicalize().unwrap();
        let storage = muxel_store::test_support::set_root(root.clone());
        let remove = RemoveDir(root.clone());

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
        let (active, other, existing, other_pane) = visual.update(|window, cx| {
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
                // The fixture's panes run no child process.
                let mut pane_in = |pid: Uuid| {
                    let instance = Instance::shell(pid);
                    let id = instance.id;
                    app.workspace.add_instance(instance);
                    let project = app.workspace.project_mut(pid).unwrap();
                    project.layout = Some(PaneNode::leaf(id));
                    project.last_focused_instance = Some(id);
                    app.failed_launches
                        .insert(id, "fixture has no child".into());
                    id
                };
                let existing = pane_in(active);
                let other_pane = other_has_a_pane.then(|| pane_in(other));
                app.select_project(active, window, cx);
                (active, other, existing, other_pane)
            })
        });
        let mut sidebar = Self {
            app,
            visual,
            cx: Some(cx),
            active,
            other,
            existing,
            other_pane,
            chosen,
            _storage: storage,
            _root: remove,
        };
        sidebar.draw();
        sidebar
    }

    /// Right-click `other`'s row, then New session, into its submenu, down to
    /// the second preset, and pick it.
    fn start_a_session_in_other(&mut self) {
        let row = self
            .visual
            .debug_bounds("proj-row-1")
            .expect("the second project's sidebar row is drawn");
        let at = row.center();
        self.visual.simulate_event(MouseDownEvent {
            position: at,
            modifiers: Modifiers::default(),
            button: MouseButton::Right,
            click_count: 1,
            first_mouse: false,
        });
        self.visual.simulate_event(MouseUpEvent {
            position: at,
            modifiers: Modifiers::default(),
            button: MouseButton::Right,
            click_count: 1,
        });
        self.draw();
        for key in ["down", "right", "down", "enter"] {
            self.visual.simulate_keystrokes(key);
            self.draw();
        }
    }

    /// `other`'s panes that the menu started, with the preset each came from.
    fn started_in_other(&mut self) -> Vec<Option<Uuid>> {
        let (other, other_pane) = (self.other, self.other_pane);
        self.visual.update(|_, cx| {
            let app = self.app.read(cx);
            app.workspace
                .project(other)
                .unwrap()
                .instances()
                .into_iter()
                .filter(|iid| Some(*iid) != other_pane)
                .map(|iid| app.workspace.instance(iid).unwrap().preset_id)
                .collect()
        })
    }

    fn draw(&mut self) {
        self.visual.run_until_parked();
        self.visual.update(|window, cx| {
            let _ = window.draw(cx);
        });
    }
}

impl Drop for Sidebar {
    fn drop(&mut self) {
        // gpui-component's context menu keeps the last menu it opened. The
        // menu's dismiss subscription holds the state that holds the menu and
        // the subscription, until the next right-click on the row replaces
        // both. Dropping the app would fail GPUI's leak check on that menu, so
        // the test leaves the app to the process. Its timers never ran, so
        // nothing of it is still working.
        std::mem::forget(self.cx.take());
    }
}

#[test]
fn new_session_starts_the_chosen_preset_in_the_right_clicked_project() {
    let mut sidebar = Sidebar::new(false);
    sidebar.start_a_session_in_other();

    assert_eq!(
        sidebar.started_in_other(),
        vec![Some(sidebar.chosen)],
        "the right-clicked project didn't get one session from the picked preset"
    );
    let (active, other, existing) = (sidebar.active, sidebar.other, sidebar.existing);
    sidebar.visual.update(|_, cx| {
        let app = sidebar.app.read(cx);
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
}

/// Switching to a project no window shows, for a session of its own, doesn't
/// attend the pane the project last showed. That pane's notification stays
/// until the user looks at it.
#[test]
fn new_session_in_a_background_project_leaves_its_last_session_unread() {
    let mut sidebar = Sidebar::new(true);
    let finished = sidebar.other_pane.unwrap();
    sidebar.visual.update(|_, cx| {
        sidebar.app.update(cx, |app, _| {
            app.notifications.push(Notification {
                id: Uuid::new_v4(),
                instance: Some(finished),
                kind: NotifKind::Done,
                title: "Finished".into(),
                subtitle: "other".into(),
            });
        })
    });
    sidebar.start_a_session_in_other();

    assert_eq!(
        sidebar.started_in_other(),
        vec![Some(sidebar.chosen)],
        "the right-clicked project didn't get one session from the picked preset"
    );
    let other = sidebar.other;
    sidebar.visual.update(|_, cx| {
        let app = sidebar.app.read(cx);
        assert_eq!(
            app.workspace.active_project,
            Some(other),
            "the session's project isn't shown"
        );
        assert!(
            app.notifications
                .iter()
                .any(|n| n.instance == Some(finished)),
            "switching to the project read its last pane's notification"
        );
    });
}
