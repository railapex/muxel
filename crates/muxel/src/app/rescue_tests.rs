//! A prompt typed into a pane survives the pane: saved once typing stops,
//! offered when the pane starts again, gone once it is sent, and kept when
//! the pane is closed for good.
//! An editor's unsaved changes are backed up once edits stop and offered
//! when it opens again.
//! After an unclean exit one notice says what was kept and who was mid-turn.

use super::rescue::CrashNotice;
use super::{Instance, MuxelApp, PaneNode, Project, TerminalSpawnMeta, Workspace};
use crate::editor::{ExternalChange, read_disk_change};
use crate::theme;
use gpui::{TestAppContext, WindowHandle};
use muxel_core::rescue::{DraftSource, PromptDraft};
use muxel_core::{SplitDirection, split};
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
/// Claude. The child prints one line, as a real program draws its screen
/// (muxel waits for visible output before it compares an offer with the
/// box); the test then paints the screen through the production parser.
fn launch(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    iid: Uuid,
) -> Arc<TerminalSession> {
    launch_child(cx, window, iid, inert())
}

fn inert() -> CommandSpec {
    CommandSpec::program(
        "cmd.exe",
        vec![
            "/d".into(),
            "/q".into(),
            "/c".into(),
            "echo ready&more".into(),
        ],
    )
}

/// [`launch`] with `child` as the pane's program.
fn launch_child(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    iid: Uuid,
    child: CommandSpec,
) -> Arc<TerminalSession> {
    launch_as(
        cx,
        window,
        iid,
        child.with_status_program(Some("claude".into())),
    )
}

/// [`launch`], read as a shell: the test then paints its prompt.
fn launch_shell(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    iid: Uuid,
) -> Arc<TerminalSession> {
    launch_as(cx, window, iid, inert())
}

fn launch_as(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    iid: Uuid,
    spec: CommandSpec,
) -> Arc<TerminalSession> {
    window
        .update(cx, |app, window, cx| {
            if let Some(old) = app.terminals.remove(&iid) {
                old.read(cx).session().kill();
            }
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

/// A child that reads one line from its terminal, as a program in cooked
/// mode does, and writes the line to a file. What reached the child, and
/// when, is then on disk instead of in muxel's bookkeeping.
struct LineReader(PathBuf);

impl LineReader {
    fn spawn() -> (CommandSpec, LineReader) {
        let dir = std::env::temp_dir();
        let name = format!("muxel-line-{}.txt", Uuid::new_v4());
        // `title` because nothing else here is a program cmd starts, so no
        // title reaches the pane otherwise and `launch_child` waits for one.
        // `line` starts as one space: an empty line leaves it as it was, and
        // cmd would write an undefined `!line!` as it stands.
        let read_a_line = format!("title line reader&set line= &set /p line=&>{name} echo(!line!");
        let spec = CommandSpec::program(
            "cmd.exe",
            vec![
                "/d".into(),
                "/q".into(),
                "/v:on".into(),
                "/c".into(),
                read_a_line,
            ],
        )
        .with_cwd(dir.display().to_string());
        (spec, LineReader(dir.join(name)))
    }

    /// The line the child read, if it has read one yet, without trailing
    /// whitespace.
    fn line(&self) -> Option<String> {
        let text = std::fs::read_to_string(&self.0).ok()?;
        Some(text.trim_end().to_string())
    }

    /// Wait (eight seconds at most) for the child to read a line.
    fn wait_line(&self) -> Option<String> {
        let started = Instant::now();
        while !self.0.exists() && started.elapsed() < Duration::from_secs(8) {
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(200));
        self.line()
    }
}

impl Drop for LineReader {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Restore on the pane's newest offered draft, as its row's button does.
fn restore_newest(
    app: &mut MuxelApp,
    iid: Uuid,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<MuxelApp>,
) {
    let path = app.rescue.offers[&iid][0].path.clone();
    app.restore_offer(iid, &path, window, cx);
}

/// A second of real and test time for the pane's child and timers.
fn settle(cx: &mut TestAppContext) {
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(250));
        cx.executor().advance_clock(Duration::from_millis(250));
        cx.run_until_parked();
    }
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
        // Painted, never typed through muxel: the screen alone can't show
        // where the prompt ends.
        assert!(saved.partial);

        // The pane starts again (a crash and relaunch, or Restart): its old
        // draft is offered, and the new launch can't overwrite it. This
        // child writes down the line it reads.
        let (child, reader) = LineReader::spawn();
        let session = launch_child(cx, window, iid, child);
        window
            .read_with(cx, |app, _| {
                let offers = &app.rescue.offers[&iid];
                assert_eq!(offers.len(), 1);
                assert_eq!(offers[0].draft.text, "fix the flaky resize test");
            })
            .unwrap();
        assert!(dir.draft(iid).is_none());
        paint_claude(&session, "");
        session.process_probe_output(b"\x1b[?2004l");

        // Restore types it into the box without pressing Enter: the child
        // has no line until the test presses Enter itself.
        window
            .update(cx, |app, window, cx| restore_newest(app, iid, window, cx))
            .unwrap();
        settle(cx);
        assert_eq!(reader.line(), None, "Restore never sends Enter");
        session.try_write_input(b"\r").unwrap();
        assert_eq!(
            reader.wait_line().as_deref(),
            Some("fix the flaky resize test"),
            "the child got the draft as it was typed"
        );
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
                assert!(editor.is_dirty(cx), "restored changes are unsaved");
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
                // Restoring offers their version beside mine at once.
                editor.update(cx, |editor, cx| editor.restore_backup(window, cx));
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

/// A workspace saved to disk with one empty project, as another run left it.
fn saved_workspace(name: &str) -> Uuid {
    let id = Uuid::new_v4();
    let root = muxel_store::data_dir()
        .unwrap()
        .join(format!("{name}-project"));
    std::fs::create_dir_all(&root).unwrap();
    let mut doc = Workspace::default();
    doc.add_project(Project::new(name, root));
    muxel_store::save_workspace_to(&muxel_store::workspace_doc_path(id).unwrap(), &doc).unwrap();
    id
}

fn draft(text: &str) -> PromptDraft {
    PromptDraft {
        text: text.into(),
        partial: false,
        saved_at_ms: 1,
        source: DraftSource::Agent,
        pane: None,
    }
}

/// Every file under `dir`, recursively, as paths relative to it.
fn files_under(dir: &std::path::Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                found.push(path.strip_prefix(dir).unwrap().display().to_string());
            }
        }
    }
    found.sort();
    found
}

