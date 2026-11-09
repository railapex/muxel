//! A code-editor pane backed by gpui-component's `InputState` code editor
//! (syntax highlighting, line numbers, indent guides, folding, Ctrl+F find,
//! tab indent, undo — all provided by the widget). This module adds file
//! load/save plumbing, language detection, and a dirty flag on top.

use crate::i18n::{t, tf};
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState, Position, TabSize};
use gpui_component::text::markdown;
use gpui_component::{ActiveTheme, Sizable};
use muxel_core::rescue::{BackupOffer, EditorBackup, FileStamp};
use muxel_store::rescue::RescueDir;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime};
use uuid::Uuid;

/// Don't try to open files larger than this (treated as non-text).
pub const MAX_EDITOR_BYTES: u64 = 8 * 1024 * 1024;

/// User-configurable editor appearance/behavior (from `Settings`).
#[derive(Clone)]
pub struct EditorConfig {
    /// Empty = the theme's monospace font.
    pub font_family: String,
    pub font_size: f32,
    pub tab_size: usize,
    pub soft_wrap: bool,
    pub line_numbers: bool,
    pub indent_guides: bool,
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            font_family: String::new(),
            font_size: 13.0,
            tab_size: 4,
            soft_wrap: false,
            line_numbers: true,
            indent_guides: true,
        }
    }
}

/// One editor pane: an open file (or an unsaved "Untitled" buffer), or — when
/// `diff_dir` is set — a read-only git-diff view of that directory.
pub struct EditorView {
    input: Entity<InputState>,
    path: Option<PathBuf>,
    dirty: bool,
    config: EditorConfig,
    /// Set for read-only diff panes: the directory whose `git diff` is shown.
    diff_dir: Option<PathBuf>,
    /// The file is an image — offer a rendered (`img`) view.
    is_image: bool,
    /// The file is markdown (`.md`/`.markdown`) — offer a rendered view.
    is_markdown: bool,
    /// Show the rendered view (image / markdown) vs the raw text editor.
    show_rendered: bool,
    /// Latest external change that could not be applied without discarding edits.
    external_change: Option<ExternalChange>,
    /// Last observed on-disk revision; polled off the UI thread.
    disk_stamp: Option<DiskStamp>,
    /// Backups of unsaved changes, so a crash doesn't lose them.
    rescue: EditorRescue,
}

/// An editor's backup of its unsaved changes: where it goes, the debounce,
/// and a backup found when the editor opened.
#[derive(Default)]
struct EditorRescue {
    target: Option<(RescueDir, Uuid)>,
    last_edit: Option<Instant>,
    pending: Option<Task<()>>,
    /// The on-disk version the unsaved edits started from.
    base: Option<FileStamp>,
    /// A backup left by the last run, until it is restored or discarded.
    offer: Option<(EditorBackup, BackupOffer)>,
    /// Orders this pane's backup writes; an editor rebuilt in another window
    /// keeps it.
    lane: Arc<BackupLane>,
    /// An input method's uncommitted text in the buffer: where it is and
    /// what it says.
    composing: Option<(std::ops::Range<usize>, String)>,
    /// Background writes wait on this before they land (tests only).
    #[cfg(all(test, windows))]
    gate: Option<async_channel::Receiver<()>>,
}

/// One pane's backup writes, in order. A background write takes a ticket
/// when it starts and lands only if no write or delete came after it, so an
/// older snapshot never replaces a newer one or brings a deleted one back.
#[derive(Debug, Default)]
pub(crate) struct BackupLane {
    /// Bumped by every write and delete.
    generation: AtomicU64,
    /// Held while a write lands or a delete runs.
    order: std::sync::Mutex<()>,
}

