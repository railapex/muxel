//! Text someone typed but hadn't sent. A pane that starts again (after a
//! crash, a restart, or a Restart of its program) offers the prompt it held;
//! a pane closed for good leaves its prompt in a strip under the title bar.
//!
//! The pane's draft watcher (see `muxel_terminal::DraftSink`) keeps the live
//! file current from a background thread. This module renames, lists and
//! deletes those files, and writes a pane's draft itself only on events that
//! happen once per pane: the pane going away, or an offer giving way to the
//! text the pane now shows.

use super::*;
use muxel_core::rescue::PromptDraft;
use muxel_store::rescue::{DraftOffer, RescueDir};

/// A started pane's program can still show the offered draft (a tmux session
/// that outlived muxel). Once its screen has been quiet this long, muxel looks.
const OFFER_PROBE_QUIET: Duration = Duration::from_millis(1500);
const OFFER_PROBE_POLL: Duration = Duration::from_millis(300);
/// Stop looking after this long; the offer stays.
const OFFER_PROBE_LIMIT: Duration = Duration::from_secs(30);
/// Rows of a prompt an open list shows before it clips the rest.
const PREVIEW_ROWS: usize = 3;
/// More characters than one row of a pane's offer holds: 7680 pixels of
/// 8-pixel text, no glyph narrower than a fifth of its size. A line is cut
/// here before it's laid out, so a 2 MB draft isn't laid out to show a row
/// of it, and a cut line still overflows, so it still ends in an ellipsis.
const OFFER_ROW_CHARS: usize = 4800;

/// What the app holds of the workspace's rescued text.
#[derive(Default)]
pub(super) struct Rescue {
    /// Drafts offered on panes that started again, newest first.
    pub(super) offers: HashMap<Uuid, Vec<DraftOffer>>,
    /// Panes whose offer row is open to the drafts' text.
    pub(super) offers_open: HashSet<Uuid>,
    /// Drafts kept from panes closed for good, newest first.
    pub(super) closed: Vec<DraftOffer>,
    /// The strip was hidden; it shows again when another pane's draft is kept.
    pub(super) closed_hidden: bool,
    /// The list under the strip's one line is open.
    pub(super) closed_open: bool,
    /// Drafts an open list shows whole, by file: closed panes' in the strip,
    /// a pane's older offers under its row.
    pub(super) whole: HashSet<std::path::PathBuf>,
    /// What muxel kept when this workspace last stopped without closing
    /// cleanly, until dismissed.
    pub(super) crash: Option<CrashNotice>,
}

/// The one notice after an unclean exit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct CrashNotice {
    /// Panes whose unsent prompt was kept.
    pub(super) drafts: usize,
    /// Editors whose unsaved changes were kept.
    pub(super) editors: usize,
    /// Agents the workspace's last save recorded as working. That save can
    /// be older than the crash, so this is what was last known, not checked.
    pub(super) mid_turn: Vec<String>,
}

pub(super) use crate::editor::rescue_time;

impl MuxelApp {
    /// Where the current workspace keeps rescued text, while keeping it is on.
    pub(super) fn rescue_dir(&self) -> Option<RescueDir> {
        if !self.settings.rescue_unsent_text || muxel_store::rescue::rescue_turned_off() {
            return None;
        }
        muxel_store::rescue::rescue_dir(self.current_workspace?)
    }

    /// The current workspace's rescue directory whatever the setting, for
    /// deleting what someone dismissed.
    fn rescue_dir_any(&self) -> Option<RescueDir> {
        muxel_store::rescue::rescue_dir(self.current_workspace?)
    }

    fn draft_sink(dir: RescueDir, iid: Uuid) -> muxel_terminal::DraftSink {
        Arc::new(move |update| match update {
            muxel_terminal::DraftUpdate::Save(draft) => {
                if let Err(e) = dir.save_draft(iid, &draft) {
                    log::warn!("couldn't save draft for {iid}: {e:#}");
                }
            }
            muxel_terminal::DraftUpdate::Clear => dir.delete_draft(iid),
        })
    }