#[test]
fn turning_rescue_off_deletes_what_every_workspace_kept_and_stops_other_writers() {
    with_app(|cx, window| {
        let (iid, first) = pane(cx, window);
        let session = launch(cx, window, iid);
        paint_claude(&session, "kept in the first workspace");
        session.mark_user_input();
        after_debounce(cx);
        assert!(first.draft(iid).is_some());

        // Move to another workspace, then turn saving off there.
        let other = saved_workspace("other");
        window
            .update(cx, |app, window, cx| {
                app.use_tmux = false;
                app.enter_workspace(other, window, cx);
                app.set_rescue_enabled(false, cx);
            })
            .unwrap();
        assert_eq!(
            files_under(first.path()),
            Vec::<String>::new(),
            "the first workspace's draft is deleted too"
        );

        // A writer that still thinks saving is on (another muxel window on
        // the same data) leaves nothing behind either.
        let elsewhere = Uuid::new_v4();
        let _ = first.save_draft(elsewhere, &draft("typed in another window"));
        assert!(first.draft(elsewhere).is_none());
    });
}

#[test]
fn restore_checks_the_disk_again_and_keeps_a_newer_version_beside_the_backup() {
    with_app(|cx, window| {
        let file = muxel_store::data_dir()
            .unwrap()
            .join("project")
            .join("late.txt");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "v1\n").unwrap();
        let (iid, dir) = editor_pane(cx, window, &file);
        type_into(cx, window, iid, "mine ");
        after_debounce(cx);
        assert!(dir.editor_backup(iid).is_some());

        reopen_editor(cx, window, iid);
        window
            .update(cx, |app, window, cx| {
                let editor = app.editors[&iid].clone();
                assert!(editor.read(cx).backup_offered());
                editor.update(cx, |editor, cx| {
                    // Someone writes the file while the offer is showing; the
                    // clean buffer takes their version.
                    std::fs::write(&file, "theirs, written later\n").unwrap();
                    let change = read_disk_change(&file, editor.disk_stamp())
                        .expect("the poll sees their version");
                    editor.apply_disk_change(change, window, cx);
                    assert_eq!(editor.text(cx), "theirs, written later\n");
                    editor.restore_backup(window, cx);
                    // A later poll changes nothing about the outcome.
                    if let Some(change) = read_disk_change(&file, editor.disk_stamp()) {
                        editor.apply_disk_change(change, window, cx);
                    }
                });
                let editor = editor.read(cx);
                assert_eq!(editor.text(cx), "mine v1\n");
                assert!(editor.is_dirty(cx));
                assert!(
                    matches!(
                        editor.external_change(),
                        Some(ExternalChange::Modified(theirs)) if theirs == "theirs, written later\n"
                    ),
                    "their version is kept beside the restored changes"
                );
            })
            .unwrap();
    });
}

#[test]
fn a_pane_still_showing_its_offered_draft_keeps_a_saved_copy_and_whitespace_counts() {
    with_app(|cx, window| {
        let (iid, dir) = pane(cx, window);
        let session = launch(cx, window, iid);
        paint_claude(&session, "printf 'a  b'");
        session.mark_user_input();
        after_debounce(cx);
        assert_eq!(dir.draft(iid).unwrap().text, "printf 'a  b'");

        // The pane starts again showing nearly the same text: one space
        // fewer is a different command, so the offer stays.
        let session = launch(cx, window, iid);
        paint_claude(&session, "printf 'a b'");
        settle(cx);
        settle(cx);
        window
            .read_with(cx, |app, _| {
                assert!(
                    app.rescue.offers.contains_key(&iid),
                    "whitespace that differs keeps the offer"
                )
            })
            .unwrap();

        // Started again showing exactly the offered text (a tmux session that
        // outlived muxel): the offer goes, but a saved copy stays.
        let session = launch(cx, window, iid);
        paint_claude(&session, "printf 'a  b'");
        settle(cx);
        settle(cx);
        window
            .read_with(cx, |app, _| assert!(!app.rescue.offers.contains_key(&iid)))
            .unwrap();
        assert_eq!(
            dir.draft(iid).map(|d| d.text).as_deref(),
            Some("printf 'a  b'"),
            "the text the pane shows is still saved"
        );
    });
}

#[test]
fn an_editor_offering_an_old_backup_still_backs_up_new_edits() {
    with_app(|cx, window| {
        let file = muxel_store::data_dir()
            .unwrap()
            .join("project")
            .join("both.txt");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "base\n").unwrap();
        let (iid, dir) = editor_pane(cx, window, &file);
        type_into(cx, window, iid, "old ");
        after_debounce(cx);
        assert_eq!(dir.editor_backup(iid).unwrap().text, "old base\n");

        reopen_editor(cx, window, iid);
        type_into(cx, window, iid, "new ");
        after_debounce(cx);
        assert_eq!(
            dir.editor_backup(iid).map(|b| b.text).as_deref(),
            Some("new base\n"),
            "edits made while the offer shows are backed up"
        );

        // Discarding the old backup keeps the new edits' backup.
        window
            .update(cx, |app, _, cx| {
                let editor = app.editors[&iid].clone();
                assert!(editor.read(cx).backup_offered());
                editor.update(cx, |editor, cx| editor.discard_backup(cx));
            })
            .unwrap();
        after_debounce(cx);
        assert_eq!(
            dir.editor_backup(iid).map(|b| b.text).as_deref(),
            Some("new base\n")
        );
    });
}

#[test]
fn a_popped_out_editor_keeps_the_disk_version_its_edits_started_from() {
    with_app(|cx, window| {
        let file = muxel_store::data_dir()
            .unwrap()
            .join("project")
            .join("moved.txt");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "A\n").unwrap();
        let (iid, dir) = editor_pane(cx, window, &file);
        type_into(cx, window, iid, "x");
        window
            .update(cx, |app, _, cx| {
                let editor = app.editors[&iid].clone();
                editor.update(cx, |editor, cx| editor.flush_backup(cx));
            })
            .unwrap();
        let base = dir.editor_backup(iid).unwrap().base;
        assert!(base.is_some());

        // The file changes on disk beside the unsaved edits, then the pane
        // is popped out (its editor rebuilt) and backed up again.
        std::fs::write(&file, "B, written elsewhere\n").unwrap();
        window
            .update(cx, |app, window, cx| {
                let editor = app.editors[&iid].clone();
                editor.update(cx, |editor, cx| {
                    let change = read_disk_change(&file, editor.disk_stamp()).unwrap();
                    editor.apply_disk_change(change, window, cx);
                });
                let rebuilt = super::EditorSnapshot::capture(&editor, cx).build(
                    app.editor_config(),
                    window,
                    cx,
                );
                rebuilt.update(cx, |editor, cx| editor.flush_backup(cx));
                app.editors.insert(iid, rebuilt);
            })
            .unwrap();
        assert_eq!(
            dir.editor_backup(iid).unwrap().base,
            base,
            "the backup still names the version the edits started from"
        );
    });
}

