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
//!   inline: `{"projects": [{"name", "path", "panes": [{"id", "title",
//!   "program"}], "layout"}], "active"}`. A layout node is `{"tabs": [ids],
//!   "active": id}` or `{"split": "horizontal" | "vertical", "children":
//!   [nodes], "sizes": [fractions]}`; horizontal puts children side by side.
//!   Without a layout a project's panes are tabs of one group.
//! - A project's `path` is the folder it says it is in, `C:\scenes\<name>` by
//!   default. It is never created, and must not exist on the machine that
//!   draws, so no frame shows a real folder. `branch` is the git branch its
//!   header shows, and `collapsed` folds it.
//! - A pane's `opened_by` names the pane that opened it. Its `instance` sets
//!   fields of muxel's saved pane record over the runner's own, for example
//!   `{"pinned": true, "activity": {"completed_at": {"ago": "5m"},
//!   "last_state": "done"}}`. Anywhere in it, `{"ago": "5m"}` (s, m, h, d) is
//!   that long before `now`, in Unix milliseconds. Fields a build doesn't have
//!   are ignored, so one scene serves builds with and without a feature.
//! - `settings` sets fields of muxel's settings the same way, for example
//!   `{"sidebar_sort": "RecentWork"}`.
//! - `notifications` fills the sidebar's list, the last entry newest:
//!   `[{"kind": "blocked" | "done" | "success" | "error", "title",
//!   "subtitle", "pane"}]`.
//! - `dialog: "quit"` opens the quit question.
//! - `closing: {"panes": [ids], "left": 13}` shows those panes just closed:
//!   out of the layout, with the title bar's Undo countdown at `left` seconds.
//! - `panes` gives a pane's content. `markdown` makes it a Markdown file of
//!   that text, named by the pane's title, held in memory. Any other pane is a
//!   terminal that never starts, drawn in muxel's starting state; `cast` is
//!   accepted and drawn that way until the replay session can play it.
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

/// Where a project says it is when its fixture names no folder.
const DEFAULT_FOLDER: &str = r"C:\scenes";

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
    pub settings: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default)]
    pub notifications: Vec<SceneNotification>,
    #[serde(default)]
    pub dialog: Option<String>,
    #[serde(default)]
    pub closing: Option<Closing>,
    #[serde(default)]
    pub mock: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Closing {
    pub panes: Vec<String>,
    #[serde(default = "default_undo_left")]
    pub left: u64,
}

