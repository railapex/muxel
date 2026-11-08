//! A prompt typed into a pane survives the pane: saved once typing stops,
//! offered when the pane starts again, gone once it is sent, and kept when
//! the pane is closed for good.
//! An editor's unsaved changes are backed up once edits stop and offered
//! when it opens again.

use super::{Instance, MuxelApp, PaneNode, Project, TerminalSpawnMeta};
use crate::editor::{ExternalChange, read_disk_change};
use crate::theme;
use gpui::{TestAppContext, WindowHandle};
use muxel_store::rescue::RescueDir;
use muxel_terminal::{CommandSpec, TerminalLaunch, TerminalSession};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

struct TempRoot(PathBuf);

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn with_app(run: impl FnOnce(&mut TestAppContext, WindowHandle<MuxelApp>)) {
    let root = std::env::temp_dir().join(format!("muxel-rescue-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let _temp = TempRoot(root.clone());
    let _storage = muxel_store::test_support::set_root(root.clone());
    assert_eq!(muxel_store::data_dir(), Some(root.join("data")));
    let mut cx = TestAppContext::single();
    cx.skip_drawing();
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::register_bundled_themes(cx);
        cx.set_global(theme::UiScale(1.0));
        cx.set_global(theme::UiFontSize(14.0));
        theme::apply_initial_theme("", cx);
    });
    let window = cx.add_window(MuxelApp::new);
    let app = window.entity(&cx).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&mut cx, window)));
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        app.update(&mut cx, |app, cx| {
            for view in app.terminals.values() {
                view.read(cx).session().kill();
            }
        });
    }));
    drop(app);
    drop(cx);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// A pane in a workspace of its own, not yet launched.
fn pane(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>) -> (Uuid, RescueDir) {
    window
        .update(cx, |app, _, _| {
            let cwd = muxel_store::data_dir().unwrap().join("project");
            std::fs::create_dir_all(&cwd).unwrap();
            let project = app.workspace.add_project(Project::new("rescue", cwd));
            let mut instance = Instance::shell(project);
            instance.title = "Claude 2".into();
            let iid = instance.id;
            app.workspace.add_instance(instance);
            app.workspace.project_mut(project).unwrap().layout = Some(PaneNode::leaf(iid));
            app.workspace.active_project = Some(project);
            let workspace = *app.current_workspace.get_or_insert_with(Uuid::new_v4);
            app.use_tmux = false;
            app.use_worktree = false;
            (iid, muxel_store::rescue::rescue_dir(workspace).unwrap())
        })
        .unwrap()
}

/// Start (or restart) the pane's view on an inert child that muxel reads as
/// Claude. The screen is painted by the test through the production parser.
fn launch(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    iid: Uuid,
) -> Arc<TerminalSession> {
    window
        .update(cx, |app, window, cx| {
            if let Some(old) = app.terminals.remove(&iid) {
                old.read(cx).session().kill();
            }
            let spec = CommandSpec::program(
                "cmd.exe",
                vec!["/d".into(), "/q".into(), "/c".into(), "more".into()],
            )
            .with_status_program(Some("claude".into()));
            let meta = TerminalSpawnMeta {
                instance_id: iid,
                token: 0,
                program: "cmd.exe".into(),
                cwd: String::new(),
                was_resume: false,
                started: Instant::now(),
            };
            let launch = TerminalLaunch::spawn_for_instance(spec, (80, 24), iid);
            app.install_terminal_spawn(meta, launch, window, cx);
        })
        .unwrap();
    let session = window
        .read_with(cx, |app, cx| app.terminals[&iid].read(cx).session().clone())
        .unwrap();
    // Let ConPTY finish painting its own frames (cmd.exe setting its title is
    // the last of them) before the test paints over them. An unfocused pane
    // parses PTY output a paint interval after it arrives, on the executor's
    // clock, so the test clock moves while waiting; output left unparsed would
    // land on the painted input box at the next clock advance.
    let started = Instant::now();
    while (session.title().is_none() || session.idle_for() < Duration::from_millis(500))
        && started.elapsed() < Duration::from_secs(8)
    {
        cx.executor().advance_clock(Duration::from_millis(250));
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        session.title().is_some(),
        "the pane's own output is parsed before the test paints"
    );
    session
}

const RULE: &str = "────────────────────────────────────────────────────────────";

/// Claude's screen with `draft` in its input box (empty draft: an empty box).
fn paint_claude(session: &TerminalSession, draft: &str) {
    let mut screen = String::from("\x1b[?2004h\x1b[2J\x1b[H");
    screen.push_str("⏺ Ready when you are.\r\n\r\n");
    screen.push_str(RULE);
    screen.push_str("\r\n❯ ");
    screen.push_str(draft);
    screen.push_str("\r\n");
    screen.push_str(RULE);
    screen.push_str("\r\n  ? for shortcuts");
    // Cursor at the end of the draft, on the prompt row (row 4, 1-based).
    screen.push_str(&format!("\x1b[4;{}H", 3 + draft.chars().count()));
    session.process_probe_output(screen.as_bytes());
}