impl BackupLane {
    fn ticket(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Land a background write holding `ticket`, unless something came after.
    fn land<T>(&self, ticket: u64, write: impl FnOnce() -> T) -> Option<T> {
        let _order = self.order.lock().unwrap_or_else(|e| e.into_inner());
        (self.generation.load(Ordering::SeqCst) == ticket).then(write)
    }

    /// Act now, after any write that is landing, and void those still on
    /// their way.
    fn barrier<T>(&self, act: impl FnOnce() -> T) -> T {
        let _order = self.order.lock().unwrap_or_else(|e| e.into_inner());
        self.generation.fetch_add(1, Ordering::SeqCst);
        act()
    }
}

/// What an editor's backups carry when it is rebuilt in another window.
#[derive(Clone, Default)]
pub(crate) struct BackupState {
    target: Option<(RescueDir, Uuid)>,
    base: Option<FileStamp>,
    offer: Option<(EditorBackup, BackupOffer)>,
    lane: Arc<BackupLane>,
}

#[derive(Clone)]
pub(crate) enum ExternalChange {
    Modified(String),
    Deleted,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DiskStamp {
    modified: Option<SystemTime>,
    len: u64,
}

pub(crate) enum DiskChange {
    Content { stamp: DiskStamp, content: String },
    Deleted,
    Unavailable { stamp: DiskStamp },
}

pub(crate) fn disk_stamp(path: &Path) -> Option<DiskStamp> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(DiskStamp {
        modified: metadata.modified().ok(),
        len: metadata.len(),
    })
}

/// Poll one text file. Metadata is cheap; content is read only after its stamp changes.
pub(crate) fn read_disk_change(path: &Path, known: Option<DiskStamp>) -> Option<DiskChange> {
    let Some(stamp) = disk_stamp(path) else {
        return known.is_some().then_some(DiskChange::Deleted);
    };
    if known == Some(stamp) {
        return None;
    }
    if stamp.len > MAX_EDITOR_BYTES {
        return Some(DiskChange::Unavailable { stamp });
    }
    match std::fs::read_to_string(path) {
        Ok(content) => Some(DiskChange::Content { stamp, content }),
        Err(_) => Some(DiskChange::Unavailable { stamp }),
    }
}

/// Whether `path` is a markdown file by extension.
fn is_markdown_path(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
}

/// Whether `path` is a renderable image by extension (`img()` decodes these,
/// incl. SVG via rasterization).
fn is_image_path(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| {
            matches!(
                e.as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "svg"
            )
        })
}

impl EditorView {
    /// Open `path` (or a blank Untitled buffer when `None`).
    pub fn open(
        path: Option<PathBuf>,
        config: EditorConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let content = path.as_deref().and_then(read_text_file).unwrap_or_default();
        let language = path
            .as_deref()
            .map(language_for_path)
            .unwrap_or("text")
            .to_string();
        Self::build(path, content, language, None, false, config, window, cx)
    }

