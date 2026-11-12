//! Draws every scene in one `Application` on the real Windows platform. Each
//! frame gets a fresh window: shown under a DWM cloak, so it draws but never
//! appears, and sized from outside any window update, because a resize sent
//! while the window is leased never reaches GPUI's layout.

use super::scene::{self, Loaded};
use crate::app::MuxelApp;
use crate::editor::EditorView;
use crate::{scene_mode, theme};
use anyhow::{Context as _, Result, bail};
use gpui::{
    AnyWindowHandle, App, AppContext as _, AsyncApp, Bounds, Entity, Task, Window, WindowBounds,
    WindowOptions, point, px, size,
};
use muxel_core::{Instance, Project};
use sha2::{Digest as _, Sha256};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

#[derive(Default)]
struct Report {
    lines: Vec<String>,
    errors: Vec<String>,
    skipped: Option<String>,
}

enum Frame {
    Drawn(String),
    NoReadback(String),
}

pub(super) fn run() {
    let Some(source) = std::env::var_os("MUXEL_SCENES") else {
        eprintln!("scenes: MUXEL_SCENES is not set; nothing to draw");
        return;
    };
    let out = PathBuf::from(
        std::env::var_os("MUXEL_SCENE_OUT").expect("MUXEL_SCENE_OUT names the folder for frames"),
    );
    let repeat = std::env::var("MUXEL_SCENE_REPEAT")
        .ok()
        .map(|n| n.parse::<usize>().expect("MUXEL_SCENE_REPEAT is a count"))
        .unwrap_or(1)
        .max(1);
    let scenes = scene::load_all(Path::new(&source)).unwrap_or_else(|e| panic!("scenes: {e:#}"));
    assert!(
        !scenes.is_empty(),
        "scenes: no *.scene.json under {}",
        Path::new(&source).display()
    );
    std::fs::create_dir_all(&out).expect("create the frame folder");

    // Before any window exists: nothing this process starts can run a program.
    muxel_terminal::test_support::refuse_spawns();
    let root = std::env::temp_dir().join(format!("muxel-scenes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the scene root");
    let _storage = muxel_store::test_support::set_root(root.join("store"));

    let frames = scenes.iter().map(|s| s.scene.window.len()).sum::<usize>() * repeat;
    let limit = Duration::from_secs(60 + 10 * frames as u64);
    std::thread::spawn(move || {
        std::thread::sleep(limit);
        eprintln!("scenes: still drawing after {limit:?}; giving up");
        std::process::exit(3);
    });

    let started = Instant::now();
    let report = Rc::new(RefCell::new(Report::default()));
    {
        let report = report.clone();
        let root = root.clone();
        let out = out.clone();
        gpui_platform::application()
            .with_assets(crate::AppAssets)
            .run(move |cx: &mut App| {
                init(cx);
                cx.spawn(async move |cx| {
                    draw_all(&scenes, repeat, &root, &out, &report, cx).await;
                    cx.update(|cx| cx.quit());
                })
                .detach();
            });
    }
    let report = report.take();
    let _ = std::fs::remove_dir_all(&root);

    if let Some(reason) = report.skipped {
        eprintln!("scenes: skipped, this GPUI can't read a frame back ({reason})");
        return;
    }
    let mut log = report.lines.join("\n");
    if !log.is_empty() {
        log.push('\n');
    }
    std::fs::write(out.join("frames.jsonl"), log).expect("write frames.jsonl");
    eprintln!(
        "scenes: {} frames in {:?}, written to {}",
        report.lines.len(),
        started.elapsed(),
        out.display()
    );
    assert!(
        report.errors.is_empty(),
        "scenes: {} frames failed:\n{}",
        report.errors.len(),
        report.errors.join("\n")
    );
}

fn init(cx: &mut App) {
    gpui_component::init(cx);
    theme::register_bundled_themes(cx);
    cx.set_global(theme::UiScale(1.0));
    cx.set_global(theme::UiFontSize(14.0));
    theme::apply_initial_theme("", cx);
    crate::app::install_keybindings(&muxel_core::Settings::default(), cx);
}

async fn draw_all(
    scenes: &[Loaded],
    repeat: usize,
    root: &Path,
    out: &Path,
    report: &RefCell<Report>,
    cx: &mut AsyncApp,
) {
    for loaded in scenes {
        for &(width, height) in &loaded.scene.window {
            for pass in 1..=repeat {
                let label = format!("{} {width}x{height} pass {pass}", loaded.scene.id);
                match draw_frame(loaded, (width, height), pass, root, out, cx).await {
                    Ok(Frame::Drawn(line)) => {
                        eprintln!("scenes: {label}: {line}");
                        report.borrow_mut().lines.push(line);
                    }
                    Ok(Frame::NoReadback(reason)) => {
                        report.borrow_mut().skipped = Some(reason);
                        return;
                    }
                    Err(error) => {
                        eprintln!("scenes: {label}: FAILED {error:#}");
                        report
                            .borrow_mut()
                            .errors
                            .push(format!("{label}: {error:#}"));
                    }
                }
            }
        }
    }
}

async fn draw_frame(
    loaded: &Loaded,
    (width, height): (u32, u32),
    pass: usize,
    root: &Path,
    out: &Path,
    cx: &mut AsyncApp,
) -> Result<Frame> {
    let scene = &loaded.scene;
    scene_mode::record_regions();
    let dir = root.join(format!("{}-{width}x{height}-{pass}", scene.id));
    std::fs::create_dir_all(&dir)?;

    let mut built: Option<Entity<MuxelApp>> = None;
    let handle: AnyWindowHandle = cx
        .update(|cx| {
            cx.open_window(options(width, height), |window, cx| {
                let view = cx.new(|cx| MuxelApp::new(window, cx));
                built = Some(view.clone());
                cx.new(|cx| gpui_component::Root::new(view, window, cx))
            })
        })?
        .into();
    let app = built.context("the window didn't build its app")?;
    let result = draw_into(loaded, handle, &app, (width, height), pass, &dir, out, cx).await;
    let _ = handle.update(cx, |_, window, _| window.remove_window());
    result
}

#[allow(clippy::too_many_arguments)]
async fn draw_into(
    loaded: &Loaded,
    handle: AnyWindowHandle,
    app: &Entity<MuxelApp>,
    (width, height): (u32, u32),
    pass: usize,
    dir: &Path,
    out: &Path,
    cx: &mut AsyncApp,
) -> Result<Frame> {
    let scene = &loaded.scene;
    let hwnd = handle.update(cx, |_, window, _| win::hwnd(window))??;
    win::show_cloaked(hwnd)?;

    let mut notes = Vec::new();
    handle.update(cx, |_, window, cx| {
        app.update(cx, |app, cx| {
            build(app, loaded, dir, &mut notes, window, cx)
        })
    })??;
    cx.update(|cx| apply_theme(scene.theme.as_deref(), cx));

    let scale = handle.update(cx, |_, window, _| window.scale_factor())?;
    win::size_client(
        hwnd,
        (width as f32 * scale).round() as i32,
        (height as f32 * scale).round() as i32,
    )?;
    settle(handle, (width, height), cx).await?;

    let (drawn, read, image) = handle.update(cx, |_, window, cx| {
        window.refresh();
        let started = Instant::now();
        window.draw(cx).clear(cx);
        let drawn = started.elapsed();
        let started = Instant::now();
        let image = window.render_to_image();
        (drawn, started.elapsed(), image)
    })?;
    let image = match image {
        Ok(image) => image,
        Err(error) if format!("{error:#}").contains("not implemented") => {
            return Ok(Frame::NoReadback(format!("{error:#}")));
        }
        Err(error) => return Err(error.context("read the frame back")),
    };

    let (image, crop_px) = match &scene.crop {
        None => (image, None),
        Some(name) => {
            let bounds = scene_mode::region_bounds(name).with_context(|| {
                format!(
                    "no region {name:?} was drawn; drawn: {}",
                    scene_mode::region_names().join(", ")
                )
            })?;
            let [x, y, w, h] = device_rect(bounds, scale, image.width(), image.height());
            (
                image::imageops::crop_imm(&image, x, y, w, h).to_image(),
                Some([x, y, w, h]),
            )
        }
    };

    let file = if pass == 1 {
        format!("{}.{width}x{height}.png", scene.id)
    } else {
        format!("{}.{width}x{height}.r{pass}.png", scene.id)
    };
    let path = out.join(&file);
    image
        .save(&path)
        .with_context(|| format!("save {}", path.display()))?;
    let sha256 = Sha256::digest(std::fs::read(&path)?)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let line = serde_json::json!({
        "scene": scene.id,
        "file": file,
        "window": [width, height],
        "pass": pass,
        "now": loaded.now.to_rfc3339(),
        "scale": scale,
        "png": [image.width(), image.height()],
        "crop": scene.crop,
        "crop_px": crop_px,
        "sha256": sha256,
        "draw_ms": drawn.as_secs_f64() * 1000.0,
        "readback_ms": read.as_secs_f64() * 1000.0,
        "regions": scene_mode::region_names(),
        "notes": notes,
        "for": scene.feature,
        "source": scene.source,
        "ask": scene.ask,
        "mock": scene.mock,
        "scene_file": loaded.path.file_name().map(|n| n.to_string_lossy()),
    });
    Ok(Frame::Drawn(line.to_string()))
}

fn options(width: u32, height: u32) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(width as f32), px(height as f32)),
        })),
        focus: false,
        show: false,
        ..Default::default()
    }
}

