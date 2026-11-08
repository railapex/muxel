//! Text a person typed but hadn't sent or saved, kept beside a workspace's
//! document so a crash, a restart or an accidental close doesn't lose it.
//!
//! One small JSON file per pane in `workspaces/<id>/rescue/`:
//!
//! - `draft-<instance>.json` — the live draft a pane keeps up to date;
//! - `draft-<instance>.<ms>.offer.json` — a draft offered when its pane
//!   started again, kept until Restore or Discard;
//! - `closed-<instance>.<ms>.json` — the draft of a pane closed for good;
//! - `editor-<instance>.json` — an editor's unsaved buffer.
//!
//! Every write goes to a unique temporary sibling, is flushed, and is renamed
//! over its target, so a reader never sees half a file. On Unix the files are
//! readable by their owner only; elsewhere they take the data directory's
//! permissions, as `workspace.json` does. File contents never reach a log.

use anyhow::{Context, Result, bail};
use muxel_core::Uuid;
use muxel_core::rescue::{EditorBackup, MAX_EDITOR_BACKUP_BYTES, MAX_RESCUE_BYTES, PromptDraft};
use std::path::{Path, PathBuf};

/// How many closed panes' drafts are kept; older ones are deleted.
pub const MAX_CLOSED_DRAFTS: usize = 20;

/// Slack over a file's text cap for its JSON escaping and fields.
const ENVELOPE_BYTES: usize = 64 * 1024;

/// The marker a workspace keeps while it is open.
const RUNNING: &str = "running";

/// A saved draft and the file holding it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftOffer {
    pub path: PathBuf,
    pub instance: Uuid,
    pub draft: PromptDraft,
}

/// How much a workspace's rescue directory holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RescueCounts {
    /// Panes with an unsent draft (live or offered).
    pub drafts: usize,
    /// Editors with unsaved changes backed up.
    pub editors: usize,
    /// Drafts kept from closed panes.
    pub closed: usize,
}

/// One workspace's rescue directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RescueDir {
    dir: PathBuf,
}

/// The rescue directory of `workspace`, under the data directory.
pub fn rescue_dir(workspace: Uuid) -> Option<RescueDir> {
    let dir = crate::workspaces_dir()?
        .join(workspace.to_string())
        .join("rescue");
    Some(RescueDir::at(dir))
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Write `bytes` to `path` atomically, owner-only on Unix, refusing more
/// than `cap` bytes of text.
fn write_private(path: &Path, bytes: &[u8], cap: usize) -> Result<()> {
    use std::io::Write as _;
    if bytes.len() > cap + ENVELOPE_BYTES {
        bail!("rescue file too large ({} bytes)", bytes.len());
    }
    let parent = path.parent().context("rescue path has no directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating rescue dir {}", parent.display()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = parent.join(format!(".{name}.{}.tmp", Uuid::new_v4()));
    let written = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        crate::rename_with_retry(&tmp, path)
    })()
    .with_context(|| format!("writing {}", path.display()));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

fn read_draft(path: &Path) -> Option<PromptDraft> {
    let data = std::fs::read(path).ok()?;
    serde_json::from_slice(&data).ok()
}

fn remove(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log::warn!("couldn't delete rescue file {}: {e}", path.display()),
    }
}

/// Parse `<prefix><uuid><rest>` file names.
fn split_name<'a>(name: &'a str, prefix: &str) -> Option<(Uuid, &'a str)> {
    let rest = name.strip_prefix(prefix)?;
    let id = rest.get(..36)?;
    Some((Uuid::parse_str(id).ok()?, &rest[36..]))
}

/// The timestamp of a `.<ms>.offer.json` / `.<ms>.json` suffix.
fn suffix_ms(rest: &str, ending: &str) -> Option<i64> {
    rest.strip_prefix('.')?.strip_suffix(ending)?.parse().ok()
}

/// `<dir>/<prefix><ms><ending>` for the first `ms` from `at` that no file
/// uses yet, so two drafts saved in the same millisecond keep both.
fn unused(dir: &Path, prefix: &str, at: i64, ending: &str) -> PathBuf {
    let mut ms = at.max(0);
    loop {
        let path = dir.join(format!("{prefix}{ms}{ending}"));
        if !path.exists() {
            return path;
        }
        ms += 1;
    }
}

impl RescueDir {
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    fn live_draft(&self, instance: Uuid) -> PathBuf {
        self.dir.join(format!("draft-{instance}.json"))
    }

    fn editor_file(&self, instance: Uuid) -> PathBuf {
        self.dir.join(format!("editor-{instance}.json"))
    }

    fn names(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        entries
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .collect()
    }