    /// Re-create an editor from captured state (used when popping a pane out to
    /// its own window — gpui-component binds text-input focus to the creating
    /// window, so the view must be rebuilt rather than moved). Undo history is
    /// not preserved.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_state(
        text: String,
        path: Option<PathBuf>,
        language: String,
        cursor: Option<Position>,
        dirty: bool,
        external_change: Option<ExternalChange>,
        disk_stamp: Option<DiskStamp>,
        show_rendered: bool,
        config: EditorConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut editor = Self::build(path, text, language, cursor, dirty, config, window, cx);
        editor.external_change = external_change;
        editor.disk_stamp = disk_stamp;
        if editor.is_renderable() {
            editor.show_rendered = show_rendered;
        }
        editor
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        path: Option<PathBuf>,
        content: String,
        language: String,
        cursor: Option<Position>,
        dirty: bool,
        config: EditorConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let cfg = config.clone();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor(language)
                .line_number(cfg.line_numbers)
                .indent_guides(cfg.indent_guides)
                .soft_wrap(cfg.soft_wrap)
                .tab_size(TabSize {
                    tab_size: cfg.tab_size,
                    hard_tabs: false,
                })
                .default_value(content)
        });
        // Mark dirty on edit; refresh the pane title's dirty dot once.
        cx.subscribe_in(
            &input,
            window,
            |this: &mut Self, _input, ev: &InputEvent, _window, cx| {
                if matches!(ev, InputEvent::Change) && !this.dirty {
                    this.dirty = true;
                    this.rescue.base = this.disk_stamp.map(file_stamp);
                    cx.notify();
                }
                if matches!(ev, InputEvent::Change | InputEvent::Blur) {
                    this.note_edit(cx);
                }
            },
        )
        .detach();
        // An input method's uncommitted text changes the buffer without a
        // Change event; only observers hear of it.
        cx.observe_in(&input, window, |this: &mut Self, input, window, cx| {
            this.note_composition(&input, window, cx)
        })
        .detach();
        if let Some(pos) = cursor {
            input.update(cx, |s, cx| s.set_cursor_position(pos, window, cx));
        }
        let stamp = path.as_deref().and_then(disk_stamp);
        let is_markdown = path.as_deref().is_some_and(is_markdown_path);
        let is_image = path.as_deref().is_some_and(is_image_path);
        Self {
            input,
            path,
            dirty,
            config,
            diff_dir: None,
            is_image,
            is_markdown,
            show_rendered: is_markdown || is_image,
            external_change: None,
            disk_stamp: stamp,
            rescue: EditorRescue::default(),
        }
    }

    /// Create a read-only git-diff view of `dir`, syntax-highlighted via the
    /// "diff" grammar (green additions / red deletions / `@@` hunks). The buffer
    /// is never saved; `refresh_diff` re-runs `git diff`.
    pub fn diff(
        dir: PathBuf,
        config: EditorConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let content = crate::integrations::git_diff(&dir);
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor("diff")
                .line_number(false)
                .indent_guides(false)
                .soft_wrap(config.soft_wrap)
                .default_value(content)
        });
        Self {
            input,
            path: None,
            dirty: false,
            config,
            diff_dir: Some(dir),
            is_image: false,
            is_markdown: false,
            show_rendered: false,
            external_change: None,
            disk_stamp: None,
            rescue: EditorRescue::default(),
        }
    }

    /// Whether this is a read-only diff pane.
    pub fn is_diff(&self) -> bool {
        self.diff_dir.is_some()
    }

    /// Whether this file has a rendered view (image or markdown).
    pub fn is_renderable(&self) -> bool {
        self.is_markdown || self.is_image
    }

    pub(crate) fn is_pollable_text(&self) -> bool {
        self.path.is_some() && self.diff_dir.is_none() && !self.is_image
    }
    pub fn is_html(&self) -> bool {
        self.path
            .as_deref()
            .and_then(Path::extension)
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("html") || ext.eq_ignore_ascii_case("htm"))
    }

    /// Whether the rendered view (vs raw editor) is currently shown.
    pub fn show_rendered(&self) -> bool {
        self.show_rendered
    }

    /// Toggle between the rendered view (image/markdown) and the raw text editor.
    pub fn toggle_rendered(&mut self, cx: &mut Context<Self>) {
        if self.is_renderable() {
            self.show_rendered = !self.show_rendered;
            cx.notify();
        }
    }

    /// The directory a diff pane is showing (for dedup), if any.
    pub fn diff_dir(&self) -> Option<&Path> {
        self.diff_dir.as_deref()
    }

    /// Re-run `git diff` for a diff pane and replace the shown text (synchronous;
    /// used by the manual refresh button and pop-out/restore).
    pub fn refresh_diff(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.diff_dir.clone() else {
            return;
        };
        let content = crate::integrations::git_diff(&dir);
        self.apply_diff_text(content, window, cx);
    }

    /// Replace a diff pane's text with already-computed `content` (used by the
    /// periodic background refresh so `git diff` doesn't run on the UI thread).
    pub fn set_diff_content(&self, content: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.diff_dir.is_some() {
            self.apply_diff_text(content, window, cx);
        }
    }

    /// Swap in new diff text while keeping the user's scroll position. Does
    /// nothing when the text is unchanged (so an idle diff never jumps).
    fn apply_diff_text(&self, content: String, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |s, cx| {
            if &*s.value() == content.as_str() {
                return;
            }
            // `set_value` snaps scroll to the top; re-apply the saved offset
            // (clamped after the next layout) so reading isn't interrupted.
            let offset = s.scroll_offset();
            s.set_value(content, window, cx);
            s.set_scroll_offset(offset, cx);
        });
    }

    /// Apply updated editor settings live (font/wrap/line-numbers/indent guides;
    /// tab width applies to newly-opened editors).
    pub fn set_config(
        &mut self,
        config: EditorConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Diff panes keep their fixed layout (no line numbers / indent guides);
        // only the font (read in `render`) follows the new config.
        if self.diff_dir.is_none() {
            self.input.update(cx, |s, cx| {
                s.set_line_number(config.line_numbers, window, cx);
                s.set_soft_wrap(config.soft_wrap, window, cx);
                s.set_indent_guides(config.indent_guides, window, cx);
            });
        }
        self.config = config;
        cx.notify();
    }

    /// Move the cursor to a 0-based line (used by find-in-project navigation).
    pub fn goto_line(&self, line: u32, window: &mut Window, cx: &mut Context<Self>) {
        self.goto_position(line, 0, window, cx);
    }

    /// Move the cursor to a zero-based source position.
    pub fn goto_position(
        &self,
        line: u32,
        character: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.input.update(cx, |s, cx| {
            s.set_cursor_position(Position { line, character }, window, cx);
        });
    }

    /// Reload an unchanged local buffer from disk. Dirty source always wins.
    pub fn reload_if_clean(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dirty {
            return;
        }
        let Some(path) = self.path.clone() else {
            return;
        };
        let Some(content) = read_text_file(&path) else {
            return;
        };
        self.disk_stamp = disk_stamp(&path);
        self.replace_disk_content(content, window, cx);
    }

    /// Apply a poll result, or retain the disk version beside dirty local edits.
    /// Keeping this state on the editor makes the same conflict UI work in main,
    /// secondary, and popped-out windows without app-level pane bookkeeping.
    pub(crate) fn apply_disk_change(
        &mut self,
        change: DiskChange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match change {
            DiskChange::Content { stamp, content } => {
                self.disk_stamp = Some(stamp);
                if self.dirty {
                    self.external_change = Some(ExternalChange::Modified(content));
                } else {
                    self.external_change = None;
                    self.replace_disk_content(content, window, cx);
                }
            }
            DiskChange::Deleted => {
                self.disk_stamp = None;
                self.external_change = Some(ExternalChange::Deleted);
            }
            DiskChange::Unavailable { stamp } => {
                self.disk_stamp = Some(stamp);
                self.external_change = Some(ExternalChange::Unavailable);
            }
        }
        cx.notify();
    }

    fn replace_disk_content(
        &mut self,
        content: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if &*self.input.read(cx).value() == content.as_str() {
            return;
        }
        self.input.update(cx, |s, cx| {
            let offset = s.scroll_offset();
            let cursor = s.cursor_position();
            s.set_value(content, window, cx);
            s.set_scroll_offset(offset, cx);
            s.set_cursor_position(cursor, window, cx);
        });
        self.dirty = false;
        cx.notify();
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
    pub(crate) fn external_change(&self) -> Option<ExternalChange> {
        self.external_change.clone()
    }
    pub(crate) fn disk_stamp(&self) -> Option<DiskStamp> {
        self.disk_stamp
    }
    pub fn text(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }
    pub fn cursor(&self, cx: &App) -> Position {
        self.input.read(cx).cursor_position()
    }
    /// Language name for the current path (for pop-out re-creation).
    pub fn language(&self) -> String {
        self.path
            .as_deref()
            .map(language_for_path)
            .unwrap_or("text")
            .to_string()
    }

    /// The pane header title: "Diff · <dir>" for diff panes, else the file name
    /// (with a "●" dirty marker) or "Untitled".
    pub fn title(&self) -> String {
        if let Some(dir) = &self.diff_dir {
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "diff".to_string());
            return tf("Diff · {name}", &[("name", &name)]);
        }
        let name = self
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| t("Untitled").to_string());
        if self.dirty {
            format!("● {name}")
        } else {
            name
        }
    }

    /// Mark the buffer as saved (clears the dirty flag).
    pub fn mark_saved(&mut self, cx: &mut Context<Self>) {
        let was_dirty = self.dirty;
        let changed = self.external_change.take().is_some();
        self.disk_stamp = self.path.as_deref().and_then(disk_stamp);
        if self.dirty {
            self.dirty = false;
        }
        if was_dirty || changed {
            cx.notify();
        }
        self.backup_clean(cx);
    }

    /// Replace the buffer contents (used after an async remote read), keeping the
    /// path/language, and clear the dirty flag.
    pub fn set_content(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |s, cx| s.set_value(text, window, cx));
        self.dirty = false;
        self.external_change = None;
        self.disk_stamp = self.path.as_deref().and_then(disk_stamp);
        cx.notify();
        self.backup_clean(cx);
    }

    /// Point the editor at a new on-disk path (after Save As), re-detecting the
    /// syntax language, and clear the dirty flag.
    pub fn set_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let lang = language_for_path(&path).to_string();
        let stamp = disk_stamp(&path);
        self.input.update(cx, |s, cx| s.set_highlighter(lang, cx));
        self.path = Some(path);
        self.dirty = false;
        self.external_change = None;
        self.disk_stamp = stamp;
        cx.notify();
        self.backup_clean(cx);
    }

    fn render_external_change(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let no_disk_version = matches!(
            self.external_change,
            Some(ExternalChange::Deleted | ExternalChange::Unavailable)
        );
        let label = match self.external_change {
            Some(ExternalChange::Deleted) => t("Moved, deleted, or inaccessible on disk"),
            Some(ExternalChange::Unavailable) => t("Couldn't read file from disk"),
            _ => t("File changed on disk"),
        };
        let mut actions = div().flex().items_center().gap_1();
        if !no_disk_version {
            actions = actions.child(
                Button::new("editor-external-reload")
                    .xsmall()
                    .label(t("Reload"))
                    .on_click(cx.listener(|this, _event, window, cx| {
                        let Some(ExternalChange::Modified(content)) = this.external_change.take()
                        else {
                            return;
                        };
                        this.replace_disk_content(content, window, cx);
                        this.dirty = false;
                        cx.notify();
                        this.backup_clean(cx);
                    })),
            );
        }
        actions = actions.child(
            Button::new("editor-external-keep")
                .ghost()
                .xsmall()
                .label(if no_disk_version {
                    t("Keep open")
                } else {
                    t("Keep mine")
                })
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.external_change = None;
                    cx.notify();
                })),
        );

        Some(
            div()
                .flex_none()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .px_2()
                .py_1()
                .bg(cx.theme().warning.opacity(0.14))
                .text_color(cx.theme().warning)
                .text_xs()
                .child(label)
                .child(actions)
                .into_any_element(),
        )
    }
}