    /// A pane's view was just created: offer the draft it held when it last
    /// ran, then keep its new draft saved.
    pub(super) fn attach_draft_rescue(
        &mut self,
        iid: Uuid,
        view: &Entity<TerminalView>,
        cx: &mut Context<Self>,
    ) {
        let Some(dir) = self.rescue_dir() else {
            self.drop_offers(iid);
            return;
        };
        // Turn the old draft into an offer before the watcher can replace it.
        let offers = dir.offer_drafts(iid);
        view.update(cx, |view, cx| {
            view.set_draft_sink(Some(Self::draft_sink(dir, iid)), cx)
        });
        if offers.is_empty() {
            self.drop_offers(iid);
            return;
        }
        log::info!("pane {iid} offers {} unsent draft(s)", offers.len());
        self.rescue.offers_open.remove(&iid);
        self.rescue.offers.insert(iid, offers);
        self.probe_draft_offer(iid, view.clone(), cx);
    }

    /// The pane offers nothing any more.
    fn drop_offers(&mut self, iid: Uuid) {
        self.rescue.offers.remove(&iid);
        self.rescue.offers_open.remove(&iid);
    }

    /// Settings toggled keeping rescued text on or off. Off stops every
    /// watcher and backup in this window, then deletes what every workspace
    /// kept; other muxel windows on the same data stop writing too.
    pub(super) fn set_rescue_enabled(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.rescue_unsent_text = on;
        self.persist_settings();
        if on && let Err(e) = muxel_store::rescue::set_rescue_turned_off(false) {
            log::warn!("couldn't turn saving unsent text back on: {e:#}");
        }
        self.point_rescue_writers(self.rescue_dir(), cx);
        if !on {
            self.rescue = Rescue::default();
            // The marker first: a write that races the purge deletes itself.
            let purged = muxel_store::rescue::set_rescue_turned_off(true)
                .and_then(|()| muxel_store::rescue::purge_every_workspace());
            match purged {
                Ok(workspaces) => {
                    log::info!("saving unsent text off: purged {workspaces} workspace(s)")
                }
                Err(e) => self.add_event(
                    NotifKind::Error,
                    t("Couldn't delete all saved unsent text"),
                    format!("{e:#}"),
                ),
            }
        }
        cx.notify();
    }

    /// Point every pane's draft watcher and every editor's backups in this
    /// window at `dir`; `None` stops them, with no write left to land.
    pub(super) fn point_rescue_writers(&self, dir: Option<RescueDir>, cx: &mut App) {
        let views: Vec<(Uuid, Entity<TerminalView>)> = self
            .terminals
            .iter()
            .map(|(iid, view)| (*iid, view.clone()))
            .chain(
                self.popouts
                    .iter()
                    .filter_map(|(iid, popout)| match &popout.view {
                        PaneView::Terminal(view) => Some((*iid, view.clone())),
                        _ => None,
                    }),
            )
            .collect();
        for (iid, view) in views {
            let sink = dir.clone().map(|dir| Self::draft_sink(dir, iid));
            view.update(cx, |view, cx| view.set_draft_sink(sink, cx));
        }
        for (iid, editor) in self.rescue_editors() {
            let target = dir.clone().map(|dir| (dir, iid));
            editor.update(cx, |editor, cx| editor.adopt_backup(target, false, cx));
        }
    }

    /// The terminal view of `iid`, docked or popped out.
    fn rescue_view(&self, iid: Uuid) -> Option<&Entity<TerminalView>> {
        self.terminals
            .get(&iid)
            .or_else(|| match self.popouts.get(&iid).map(|p| &p.view) {
                Some(PaneView::Terminal(view)) => Some(view),
                _ => None,
            })
    }

    /// Settle a pane's draft now, for a pane about to go away within its
    /// watcher's debounce (closed, restarted, or muxel quitting): save what
    /// its box shows, or delete the file when the box is empty or a
    /// submission hasn't been cleared yet.
    pub(super) fn flush_draft(&self, iid: Uuid, cx: &App) {
        let Some(dir) = self.rescue_dir() else {
            return;
        };
        let Some(view) = self.rescue_view(iid).map(|view| view.read(cx)) else {
            return;
        };
        if !view.draft_watched() {
            return;
        }
        match view.settle_draft() {
            Some(muxel_terminal::DraftUpdate::Save(draft)) => {
                if let Err(e) = dir.save_draft(iid, &draft) {
                    log::warn!("couldn't save draft for {iid}: {e:#}");
                }
            }
            Some(muxel_terminal::DraftUpdate::Clear) => dir.delete_draft(iid),
            None => {}
        }
    }