/// Let the watcher's one-second debounce run out, in real and test time.
fn after_debounce(cx: &mut TestAppContext) {
    std::thread::sleep(muxel_core::rescue::DRAFT_DEBOUNCE + Duration::from_millis(150));
    cx.executor()
        .advance_clock(muxel_core::rescue::DRAFT_DEBOUNCE + Duration::from_millis(150));
    cx.run_until_parked();
}

#[test]
fn a_typed_prompt_is_saved_once_typing_stops_and_offered_when_the_pane_starts_again() {
    with_app(|cx, window| {
        let (iid, dir) = pane(cx, window);
        let session = launch(cx, window, iid);
        paint_claude(&session, "fix the flaky resize test");
        session.mark_user_input();
        cx.run_until_parked();
        assert!(dir.draft(iid).is_none(), "nothing is written while typing");

        after_debounce(cx);
        let saved = dir.draft(iid).unwrap_or_else(|| {
            let (read, screen) = window
                .read_with(cx, |app, cx| {
                    let view = app.terminals[&iid].read(cx);
                    (view.current_draft(), view.visible_text())
                })
                .unwrap();
            panic!(
                "draft saved after typing stopped; the box reads {read:?}, bracketed={}, screen:\n{screen}",
                session.is_bracketed_paste()
            )
        });
        assert_eq!(saved.text, "fix the flaky resize test");
        assert!(!saved.partial);

        // The pane starts again (a crash and relaunch, or Restart): its old
        // draft is offered, and the new launch can't overwrite it.
        let session = launch(cx, window, iid);
        window
            .read_with(cx, |app, _| {
                let offers = &app.rescue.offers[&iid];
                assert_eq!(offers.len(), 1);
                assert_eq!(offers[0].draft.text, "fix the flaky resize test");
            })
            .unwrap();
        assert!(dir.draft(iid).is_none());
        paint_claude(&session, "");

        // Restore pastes it into the box without submitting it.
        let rev = session.input_rev();
        window
            .update(cx, |app, window, cx| app.restore_draft(iid, window, cx))
            .unwrap();
        assert!(session.input_rev() > rev, "the draft was pasted");
        assert!(!session.has_submitted_turn(), "Restore never submits");
        window
            .read_with(cx, |app, _| assert!(!app.rescue.offers.contains_key(&iid)))
            .unwrap();
        assert!(
            dir.draft_offers(iid).is_empty(),
            "Restore deletes the offer"
        );
    });
}

#[test]
fn a_submitted_prompt_is_cleared_and_a_closed_panes_prompt_is_kept() {
    with_app(|cx, window| {
        let (iid, dir) = pane(cx, window);
        let session = launch(cx, window, iid);
        paint_claude(&session, "ship it");
        session.mark_user_input();
        after_debounce(cx);
        assert_eq!(dir.draft(iid).unwrap().text, "ship it");

        // Enter: the file goes at once, before the box is read again.
        session.mark_turn_submitted();
        cx.run_until_parked();
        assert!(dir.draft(iid).is_none(), "submission clears the draft");
        paint_claude(&session, "");
        after_debounce(cx);
        assert!(dir.draft(iid).is_none());

        // A new draft, then the pane is closed for good.
        paint_claude(&session, "half a thought");
        session.mark_user_input();
        after_debounce(cx);
        // The strip names the pane as its tab did.
        let shown = window
            .update(cx, |app, _, cx| {
                let shown = app
                    .workspace
                    .instance(iid)
                    .unwrap()
                    .display_name()
                    .to_string();
                app.close_instance_inner(iid, "close (test)", cx);
                shown
            })
            .unwrap();
        let closed = dir.closed_drafts();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].draft.text, "half a thought");
        assert_eq!(closed[0].draft.pane.as_deref(), Some(shown.as_str()));
        window
            .read_with(cx, |app, _| {
                assert_eq!(app.rescue.closed.len(), 1);
                assert!(!app.rescue.closed_hidden);
            })
            .unwrap();
    });
}

#[test]
fn turning_rescue_off_stops_saving_and_deletes_what_was_kept() {
    with_app(|cx, window| {
        let (iid, dir) = pane(cx, window);
        let session = launch(cx, window, iid);
        paint_claude(&session, "a secret-looking draft");
        session.mark_user_input();
        after_debounce(cx);
        assert!(dir.draft(iid).is_some());

        window
            .update(cx, |app, _, cx| app.set_rescue_enabled(false, cx))
            .unwrap();
        assert!(dir.draft(iid).is_none(), "turning it off deletes the draft");
        paint_claude(&session, "typed after turning it off");
        session.mark_user_input();
        after_debounce(cx);
        assert!(dir.draft(iid).is_none(), "nothing is saved while off");
        assert_eq!(dir.counts(), muxel_store::rescue::RescueCounts::default());
    });
}

