#![cfg(windows)]
//! The pane bar against real PTY children: an ended pane's lines and its
//! Enter, beside a running pane whose Enter stays its program's.

use super::popout_lifecycle_tests::with_app_drawing;
use super::{Instance, MuxelApp, PaneNode, Project};
use gpui::{AppContext, Context, Entity, TestAppContext, VisualTestContext, Window, WindowHandle};
use muxel_core::add_tab;
use muxel_core::pane_bar::{BarLine, Ending};
use muxel_core::rescue::{DraftSource, PromptDraft};
use muxel_store::rescue::DraftOffer;
use muxel_terminal::{CommandSpec, TerminalLaunch, TerminalView};
use std::time::{Duration, Instant};
use uuid::Uuid;

fn project(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>) -> Uuid {
    window
        .update(cx, |app, _, _| {
            let root = muxel_store::data_dir()
                .unwrap()
                .join(Uuid::new_v4().to_string());
            std::fs::create_dir_all(&root).unwrap();
            let pid = app.workspace.add_project(Project::new("bar", root));
            app.workspace.active_project = Some(pid);
            app.current_workspace = Some(Uuid::new_v4());
            app.use_tmux = false;
            app.use_worktree = false;
            app.notifications_enabled = false;
            app.show_terms = false;
            app.show_workspace_selector = false;
            pid
        })
        .unwrap()
}

/// Pane `name`, a tab in project `pid`, running `cmd.exe` on `script`. Its
/// saved program doesn't exist, so a restart can start nothing real.
fn pane(
    app: &mut MuxelApp,
    window: &mut Window,
    cx: &mut Context<MuxelApp>,
    pid: Uuid,
    name: &str,
    script: &str,
) -> (Uuid, Entity<TerminalView>) {
    let root = app.workspace.project(pid).unwrap().root_path.clone();
    let mut instance = Instance::shell(pid);
    instance.custom_name = Some(name.into());
    instance.program = Some(root.join("missing").join("shell.exe").display().to_string());
    let iid = instance.id;
    app.workspace.add_instance(instance);
    let project = app.workspace.project_mut(pid).unwrap();
    match project.first_instance() {
        Some(anchor) if anchor != iid => assert!(add_tab(&mut project.layout, anchor, iid)),
        _ => project.layout = Some(PaneNode::leaf(iid)),
    }
    let filename = format!("bar-{}.cmd", Uuid::new_v4());
    std::fs::write(root.join(&filename), script).unwrap();
    let directory = root.to_string_lossy();
    let directory = directory
        .strip_prefix(r"\\?\")
        .unwrap_or(&directory)
        .to_string();
    let args = ["/d", "/q", "/c"]
        .into_iter()
        .map(str::to_string)
        .chain([format!(r".\{filename}")])
        .collect();
    let launch = TerminalLaunch::spawn(
        CommandSpec::program("cmd.exe", args).with_cwd(directory),
        (80, 24),
    )
    .unwrap();
    let view = cx.new(|cx| TerminalView::new(launch, iid, Instant::now(), window, cx));
    app.terminals.insert(iid, view.clone());
    (iid, view)
}

fn wait_until(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    what: &str,
    condition: impl Fn(&MuxelApp, &gpui::App) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        // The PTY reader is real; output parsing runs on GPUI's virtual clock.
        cx.executor().advance_clock(Duration::from_millis(50));
        cx.run_until_parked();
        if window.read_with(cx, |app, cx| condition(app, cx)).unwrap() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn restarts_logged(iid: Uuid) -> usize {
    std::fs::read_to_string(muxel_store::event_log_path().unwrap())
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("restart: ") && line.contains(&format!("[{iid}]")))
        .count()
}

#[test]
fn an_ended_pane_shows_ended_and_its_unsent_prompt_and_enter_restarts_only_it() {
    with_app_drawing(true, |cx, window| {
        let pid = project(cx, window);
        let (running, ended, ended_view) = window
            .update(cx, |app, window, cx| {
                let (running, _) = pane(
                    app,
                    window,
                    cx,
                    pid,
                    "running",
                    "@echo still here\r\n@more\r\n",
                );
                let (ended, ended_view) = pane(
                    app,
                    window,
                    cx,
                    pid,
                    "ended",
                    "@echo crash output\r\n@exit /b 3\r\n",
                );
                (running, ended, ended_view)
            })
            .unwrap();
        let watched = ended_view.clone();
        wait_until(cx, window, "the pane to end", move |_, cx| {
            watched.read(cx).exited()
        });
        window
            .update(cx, |app, window, cx| {
                let root = app.workspace.project(pid).unwrap().root_path.clone();
                app.rescue.offers.insert(
                    ended,
                    vec![DraftOffer {
                        path: root.join("offered.json"),
                        instance: ended,
                        draft: PromptDraft {
                            text: "fix the flaky test".into(),
                            partial: false,
                            saved_at_ms: chrono::Utc::now().timestamp_millis(),
                            source: DraftSource::Agent,
                            pane: None,
                        },
                    }],
                );
                // The loss first, then what waits; a running pane says nothing.
                assert_eq!(
                    app.pane_bar_lines(ended, cx),
                    vec![
                        BarLine::Ended(Ending::Code(3)),
                        BarLine::Unsent { count: 1 }
                    ]
                );
                assert!(app.pane_bar_lines(running, cx).is_empty());
                app.focus_instance(running, window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        VisualTestContext::from_window(window.into(), cx).simulate_keystrokes("enter");
        window
            .update(cx, |app, window, cx| {
                assert_eq!(restarts_logged(running), 0, "Enter in a running pane");
                assert!(!app.terminals.get(&running).unwrap().read(cx).exited());
                app.focus_instance(ended, window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        {
            let mut visual = VisualTestContext::from_window(window.into(), cx);
            let selector: &'static str =
                Box::leak(format!("pane-bar-{}", ended.simple()).into_boxed_str());
            assert!(
                visual.debug_bounds(selector).is_some(),
                "the bar wasn't drawn"
            );
            visual.simulate_keystrokes("enter");
        }
        window
            .update(cx, |app, _, cx| {
                assert_eq!(restarts_logged(ended), 1, "Enter in the ended pane");
                assert!(
                    app.terminals
                        .get(&ended)
                        .is_none_or(|view| view.entity_id() != ended_view.entity_id()),
                    "the ended terminal is still in place"
                );
                // Never let the deferred launch run its (missing) program.
                app.close_instance(ended, cx);
            })
            .unwrap();
    });
}