    /// Replace a pane's live draft.
    pub fn save_draft(&self, instance: Uuid, draft: &PromptDraft) -> Result<()> {
        let bytes = serde_json::to_vec(draft).context("serializing draft")?;
        write_private(&self.live_draft(instance), &bytes, MAX_RESCUE_BYTES)
    }

    /// The pane's live draft, if it has one.
    pub fn draft(&self, instance: Uuid) -> Option<PromptDraft> {
        read_draft(&self.live_draft(instance))
    }

    /// Delete a pane's live draft (submitted, emptied, or saving turned off).
    pub fn delete_draft(&self, instance: Uuid) {
        remove(&self.live_draft(instance));
    }

    /// A pane is starting: turn its live draft into an offer, so the pane's
    /// new draft can't overwrite it, and return every offer it has, newest
    /// first. An unreadable draft is deleted rather than offered.
    pub fn offer_drafts(&self, instance: Uuid) -> Vec<DraftOffer> {
        let live = self.live_draft(instance);
        if let Some(draft) = read_draft(&live) {
            let offer = unused(
                &self.dir,
                &format!("draft-{instance}."),
                draft.saved_at_ms,
                ".offer.json",
            );
            if let Err(e) = crate::rename_with_retry(&live, &offer) {
                log::warn!("couldn't keep draft offer for {instance}: {e}");
            }
        } else {
            remove(&live);
        }
        self.draft_offers(instance)
    }

    /// The drafts offered to a pane, newest first.
    pub fn draft_offers(&self, instance: Uuid) -> Vec<DraftOffer> {
        let mut offers: Vec<DraftOffer> = self
            .names()
            .into_iter()
            .filter_map(|name| {
                let (id, rest) = split_name(&name, "draft-")?;
                suffix_ms(rest, ".offer.json")?;
                (id == instance).then_some(name)
            })
            .filter_map(|name| {
                let path = self.dir.join(name);
                let draft = read_draft(&path)?;
                Some(DraftOffer {
                    path,
                    instance,
                    draft,
                })
            })
            .collect();
        offers.sort_by_key(|offer| std::cmp::Reverse(offer.draft.saved_at_ms));
        offers
    }

    /// A pane is closed for good: keep its draft, labelled with the pane's
    /// title, as a closed draft. Offers it still had are kept the same way.
    /// Only the newest [`MAX_CLOSED_DRAFTS`] closed drafts are kept.
    pub fn keep_closed_drafts(&self, instance: Uuid, pane: &str) -> usize {
        let mut drafts: Vec<(PathBuf, PromptDraft)> = self
            .draft_offers(instance)
            .into_iter()
            .map(|offer| (offer.path, offer.draft))
            .collect();
        let live = self.live_draft(instance);
        if let Some(draft) = read_draft(&live) {
            drafts.push((live.clone(), draft));
        } else {
            remove(&live);
        }
        let mut kept = 0;
        for (from, mut draft) in drafts {
            draft.pane = (!pane.is_empty()).then(|| pane.to_string());
            let to = unused(
                &self.dir,
                &format!("closed-{instance}."),
                draft.saved_at_ms,
                ".json",
            );
            let saved = serde_json::to_vec(&draft)
                .context("serializing draft")
                .and_then(|bytes| write_private(&to, &bytes, MAX_RESCUE_BYTES));
            match saved {
                Ok(()) => {
                    kept += 1;
                    if from != to {
                        remove(&from);
                    }
                }
                Err(e) => log::warn!("couldn't keep closed draft for {instance}: {e:#}"),
            }
        }
        let closed = self.closed_drafts();
        for old in closed.iter().skip(MAX_CLOSED_DRAFTS) {
            remove(&old.path);
        }
        kept
    }

    /// Drafts kept from closed panes, newest first.
    pub fn closed_drafts(&self) -> Vec<DraftOffer> {
        let mut closed: Vec<DraftOffer> = self
            .names()
            .into_iter()
            .filter_map(|name| {
                let (instance, rest) = split_name(&name, "closed-")?;
                suffix_ms(rest, ".json")?;
                let path = self.dir.join(&name);
                let draft = read_draft(&path)?;
                Some(DraftOffer {
                    path,
                    instance,
                    draft,
                })
            })
            .collect();
        closed.sort_by_key(|offer| std::cmp::Reverse(offer.draft.saved_at_ms));
        closed
    }

    /// Delete one offered or closed draft (Restore, Copy or Discard). Only a
    /// file inside this directory is touched.
    pub fn delete_offer(&self, offer: &Path) {
        if offer.parent() == Some(self.dir.as_path()) {
            remove(offer);
        }
    }

