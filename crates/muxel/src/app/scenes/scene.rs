//! The scene file: one JSON object in `<id>.scene.json`.
//!
//! ```json
//! {"id": "notes-beside-agent",
//!  "for": "feat/example",
//!  "source": "plans/example.md: \"the promise this frame checks\"",
//!  "ask": "Can you tell which pane is active?",
//!  "workspace": "fixture.json",
//!  "panes": {"notes": {"markdown": "# Notes"}},
//!  "focus": "notes",
//!  "window": [[1100, 700]],
//!  "sidebar_width": 300,
//!  "theme": "dark",
//!  "now": "2026-11-12T12:00:00Z",
//!  "crop": "sidebar"}
//! ```
//!
//! - `workspace` is a fixture file, relative to the scene, or the fixture
//!   inline: `{"projects": [{"name", "panes": [{"id", "title", "program"}],
//!   "layout"}], "active"}`. A layout node is `{"tabs": [ids], "active": id}`
//!   or `{"split": "horizontal" | "vertical", "children": [nodes], "sizes":
//!   [fractions]}`; horizontal puts children side by side. Without a layout a
//!   project's panes are tabs of one group.
//! - `panes` gives a pane's content. `markdown` makes it a Markdown file of
//!   that text, named by the pane's title. Any other pane is a terminal that
//!   never starts, drawn in muxel's starting state; `cast` is accepted and
//!   drawn that way until the replay session can play it.
//! - `window` lists sizes in logical pixels, `theme` is `dark` or `light`, and
//!   `now` fixes the clock. `crop` names a region: titlebar, sidebar, toolbar,
//!   hotbar, panes, shell, overlay.
//! - `for`, `source`, `ask` and `mock` are for the check sheet; the runner
//!   copies them into each frame's line unread.

use anyhow::{Context as _, Result, bail, ensure};
use chrono::{DateTime, Utc};
use muxel_core::{PaneNode, SplitDirection};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// The instant a scene shows when it names none.
const DEFAULT_NOW: &str = "2026-01-01T12:00:00Z";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Scene {
    pub id: String,
    #[serde(rename = "for", default)]
    pub feature: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub ask: Option<String>,
    workspace: WorkspaceSource,
    #[serde(default)]
    pub panes: BTreeMap<String, PaneContent>,
    #[serde(default)]
    pub focus: Option<String>,
    #[serde(default = "default_window")]
    pub window: Vec<(u32, u32)>,
    #[serde(default)]
    pub sidebar_width: Option<f32>,
    #[serde(default)]
    pub theme: Option<String>,
    #[serde(default)]
    now: Option<String>,
    #[serde(default)]
    pub crop: Option<String>,
    #[serde(default)]
    pub mock: Option<serde_json::Value>,
}