#[test]
fn an_input_method_composition_is_backed_up_and_its_cancellation_too() {
    with_app(|cx, window| {
        let file = muxel_store::data_dir()
            .unwrap()
            .join("project")
            .join("ime.txt");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "text\n").unwrap();
        let (iid, dir) = editor_pane(cx, window, &file);
        let compose = |cx: &mut TestAppContext, text: &str| {
            window
                .update(cx, |app, window, cx| {
                    let editor = app.editors[&iid].clone();
                    editor.update(cx, |editor, cx| editor.compose_text(text, window, cx));
                })
                .unwrap();
            cx.run_until_parked();
        };
        compose(cx, "にほ");
        after_debounce(cx);
        assert_eq!(
            dir.editor_backup(iid).map(|b| b.text).as_deref(),
            Some("にほtext\n"),
            "an uncommitted composition is backed up"
        );
        compose(cx, "");
        after_debounce(cx);
        assert!(
            dir.editor_backup(iid).is_none(),
            "a cancelled composition leaves the buffer as the file is"
        );
    });
}

#[test]
fn restart_keeps_a_prompt_typed_just_before_it() {
    with_app(|cx, window| {
        let (iid, dir) = pane(cx, window);
        let session = launch(cx, window, iid);
        paint_claude(&session, "typed right before Restart");
        session.mark_user_input();
        cx.run_until_parked();
        window
            .update(cx, |app, window, cx| app.restart_agent(iid, window, cx))
            .unwrap();
        cx.run_until_parked();
        let kept = dir
            .draft(iid)
            .or_else(|| dir.draft_offers(iid).into_iter().next().map(|o| o.draft))
            .map(|d| d.text);
        assert_eq!(kept.as_deref(), Some("typed right before Restart"));
    });
}

#[test]
fn a_submission_during_the_debounce_clears_the_draft_at_once() {
    with_app(|cx, window| {
        let (iid, dir) = pane(cx, window);
        let session = launch(cx, window, iid);
        paint_claude(&session, "first");
        session.mark_user_input();
        after_debounce(cx);
        assert_eq!(dir.draft(iid).unwrap().text, "first");

        // More typing starts the debounce; Enter lands inside it.
        paint_claude(&session, "first, then more");
        session.mark_user_input();
        cx.run_until_parked();
        session.mark_turn_submitted();
        cx.run_until_parked();
        assert!(
            dir.draft(iid).is_none(),
            "submission clears it without waiting"
        );

        // Typed, sent and closed inside one debounce: nothing is kept.
        paint_claude(&session, "sent just before closing");
        session.mark_user_input();
        cx.run_until_parked();
        session.mark_turn_submitted();
        paint_claude(&session, "");
        window
            .update(cx, |app, _, cx| {
                app.close_instance_inner(iid, "close (test)", cx)
            })
            .unwrap();
        assert!(dir.closed_drafts().is_empty(), "a sent prompt isn't kept");
    });
}