/// A disk stamp as a backup records it.
fn file_stamp(stamp: DiskStamp) -> FileStamp {
    FileStamp {
        modified_ms: stamp
            .modified
            .and_then(|at| at.duration_since(SystemTime::UNIX_EPOCH).ok())
            .and_then(|d| i64::try_from(d.as_millis()).ok()),
        len: stamp.len,
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// `HH:MM` for today, else the date as well: when rescued text was kept.
pub(crate) fn rescue_time(ms: i64) -> String {
    use chrono::TimeZone as _;
    let Some(at) = chrono::Local.timestamp_millis_opt(ms).single() else {
        return String::new();
    };
    if at.date_naive() == chrono::Local::now().date_naive() {
        at.format("%H:%M").to_string()
    } else {
        at.format("%b %-d %H:%M").to_string()
    }
}

/// Backing up unsaved changes. A dirty buffer (or one holding an input
/// method's uncommitted text) is written about a second after edits stop,
/// staged off the UI thread; saving, reloading from disk, or closing without
/// saving deletes the backup. Deletes and the final write when an editor goes
/// away (quit, workspace switch) run on the UI thread, after any write that
/// is landing, and no earlier write lands after them.
impl EditorView {
    /// Back up this editor's unsaved changes to `target` (its workspace's
    /// rescue directory and its pane); `None` stops, and no write already on
    /// its way lands after it.
    pub(crate) fn set_backup_target(&mut self, target: Option<(RescueDir, Uuid)>) {
        if target.is_none() {
            self.rescue.pending = None;
            self.rescue.offer = None;
            self.rescue.lane.barrier(|| ());
        }
        self.rescue.target = target;
    }

    /// Keep this editor's unsaved changes backed up under `target`; with
    /// `offer`, first offer the backup a previous run left.
    pub(crate) fn adopt_backup(
        &mut self,
        target: Option<(RescueDir, Uuid)>,
        offer: bool,
        cx: &mut Context<Self>,
    ) {
        self.set_backup_target(target);
        if offer {
            self.offer_backup(cx);
        }
        self.note_edit(cx);
    }

    /// What this editor's backups need to carry on in a rebuilt editor.
    pub(crate) fn backup_state(&self) -> BackupState {
        BackupState {
            target: self.rescue.target.clone(),
            base: self.rescue.base,
            offer: self.rescue.offer.clone(),
            lane: self.rescue.lane.clone(),
        }
    }

    /// Carry on the backups of the editor this one was rebuilt from.
    pub(crate) fn adopt_backup_state(&mut self, state: BackupState, cx: &mut Context<Self>) {
        self.rescue.target = state.target;
        self.rescue.base = state.base;
        self.rescue.offer = state.offer;
        self.rescue.lane = state.lane;
        self.note_edit(cx);
    }

    /// Whether the buffer holds changes a backup must keep. Backups ask this
    /// and nothing else about dirtiness.
    fn needs_backup(&self, _cx: &App) -> bool {
        self.diff_dir.is_none()
            && !self.is_image
            && (self.is_dirty() || self.rescue.composing.is_some())
    }

    /// The input notified: an input method's uncommitted text may have
    /// changed, which no Change event reports. Back it up like an edit; once
    /// it's cancelled from a clean buffer, the backup goes.
    fn note_composition(
        &mut self,
        input: &Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gpui::EntityInputHandler as _;
        use gpui_component::RopeExt as _;
        let marked = input.update(cx, |state, cx| state.marked_text_range(window, cx));
        let composing = marked.map(|utf16| {
            let text = input.read(cx).text();
            let start = text.offset_utf16_to_offset(utf16.start);
            let bytes = start..text.offset_utf16_to_offset(utf16.end).max(start);
            let marked = text.slice(bytes.clone()).to_string();
            (bytes, marked)
        });
        if composing == self.rescue.composing {
            return;
        }
        self.rescue.composing = composing;
        if self.needs_backup(cx) {
            self.note_edit(cx);
        } else {
            self.backup_clean(cx);
        }
    }

    /// An edit (or the editor losing focus): restart the debounce.
    fn note_edit(&mut self, cx: &mut Context<Self>) {
        if self.rescue.target.is_none() || !self.needs_backup(cx) {
            return;
        }
        self.rescue.last_edit = Some(Instant::now());
        if self.rescue.pending.is_some() {
            return;
        }
        self.rescue.pending = Some(cx.spawn(async move |this, cx| {
            loop {
                let wait = this.update(cx, |this, _| {
                    this.rescue
                        .last_edit
                        .and_then(|at| muxel_core::rescue::debounce_remaining(at.elapsed()))
                });
                match wait {
                    Ok(Some(wait)) => cx.background_executor().timer(wait).await,
                    Ok(None) => break,
                    Err(_) => return,
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.rescue.pending = None;
                this.write_backup(false, cx);
            });
        }));
    }

    /// Write the backup, or delete it once the buffer is clean. A write is
    /// staged in the background and lands only if nothing came after it;
    /// `now` writes on this thread, for an editor about to be dropped. A
    /// delete always runs now.
    fn write_backup(&mut self, now: bool, cx: &mut Context<Self>) {
        let Some((dir, iid)) = self.rescue.target.clone() else {
            return;
        };
        let lane = self.rescue.lane.clone();
        if !self.needs_backup(cx) {
            self.rescue.base = None;
            lane.barrier(|| dir.delete_editor_backup(iid));
            return;
        }
        let text = self.text(cx);
        if text.len() > muxel_core::rescue::MAX_EDITOR_BACKUP_BYTES {
            log::warn!(
                "editor {iid} is too large to back up ({} bytes)",
                text.len()
            );
            return;
        }
        if self.rescue.base.is_none() {
            self.rescue.base = self.disk_stamp.map(file_stamp);
        }
        let backup = EditorBackup {
            path: self.path.clone(),
            text,
            base: self.rescue.base,
            saved_at_ms: now_ms(),
        };
        if now {
            if let Err(e) = lane.barrier(|| dir.save_editor_backup(iid, &backup)) {
                log::warn!("couldn't back up editor {iid}: {e:#}");
            }
            return;
        }
        let ticket = lane.ticket();
        #[cfg(all(test, windows))]
        let gate = self.rescue.gate.clone();
        cx.background_executor()
            .spawn(async move {
                let started = Instant::now();
                let staged = match dir.stage_editor_backup(iid, &backup) {
                    Ok(staged) => staged,
                    Err(e) => {
                        log::warn!("couldn't back up editor {iid}: {e:#}");
                        return;
                    }
                };
                #[cfg(all(test, windows))]
                if let Some(gate) = gate {
                    let _ = gate.recv().await;
                }
                match lane.land(ticket, || staged.commit()) {
                    Some(Ok(())) => log::debug!(
                        "editor backup saved: {} bytes in {:?}",
                        backup.text.len(),
                        started.elapsed()
                    ),
                    Some(Err(e)) => log::warn!("couldn't back up editor {iid}: {e:#}"),
                    None => log::debug!("editor backup superseded before it landed"),
                }
            })
            .detach();
    }

    /// Back up unsaved changes now (quit, workspace switch).
    pub(crate) fn flush_backup(&mut self, cx: &mut Context<Self>) {
        self.rescue.pending = None;
        if self.needs_backup(cx) {
            self.write_backup(true, cx);
        }
    }

    /// The buffer now matches the disk (saved, reloaded, replaced): its
    /// backup has nothing left to keep. An offered backup stays offered.
    fn backup_clean(&mut self, cx: &mut Context<Self>) {
        self.rescue.pending = None;
        self.write_backup(false, cx);
    }

    /// Offer the backup this pane left when muxel last ran, unless the file
    /// already holds that text. Call once, after the buffer was loaded.
    pub(crate) fn offer_backup(&mut self, cx: &mut Context<Self>) {
        let Some((dir, iid)) = self.rescue.target.clone() else {
            return;
        };
        let Some(backup) = dir.offer_editor_backup(iid) else {
            return;
        };
        let current = self.path.as_deref().and_then(disk_stamp).map(file_stamp);
        let loaded = self.text(cx);
        let disk_text = current.is_some().then_some(loaded.as_str());
        match muxel_core::rescue::backup_offer(&backup, current, disk_text) {
            BackupOffer::Stale => dir.delete_editor_offer(iid),
            offer => {
                log::info!("editor {iid} offers unsaved changes ({offer:?})");
                self.rescue.offer = Some((backup, offer));
                cx.notify();
            }
        }
    }

    /// Whether this editor is offering a backup. Only the Windows app tests
    /// use it.
    #[cfg(all(test, windows))]
    pub(crate) fn backup_offered(&self) -> bool {
        self.rescue.offer.is_some()
    }

    /// Type `text` at the cursor, as a keystroke would. Only the Windows app
    /// tests use it.
    #[cfg(all(test, windows))]
    pub(crate) fn type_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = text.to_string();
        self.input.update(cx, |s, cx| s.insert(text, window, cx));
    }

    /// Put `text` in the buffer as an input method's uncommitted composition
    /// does; empty cancels it. Only the Windows app tests use it.
    #[cfg(all(test, windows))]
    pub(crate) fn compose_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        use gpui::EntityInputHandler as _;
        self.input.update(cx, |s, cx| {
            s.replace_and_mark_text_in_range(None, text, None, window, cx)
        });
    }

    /// Hold this editor's background backup writes just before they land,
    /// until the returned sender sends or drops. Only the Windows app tests
    /// use it.
    #[cfg(all(test, windows))]
    pub(crate) fn hold_backup_writes(&mut self) -> async_channel::Sender<()> {
        let (release, gate) = async_channel::unbounded();
        self.rescue.gate = Some(gate);
        release
    }

    /// Restore the offered backup into the buffer, unsaved. Whether the file
    /// changed is decided now, against the version the backup's edits started
    /// from: a disk version newer than that (written before muxel started
    /// again, or while the offer showed) is kept beside the restored changes.
    pub(crate) fn restore_backup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((backup, _)) = self.rescue.offer.take() else {
            return;
        };
        if let Some((dir, iid)) = &self.rescue.target {
            dir.delete_editor_offer(*iid);
        }
        let base = backup.base;
        self.input
            .update(cx, |s, cx| s.set_value(backup.text, window, cx));
        self.dirty = true;
        self.rescue.base = base;
        if let Some(path) = self.path.clone() {
            let now = disk_stamp(&path);
            if base.is_some() && now.map(file_stamp) == base {
                self.disk_stamp = now;
            } else {
                match read_disk_change(&path, None) {
                    Some(change) => self.apply_disk_change(change, window, cx),
                    None => {
                        self.disk_stamp = None;
                        self.external_change = Some(ExternalChange::Deleted);
                    }
                }
            }
        }
        cx.notify();
        self.note_edit(cx);
    }

    /// Discard the offered backup and its file. Edits made while it showed
    /// are still backed up.
    pub(crate) fn discard_backup(&mut self, cx: &mut Context<Self>) {
        if self.rescue.offer.take().is_some()
            && let Some((dir, iid)) = self.rescue.target.clone()
        {
            dir.delete_editor_offer(iid);
        }
        cx.notify();
        self.note_edit(cx);
    }

    fn render_backup_offer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (backup, offer) = self.rescue.offer.as_ref()?;
        let time = rescue_time(backup.saved_at_ms);
        let label = match offer {
            BackupOffer::ChangedOnDisk => tf(
                "Restore unsaved changes from {time}? The file changed on disk since; restoring keeps both.",
                &[("time", &time)],
            ),
            _ => tf("Restore unsaved changes from {time}?", &[("time", &time)]),
        };
        Some(
            div()
                .flex_none()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .px_2()
                .py_1()
                .bg(cx.theme().primary.opacity(0.12))
                .text_color(cx.theme().foreground)
                .text_xs()
                .child(label)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            Button::new("editor-backup-restore")
                                .xsmall()
                                .label(t("Restore"))
                                .on_click(cx.listener(|this, _event, window, cx| {
                                    this.restore_backup(window, cx)
                                })),
                        )
                        .child(
                            Button::new("editor-backup-discard")
                                .ghost()
                                .xsmall()
                                .label(t("Discard"))
                                .on_click(
                                    cx.listener(|this, _event, _window, cx| {
                                        this.discard_backup(cx)
                                    }),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}

impl Focusable for EditorView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }
}