/// An editor pane on `file` in a workspace of its own, opened the way a
/// restored workspace opens it.
fn editor_pane(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    file: &std::path::Path,
) -> (Uuid, RescueDir) {
    window
        .update(cx, |app, window, cx| {
            let root = file.parent().unwrap().to_path_buf();
            let project = app.workspace.add_project(Project::new("rescue", root));
            let instance = Instance::editor(project, Some(file.to_path_buf()));
            let iid = instance.id;
            app.workspace.add_instance(instance);
            app.workspace.project_mut(project).unwrap().layout = Some(PaneNode::leaf(iid));
            app.workspace.active_project = Some(project);
            let workspace = *app.current_workspace.get_or_insert_with(Uuid::new_v4);
            app.spawn_project_pane(iid, window, cx);
            (iid, muxel_store::rescue::rescue_dir(workspace).unwrap())
        })
        .unwrap()
}

/// The editor pane `iid` goes away unsaved (a crash) and opens again.
fn reopen_editor(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>, iid: Uuid) {
    window
        .update(cx, |app, window, cx| {
            app.editors.remove(&iid);
            app.spawn_project_pane(iid, window, cx);
        })
        .unwrap();
    cx.run_until_parked();
}

fn type_into(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>, iid: Uuid, text: &str) {
    window
        .update(cx, |app, window, cx| {
            let editor = app.editors[&iid].clone();
            editor.update(cx, |editor, cx| editor.type_text(text, window, cx));
        })
        .unwrap();
    cx.run_until_parked();
}

#[test]
fn an_editors_unsaved_changes_are_backed_up_and_restored_after_a_crash() {
    with_app(|cx, window| {
        let file = muxel_store::data_dir()
            .unwrap()
            .join("project")
            .join("notes.txt");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "original\n").unwrap();
        let (iid, dir) = editor_pane(cx, window, &file);
        type_into(cx, window, iid, "mine: ");
        assert!(
            dir.editor_backup(iid).is_none(),
            "nothing is written while typing"
        );

        after_debounce(cx);
        let backup = dir
            .editor_backup(iid)
            .expect("backup saved after edits stopped");
        assert_eq!(backup.text, "mine: original\n");
        assert_eq!(backup.path.as_deref(), Some(file.as_path()));

        // muxel dies; the file opens again as it is on disk, with an offer.
        reopen_editor(cx, window, iid);
        window
            .update(cx, |app, window, cx| {
                let editor = app.editors[&iid].clone();
                assert!(editor.read(cx).backup_offered());
                assert_eq!(editor.read(cx).text(cx), "original\n");
                editor.update(cx, |editor, cx| editor.restore_backup(window, cx));
                let editor = editor.read(cx);
                assert_eq!(editor.text(cx), "mine: original\n");
                assert!(editor.is_dirty(), "restored changes are unsaved");
            })
            .unwrap();

        // Saving clears the backup.
        window
            .update(cx, |app, _, cx| {
                let editor = app.editors[&iid].clone();
                std::fs::write(&file, editor.read(cx).text(cx)).unwrap();
                editor.update(cx, |editor, cx| editor.mark_saved(cx));
            })
            .unwrap();
        cx.run_until_parked();
        assert!(
            dir.editor_backup(iid).is_none(),
            "saving deletes the backup"
        );
    });
}

#[test]
fn a_backup_of_a_file_changed_on_disk_since_keeps_both_versions() {
    with_app(|cx, window| {
        let file = muxel_store::data_dir()
            .unwrap()
            .join("project")
            .join("plan.md");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "v1\n").unwrap();
        let (iid, dir) = editor_pane(cx, window, &file);
        type_into(cx, window, iid, "mine ");
        after_debounce(cx);
        assert!(dir.editor_backup(iid).is_some());

        // Someone else writes the file while muxel is down.
        std::fs::write(&file, "theirs, longer\n").unwrap();
        reopen_editor(cx, window, iid);
        window
            .update(cx, |app, window, cx| {
                let editor = app.editors[&iid].clone();
                assert!(editor.read(cx).backup_offered());
                editor.update(cx, |editor, cx| {
                    editor.restore_backup(window, cx);
                    // The next disk poll offers their version beside mine.
                    let change = read_disk_change(&file, editor.disk_stamp())
                        .expect("the poll sees the disk version");
                    editor.apply_disk_change(change, window, cx);
                });
                let editor = editor.read(cx);
                assert_eq!(editor.text(cx), "mine v1\n");
                assert!(matches!(
                    editor.external_change(),
                    Some(ExternalChange::Modified(theirs)) if theirs == "theirs, longer\n"
                ));
            })
            .unwrap();
    });
}

#[test]
fn closing_an_editor_without_saving_deletes_its_backup() {
    with_app(|cx, window| {
        let file = muxel_store::data_dir()
            .unwrap()
            .join("project")
            .join("scratch.txt");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "x\n").unwrap();
        let (iid, dir) = editor_pane(cx, window, &file);
        type_into(cx, window, iid, "y");
        after_debounce(cx);
        assert!(dir.editor_backup(iid).is_some());
        window
            .update(cx, |app, _, cx| {
                app.close_instance_inner(iid, "close (without saving)", cx)
            })
            .unwrap();
        cx.run_until_parked();
        assert!(dir.editor_backup(iid).is_none());
    });
}