#[test]
fn a_write_cut_off_by_a_crash_leaves_no_hidden_copy() {
    with_app(|cx, window| {
        let crashed = saved_workspace("cut-off");
        let dir = muxel_store::rescue::rescue_dir(crashed).unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        let pane = Uuid::new_v4();
        let temp = dir
            .path()
            .join(format!(".draft-{pane}.json.{}.tmp", Uuid::new_v4()));
        std::fs::write(&temp, r#"{"text":"half-written secret","saved_at_ms":1}"#).unwrap();
        window
            .update(cx, |app, window, cx| {
                app.use_tmux = false;
                app.enter_workspace(crashed, window, cx);
            })
            .unwrap();
        assert!(!temp.exists(), "the interrupted write's temporary is gone");
    });
}

#[test]
fn deleting_a_workspace_deletes_what_it_kept() {
    with_app(|cx, window| {
        let doomed = saved_workspace("doomed");
        let dir = muxel_store::rescue::rescue_dir(doomed).unwrap();
        dir.save_draft(Uuid::new_v4(), &draft("never sent"))
            .unwrap();
        let workspace_dir = dir.path().parent().unwrap().to_path_buf();
        window
            .update(cx, |app, _, cx| {
                let current = *app.current_workspace.get_or_insert_with(Uuid::new_v4);
                for id in [current, doomed] {
                    app.workspaces.workspaces.push(muxel_core::WorkspaceMeta {
                        id,
                        name: id.to_string(),
                    });
                }
                app.delete_workspace(doomed, cx);
            })
            .unwrap();
        assert_eq!(files_under(&workspace_dir), Vec::<String>::new());
        assert!(!workspace_dir.exists(), "its directory is gone");
    });
}

#[test]
fn a_crash_while_deleting_a_workspace_leaves_nothing_it_kept_once_muxel_starts_again() {
    with_app(|cx, window| {
        let doomed = saved_workspace("doomed");
        let kept = saved_workspace("kept");
        let dir = muxel_store::rescue::rescue_dir(doomed).unwrap();
        dir.save_draft(Uuid::new_v4(), &draft("never sent"))
            .unwrap();
        let workspace_dir = dir.path().parent().unwrap().to_path_buf();
        super::STOP_AFTER_INDEX_SAVE.with(|stop| stop.set(true));
        window
            .update(cx, |app, _, cx| {
                let current = *app.current_workspace.get_or_insert_with(Uuid::new_v4);
                for id in [current, doomed, kept] {
                    app.workspaces.workspaces.push(muxel_core::WorkspaceMeta {
                        id,
                        name: id.to_string(),
                    });
                }
                app.delete_workspace(doomed, cx);
            })
            .unwrap();
        super::STOP_AFTER_INDEX_SAVE.with(|stop| stop.set(false));
        // muxel starts again and opens a workspace.
        window
            .update(cx, |app, window, cx| {
                app.use_tmux = false;
                app.enter_workspace(kept, window, cx);
            })
            .unwrap();
        assert_eq!(
            files_under(&workspace_dir),
            Vec::<String>::new(),
            "nothing the deleted workspace kept is left"
        );
    });
}

#[test]
fn saving_turned_off_elsewhere_deletes_what_every_workspace_kept_when_muxel_starts() {
    with_app(|cx, window| {
        let other = saved_workspace("other");
        let opened = saved_workspace("opened");
        let dir = muxel_store::rescue::rescue_dir(other).unwrap();
        dir.save_draft(Uuid::new_v4(), &draft("never sent"))
            .unwrap();
        // Another window turned saving off; this draft outlived its purge.
        muxel_store::rescue::set_rescue_turned_off(true).unwrap();
        window
            .update(cx, |app, window, cx| {
                app.use_tmux = false;
                app.enter_workspace(opened, window, cx);
            })
            .unwrap();
        assert_eq!(
            files_under(dir.path()),
            Vec::<String>::new(),
            "the other workspace's draft is gone too"
        );
    });
}

#[test]
fn a_window_started_with_saving_off_keeps_what_another_window_kept_once_it_is_back_on() {
    with_app(|cx, window| {
        let other = saved_workspace("other");
        let opened = saved_workspace("opened");
        // This window loaded its settings while saving was off. Another
        // window has since turned it back on (the marker is gone) and kept a
        // draft.
        window
            .update(cx, |app, _, _| app.settings.rescue_unsent_text = false)
            .unwrap();
        let dir = muxel_store::rescue::rescue_dir(other).unwrap();
        dir.save_draft(Uuid::new_v4(), &draft("kept by the other window"))
            .unwrap();
        window
            .update(cx, |app, window, cx| {
                app.use_tmux = false;
                app.enter_workspace(opened, window, cx);
            })
            .unwrap();
        assert_eq!(
            files_under(dir.path()).len(),
            1,
            "the other window's draft stays"
        );
        window
            .read_with(cx, |app, _| {
                assert!(app.settings.rescue_unsent_text, "this window saves again")
            })
            .unwrap();
    });
}

#[test]
fn a_window_that_loaded_saving_off_starts_saving_once_it_takes_up_the_shared_setting() {
    with_app(|cx, window| {
        let (iid, dir) = pane(cx, window);
        // Loaded while saving was off; another window has turned it on since.
        window
            .update(cx, |app, _, _| app.settings.rescue_unsent_text = false)
            .unwrap();
        let session = launch(cx, window, iid);
        paint_claude(&session, "typed while this window thought saving was off");
        session.mark_user_input();
        after_debounce(cx);
        assert!(dir.draft(iid).is_none(), "nothing is saved yet");

        // Opening the settings takes up the shared setting.
        window
            .update(cx, |app, _, cx| app.adopt_shared_rescue_setting(cx))
            .unwrap();
        session.mark_user_input();
        after_debounce(cx);
        assert_eq!(
            dir.draft(iid).map(|d| d.text).as_deref(),
            Some("typed while this window thought saving was off")
        );
    });
}

#[test]
fn a_window_that_loaded_saving_off_never_saves_it_off_for_the_others() {
    with_app(|cx, window| {
        // Loaded while saving was off; another window has turned it on since.
        window
            .update(cx, |app, _, _| {
                app.settings.rescue_unsent_text = false;
                app.persist_settings();
            })
            .unwrap();
        assert!(muxel_store::load_settings().rescue_unsent_text);
        assert!(!muxel_store::rescue::rescue_turned_off());
    });
}

#[test]
fn restoring_a_multi_line_prompt_where_enter_would_send_it_types_nothing() {
    with_app(|cx, window| {
        let (iid, dir) = pane(cx, window);
        dir.save_draft(iid, &draft("first line\nsecond line"))
            .unwrap();
        let (child, reader) = LineReader::spawn();
        let session = launch_child(cx, window, iid, child);
        window
            .read_with(cx, |app, _| assert_eq!(app.rescue.offers[&iid].len(), 1))
            .unwrap();
        // This child never asked for bracketed paste, so a pasted newline
        // would reach it as Enter.
        paint_claude(&session, "");
        session.process_probe_output(b"\x1b[?2004l");
        window
            .update(cx, |app, window, cx| restore_newest(app, iid, window, cx))
            .unwrap();
        settle(cx);
        assert_eq!(reader.line(), None, "nothing reached the child");
        assert_eq!(
            cx.read_from_clipboard().and_then(|c| c.text()).as_deref(),
            Some("first line\nsecond line"),
            "the prompt is on the clipboard instead"
        );
        session.try_write_input(b"\r").unwrap();
        assert_eq!(
            reader.wait_line().as_deref(),
            Some(""),
            "the child's line is empty: nothing was typed into it"
        );
    });
}

#[test]
fn a_backup_write_still_in_flight_never_brings_back_what_saving_or_closing_deleted() {
    with_app(|cx, window| {
        let file = muxel_store::data_dir()
            .unwrap()
            .join("project")
            .join("racing.txt");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "v1\n").unwrap();
        let (iid, dir) = editor_pane(cx, window, &file);
        let hold = |cx: &mut TestAppContext| {
            window
                .update(cx, |app, _, cx| {
                    app.editors[&iid].update(cx, |editor, _| editor.hold_backup_writes())
                })
                .unwrap()
        };

        // A backup write is on its way when the file is saved; it lands after.
        let release = hold(cx);
        type_into(cx, window, iid, "mine ");
        after_debounce(cx);
        assert!(dir.editor_backup(iid).is_none(), "the write is held");
        window
            .update(cx, |app, _, cx| {
                let editor = app.editors[&iid].clone();
                std::fs::write(&file, editor.read(cx).text(cx)).unwrap();
                editor.update(cx, |editor, cx| editor.mark_saved(cx));
            })
            .unwrap();
        drop(release);
        cx.run_until_parked();
        assert_eq!(
            files_under(dir.path()),
            Vec::<String>::new(),
            "the older write doesn't bring the saved edits back"
        );

        // The same race against closing without saving.
        let release = hold(cx);
        type_into(cx, window, iid, "more ");
        after_debounce(cx);
        window
            .update(cx, |app, _, cx| {
                app.close_instance_inner(iid, "close (without saving)", cx)
            })
            .unwrap();
        drop(release);
        cx.run_until_parked();
        assert_eq!(
            files_under(dir.path()),
            Vec::<String>::new(),
            "nothing is left of the closed editor"
        );
    });
}