/// Put the fixture's workspace into a fresh app and stop everything that
/// would change it while it's drawn: the status tick, loops, diff and file
/// polling, and the update check.
fn build(
    app: &mut MuxelApp,
    loaded: &Loaded,
    dir: &Path,
    notes: &mut Vec<String>,
    window: &mut Window,
    cx: &mut gpui::Context<MuxelApp>,
) -> Result<()> {
    let scene = &loaded.scene;
    app._status_timer = Task::ready(());
    app._loop_timer = Task::ready(());
    app._diff_timer = Task::ready(());
    app._resource_timer = Task::ready(());
    app._update_timer = Task::ready(());
    app.show_terms = false;
    app.show_workspace_selector = false;
    app.current_workspace = Some(scene::stable_id(&[&scene.id, "workspace"]));

    let mut ids: HashMap<&str, uuid::Uuid> = HashMap::new();
    let mut pids = HashMap::new();
    for project in &loaded.fixture.projects {
        let project_root = dir.join(&project.name);
        std::fs::create_dir_all(&project_root)?;
        let mut record = Project::new(project.name.clone(), project_root.clone());
        record.id = scene::stable_id(&[&scene.id, "project", &project.name]);
        let pid = app.workspace.add_project(record);
        pids.insert(project.name.as_str(), pid);
        for pane in &project.panes {
            let iid = scene::stable_id(&[&scene.id, "pane", &pane.id]);
            let content = scene.panes.get(&pane.id);
            let mut instance = match content.and_then(|c| c.markdown.as_ref()) {
                Some(text) => {
                    let file = project_root.join(&pane.title);
                    std::fs::write(&file, text)?;
                    let config = app.editor_config();
                    let path = Some(file.clone());
                    let editor = cx.new(|cx| EditorView::open(path, config, window, cx));
                    app.editors.insert(iid, editor);
                    Instance::editor(pid, Some(file))
                }
                None => {
                    if content.is_some_and(|c| c.cast.is_some()) {
                        notes.push(format!(
                            "pane {:?}: casts play once the replay session is in this build; drawn as starting",
                            pane.id
                        ));
                    }
                    let mut shell = Instance::shell(pid);
                    if let Some(program) = &pane.program {
                        shell.program = Some(program.clone());
                    }
                    shell.custom_name = Some(pane.title.clone());
                    shell
                }
            };
            instance.id = iid;
            app.workspace.add_instance(instance);
            ids.insert(pane.id.as_str(), iid);
        }
        let layout = project.layout(&scene.id, &ids)?;
        app.workspace
            .project_mut(pid)
            .context("the project was just added")?
            .layout = Some(layout);
    }

    let active = loaded
        .fixture
        .active
        .as_deref()
        .unwrap_or(&loaded.fixture.projects[0].name);
    let active_project = loaded
        .fixture
        .projects
        .iter()
        .find(|p| p.name == active)
        .context("active project")?;
    app.workspace.active_project = pids.get(active).copied();
    app.workspace.sidebar_width = scene.sidebar_width;
    let focus = scene
        .focus
        .as_deref()
        .or_else(|| active_project.first_pane())
        .and_then(|name| ids.get(name).copied());
    if let Some(iid) = focus {
        app.focus_instance(iid, window, cx);
    }
    cx.notify();
    Ok(())
}