impl Render for EditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let backup = self.render_backup_offer(cx);
        let notice = self
            .external_change
            .as_ref()
            .and_then(|_| self.render_external_change(cx));
        // Rendered image view (incl. SVG, which `img` rasterizes).
        let content = if self.is_image
            && self.show_rendered
            && let Some(path) = self.path.clone()
        {
            div()
                .id("img-view")
                .size_full()
                .overflow_scroll()
                .flex()
                .justify_center()
                .bg(cx.theme().background)
                .p_4()
                .child(img(path).max_w_full())
                .into_any_element()
        // Rendered markdown view: a fixed-size scrollable container holding the
        // formatted markdown (the InputState text stays the source of truth, so
        // edits made in raw mode show here when toggled back).
        } else if self.is_markdown && self.show_rendered {
            let src = self.input.read(cx).value().to_string();
            div()
                .id("md-view")
                .size_full()
                .bg(cx.theme().background)
                .child(markdown(src).selectable(true).scrollable(true).p_4())
                .into_any_element()
        } else {
            let family: SharedString = if self.config.font_family.trim().is_empty() {
                cx.theme().mono_font_family.clone()
            } else {
                self.config.font_family.clone().into()
            };
            Input::new(&self.input)
                .h_full()
                .bordered(false)
                .focus_bordered(false)
                .font_family(family)
                .text_size(px(self.config.font_size))
                .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .children(backup)
            .children(notice)
            .child(div().flex_1().min_h_0().child(content))
            .into_any_element()
    }
}