#[test]
fn after_an_unclean_exit_one_notice_says_what_was_kept_and_who_was_mid_turn() {
    with_app(|cx, window| {
        let crashed = Uuid::new_v4();
        let other = Uuid::new_v4();
        // A previous run of `crashed` marked it running and never stopped it,
        // leaving a draft and an agent mid-turn.
        assert!(!muxel_store::rescue::mark_workspace_running(crashed));
        let root = muxel_store::data_dir().unwrap().join("crashed-project");
        std::fs::create_dir_all(&root).unwrap();
        let mut doc = Workspace::default();
        let project = doc.add_project(Project::new("crashed", root));
        let mut agent = Instance::shell(project);
        agent.title = "Claude 2".into();
        agent.activity.last_state = Some(muxel_core::AgentActivityState::Working);
        let agent_id = agent.id;
        doc.add_instance(agent);
        let mut second = Instance::shell(project);
        second.title = "Codex 1".into();
        second.activity.last_state = Some(muxel_core::AgentActivityState::Working);
        let second_id = second.id;
        doc.add_instance(second);
        let layout = &mut doc.project_mut(project).unwrap().layout;
        *layout = Some(PaneNode::leaf(agent_id));
        assert!(split(
            layout,
            agent_id,
            SplitDirection::Horizontal,
            second_id
        ));
        muxel_store::save_workspace_to(&muxel_store::workspace_doc_path(crashed).unwrap(), &doc)
            .unwrap();
        let draft = PromptDraft {
            text: "where was I".into(),
            partial: false,
            saved_at_ms: 1,
            source: DraftSource::Agent,
            pane: None,
        };
        muxel_store::rescue::rescue_dir(crashed)
            .unwrap()
            .save_draft(agent_id, &draft)
            .unwrap();
        let mut empty = Workspace::default();
        let root = muxel_store::data_dir().unwrap().join("other-project");
        std::fs::create_dir_all(&root).unwrap();
        empty.add_project(Project::new("other", root));
        muxel_store::save_workspace_to(&muxel_store::workspace_doc_path(other).unwrap(), &empty)
            .unwrap();

        window
            .update(cx, |app, window, cx| {
                app.use_tmux = false;
                app.enter_workspace(crashed, window, cx);
                let notice = app
                    .rescue
                    .crash
                    .clone()
                    .expect("one notice after the crash");
                assert_eq!(
                    notice,
                    CrashNotice {
                        drafts: 1,
                        editors: 0,
                        mid_turn: vec![agent_id, second_id],
                    }
                );
                assert_eq!(
                    app.crash_label(&notice),
                    "Unclean exit · 1 unsent · 2 were working"
                );
                assert_eq!(app.crash_names(&notice), ["Claude 2", "Codex 1"]);
                let files = CrashNotice {
                    drafts: 0,
                    editors: 2,
                    mid_turn: Vec::new(),
                };
                assert_eq!(app.crash_label(&files), "Unclean exit · 2 unsaved files");

                // Review takes you to each pane in turn; the step to the last
                // one closes the notice.
                assert_eq!(app.crash_review_steps(&notice, cx), [agent_id, second_id]);
                app.review_crash(window, cx);
                assert_eq!(app.active_instance, Some(agent_id));
                assert!(app.rescue.crash.is_some(), "one pane still to see");
                app.review_crash(window, cx);
                assert_eq!(app.active_instance, Some(second_id));
                assert!(app.rescue.crash.is_none(), "reviewed: the notice goes");

                // Leaving cleanly and coming back: no notice.
                app.enter_workspace(other, window, cx);
                assert!(app.rescue.crash.is_none());
                app.enter_workspace(crashed, window, cx);
                assert!(app.rescue.crash.is_none(), "a clean switch is not a crash");
            })
            .unwrap();
    });
}

#[test]
fn review_visits_an_editor_whose_unsaved_changes_were_kept() {
    with_app(|cx, window| {
        let file = muxel_store::data_dir()
            .unwrap()
            .join("project")
            .join("kept.txt");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "base\n").unwrap();
        let (iid, _dir) = editor_pane(cx, window, &file);
        type_into(cx, window, iid, "mine ");
        after_debounce(cx);
        // muxel dies; the editor comes back offering what it kept.
        reopen_editor(cx, window, iid);
        window
            .update(cx, |app, window, cx| {
                assert!(app.editors[&iid].read(cx).backup_offered());
                let notice = CrashNotice {
                    drafts: 0,
                    editors: 1,
                    mid_turn: Vec::new(),
                };
                app.rescue.crash = Some(notice.clone());
                assert_eq!(app.crash_label(&notice), "Unclean exit · 1 unsaved file");
                // The notice counts the file, so Review goes to it.
                assert_eq!(app.crash_review_steps(&notice, cx), [iid]);
                app.review_crash(window, cx);
                assert_eq!(app.active_instance, Some(iid));
                assert!(app.rescue.crash.is_none(), "reviewed: the notice goes");
            })
            .unwrap();
    });
}

#[test]
fn restored_changes_survive_a_crash_before_the_next_backup_is_due() {
    with_app(|cx, window| {
        let file = muxel_store::data_dir()
            .unwrap()
            .join("project")
            .join("restored.txt");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "base\n").unwrap();
        let (iid, dir) = editor_pane(cx, window, &file);
        type_into(cx, window, iid, "mine ");
        after_debounce(cx);
        reopen_editor(cx, window, iid);
        window
            .update(cx, |app, window, cx| {
                let editor = app.editors[&iid].clone();
                editor.update(cx, |editor, cx| editor.restore_backup(window, cx));
            })
            .unwrap();
        // muxel dies at once, before the restored buffer's backup is due.
        reopen_editor(cx, window, iid);
        window
            .read_with(cx, |app, cx| {
                assert!(
                    app.editors[&iid].read(cx).backup_offered(),
                    "the restored changes are offered again"
                )
            })
            .unwrap();
        assert_eq!(
            dir.editor_offer(iid).map(|b| b.text).as_deref(),
            Some("mine base\n")
        );
    });
}

#[test]
fn text_an_input_method_commits_stays_backed_up() {
    with_app(|cx, window| {
        let file = muxel_store::data_dir()
            .unwrap()
            .join("project")
            .join("committed.txt");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "text\n").unwrap();
        let (iid, dir) = editor_pane(cx, window, &file);
        window
            .update(cx, |app, window, cx| {
                let editor = app.editors[&iid].clone();
                editor.update(cx, |editor, cx| editor.compose_text("にほ", window, cx));
            })
            .unwrap();
        cx.run_until_parked();
        after_debounce(cx);
        assert_eq!(
            dir.editor_backup(iid).map(|b| b.text).as_deref(),
            Some("にほtext\n")
        );
        // The input method commits the text as it stands.
        window
            .update(cx, |app, window, cx| {
                let editor = app.editors[&iid].clone();
                editor.update(cx, |editor, cx| editor.commit_composition(window, cx));
            })
            .unwrap();
        cx.run_until_parked();
        after_debounce(cx);
        assert_eq!(
            dir.editor_backup(iid).map(|b| b.text).as_deref(),
            Some("にほtext\n"),
            "committed text is an unsaved change, not a cancellation"
        );
        window
            .read_with(cx, |app, cx| {
                assert!(app.editors[&iid].read(cx).is_dirty(cx))
            })
            .unwrap();
        reopen_editor(cx, window, iid);
        assert_eq!(
            dir.editor_offer(iid).map(|b| b.text).as_deref(),
            Some("にほtext\n"),
            "it is offered after a crash"
        );
    });
}