    /// Save every pane's draft and every editor's unsaved changes now (quit,
    /// workspace switch).
    pub(super) fn flush_rescue(&self, cx: &mut App) {
        let ids: Vec<Uuid> = self
            .terminals
            .keys()
            .chain(self.popouts.keys())
            .copied()
            .collect();
        for iid in ids {
            self.flush_draft(iid, cx);
        }
        for (_, editor) in self.rescue_editors() {
            editor.update(cx, |editor, cx| editor.flush_backup(cx));
        }
    }

    /// Every editor pane, docked or popped out.
    fn rescue_editors(&self) -> Vec<(Uuid, Entity<EditorView>)> {
        self.editors
            .iter()
            .map(|(iid, editor)| (*iid, editor.clone()))
            .chain(
                self.popouts
                    .iter()
                    .filter_map(|(iid, popout)| match &popout.view {
                        PaneView::Editor(editor) => Some((*iid, editor.clone())),
                        _ => None,
                    }),
            )
            .collect()
    }

    /// An editor pane was just created: keep its unsaved changes backed up,
    /// first offering (`offer`) the backup it left when muxel last ran.
    pub(super) fn attach_editor_rescue(
        &self,
        iid: Uuid,
        editor: &Entity<EditorView>,
        offer: bool,
        cx: &mut App,
    ) {
        let target = self.rescue_dir().map(|dir| (dir, iid));
        editor.update(cx, |editor, cx| editor.adopt_backup(target, offer, cx));
    }

    /// An editor pane closed for good: its unsaved changes were saved or
    /// given up when it closed, so its backup goes.
    pub(super) fn drop_editor_backup(&self, iid: Uuid, cx: &mut App) {
        let Some(editor) = self.editors.get(&iid).cloned().or_else(|| {
            match self.popouts.get(&iid).map(|popout| &popout.view) {
                Some(PaneView::Editor(editor)) => Some(editor.clone()),
                _ => None,
            }
        }) else {
            return;
        };
        editor.update(cx, |editor, _| editor.set_backup_target(None));
        if let Some(dir) = self.rescue_dir_any() {
            dir.drop_editor(iid);
        }
    }

    /// Once the started pane has settled, drop an offer its program still
    /// shows in its input box.
    fn probe_draft_offer(&mut self, iid: Uuid, view: Entity<TerminalView>, cx: &mut Context<Self>) {
        // Weak: a closed pane's view goes with the pane, not at the next poll.
        let view = view.downgrade();
        cx.spawn(async move |this, cx| {
            let started = Instant::now();
            loop {
                cx.background_executor().timer(OFFER_PROBE_POLL).await;
                let state = this.update(cx, |this, cx| {
                    let view = view.upgrade()?;
                    if this.rescue_view(iid) != Some(&view)
                        || !this.rescue.offers.contains_key(&iid)
                    {
                        return None;
                    }
                    let pane = view.read(cx);
                    let settled = pane.has_visible_content()
                        && pane.session().idle_for() >= OFFER_PROBE_QUIET;
                    Some(settled.then(|| pane.current_draft()))
                });
                match state {
                    Ok(Some(Some(current))) => {
                        let _ = this.update(cx, |this, cx| {
                            this.drop_offer_still_shown(iid, current.as_ref(), cx)
                        });
                        return;
                    }
                    Ok(Some(None)) if started.elapsed() < OFFER_PROBE_LIMIT => {}
                    _ => return,
                }
            }
        })
        .detach();
    }

    fn drop_offer_still_shown(
        &mut self,
        iid: Uuid,
        current: Option<&PromptDraft>,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = current else {
            return;
        };
        let Some(offer) = self
            .rescue
            .offers
            .get(&iid)
            .and_then(|o| o.first())
            .cloned()
        else {
            return;
        };
        // Exactly the same text, spaces and all: `a  b` isn't `a b`.
        if offer.draft.text == current.text {
            log::info!("pane {iid} still shows its unsent draft; dropping the offer");
            // The pane's watcher waits for a keystroke before it saves, so
            // the text it shows is saved here before the offer goes.
            if !self.keep_as_live_draft(iid, current) {
                return;
            }
            self.forget_draft_offer(iid, &offer.path);
            cx.notify();
        }
    }