/// Read a file as UTF-8 text, skipping oversized/unreadable files.
fn read_text_file(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_EDITOR_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// Map a file's extension to a gpui-component highlighter language name.
/// Unknown extensions return `"text"` (the highlighter falls back gracefully).
pub fn language_for_path(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "rs" => "rust",
        "py" | "pyi" | "pyw" => "python",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "json" | "jsonc" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "md" | "markdown" => "markdown",
        "go" => "go",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "cs" => "csharp",
        "html" | "htm" => "html",
        "css" | "scss" => "css",
        "sh" | "bash" | "zsh" => "bash",
        "rb" => "ruby",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "lua" => "lua",
        "scala" | "sc" => "scala",
        "sql" => "sql",
        "swift" => "swift",
        "ex" | "exs" => "elixir",
        "php" => "php",
        "svelte" => "svelte",
        "zig" => "zig",
        "graphql" | "gql" => "graphql",
        "proto" => "proto",
        "diff" | "patch" => "diff",
        "astro" => "astro",
        "cmake" => "cmake",
        "mk" | "mak" => "make",
        _ => "text",
    }
}

#[cfg(test)]
mod tests {
    use super::{DiskChange, disk_stamp, language_for_path, read_disk_change};
    use std::path::Path;

    #[test]
    fn detects_languages_by_extension() {
        assert_eq!(language_for_path(Path::new("a/b/main.rs")), "rust");
        assert_eq!(language_for_path(Path::new("x.PY")), "python"); // case-insensitive
        assert_eq!(language_for_path(Path::new("comp.tsx")), "tsx");
        assert_eq!(language_for_path(Path::new("s.unknownext")), "text");
        assert_eq!(language_for_path(Path::new("Makefile")), "text"); // no extension
        assert_eq!(language_for_path(Path::new("notes.md")), "markdown");
    }