fn default_undo_left() -> u64 {
    13
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SceneNotification {
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub subtitle: String,
    #[serde(default)]
    pub pane: Option<String>,
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
    #[serde(default)]
    path: Option<PathBuf>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub collapsed: bool,
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
    #[serde(default)]
    pub opened_by: Option<String>,
    #[serde(default)]
    pub instance: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Instance fields the runner sets itself, which an `instance` overlay can't.
const RUNNER_FIELDS: [&str; 4] = ["id", "project_id", "kind", "opened_by"];

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
    let closing = scene.closing_panes();
    if let Some(closing) = &scene.closing {
        ensure!(
            !closing.panes.is_empty() && (1..=60).contains(&closing.left),
            "closing needs panes and a left of 1 to 60 seconds"
        );
    }
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
        if let Some(path) = &project.path {
            ensure!(
                path.is_absolute(),
                "project {:?}: path {} must be absolute",
                project.name,
                path.display()
            );
        }
        for pane in &project.panes {
            ensure!(
                panes.insert(pane.id.as_str()),
                "two panes have id {:?}",
                pane.id
            );
            ensure!(!pane.title.is_empty(), "pane {:?} has no title", pane.id);
            if let Some(overlay) = &pane.instance {
                for field in RUNNER_FIELDS {
                    ensure!(
                        !overlay.contains_key(field),
                        "pane {:?}: the runner sets {field}; leave it out of instance",
                        pane.id
                    );
                }
            }
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
        // A pane that is closing has left the layout.
        let mut listed: Vec<_> = project
            .panes
            .iter()
            .map(|p| p.id.as_str())
            .filter(|id| !closing.contains(id))
            .collect();
        let mut placed = Vec::new();
        if let Some(layout) = &project.layout {
            layout_ids(layout, &mut placed)?;
        } else {
            placed = listed.clone();
        }
        ensure!(
            !placed.is_empty(),
            "project {:?} has no pane left in its layout",
            project.name
        );
        let mut sorted = placed.clone();
        sorted.sort_unstable();
        listed.sort_unstable();
        ensure!(
            sorted == listed,
            "project {:?}: the layout must place each of its panes once, except closing ones (places {placed:?})",
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
        ensure!(
            !closing.contains(focus.as_str()),
            "focus {focus:?} is closing, so it has left the layout"
        );
    }
    for pane in fixture.projects.iter().flat_map(|p| &p.panes) {
        if let Some(parent) = &pane.opened_by {
            ensure!(
                panes.contains(parent.as_str()) && parent != &pane.id,
                "pane {:?}: opened_by {parent:?} isn't another pane",
                pane.id
            );
        }
    }
    for note in &scene.notifications {
        ensure!(
            matches!(note.kind.as_str(), "blocked" | "done" | "success" | "error"),
            "notification kind {:?} must be blocked, done, success or error",
            note.kind
        );
        if let Some(pane) = &note.pane {
            ensure!(
                panes.contains(pane.as_str()),
                "notification pane {pane:?} isn't a pane"
            );
        }
    }
    if let Some(dialog) = &scene.dialog {
        ensure!(dialog == "quit", "dialog {dialog:?} must be quit");
    }
    for id in &closing {
        ensure!(
            panes.contains(id),
            "closing names {id:?}, which isn't a pane"
        );
    }
    Ok(())
}

impl Scene {
    /// The panes `closing` names.
    pub(super) fn closing_panes(&self) -> HashSet<&str> {
        self.closing
            .iter()
            .flat_map(|c| c.panes.iter().map(String::as_str))
            .collect()
    }
}

/// Replace every `{"ago": "<n><s|m|h|d>"}` in `value` with the Unix
/// milliseconds that long before `now_ms`, so a fixture's times move with the
/// scene's clock.
pub(super) fn resolve_ago(value: &mut serde_json::Value, now_ms: i64) -> Result<()> {
    use serde_json::Value;
    match value {
        Value::Object(map) if map.len() == 1 && map.contains_key("ago") => {
            let text = map["ago"].as_str().unwrap_or_default();
            let unit_ms = match text.chars().last() {
                Some('s') => 1_000,
                Some('m') => 60_000,
                Some('h') => 3_600_000,
                Some('d') => 86_400_000,
                _ => bail!(
                    "ago {} must be a count and s, m, h or d, like \"5m\"",
                    map["ago"]
                ),
            };
            // The unit is one ASCII byte, so this cut is on a char boundary.
            let number = &text[..text.len() - 1];
            let count: i64 = number
                .parse()
                .with_context(|| format!("ago {text:?} must start with a whole number"))?;
            *value = Value::from(now_ms - count * unit_ms);
        }
        Value::Object(map) => {
            for child in map.values_mut() {
                resolve_ago(child, now_ms)?;
            }
        }
        Value::Array(list) => {
            for child in list {
                resolve_ago(child, now_ms)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// `value` serialized, with `overlay`'s fields set over its own (objects merge
/// field by field), read back as a `T`. Fields this build's `T` doesn't have
/// are dropped, so a scene can name fields only some builds know.
pub(super) fn overlay<T: serde::Serialize + serde::de::DeserializeOwned>(
    value: &T,
    overlay: &serde_json::Map<String, serde_json::Value>,
    now_ms: i64,
) -> Result<T> {
    fn merge(base: &mut serde_json::Value, top: serde_json::Value) {
        match (base, top) {
            (serde_json::Value::Object(base), serde_json::Value::Object(top)) => {
                for (key, value) in top {
                    merge(base.entry(key).or_insert(serde_json::Value::Null), value);
                }
            }
            (base, top) => *base = top,
        }
    }
    let mut top = serde_json::Value::Object(overlay.clone());
    resolve_ago(&mut top, now_ms)?;
    let mut base = serde_json::to_value(value)?;
    merge(&mut base, top);
    Ok(serde_json::from_value(base)?)
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
    /// The folder the project says it is in. The runner never creates it, and
    /// refuses a scene whose folder exists, so a frame can show no real
    /// folder, its files or its git state.
    pub(super) fn folder(&self) -> PathBuf {
        self.path
            .clone()
            .unwrap_or_else(|| Path::new(DEFAULT_FOLDER).join(&self.name))
    }

    /// This project's pane tree, with each fixture id mapped through `ids`;
    /// without a layout, its panes but the `closing` ones as one tab group.
    pub(super) fn layout(
        &self,
        scene: &str,
        ids: &HashMap<&str, Uuid>,
        closing: &HashSet<&str>,
    ) -> Result<PaneNode> {
        match &self.layout {
            Some(layout) => node(layout, scene, ids),
            None => node(
                &Layout::Tabs(Tabs {
                    tabs: self
                        .panes
                        .iter()
                        .filter(|p| !closing.contains(p.id.as_str()))
                        .map(|p| p.id.clone())
                        .collect(),
                    active: None,
                }),
                scene,
                ids,
            ),
        }
    }

    /// The first pane in layout order.
    pub(super) fn first_pane(&self, closing: &HashSet<&str>) -> Option<&str> {
        match &self.layout {
            Some(layout) => first(layout),
            None => self
                .panes
                .iter()
                .map(|p| p.id.as_str())
                .find(|id| !closing.contains(id)),
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

#[cfg(test)]
mod tests {
    use super::{overlay, resolve_ago};
    use muxel_core::{AgentActivityState, Instance};
    use uuid::Uuid;

    const NOW_MS: i64 = 1_700_000_000_000;

    fn map(json: &str) -> serde_json::Map<String, serde_json::Value> {
        serde_json::from_str(json).unwrap()
    }

    /// One scene serves builds with and without a feature only while the saved
    /// pane record drops fields it doesn't know.
    #[test]
    fn instance_overlay_sets_known_fields_and_drops_unknown_ones() {
        let base = Instance::shell(Uuid::nil());
        let instance = overlay(
            &base,
            &map(r#"{
                "pinned": true,
                "a_field_no_build_has": {"ago": "1d"},
                "activity": {"completed_at": {"ago": "5m"}, "last_state": "done"}
            }"#),
            NOW_MS,
        )
        .unwrap();
        assert!(instance.pinned);
        assert_eq!(instance.activity.completed_at, Some(NOW_MS - 300_000));
        assert_eq!(instance.activity.last_state, Some(AgentActivityState::Done));
        assert_eq!(instance.id, base.id);
        assert_eq!(instance.program, base.program);
    }

    #[test]
    fn ago_without_a_count_and_unit_is_an_error() {
        for bad in [r#""""#, "5", r#""5""#, r#""m""#, r#""5w""#, r#""-""#] {
            let mut value = serde_json::from_str(&format!(r#"{{"ago": {bad}}}"#)).unwrap();
            assert!(
                resolve_ago(&mut value, NOW_MS).is_err(),
                "ago {bad} was accepted"
            );
        }
    }
}