#[test]
fn text_an_input_method_commits_silently_survives_a_disk_refresh() {
    with_app(|cx, window| {
        let file = muxel_store::data_dir()
            .unwrap()
            .join("project")
            .join("silent.txt");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "text\n").unwrap();
        let (iid, dir) = editor_pane(cx, window, &file);
        window
            .update(cx, |app, window, cx| {
                let editor = app.editors[&iid].clone();
                editor.update(cx, |editor, cx| editor.compose_text("にほ", window, cx));
            })
            .unwrap();
        cx.run_until_parked();
        after_debounce(cx);
        assert_eq!(
            dir.editor_backup(iid).map(|b| b.text).as_deref(),
            Some("にほtext\n")
        );
        // The input method commits the text, and nothing notifies. Before
        // anything else happens, the file changes on disk and the editor
        // polls it.
        window
            .update(cx, |app, window, cx| {
                let editor = app.editors[&iid].clone();
                editor.update(cx, |editor, cx| {
                    editor.unmark_composition_silently(window, cx);
                    std::fs::write(&file, "theirs, longer\n").unwrap();
                    let change = read_disk_change(&file, editor.disk_stamp())
                        .expect("the poll sees their version");
                    editor.apply_disk_change(change, window, cx);
                });
                let editor = editor.read(cx);
                assert_eq!(editor.text(cx), "にほtext\n", "the committed text stays");
                assert!(
                    matches!(
                        editor.external_change(),
                        Some(ExternalChange::Modified(theirs)) if theirs == "theirs, longer\n"
                    ),
                    "the disk version waits beside it"
                );
            })
            .unwrap();
        cx.run_until_parked();
        after_debounce(cx);
        reopen_editor(cx, window, iid);
        assert_eq!(
            dir.editor_offer(iid).map(|b| b.text).as_deref(),
            Some("にほtext\n"),
            "it is still offered after a crash"
        );
    });
}

#[test]
fn a_sent_prompt_the_screen_still_shows_is_never_saved_again() {
    with_app(|cx, window| {
        let (iid, dir) = pane(cx, window);
        let session = launch(cx, window, iid);
        paint_claude(&session, "sent before the redraw");
        session.mark_user_input();
        after_debounce(cx);
        assert!(dir.draft(iid).is_some());

        // Enter; the watcher deletes the file before the program redraws its
        // box, and the pane is closed.
        session.mark_turn_submitted();
        cx.run_until_parked();
        assert!(dir.draft(iid).is_none(), "submission clears the draft");
        window
            .update(cx, |app, _, cx| {
                app.close_instance_inner(iid, "close (test)", cx)
            })
            .unwrap();
        assert!(
            dir.closed_drafts().is_empty(),
            "closing before the redraw doesn't keep it"
        );

        // Nor does the next read once input stops, or a Restart.
        let (iid, dir) = pane(cx, window);
        let session = launch(cx, window, iid);
        paint_claude(&session, "also sent");
        session.mark_user_input();
        after_debounce(cx);
        session.mark_turn_submitted();
        cx.run_until_parked();
        after_debounce(cx);
        assert!(
            dir.draft(iid).is_none(),
            "the next read doesn't save the sent prompt again"
        );
        window
            .update(cx, |app, window, cx| app.restart_agent(iid, window, cx))
            .unwrap();
        cx.run_until_parked();
        assert!(dir.draft(iid).is_none());
        assert!(dir.draft_offers(iid).is_empty());
    });
}

#[test]
fn restore_keeps_the_line_break_a_draft_ends_with() {
    with_app(|cx, window| {
        let (iid, dir) = pane(cx, window);
        dir.save_draft(iid, &draft("one line\n")).unwrap();
        let (child, reader) = LineReader::spawn();
        let session = launch_child(cx, window, iid, child);
        paint_claude(&session, "");
        session.process_probe_output(b"\x1b[?2004l");
        window
            .update(cx, |app, window, cx| restore_newest(app, iid, window, cx))
            .unwrap();
        settle(cx);
        assert_eq!(reader.line(), None, "nothing reached the child");
        assert_eq!(
            cx.read_from_clipboard().and_then(|c| c.text()).as_deref(),
            Some("one line\n"),
            "the draft, line break and all, is on the clipboard"
        );
        session.try_write_input(b"\r").unwrap();
        assert_eq!(
            reader.wait_line().as_deref(),
            Some(""),
            "nothing was typed into the child"
        );
    });
}

#[test]
fn deleting_a_workspace_open_in_another_window_leaves_it_listed() {
    with_app(|cx, window| {
        let busy = saved_workspace("busy");
        let dir = muxel_store::rescue::rescue_dir(busy).unwrap();
        let pane = Uuid::new_v4();
        dir.save_draft(pane, &draft("kept while open elsewhere"))
            .unwrap();
        let workspace_dir = dir.path().parent().unwrap().to_path_buf();
        // Another muxel window has it open.
        let muxel_store::WorkspaceLock::Held(elsewhere) = muxel_store::try_lock_workspace(busy)
        else {
            panic!("lock the workspace");
        };
        let listed = |app: &MuxelApp| app.workspaces.workspaces.iter().any(|w| w.id == busy);
        window
            .update(cx, |app, _, cx| {
                let current = *app.current_workspace.get_or_insert_with(Uuid::new_v4);
                for id in [current, busy] {
                    app.workspaces.workspaces.push(muxel_core::WorkspaceMeta {
                        id,
                        name: id.to_string(),
                    });
                }
                app.delete_workspace(busy, cx);
                assert!(listed(app), "it stays listed, to delete later");
            })
            .unwrap();
        assert!(dir.draft(pane).is_some(), "nothing of it is deleted");

        // Once the other window lets go, deleting it works.
        drop(elsewhere);
        window
            .update(cx, |app, _, cx| {
                app.delete_workspace(busy, cx);
                assert!(!listed(app));
            })
            .unwrap();
        assert!(!workspace_dir.exists());
    });
}