    /// Replace an editor's backup.
    pub fn save_editor_backup(&self, instance: Uuid, backup: &EditorBackup) -> Result<()> {
        let bytes = serde_json::to_vec(backup).context("serializing editor backup")?;
        write_private(&self.editor_file(instance), &bytes, MAX_EDITOR_BACKUP_BYTES)
    }

    pub fn editor_backup(&self, instance: Uuid) -> Option<EditorBackup> {
        let data = std::fs::read(self.editor_file(instance)).ok()?;
        serde_json::from_slice(&data).ok()
    }

    /// Delete an editor's backup (saved, discarded, or closed without saving).
    pub fn delete_editor_backup(&self, instance: Uuid) {
        remove(&self.editor_file(instance));
    }

    /// Settle files whose pane no longer exists in the workspace (it closed
    /// while muxel was going down): a draft is kept as a closed draft, an
    /// editor backup is deleted, as closing an editor without saving does.
    pub fn adopt_orphans(&self, live: &std::collections::HashSet<Uuid>) {
        let mut orphans = std::collections::BTreeSet::new();
        for name in self.names() {
            if let Some((instance, _)) = split_name(&name, "draft-")
                && !live.contains(&instance)
            {
                orphans.insert(instance);
            } else if let Some((instance, ".json")) = split_name(&name, "editor-")
                && !live.contains(&instance)
            {
                self.delete_editor_backup(instance);
            }
        }
        for instance in orphans {
            self.keep_closed_drafts(instance, "");
        }
    }

    /// What the directory holds.
    pub fn counts(&self) -> RescueCounts {
        let mut drafts = std::collections::BTreeSet::new();
        let mut counts = RescueCounts::default();
        for name in self.names() {
            if let Some((instance, _)) = split_name(&name, "draft-") {
                drafts.insert(instance);
            } else if split_name(&name, "editor-").is_some_and(|(_, rest)| rest == ".json") {
                counts.editors += 1;
            } else if split_name(&name, "closed-").is_some() {
                counts.closed += 1;
            }
        }
        counts.drafts = drafts.len();
        counts
    }

    /// Delete everything kept, as turning saving off does.
    pub fn purge(&self) {
        for name in self.names() {
            if ["draft-", "closed-", "editor-", "."]
                .iter()
                .any(|prefix| name.starts_with(prefix))
            {
                remove(&self.dir.join(name));
            }
        }
    }
}

/// Mark `workspace` open. Returns whether the marker was already there: the
/// last run of this workspace ended without [`mark_workspace_stopped`], so it
/// crashed or was killed. Call it only while holding the workspace lock.
pub fn mark_workspace_running(workspace: Uuid) -> bool {
    match crate::workspaces_dir() {
        Some(dir) => mark_running_in(&dir.join(workspace.to_string())),
        None => false,
    }
}

/// Clear the marker on a clean quit or workspace switch.
pub fn mark_workspace_stopped(workspace: Uuid) {
    if let Some(dir) = crate::workspaces_dir() {
        remove(&dir.join(workspace.to_string()).join(RUNNING));
    }
}

fn mark_running_in(dir: &Path) -> bool {
    let marker = dir.join(RUNNING);
    let unclean = marker.exists();
    let stamp = format!("{} {}\n", std::process::id(), now_ms());
    if let Err(e) = write_private(&marker, stamp.as_bytes(), MAX_RESCUE_BYTES) {
        log::warn!("couldn't mark workspace running: {e:#}");
    }
    unclean
}

#[cfg(test)]
mod tests {
    use super::{MAX_CLOSED_DRAFTS, RescueCounts, RescueDir, mark_running_in};
    use muxel_core::Uuid;
    use muxel_core::rescue::{DraftSource, EditorBackup, MAX_RESCUE_BYTES, PromptDraft};