fn apply_theme(name: Option<&str>, cx: &mut App) {
    let name = match name {
        Some("light") => gpui_component::ThemeRegistry::global(cx)
            .default_light_theme()
            .name
            .to_string(),
        _ => String::new(),
    };
    theme::apply_initial_theme(&name, cx);
    cx.refresh_windows();
}

/// Wait until GPUI's layout has the size the scene asked for.
async fn settle(
    handle: AnyWindowHandle,
    (width, height): (u32, u32),
    cx: &mut AsyncApp,
) -> Result<()> {
    let mut last = None;
    for _ in 0..60 {
        let viewport = handle.update(cx, |_, window, _| window.viewport_size())?;
        let got = (
            f32::from(viewport.width).round() as u32,
            f32::from(viewport.height).round() as u32,
        );
        if got == (width, height) {
            return Ok(());
        }
        last = Some(got);
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
    }
    bail!("the window's layout never reached {width}x{height} (last {last:?})")
}

/// `bounds` in device pixels, clamped to the image.
fn device_rect(bounds: Bounds<gpui::Pixels>, scale: f32, max_w: u32, max_h: u32) -> [u32; 4] {
    let px = |v: gpui::Pixels| (f32::from(v) * scale).round().max(0.0) as u32;
    let x = px(bounds.origin.x).min(max_w);
    let y = px(bounds.origin.y).min(max_h);
    let w = px(bounds.size.width).min(max_w - x);
    let h = px(bounds.size.height).min(max_h - y);
    [x, y, w, h]
}