fn default_window() -> Vec<(u32, u32)> {
    vec![(1100, 700)]
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WorkspaceSource {
    File(String),
    Inline(Fixture),
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PaneContent {
    #[serde(default)]
    pub markdown: Option<String>,
    #[serde(default)]
    pub cast: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Fixture {
    pub projects: Vec<FixtureProject>,
    #[serde(default)]
    pub active: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FixtureProject {
    pub name: String,
    pub panes: Vec<FixturePane>,
    #[serde(default)]
    layout: Option<Layout>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FixturePane {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub program: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Layout {
    Tabs(Tabs),
    Split(Split),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Tabs {
    tabs: Vec<String>,
    #[serde(default)]
    active: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Split {
    split: String,
    children: Vec<Layout>,
    #[serde(default)]
    sizes: Option<Vec<f32>>,
}

/// A scene with its fixture read and every reference checked.
pub(super) struct Loaded {
    pub scene: Scene,
    pub fixture: Fixture,
    pub now: DateTime<Utc>,
    pub path: PathBuf,
}

/// Every `*.scene.json` under `path` (a folder, searched recursively, in name
/// order), or the one scene file `path` names.
pub(super) fn load_all(path: &Path) -> Result<Vec<Loaded>> {
    let mut files = Vec::new();
    if path.is_dir() {
        collect(path, &mut files)?;
        files.sort();
    } else {
        files.push(path.to_path_buf());
    }
    let mut seen = HashSet::new();
    files
        .into_iter()
        .map(|file| {
            let loaded = load(&file).with_context(|| file.display().to_string())?;
            ensure!(
                seen.insert(loaded.scene.id.clone()),
                "a second scene is called {:?}",
                loaded.scene.id
            );
            Ok(loaded)
        })
        .collect()
}

fn collect(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| dir.display().to_string())? {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, files)?;
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".scene.json"))
        {
            files.push(path);
        }
    }
    Ok(())
}

fn load(path: &Path) -> Result<Loaded> {
    let mut scene: Scene = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let fixture =
        match std::mem::replace(&mut scene.workspace, WorkspaceSource::File(String::new())) {
            WorkspaceSource::Inline(fixture) => fixture,
            WorkspaceSource::File(name) => {
                ensure!(
                    Path::new(&name).is_relative(),
                    "workspace {name:?} must be relative to the scene"
                );
                let file = path.parent().unwrap_or(Path::new(".")).join(&name);
                let text = std::fs::read_to_string(&file)
                    .with_context(|| format!("workspace {}", file.display()))?;
                serde_json::from_str(&text)
                    .with_context(|| format!("workspace {}", file.display()))?
            }
        };
    let now = DateTime::parse_from_rfc3339(scene.now.as_deref().unwrap_or(DEFAULT_NOW))
        .context("now must be an RFC 3339 instant, like 2026-11-12T12:00:00Z")?
        .to_utc();
    check(&scene, &fixture)?;
    Ok(Loaded {
        scene,
        fixture,
        now,
        path: path.to_path_buf(),
    })
}

fn check(scene: &Scene, fixture: &Fixture) -> Result<()> {
    ensure!(
        !scene.id.is_empty()
            && scene
                .id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "id {:?} must be lowercase letters, digits and dashes",
        scene.id
    );
    ensure!(!scene.window.is_empty(), "window lists no size");
    for &(w, h) in &scene.window {
        ensure!(
            (320..=3840).contains(&w) && (240..=2160).contains(&h),
            "window {w}x{h} is outside 320x240 to 3840x2160"
        );
    }
    if let Some(theme) = &scene.theme {
        ensure!(
            theme == "dark" || theme == "light",
            "theme {theme:?} must be dark or light"
        );
    }
    ensure!(!fixture.projects.is_empty(), "the workspace has no project");
    let mut projects = HashSet::new();
    let mut panes = HashSet::new();
    for project in &fixture.projects {
        ensure!(
            plain_name(&project.name),
            "project name {:?} must work as a folder name",
            project.name
        );
        ensure!(
            projects.insert(project.name.as_str()),
            "two projects are called {:?}",
            project.name
        );
        for pane in &project.panes {
            ensure!(
                panes.insert(pane.id.as_str()),
                "two panes have id {:?}",
                pane.id
            );
            ensure!(!pane.title.is_empty(), "pane {:?} has no title", pane.id);
            if scene
                .panes
                .get(&pane.id)
                .is_some_and(|c| c.markdown.is_some())
            {
                ensure!(
                    plain_name(&pane.title),
                    "Markdown pane {:?} is saved as {:?}, which must work as a file name",
                    pane.id,
                    pane.title
                );
            }
        }
        let mut placed = Vec::new();
        if let Some(layout) = &project.layout {
            layout_ids(layout, &mut placed)?;
        } else {
            placed = project.panes.iter().map(|p| p.id.as_str()).collect();
        }
        let mut listed: Vec<_> = project.panes.iter().map(|p| p.id.as_str()).collect();
        let mut sorted = placed.clone();
        sorted.sort_unstable();
        listed.sort_unstable();
        ensure!(
            sorted == listed,
            "project {:?}: the layout must place each of its panes once (places {placed:?})",
            project.name
        );
    }
    if let Some(active) = &fixture.active {
        ensure!(
            projects.contains(active.as_str()),
            "active project {active:?} isn't in the workspace"
        );
    }
    for (id, content) in &scene.panes {
        ensure!(
            panes.contains(id.as_str()),
            "panes names {id:?}, which isn't in the workspace"
        );
        ensure!(
            content.markdown.is_none() || content.cast.is_none(),
            "pane {id:?} has both markdown and a cast"
        );
    }
    if let Some(focus) = &scene.focus {
        ensure!(
            panes.contains(focus.as_str()),
            "focus {focus:?} isn't a pane"
        );
    }
    Ok(())
}

fn layout_ids<'a>(layout: &'a Layout, ids: &mut Vec<&'a str>) -> Result<()> {
    match layout {
        Layout::Tabs(Tabs { tabs, active }) => {
            ensure!(!tabs.is_empty(), "a tab group has no tabs");
            if let Some(active) = active {
                ensure!(
                    tabs.contains(active),
                    "active tab {active:?} isn't in its group"
                );
            }
            ids.extend(tabs.iter().map(String::as_str));
        }
        Layout::Split(Split {
            split,
            children,
            sizes,
        }) => {
            direction(split)?;
            ensure!(children.len() >= 2, "a split needs two children or more");
            if let Some(sizes) = sizes {
                ensure!(
                    sizes.len() == children.len() && sizes.iter().all(|s| *s > 0.0),
                    "a split's sizes must be one positive fraction per child"
                );
            }
            for child in children {
                layout_ids(child, ids)?;
            }
        }
    }
    Ok(())
}

fn direction(split: &str) -> Result<SplitDirection> {
    Ok(match split {
        "horizontal" => SplitDirection::Horizontal,
        "vertical" => SplitDirection::Vertical,
        other => bail!("split {other:?} must be horizontal or vertical"),
    })
}

fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|'])
}

/// The same id for the same names on every run, so nothing a scene draws can
/// depend on a random one.
pub(super) fn stable_id(parts: &[&str]) -> Uuid {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    let digest = hash.finalize();
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest[..16]);
    Uuid::from_bytes(bytes)
}

impl FixtureProject {
    /// This project's pane tree, with each fixture id mapped through `ids`.
    pub(super) fn layout(&self, scene: &str, ids: &HashMap<&str, Uuid>) -> Result<PaneNode> {
        match &self.layout {
            Some(layout) => node(layout, scene, ids),
            None => node(
                &Layout::Tabs(Tabs {
                    tabs: self.panes.iter().map(|p| p.id.clone()).collect(),
                    active: None,
                }),
                scene,
                ids,
            ),
        }
    }

    /// The first pane in layout order.
    pub(super) fn first_pane(&self) -> Option<&str> {
        match &self.layout {
            Some(layout) => first(layout),
            None => self.panes.first().map(|p| p.id.as_str()),
        }
    }
}

fn first(layout: &Layout) -> Option<&str> {
    match layout {
        Layout::Tabs(Tabs { tabs, active }) => {
            active.as_deref().or(tabs.first().map(String::as_str))
        }
        Layout::Split(Split { children, .. }) => children.first().and_then(first),
    }
}

fn node(layout: &Layout, scene: &str, ids: &HashMap<&str, Uuid>) -> Result<PaneNode> {
    let id = |name: &str| {
        ids.get(name)
            .copied()
            .with_context(|| format!("pane {name:?} isn't in the workspace"))
    };
    Ok(match layout {
        Layout::Tabs(Tabs { tabs, active }) => {
            let mut leaf = PaneNode::leaf(id(&tabs[0])?);
            if let PaneNode::Leaf(data) = &mut leaf {
                data.pane_id = stable_id(&[scene, "pane", &tabs.join(",")]);
                data.tabs = tabs.iter().map(|t| id(t)).collect::<Result<_>>()?;
                data.active = active
                    .as_ref()
                    .and_then(|a| tabs.iter().position(|t| t == a))
                    .unwrap_or(0);
            }
            leaf
        }
        Layout::Split(Split {
            split,
            children,
            sizes,
        }) => PaneNode::Split {
            direction: direction(split)?,
            sizes: match sizes {
                Some(sizes) => {
                    let total: f32 = sizes.iter().sum();
                    sizes.iter().map(|s| s / total).collect()
                }
                None => vec![1.0 / children.len() as f32; children.len()],
            },
            children: children
                .iter()
                .map(|child| node(child, scene, ids))
                .collect::<Result<_>>()?,
        },
    })
}