    /// Save `draft` as the pane's live draft, so a copy outlives the offer
    /// that held it. False when it couldn't be saved.
    fn keep_as_live_draft(&self, iid: Uuid, draft: &PromptDraft) -> bool {
        let Some(dir) = self.rescue_dir() else {
            return false;
        };
        match dir.save_draft(iid, draft) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("couldn't save draft for {iid}: {e:#}");
                false
            }
        }
    }

    /// Restore: paste offered draft `path` into the pane's input box as it
    /// was saved, line breaks and all, and never submit it. Where a pasted
    /// line break would reach the program as Enter, the draft goes to the
    /// clipboard instead.
    pub(super) fn restore_offer(
        &mut self,
        iid: Uuid,
        path: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(offer) = self
            .rescue
            .offers
            .get(&iid)
            .and_then(|offers| offers.iter().find(|offer| offer.path == path))
            .cloned()
        else {
            return;
        };
        let Some(session) = self.paste_session(iid, cx) else {
            return;
        };
        let text = offer.draft.text.as_str();
        if !session.is_bracketed_paste() && text.contains(['\r', '\n']) {
            // Without bracketed paste a newline reaches the program as Enter.
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
            self.add_event(
                NotifKind::Success,
                t("Unsent prompt copied"),
                t("This pane would send a multi-line paste, so it's on the clipboard instead.")
                    .to_string(),
            );
        } else {
            session.mark_user_input();
            if let Err(e) = session.try_paste(text) {
                self.add_event(
                    NotifKind::Error,
                    t("Couldn't restore the unsent prompt"),
                    e.to_string(),
                );
                cx.notify();
                return;
            }
        }
        // Until the watcher reads the box again, the offer's text is the
        // pane's draft.
        let restored = PromptDraft {
            pane: None,
            ..offer.draft.clone()
        };
        if !self.keep_as_live_draft(iid, &restored) {
            cx.notify();
            return;
        }
        self.forget_draft_offer(iid, &offer.path);
        self.focus_instance(iid, window, cx);
        cx.notify();
    }

    /// Discard: delete offered draft `path`.
    pub(super) fn forget_draft_offer(&mut self, iid: Uuid, path: &std::path::Path) {
        if let Some(dir) = self.rescue_dir_any() {
            dir.delete_offer(path);
        }
        self.rescue.whole.remove(path);
        if let Some(offers) = self.rescue.offers.get_mut(&iid) {
            offers.retain(|offer| offer.path != path);
            if offers.is_empty() {
                self.drop_offers(iid);
            }
        }
    }

    /// A pane is closed for good: keep its unsent draft, and any draft it was
    /// still offering, in the strip.
    pub(super) fn keep_closed_draft(&mut self, iid: Uuid, cx: &App) {
        self.flush_draft(iid, cx);
        self.drop_offers(iid);
        let Some(dir) = self.rescue_dir() else {
            return;
        };
        let title = self
            .workspace
            .instance(iid)
            .map(|i| i.display_name().to_string())
            .unwrap_or_default();
        if dir.keep_closed_drafts(iid, &title) > 0 {
            log::info!("kept the unsent draft of closed pane {iid}");
            self.rescue.closed = dir.closed_drafts();
            self.rescue.closed_hidden = false;
        }
    }

    /// Workspace `id` is opening, its lock held and none of its panes
    /// started: if saving is off (here or in another window), delete what
    /// every workspace kept, a write that landed after an earlier purge
    /// included; else the temporaries of writes a crash cut off. What is
    /// left of deleted workspaces goes too.
    pub(super) fn prepare_workspace_rescue(&mut self, id: Uuid) {
        muxel_store::rescue::sweep_deleted_workspaces();
        let Some(dir) = muxel_store::rescue::rescue_dir(id) else {
            return;
        };
        if muxel_store::rescue::rescue_turned_off() {
            self.settings.rescue_unsent_text = false;
        }
        if !self.settings.rescue_unsent_text {
            if let Err(e) = muxel_store::rescue::purge_every_workspace() {
                log::warn!("couldn't delete everything kept: {e:#}");
            }
            return;
        }
        let removed = dir.remove_temporaries();
        if removed > 0 {
            log::info!("deleted {removed} rescue temporary file(s) a crash left");
        }
    }

    /// Mark workspace `id` running. When it already was, the last run never
    /// closed it: say what was kept and which agents were last saved as
    /// mid-turn (`loaded`, its document as saved), if anything.
    pub(super) fn note_workspace_running(
        &self,
        id: Uuid,
        loaded: Option<&Workspace>,
    ) -> Option<CrashNotice> {
        if !muxel_store::rescue::mark_workspace_running(id) {
            return None;
        }
        let counts = muxel_store::rescue::rescue_dir(id)
            .filter(|_| self.settings.rescue_unsent_text)
            .map(|dir| dir.counts())
            .unwrap_or_default();
        let mid_turn: Vec<String> = loaded
            .map(|ws| {
                ws.instances
                    .iter()
                    .filter(|i| {
                        i.activity.current_state() == Some(muxel_core::AgentActivityState::Working)
                    })
                    .map(|i| i.display_name().to_string())
                    .collect()
            })
            .unwrap_or_default();
        log::warn!(
            "workspace {id} didn't close cleanly: kept {} draft(s) and {} editor backup(s); {} agent(s) mid-turn",
            counts.drafts,
            counts.editors,
            mid_turn.len()
        );
        let notice = CrashNotice {
            drafts: counts.drafts,
            editors: counts.editors,
            mid_turn,
        };
        (notice != CrashNotice::default()).then_some(notice)
    }

    /// Load what the newly entered workspace kept: settle drafts of panes
    /// that no longer exist, and list closed panes' drafts.
    pub(super) fn load_workspace_rescue(&mut self, crash: Option<CrashNotice>) {
        self.rescue.crash = crash;
        let Some(dir) = self.rescue_dir() else {
            return;
        };
        let live: HashSet<Uuid> = self.workspace.instances.iter().map(|i| i.id).collect();
        dir.adopt_orphans(&live);
        self.rescue.closed = dir.closed_drafts();
    }

    fn copy_closed_draft(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        if let Some(closed) = self.rescue.closed.iter().find(|c| c.path == path) {
            cx.write_to_clipboard(ClipboardItem::new_string(closed.draft.text.clone()));
        }
        cx.notify();
    }

    fn discard_closed_draft(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        if let Some(dir) = self.rescue_dir_any() {
            dir.delete_offer(path);
        }
        self.rescue.closed.retain(|c| c.path != path);
        self.rescue.whole.remove(path);
        cx.notify();
    }

    /// Discard all: delete every closed pane's draft the strip lists.
    pub(super) fn discard_all_closed_drafts(&mut self, cx: &mut Context<Self>) {
        let dir = self.rescue_dir_any();
        for closed in std::mem::take(&mut self.rescue.closed) {
            if let Some(dir) = &dir {
                dir.delete_offer(&closed.path);
            }
            self.rescue.whole.remove(&closed.path);
        }
        self.rescue.closed_open = false;
        cx.notify();
    }

    /// Whether `text` takes more than [`PREVIEW_ROWS`] rows `width` wide in
    /// `style` at `size`, so the open list clips it. A row holds at most
    /// `row_chars` characters.
    fn preview_clips(
        text: &str,
        width: Pixels,
        row_chars: usize,
        style: &TextStyle,
        size: Pixels,
        window: &Window,
    ) -> bool {
        let mut lines = text.split('\n');
        let head: Vec<&str> = lines.by_ref().take(PREVIEW_ROWS).collect();
        if lines.next().is_some() {
            return true;
        }
        // A longer line can't fit, and shaping it would only cost.
        let mut rows = 0;
        for line in head {
            if line.chars().count() > PREVIEW_ROWS * row_chars {
                return true;
            }
            let run = style.to_run(line.len());
            rows += window
                .text_system()
                .shape_text(line.to_string().into(), size, &[run], Some(width), None)
                .map_or(PREVIEW_ROWS + 1, |shaped| {
                    shaped
                        .iter()
                        .map(|line| line.wrap_boundaries().len() + 1)
                        .sum()
                });
            if rows > PREVIEW_ROWS {
                return true;
            }
        }
        false
    }

    /// The start of `text` a clipped preview lays out: one line more than it
    /// shows, each cut one character past what [`PREVIEW_ROWS`] rows of
    /// `row_chars` can hold. A long prompt (a draft can be 2 MB) isn't laid
    /// out whole to show three rows of it, and what's left still overflows
    /// where the text did, so the preview still ends in an ellipsis.
    fn preview_head(text: &str, row_chars: usize) -> String {
        let limit = PREVIEW_ROWS * row_chars + 1;
        text.split('\n')
            .take(PREVIEW_ROWS + 1)
            .map(|line| match line.char_indices().nth(limit) {
                Some((at, _)) => &line[..at],
                None => line,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// What an offer row calls offered draft `offer`.
    fn offer_label(offer: &DraftOffer) -> String {
        let time = rescue_time(offer.draft.saved_at_ms);
        if offer.draft.partial {
            tf("Unsent prompt from {time} (partial)", &[("time", &time)])
        } else {
            tf("Unsent prompt from {time}", &[("time", &time)])
        }
    }

    /// The first line of `text` that holds any, as the offer row shows it:
    /// cut at [`OFFER_ROW_CHARS`], with an ellipsis when more text follows.
    fn offer_first_line(text: &str) -> String {
        let text = text.trim_start();
        let (line, rest) = text.split_once('\n').unwrap_or((text, ""));
        let mut shown: String = line.trim_end().chars().take(OFFER_ROW_CHARS).collect();
        if !rest.trim().is_empty() {
            shown.push_str(" …");
        }
        shown
    }

    /// Opens or closes pane `iid`'s offer row.
    pub(super) fn toggle_offer(&mut self, iid: Uuid, cx: &mut Context<Self>) {
        if !self.rescue.offers_open.remove(&iid) {
            self.rescue.offers_open.insert(iid);
        }
        cx.notify();
    }

    /// Restore and Discard for offered draft `path`, the `index`th newest.
    fn offer_buttons(
        iid: Uuid,
        index: usize,
        path: &std::path::Path,
        cx: &mut Context<Self>,
    ) -> Div {
        let restore = path.to_path_buf();
        let discard = path.to_path_buf();
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .child(
                Button::new(format!("draft-restore-{iid}-{index}"))
                    .xsmall()
                    .label(t("Restore"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.restore_offer(iid, &restore, window, cx)
                    })),
            )
            .child(
                Button::new(format!("draft-discard-{iid}-{index}"))
                    .ghost()
                    .xsmall()
                    .label(t("Discard"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.forget_draft_offer(iid, &discard);
                        cx.notify();
                    })),
            )
    }

    /// The row under a pane that offers its unsent drafts: the newest one's
    /// first line, cut with an ellipsis, then Restore and Discard. Clicking
    /// the text, or "+N older", opens the drafts in place: the newest whole,
    /// older ones three rows each (a click shows one whole), each with its
    /// own Restore and Discard when there are several. The open row pushes
    /// the pane's screen up instead of drawing over it, so browser panes
    /// need no overlay flag for it.
    pub(super) fn render_draft_offer(
        &self,
        iid: Uuid,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let offers = self.rescue.offers.get(&iid)?;
        let newest = offers.first()?;
        let open = self.rescue.offers_open.contains(&iid);
        let many = offers.len() > 1;
        let muted = cx.theme().muted_foreground;
        let title = div()
            .id(format!("draft-offer-toggle-{iid}"))
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap_1()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_offer(iid, cx)))
            .child(
                Icon::new(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .xsmall(),
            );
        let title = if open && many {
            title.child(div().truncate().child(tn(
                "{n} unsent prompt",
                "{n} unsent prompts",
                offers.len(),
                &[("n", &offers.len().to_string())],
            )))
        } else if open {
            title.child(div().truncate().child(Self::offer_label(newest)))
        } else {
            title
                .child(
                    div()
                        .flex_none()
                        .text_color(muted)
                        .child(Self::offer_label(newest)),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .child(Self::offer_first_line(&newest.draft.text)),
                )
                .children(many.then(|| {
                    div().flex_none().text_color(muted).child(tf(
                        "(+{n} older)",
                        &[("n", &(offers.len() - 1).to_string())],
                    ))
                }))
        };
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .child(title)
            .children((!open || !many).then(|| Self::offer_buttons(iid, 0, &newest.path, cx)));
        let row = div()
            .flex_none()
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .py_1()
            .text_xs()
            .bg(cx.theme().primary.opacity(0.12))
            .text_color(cx.theme().foreground)
            .child(header);
        if !open {
            return Some(row.into_any_element());
        }
        let mut list = div()
            .id(format!("draft-offer-list-{iid}"))
            .flex()
            .flex_col()
            .gap_2()
            .max_h(rems(18.))
            .overflow_y_scroll();
        for (index, offer) in offers.iter().enumerate() {
            let text = div()
                .id(format!("draft-offer-text-{iid}-{index}"))
                .pl_2()
                .border_l_2()
                .border_color(cx.theme().border);
            // The newest is what the row's text opened; an older one shows
            // whole once clicked.
            let text = if index == 0 {
                text.child(offer.draft.text.clone())
            } else {
                let path = offer.path.clone();
                let text = text
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.rescue.whole.remove(&path) {
                            this.rescue.whole.insert(path.clone());
                        }
                        cx.notify();
                    }));
                if self.rescue.whole.contains(&offer.path) {
                    text.child(offer.draft.text.clone())
                } else {
                    text.child(Self::preview_head(&offer.draft.text, OFFER_ROW_CHARS))
                        .line_clamp(PREVIEW_ROWS)
                        .text_ellipsis()
                }
            };
            let item = div().flex().flex_col().gap_0p5();
            let item = if many {
                item.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .text_color(muted)
                        .child(div().truncate().child(Self::offer_label(offer)))
                        .child(Self::offer_buttons(iid, index, &offer.path, cx)),
                )
            } else {
                item
            };
            list = list.child(item.child(text));
        }
        Some(row.child(list).into_any_element())
    }

    /// The crash notice's text.
    pub(super) fn crash_label(notice: &CrashNotice) -> String {
        let mut kept = Vec::new();
        if notice.drafts > 0 {
            kept.push(tn(
                "{n} unsent prompt",
                "{n} unsent prompts",
                notice.drafts,
                &[("n", &notice.drafts.to_string())],
            ));
        }
        if notice.editors > 0 {
            kept.push(tn(
                "{n} editor's unsaved changes",
                "{n} editors' unsaved changes",
                notice.editors,
                &[("n", &notice.editors.to_string())],
            ));
        }
        let mut label = t("muxel didn't close cleanly.").to_string();
        if !kept.is_empty() {
            label.push(' ');
            label.push_str(&tf(
                "Kept {what}; each pane offers its own.",
                &[("what", &kept.join(", "))],
            ));
        }
        if !notice.mid_turn.is_empty() {
            label.push(' ');
            // The workspace's last save, not the moment it stopped.
            label.push_str(&tf(
                "Working when muxel last saved: {names}.",
                &[("names", &notice.mid_turn.join(", "))],
            ));
        }
        label
    }

    fn render_crash_notice(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let notice = self.rescue.crash.as_ref()?;
        Some(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(div().child(Self::crash_label(notice)))
                .child(
                    Button::new("rescue-crash-dismiss")
                        .ghost()
                        .xsmall()
                        .label(t("Dismiss"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.rescue.crash = None;
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }

    /// The strip under the title bar: the crash notice, and one line for
    /// closed panes' drafts that opens a list of them, each with its text.
    /// The open list pushes the panes down instead of drawing over them, so
    /// browser panes need no overlay flag for it.
    pub(super) fn render_rescue_strip(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let crash = self.render_crash_notice(cx);
        let closed = if self.rescue.closed_hidden {
            &[][..]
        } else {
            &self.rescue.closed[..]
        };
        if crash.is_none() && closed.is_empty() {
            return None;
        }
        let strip = div()
            .flex_none()
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .py_1()
            .text_xs()
            .bg(cx.theme().secondary)
            .border_b_1()
            .border_color(cx.theme().border);
        let mut strip = strip.children(crash);
        if closed.is_empty() {
            return Some(strip.into_any_element());
        }
        let open = self.rescue.closed_open;
        let count = closed.len();
        strip =
            strip.child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        Button::new("rescue-closed-toggle")
                            .ghost()
                            .xsmall()
                            .icon(if open {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .label(tn(
                                "{n} unsent prompt from a closed pane",
                                "{n} unsent prompts from closed panes",
                                count,
                                &[("n", &count.to_string())],
                            ))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.rescue.closed_open = !this.rescue.closed_open;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .children(open.then(|| {
                                Button::new("rescue-discard-all")
                                    .ghost()
                                    .xsmall()
                                    .label(t("Discard all"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.discard_all_closed_drafts(cx)
                                    }))
                            }))
                            .child(
                                Button::new("rescue-hide")
                                    .ghost()
                                    .xsmall()
                                    .label(t("Hide"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.rescue.closed_hidden = true;
                                        this.rescue.closed_open = false;
                                        cx.notify();
                                    })),
                            ),
                    ),
            );
        if !open {
            return Some(strip.into_any_element());
        }
        // What a prompt's text is laid out in: the strip's font at its size,
        // across the window less the strip's padding and the text's border
        // and indent, with a few pixels to spare.
        let rem = window.rem_size();
        let size = rems(0.75).to_pixels(rem);
        let mut style = window.text_style();
        style.font_family = cx.theme().font_family.clone();
        let width = window.viewport_size().width - rem * 2.0 - px(6.0);
        // No glyph is narrower than a fifth of the font's size.
        let row_chars = (width / (size * 0.2)).ceil().max(1.0) as usize;
        let mut list = div()
            .id("rescue-closed-list")
            .flex()
            .flex_col()
            .gap_2()
            .max_h(window.viewport_size().height * 0.4)
            .overflow_y_scroll();
        for (index, kept) in closed.iter().enumerate() {
            let time = rescue_time(kept.draft.saved_at_ms);
            let label = match (kept.draft.pane.as_deref(), kept.draft.partial) {
                (Some(pane), false) => tf("“{pane}”, {time}", &[("pane", pane), ("time", &time)]),
                (Some(pane), true) => tf(
                    "“{pane}”, {time} (partial)",
                    &[("pane", pane), ("time", &time)],
                ),
                (None, false) => tf("A closed pane, {time}", &[("time", &time)]),
                (None, true) => tf("A closed pane, {time} (partial)", &[("time", &time)]),
            };
            let whole = self.rescue.whole.contains(&kept.path);
            let clips = whole
                || Self::preview_clips(&kept.draft.text, width, row_chars, &style, size, window);
            let text = div()
                .pl_2()
                .border_l_2()
                .border_color(cx.theme().border)
                .text_color(cx.theme().foreground);
            let text = if whole {
                text.child(kept.draft.text.clone())
            } else {
                text.child(Self::preview_head(&kept.draft.text, row_chars))
                    .line_clamp(PREVIEW_ROWS)
                    .text_ellipsis()
            };
            let copy_path = kept.path.clone();
            let discard_path = kept.path.clone();
            let whole_path = kept.path.clone();
            list = list.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .text_color(cx.theme().muted_foreground)
                            .child(div().truncate().child(label))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        Button::new(("rescue-copy", index))
                                            .xsmall()
                                            .label(t("Copy"))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.copy_closed_draft(&copy_path, cx)
                                            })),
                                    )
                                    .child(
                                        Button::new(("rescue-discard", index))
                                            .ghost()
                                            .xsmall()
                                            .label(t("Discard"))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.discard_closed_draft(&discard_path, cx)
                                            })),
                                    ),
                            ),
                    )
                    .child(text)
                    .children(clips.then(|| {
                        div().child(
                            Button::new(("rescue-whole", index))
                                .ghost()
                                .xsmall()
                                .label(if whole { t("Show less") } else { t("Show all") })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if !this.rescue.whole.remove(&whole_path) {
                                        this.rescue.whole.insert(whole_path.clone());
                                    }
                                    cx.notify();
                                })),
                        )
                    })),
            );
        }
        strip = strip.child(list).child(
            div()
                .text_color(cx.theme().muted_foreground)
                .child(t("Kept until you copy or discard them.")),
        );
        Some(strip.into_any_element())
    }
}