#[test]
fn a_workspace_another_window_deleted_does_not_open_from_this_ones_list() {
    with_app(|cx, window| {
        let gone = saved_workspace("gone");
        let workspace_dir = muxel_store::rescue::rescue_dir(gone)
            .unwrap()
            .path()
            .parent()
            .unwrap()
            .to_path_buf();
        let listed = |app: &MuxelApp| app.workspaces.workspaces.iter().any(|w| w.id == gone);
        let current = window
            .update(cx, |app, _, _| {
                app.use_tmux = false;
                let current = *app.current_workspace.get_or_insert_with(Uuid::new_v4);
                app.workspaces.workspaces.push(muxel_core::WorkspaceMeta {
                    id: current,
                    name: "this one".into(),
                });
                app.workspaces.workspaces.push(muxel_core::WorkspaceMeta {
                    id: gone,
                    name: "Deleted elsewhere".into(),
                });
                current
            })
            .unwrap();
        // Another muxel window deletes it; this one still lists it, and
        // it is picked from that list.
        muxel_store::rescue::delete_workspace_dir(gone).unwrap();
        window
            .update(cx, |app, window, cx| app.enter_workspace(gone, window, cx))
            .unwrap();
        window
            .read_with(cx, |app, _| {
                assert_eq!(
                    app.current_workspace,
                    Some(current),
                    "this window stays where it was"
                );
                assert!(!listed(app), "the deleted workspace leaves the list");
                assert!(
                    app.notifications
                        .iter()
                        .any(|n| n.title.contains("Deleted elsewhere")),
                    "the feed says why it didn't open"
                );
            })
            .unwrap();
        assert!(!workspace_dir.exists(), "nothing of it comes back");
    });
}

/// Claude's screen with a draft taller than the pane: its box fills the
/// pane down to the bottom rule and footer, and the top has scrolled off.
fn paint_claude_too_tall(session: &TerminalSession) {
    let mut screen = String::from("\x1b[2J\x1b[H");
    let last = "  line 22 of a long draft";
    for n in 1..=22 {
        screen.push_str(&format!("  line {n} of a long draft\r\n"));
    }
    screen.push_str(RULE);
    screen.push_str("\r\n  ? for shortcuts");
    // Cursor at the end of the last line, on row 22 of the pane's 24.
    screen.push_str(&format!("\x1b[22;{}H", last.chars().count() + 1));
    session.process_probe_output(screen.as_bytes());
}

#[test]
fn a_sent_prompt_is_not_kept_when_the_pane_closes_on_a_draft_too_tall_to_save() {
    with_app(|cx, window| {
        let (iid, dir) = pane(cx, window);
        let session = launch(cx, window, iid);
        paint_claude(&session, "sent before the long draft");
        session.mark_user_input();
        after_debounce(cx);
        assert!(dir.draft(iid).is_some());

        // Enter; before the watcher deletes the file, a draft taller than
        // the pane fills the box, and the pane is closed.
        window
            .update(cx, |app, _, cx| {
                session.mark_turn_submitted();
                paint_claude_too_tall(&session);
                app.close_instance_inner(iid, "close (test)", cx);
            })
            .unwrap();
        assert!(dir.closed_drafts().is_empty(), "the sent prompt isn't kept");
    });
}

#[test]
fn a_shell_prompt_line_is_never_saved() {
    with_app(|cx, window| {
        let (iid, dir) = pane(cx, window);
        let session = launch_shell(cx, window, iid);
        // PowerShell's prompt, marked the way its shell integration marks it,
        // with a secret typed after it.
        session.process_probe_shell_output(
            b"\x1b]133;A\x07PS C:\\> \x1b]133;B\x07$env:SOME_KEY=\"x\"",
        );
        session.mark_user_input();
        after_debounce(cx);
        assert!(dir.draft(iid).is_none(), "the typed line was saved");

        window
            .update(cx, |app, _, cx| {
                app.close_instance_inner(iid, "close (test)", cx);
            })
            .unwrap();
        assert!(dir.closed_drafts().is_empty(), "the typed line was kept");
    });
}

#[test]
fn reload_to_the_text_an_input_method_is_composing_keeps_no_backup() {
    with_app(|cx, window| {
        let file = muxel_store::data_dir()
            .unwrap()
            .join("project")
            .join("reload.txt");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "text\n").unwrap();
        let (iid, dir) = editor_pane(cx, window, &file);
        type_into(cx, window, iid, "a");
        // The file changes on disk; the edited buffer offers their version.
        std::fs::write(&file, "aにほtext\n").unwrap();
        window
            .update(cx, |app, window, cx| {
                let editor = app.editors[&iid].clone();
                editor.update(cx, |editor, cx| {
                    let change = read_disk_change(&file, editor.disk_stamp())
                        .expect("the poll sees their version");
                    editor.apply_disk_change(change, window, cx);
                    // An input method then composes the same text.
                    editor.compose_text("にほ", window, cx);
                    assert_eq!(editor.text(cx), "aにほtext\n");
                });
            })
            .unwrap();
        cx.run_until_parked();
        after_debounce(cx);
        assert_eq!(
            dir.editor_backup(iid).map(|b| b.text).as_deref(),
            Some("aにほtext\n")
        );

        // Reload: the buffer holds the disk version, nothing unsaved.
        window
            .update(cx, |app, window, cx| {
                let editor = app.editors[&iid].clone();
                editor.update(cx, |editor, cx| editor.reload_external_change(window, cx));
                assert!(!editor.read(cx).is_dirty(cx));
            })
            .unwrap();
        cx.run_until_parked();
        after_debounce(cx);
        assert_eq!(
            dir.editor_backup(iid).map(|b| b.text),
            None,
            "a clean buffer keeps no backup"
        );
        reopen_editor(cx, window, iid);
        assert!(dir.editor_offer(iid).is_none(), "nothing is offered");
    });
}

#[test]
fn the_strip_starts_as_one_line_and_discard_all_deletes_every_closed_panes_draft() {
    with_app(|cx, window| {
        let (_, dir) = pane(cx, window);
        for (n, text) in ["first thought", "second thought"].iter().enumerate() {
            let closed = Uuid::new_v4();
            dir.save_draft(closed, &draft(text)).unwrap();
            assert_eq!(dir.keep_closed_drafts(closed, &format!("pane {n}")), 1);
        }
        window
            .update(cx, |app, _, cx| {
                app.load_workspace_rescue(None);
                assert_eq!(app.rescue.closed.len(), 2);
                assert!(!app.rescue.closed_open, "the list starts closed");
                app.rescue.closed_open = true;
                app.discard_all_closed_drafts(cx);
                assert!(app.rescue.closed.is_empty());
                assert!(!app.rescue.closed_open);
            })
            .unwrap();
        assert!(
            dir.closed_drafts().is_empty(),
            "nothing comes back the next time the workspace opens"
        );
    });
}

/// A child that never reads its input, so nothing echoes what the test
/// types: the test alone draws the screen.
fn deaf() -> CommandSpec {
    CommandSpec::program(
        "cmd.exe",
        vec![
            "/d".into(),
            "/q".into(),
            "/c".into(),
            "echo ready&ping -n 600 127.0.0.1>nul".into(),
        ],
    )
}