    #[test]
    fn disk_poll_reads_only_changed_text_and_reports_deletion() {
        let root = std::env::temp_dir().join(format!(
            "muxel-editor-poll-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("notes.md");
        std::fs::write(&path, "one").unwrap();
        let initial = disk_stamp(&path).unwrap();
        assert!(read_disk_change(&path, Some(initial)).is_none());

        std::fs::write(&path, "two lines").unwrap();
        let changed = read_disk_change(&path, Some(initial)).unwrap();
        let stamp = match changed {
            DiskChange::Content { stamp, content } => {
                assert_eq!(content, "two lines");
                stamp
            }
            _ => panic!("changed UTF-8 text was not read"),
        };

        std::fs::remove_file(&path).unwrap();
        assert!(matches!(
            read_disk_change(&path, Some(stamp)),
            Some(DiskChange::Deleted)
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn disk_poll_reports_changed_non_utf8_content_as_unavailable() {
        let root = std::env::temp_dir().join(format!(
            "muxel-editor-poll-binary-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("notes.md");
        std::fs::write(&path, "text").unwrap();
        let initial = disk_stamp(&path).unwrap();
        std::fs::write(&path, [0xff, 0xfe, 0xfd]).unwrap();

        assert!(matches!(
            read_disk_change(&path, Some(initial)),
            Some(DiskChange::Unavailable { .. })
        ));
        let _ = std::fs::remove_dir_all(root);
    }
}