    fn scratch(name: &str) -> RescueDir {
        let dir = std::env::temp_dir().join(format!(
            "muxel-rescue-{name}-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        RescueDir::at(dir)
    }

    fn draft(text: &str, at: i64) -> PromptDraft {
        PromptDraft {
            text: text.into(),
            partial: false,
            saved_at_ms: at,
            source: DraftSource::Agent,
            pane: None,
        }
    }

    fn files(dir: &RescueDir) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok()?.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn a_draft_write_replaces_the_file_whole_and_leaves_no_temporary() {
        let dir = scratch("atomic");
        let pane = Uuid::new_v4();
        dir.save_draft(pane, &draft("first", 1)).unwrap();
        dir.save_draft(pane, &draft("second, longer", 2)).unwrap();
        assert_eq!(dir.draft(pane).unwrap().text, "second, longer");
        assert_eq!(files(&dir), vec![format!("draft-{pane}.json")]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.path().join(format!("draft-{pane}.json")))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(dir.path());
    }

    #[test]
    fn an_oversized_draft_is_refused_and_the_old_file_survives() {
        let dir = scratch("cap");
        let pane = Uuid::new_v4();
        dir.save_draft(pane, &draft("kept", 1)).unwrap();
        let huge = "x".repeat(MAX_RESCUE_BYTES + 128 * 1024);
        assert!(dir.save_draft(pane, &draft(&huge, 2)).is_err());
        assert_eq!(dir.draft(pane).unwrap().text, "kept");
        assert_eq!(files(&dir).len(), 1, "no temporary left behind");
        let _ = std::fs::remove_dir_all(dir.path());
    }

    #[test]
    fn a_starting_pane_gets_its_draft_as_an_offer_the_new_draft_cannot_overwrite() {
        let dir = scratch("offer");
        let pane = Uuid::new_v4();
        dir.save_draft(pane, &draft("before the crash", 100))
            .unwrap();
        let offers = dir.offer_drafts(pane);
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].draft.text, "before the crash");
        // The relaunched pane saves its own draft beside the offer.
        dir.save_draft(pane, &draft("typed after", 200)).unwrap();
        assert_eq!(dir.draft_offers(pane)[0].draft.text, "before the crash");
        // A second crash before deciding keeps both, newest first.
        let offers = dir.offer_drafts(pane);
        assert_eq!(
            offers
                .iter()
                .map(|o| o.draft.text.as_str())
                .collect::<Vec<_>>(),
            ["typed after", "before the crash"]
        );
        assert!(dir.draft(pane).is_none());
        dir.delete_offer(&offers[0].path);
        assert_eq!(dir.draft_offers(pane).len(), 1);
        // Another pane's offers are its own.
        assert!(dir.offer_drafts(Uuid::new_v4()).is_empty());
        let _ = std::fs::remove_dir_all(dir.path());
    }

    #[test]
    fn a_closed_pane_keeps_its_draft_labelled_and_the_list_is_bounded() {
        let dir = scratch("closed");
        let pane = Uuid::new_v4();
        dir.save_draft(pane, &draft("half a prompt", 5)).unwrap();
        assert_eq!(dir.keep_closed_drafts(pane, "Claude 2"), 1);
        let closed = dir.closed_drafts();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].draft.pane.as_deref(), Some("Claude 2"));
        assert_eq!(closed[0].instance, pane);
        assert!(dir.draft(pane).is_none());
        for i in 0..MAX_CLOSED_DRAFTS + 3 {
            let other = Uuid::new_v4();
            dir.save_draft(other, &draft("x", 10 + i as i64)).unwrap();
            dir.keep_closed_drafts(other, "pane");
        }
        let closed = dir.closed_drafts();
        assert_eq!(closed.len(), MAX_CLOSED_DRAFTS);
        assert!(closed.iter().all(|c| c.draft.saved_at_ms >= 10));
        let _ = std::fs::remove_dir_all(dir.path());
    }

    #[test]
    fn orphans_become_closed_drafts_and_counts_and_purge_cover_every_kind() {
        let dir = scratch("orphans");
        let (live, gone, editor, gone_editor) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        dir.save_draft(live, &draft("live", 1)).unwrap();
        dir.save_draft(gone, &draft("gone", 2)).unwrap();
        let backup = EditorBackup {
            path: None,
            text: "unsaved".into(),
            base: None,
            saved_at_ms: 3,
        };
        dir.save_editor_backup(editor, &backup).unwrap();
        dir.save_editor_backup(gone_editor, &backup).unwrap();
        dir.adopt_orphans(&[live, editor].into_iter().collect());
        assert_eq!(
            dir.counts(),
            RescueCounts {
                drafts: 1,
                editors: 1,
                closed: 1
            }
        );
        assert_eq!(dir.editor_backup(editor).unwrap(), backup);
        assert!(dir.editor_backup(gone_editor).is_none());
        dir.purge();
        assert_eq!(dir.counts(), RescueCounts::default());
        assert!(files(&dir).is_empty());
        let _ = std::fs::remove_dir_all(dir.path());
    }

    #[test]
    fn the_running_marker_reports_a_run_that_never_stopped() {
        let dir = scratch("running");
        let ws = dir.path().to_path_buf();
        assert!(!mark_running_in(&ws), "a first run is clean");
        assert!(mark_running_in(&ws), "the marker survived: unclean");
        std::fs::remove_file(ws.join("running")).unwrap();
        assert!(!mark_running_in(&ws));
        let _ = std::fs::remove_dir_all(ws);
    }
}