/// Claude's screen as it shows a slash command: its menu above the box
/// (Claude Code 2.1.322), `draft` in the box.
fn paint_claude_command(session: &TerminalSession, draft: &str, menu: &[&str]) {
    let mut screen = String::from("\x1b[?2004h\x1b[2J\x1b[H");
    for row in menu {
        screen.push_str(row);
        screen.push_str("\r\n");
    }
    screen.push_str(RULE);
    screen.push_str("\r\n❯ ");
    screen.push_str(draft);
    screen.push_str("\r\n");
    screen.push_str(RULE);
    screen.push_str("\r\n  ? for shortcuts");
    screen.push_str(&format!(
        "\x1b[{};{}H",
        menu.len() + 2,
        3 + draft.chars().count()
    ));
    session.process_probe_output(screen.as_bytes());
}

/// `text` and Enter as a pane's keys send them, while the program is too
/// slow to draw them (a machine short of memory): the box shows none of it
/// when the Enter is admitted.
fn type_and_send_unseen(session: &TerminalSession, text: &str) {
    for key in text.bytes() {
        session.write_user_input(&[key]);
    }
    session.mark_turn_submitted();
    session.write_user_input(b"\r");
}

#[test]
fn a_prompt_drawn_only_after_its_enter_is_never_kept_but_one_typed_after_it_is() {
    with_app(|cx, window| {
        let (iid, dir) = pane(cx, window);
        let session = launch_child(cx, window, iid, deaf());
        paint_claude(&session, "");
        type_and_send_unseen(&session, "/exit");
        cx.run_until_parked();
        // The program catches up: `/exit` in the box, its command menu open
        // over it, and the watcher reads the box once input stops.
        paint_claude_command(
            &session,
            "/exit",
            &[
                "  ❯ /exit      Exit the CLI",
                "    /export    Export the conversation",
            ],
        );
        after_debounce(cx);
        assert!(
            dir.draft(iid).is_none(),
            "what was sent, drawn after its Enter, isn't saved"
        );
        // It runs the command: leaves its screen with bracketed paste off,
        // and the pane closes.
        session.process_probe_output(b"\x1b[?2004l\x1b[2J\x1b[H");
        window
            .update(cx, |app, _, cx| {
                app.close_instance_inner(iid, "close (test)", cx)
            })
            .unwrap();
        assert!(
            dir.closed_drafts().is_empty(),
            "the sent command isn't kept"
        );

        // A prompt sent the same way, then another typed after its Enter:
        // that one wasn't sent, and is kept.
        let (iid, dir) = pane(cx, window);
        let session = launch_child(cx, window, iid, deaf());
        paint_claude(&session, "");
        type_and_send_unseen(&session, "ship it");
        cx.run_until_parked();
        paint_claude(&session, "ship it");
        for key in "next step".bytes() {
            session.write_user_input(&[key]);
        }
        paint_claude(&session, "next step");
        after_debounce(cx);
        window
            .update(cx, |app, _, cx| {
                app.close_instance_inner(iid, "close (test)", cx)
            })
            .unwrap();
        let closed = dir.closed_drafts();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].draft.text, "next step");
        assert!(!closed[0].draft.partial, "it is what muxel saw typed");
    });
}

/// Claude's permission question drawn where its input box was.
fn paint_claude_question(session: &TerminalSession) {
    let mut screen = String::from("\x1b[?2004h\x1b[2J\x1b[H");
    screen.push_str("⏺ Bash(ping -n 8 localhost)\r\n\r\n");
    screen.push_str(RULE);
    screen.push_str("\r\n Bash command\r\n\r\n   ping -n 8 localhost\r\n\r\n");
    screen.push_str(" Do you want to proceed?\r\n ❯ 1. Yes\r\n   2. No\r\n\r\n Esc to cancel");
    screen.push_str("\x1b[9;4H");
    session.process_probe_output(screen.as_bytes());
}

#[test]
fn restore_types_nothing_and_keeps_the_offer_while_a_question_covers_the_box() {
    with_app(|cx, window| {
        let (iid, dir) = pane(cx, window);
        dir.save_draft(iid, &draft("fix the flaky resize test"))
            .unwrap();
        let session = launch_child(cx, window, iid, deaf());
        paint_claude_question(&session);
        let typed_before = session.last_user_input();
        window
            .update(cx, |app, window, cx| restore_newest(app, iid, window, cx))
            .unwrap();
        settle(cx);
        assert_eq!(
            session.last_user_input(),
            typed_before,
            "nothing was pasted into the question"
        );
        window
            .read_with(cx, |app, _| {
                assert_eq!(app.rescue.offers.get(&iid).map(Vec::len), Some(1))
            })
            .unwrap();
        assert_eq!(dir.draft_offers(iid).len(), 1, "the draft is still offered");
    });
}

#[test]
fn an_older_offer_restores_on_its_own_and_the_newer_one_stays_offered() {
    with_app(|cx, window| {
        let (iid, dir) = pane(cx, window);
        let at = |text: &str, saved_at_ms| PromptDraft {
            saved_at_ms,
            ..draft(text)
        };
        dir.save_draft(iid, &at("the older thought", 1)).unwrap();
        assert_eq!(dir.offer_drafts(iid).len(), 1);
        dir.save_draft(iid, &at("the newer thought", 2)).unwrap();
        let (child, reader) = LineReader::spawn();
        let session = launch_child(cx, window, iid, child);
        paint_claude(&session, "");
        session.process_probe_output(b"\x1b[?2004l");
        window
            .update(cx, |app, window, cx| {
                let offers = &app.rescue.offers[&iid];
                assert_eq!(offers.len(), 2);
                assert_eq!(offers[0].draft.text, "the newer thought");
                let older = offers[1].path.clone();
                app.toggle_offer(iid, cx);
                app.restore_offer(iid, &older, window, cx);
            })
            .unwrap();
        settle(cx);
        session.try_write_input(b"\r").unwrap();
        assert_eq!(reader.wait_line().as_deref(), Some("the older thought"));
        let newer = window
            .read_with(cx, |app, _| {
                let offers = &app.rescue.offers[&iid];
                assert_eq!(offers.len(), 1);
                assert_eq!(offers[0].draft.text, "the newer thought");
                assert!(app.rescue.offers_open.contains(&iid), "the row stays open");
                offers[0].path.clone()
            })
            .unwrap();
        assert_eq!(dir.draft_offers(iid).len(), 1);

        // Discarding the last one closes the row, so the pane's next offer
        // starts as one line.
        window
            .update(cx, |app, _, cx| {
                app.forget_draft_offer(iid, &newer);
                cx.notify();
                assert!(!app.rescue.offers.contains_key(&iid));
                assert!(!app.rescue.offers_open.contains(&iid));
            })
            .unwrap();
        assert!(dir.draft_offers(iid).is_empty());
    });
}