mod win {
    use anyhow::{Context as _, Result, anyhow};
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClientRect, GetWindowRect, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER,
        SetWindowPos, ShowWindow,
    };
    use wry::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmSetWindowAttribute(
            hwnd: isize,
            attribute: u32,
            value: *const std::ffi::c_void,
            size: u32,
        ) -> i32;
    }
    const DWMWA_CLOAK: u32 = 13;

    pub(super) fn hwnd(window: &gpui::Window) -> Result<isize> {
        let handle = HasWindowHandle::window_handle(window).map_err(|e| anyhow!("{e}"))?;
        match handle.as_raw() {
            RawWindowHandle::Win32(handle) => Ok(handle.hwnd.get()),
            _ => Err(anyhow!("not a Win32 window")),
        }
    }

    /// Cloak the window, then show it without activating it: DWM keeps a
    /// cloaked window off the screen while it still draws.
    pub(super) fn show_cloaked(hwnd: isize) -> Result<()> {
        let on: i32 = 1;
        let hr = unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_CLOAK,
                &on as *const i32 as *const std::ffi::c_void,
                4,
            )
        };
        anyhow::ensure!(hr == 0, "DWM refused to cloak the window ({hr:#x})");
        let _ = unsafe { ShowWindow(HWND(hwnd), SW_SHOWNOACTIVATE) };
        Ok(())
    }

    /// Size the window so its client area is `width` x `height` device pixels.
    pub(super) fn size_client(hwnd: isize, width: i32, height: i32) -> Result<()> {
        let mut outer = RECT::default();
        let mut client = RECT::default();
        unsafe {
            GetWindowRect(HWND(hwnd), &mut outer).context("read the window's frame")?;
            GetClientRect(HWND(hwnd), &mut client).context("read the client area")?;
        }
        let frame_w = (outer.right - outer.left) - (client.right - client.left);
        let frame_h = (outer.bottom - outer.top) - (client.bottom - client.top);
        unsafe {
            SetWindowPos(
                HWND(hwnd),
                HWND::default(),
                0,
                0,
                width + frame_w,
                height + frame_h,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE,
            )
        }
        .context("size the window")
    }
}
