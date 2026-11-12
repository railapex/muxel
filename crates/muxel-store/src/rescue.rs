//! Text a person typed but hadn't sent or saved, kept beside a workspace's
//! document so a crash, a restart or an accidental close doesn't lose it.
//!
//! Plain JSON files in `workspaces/<id>/rescue/`:
//!
//! - `draft-<instance>.json` — the live draft a pane keeps up to date;
//! - `draft-<instance>.<ms>.offer.json` — a draft offered when its pane
//!   started again, kept until Restore or Discard;
//! - `closed-<instance>.<ms>.json` — the draft of a pane closed for good,
//!   the newest [`MAX_CLOSED_DRAFTS`] kept;
//! - `editor-<instance>.json` — an editor's unsaved buffer, kept up to date;
//! - `editor-<instance>.offer.json` — the buffer offered when the editor
//!   opened again, kept until Restore or Discard.
//!
//! Each file is capped in size. Offers have no age or count limit: they stay
//! until someone decides, the pane is closed for good, or saving is turned
//! off. Every write goes to a unique temporary sibling, is flushed, and is
//! renamed over its target, so a reader never sees half a file; a temporary
//! left by a crash is deleted when the workspace opens again. On Unix the
//! files are readable by their owner only; elsewhere they take the data
//! directory's permissions, as `workspace.json` does. File contents never
//! reach a log.
//!
//! Turning saving off leaves a `rescue-off` marker in the data directory and
//! deletes every workspace's files. Deleting a workspace leaves a
//! `<id>.deleted` marker beside its directory before the directory goes.
//! Every write checks for both markers before it starts and again once it has
//! landed. A write that finds one afterwards (one already on its way, or one
//! from another muxel window or process) deletes what it wrote, and for a
//! deleted workspace the directory it recreated.

use anyhow::{Context, Result, bail};
use muxel_core::Uuid;
use muxel_core::rescue::{
    DraftSource, EditorBackup, MAX_EDITOR_BACKUP_BYTES, MAX_RESCUE_BYTES, PromptDraft,
};
use std::path::{Path, PathBuf};

/// How many closed panes' drafts are kept; older ones are deleted.
pub const MAX_CLOSED_DRAFTS: usize = 20;

/// Slack over a file's text cap for its JSON escaping and fields.
const ENVELOPE_BYTES: usize = 64 * 1024;

/// The marker a workspace keeps while it is open.
const RUNNING: &str = "running";

/// The marker in the data directory while saving is turned off.
const RESCUE_OFF: &str = "rescue-off";

/// The extension of the marker beside a deleted workspace's directory.
/// Workspace ids are never reused, so it stays.
const DELETED: &str = "deleted";

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
    /// What stops its writes, checked by every one.
    stops: Stops,
}

/// What stops a rescue write: saving turned off, or its workspace deleted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Stops {
    /// The data directory's [`RESCUE_OFF`] marker.
    off: Option<PathBuf>,
    /// The workspace's [`DELETED`] marker and the directory it stands for.
    deleted: Option<(PathBuf, PathBuf)>,
}

impl Stops {
    /// Refuse to start a write.
    fn check(&self) -> Result<()> {
        if self.off.as_deref().is_some_and(Path::exists) {
            bail!("saving unsent text is turned off");
        }
        if self
            .deleted
            .as_ref()
            .is_some_and(|(marker, _)| marker.exists())
        {
            bail!("the workspace was deleted");
        }
        Ok(())
    }

    /// A write landed at `target`: take it back if a stop appeared after it
    /// started, with the directory it may have recreated for a deleted
    /// workspace. This cleans up after the write; it doesn't stop it. What
    /// it can't delete stays until a workspace next opens, whose sweep (or
    /// purge, with saving off) tries again.
    fn undo(&self, target: &Path) -> Result<()> {
        if self.workspace_gone() {
            bail!("the workspace was deleted");
        }
        if self.off.as_deref().is_some_and(Path::exists) {
            remove(target);
            bail!("saving unsent text is turned off");
        }
        Ok(())
    }

    /// When the workspace was deleted, delete the directory a write may have
    /// recreated, and say so.
    fn workspace_gone(&self) -> bool {
        match &self.deleted {
            Some((marker, workspace)) if marker.exists() => {
                remove_tree(workspace);
                true
            }
            _ => false,
        }
    }
}

/// The marker left beside workspace directory `dir` once it was deleted.
fn deleted_marker(dir: &Path) -> PathBuf {
    dir.with_extension(DELETED)
}

/// Whether workspace `workspace`, under the workspaces directory
/// `workspaces`, was marked deleted.
pub(crate) fn marked_deleted_in(workspaces: &Path, workspace: Uuid) -> bool {
    deleted_marker(&workspaces.join(workspace.to_string())).exists()
}

pub(crate) fn remove_tree(dir: &Path) {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log::warn!("couldn't delete {}: {e}", dir.display()),
    }
}

/// The rescue directory of `workspace`, under the data directory.
pub fn rescue_dir(workspace: Uuid) -> Option<RescueDir> {
    Some(rescue_dir_in(
        &crate::workspaces_dir()?,
        crate::data_dir().as_deref(),
        workspace,
    ))
}

/// [`rescue_dir`] under `workspaces`, with the marker of `data` (if any).
fn rescue_dir_in(workspaces: &Path, data: Option<&Path>, workspace: Uuid) -> RescueDir {
    let dir = workspaces.join(workspace.to_string());
    RescueDir {
        stops: Stops {
            off: data.map(|data| data.join(RESCUE_OFF)),
            deleted: Some((deleted_marker(&dir), dir.clone())),
        },
        dir: dir.join("rescue"),
    }
}

/// Whether saving was turned off, in this window or another on the same data.
pub fn rescue_turned_off() -> bool {
    crate::data_dir().is_some_and(|data| data.join(RESCUE_OFF).exists())
}

/// Turn saving off or on for every muxel window on this data directory. Off
/// writes the marker first, so a write racing the purge that follows deletes
/// itself.
pub fn set_rescue_turned_off(off: bool) -> Result<()> {
    let marker = crate::data_dir()
        .context("could not determine data directory")?
        .join(RESCUE_OFF);
    if off {
        write_private(&marker, b"", 0, &Stops::default())
    } else {
        match std::fs::remove_file(&marker) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("deleting {}", marker.display()))
            }
            _ => Ok(()),
        }
    }
}

/// Whether saving is on for a window starting with `config_on` in its
/// settings. The marker decides, for every window on the data directory. A
/// config that says off without the marker (edited by hand, or saved before
/// the marker existed) writes it, so saving stays off everywhere.
pub fn settle_rescue_setting(config_on: bool) -> bool {
    crate::data_dir().is_some_and(|data| settle_rescue_setting_in(&data, config_on))
}

fn settle_rescue_setting_in(data: &Path, config_on: bool) -> bool {
    let marker = data.join(RESCUE_OFF);
    if marker.exists() {
        return false;
    }
    if !config_on {
        if let Err(e) = write_private(&marker, b"", 0, &Stops::default()) {
            log::warn!("couldn't mark saving unsent text off: {e:#}");
        }
        return false;
    }
    true
}

/// Delete what every workspace kept, as turning saving off does. Returns how
/// many workspaces had something to delete. A directory that couldn't be
/// listed and a file that couldn't be deleted are reported; only a data
/// directory with no workspaces yet holds nothing.
pub fn purge_every_workspace() -> Result<usize> {
    let root = crate::workspaces_dir().context("could not determine data directory")?;
    purge_every_workspace_in(&root)
}

fn purge_every_workspace_in(root: &Path) -> Result<usize> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).with_context(|| format!("listing {}", root.display())),
    };
    let mut purged = 0;
    let mut failed = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                failed.push(format!("{}: {e}", root.display()));
                continue;
            }
        };
        let workspace = entry
            .file_name()
            .to_str()
            .is_some_and(|name| Uuid::parse_str(name).is_ok());
        if !workspace {
            continue;
        }
        let dir = RescueDir::at(entry.path().join("rescue"));
        match dir.try_names() {
            Ok(names) if names.is_empty() => {
                let _ = std::fs::remove_dir(dir.path());
                continue;
            }
            Ok(_) => {}
            Err(e) => {
                failed.push(format!("{}: {e}", dir.path().display()));
                continue;
            }
        }
        purged += 1;
        failed.extend(dir.purge());
        let _ = std::fs::remove_dir(dir.path());
    }
    if !failed.is_empty() {
        bail!(
            "couldn't delete everything kept ({} failure(s)): {}",
            failed.len(),
            failed
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    Ok(purged)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// A file written in full beside its target, not yet in place. Dropping it
/// without [`StagedWrite::commit`] deletes it.
#[derive(Debug)]
pub struct StagedWrite {
    tmp: PathBuf,
    target: PathBuf,
    stops: Stops,
}

impl StagedWrite {
    /// Rename the file over its target. If saving was turned off or the
    /// workspace deleted after the write started, what landed is deleted
    /// again and this fails.
    pub fn commit(self) -> Result<()> {
        crate::rename_with_retry(&self.tmp, &self.target)
            .with_context(|| format!("writing {}", self.target.display()))?;
        self.stops.undo(&self.target)
    }
}

impl Drop for StagedWrite {
    fn drop(&mut self) {
        // Gone after a commit; otherwise this write was abandoned, and for a
        // deleted workspace the directory it recreated goes too.
        let _ = std::fs::remove_file(&self.tmp);
        self.stops.workspace_gone();
    }
}

/// Write `bytes` beside `path`, owner-only on Unix, refusing more than `cap`
/// bytes of text, or anything once one of `stops` exists.
fn stage_private(path: &Path, bytes: &[u8], cap: usize, stops: &Stops) -> Result<StagedWrite> {
    use std::io::Write as _;
    stops.check()?;
    #[cfg(test)]
    tests::after_check();
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
    let staged = StagedWrite {
        tmp: parent.join(format!(".{name}.{}.tmp", Uuid::new_v4())),
        target: path.to_path_buf(),
        stops: stops.clone(),
    };
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    (|| -> std::io::Result<()> {
        let mut file = options.open(&staged.tmp)?;
        file.write_all(bytes)?;
        file.sync_all()
    })()
    .with_context(|| format!("writing {}", path.display()))?;
    Ok(staged)
}

/// Write `bytes` to `path` atomically; see [`stage_private`].
fn write_private(path: &Path, bytes: &[u8], cap: usize, stops: &Stops) -> Result<()> {
    stage_private(path, bytes, cap, stops)?.commit()
}

/// Read a saved draft. A shell's prompt line, which earlier builds saved, is
/// deleted instead of read.
fn read_draft(path: &Path) -> Option<PromptDraft> {
    let data = std::fs::read(path).ok()?;
    let draft: PromptDraft = serde_json::from_slice(&data).ok()?;
    if draft.source == DraftSource::Shell {
        remove(path);
        return None;
    }
    Some(draft)
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
        Self {
            dir: dir.into(),
            stops: Stops::default(),
        }
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

    fn editor_offer_file(&self, instance: Uuid) -> PathBuf {
        self.dir.join(format!("editor-{instance}.offer.json"))
    }

    /// The directory's file names, for reading: one that can't be listed
    /// reads as empty.
    fn names(&self) -> Vec<String> {
        self.try_names().unwrap_or_default()
    }

    /// The directory's file names. Only a missing directory is empty.
    fn try_names(&self) -> std::io::Result<Vec<String>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut names = Vec::new();
        for entry in entries {
            if let Ok(name) = entry?.file_name().into_string() {
                names.push(name);
            }
        }
        Ok(names)
    }

    /// Replace a pane's live draft.
    pub fn save_draft(&self, instance: Uuid, draft: &PromptDraft) -> Result<()> {
        let bytes = serde_json::to_vec(draft).context("serializing draft")?;
        write_private(
            &self.live_draft(instance),
            &bytes,
            MAX_RESCUE_BYTES,
            &self.stops,
        )
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
                .and_then(|bytes| write_private(&to, &bytes, MAX_RESCUE_BYTES, &self.stops));
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

    /// Write an editor's backup beside its file, to be put in place with
    /// [`StagedWrite::commit`] once the caller knows it's still wanted.
    pub fn stage_editor_backup(
        &self,
        instance: Uuid,
        backup: &EditorBackup,
    ) -> Result<StagedWrite> {
        let bytes = serde_json::to_vec(backup).context("serializing editor backup")?;
        stage_private(
            &self.editor_file(instance),
            &bytes,
            MAX_EDITOR_BACKUP_BYTES,
            &self.stops,
        )
    }

    /// Replace an editor's backup.
    pub fn save_editor_backup(&self, instance: Uuid, backup: &EditorBackup) -> Result<()> {
        self.stage_editor_backup(instance, backup)?.commit()
    }

    /// The editor's live backup.
    pub fn editor_backup(&self, instance: Uuid) -> Option<EditorBackup> {
        read_backup(&self.editor_file(instance))
    }

    /// The editor is opening: turn its live backup into its offer, so the
    /// backups of edits made while the offer shows can't replace it, and
    /// return the offer. A newer live backup replaces an older offer: it holds
    /// the edits made last.
    pub fn offer_editor_backup(&self, instance: Uuid) -> Option<EditorBackup> {
        let live = self.editor_file(instance);
        let offer = self.editor_offer_file(instance);
        if read_backup(&live).is_some() {
            if let Err(e) = crate::rename_with_retry(&live, &offer) {
                log::warn!("couldn't keep editor backup offer for {instance}: {e}");
            }
        } else {
            remove(&live);
        }
        self.editor_offer(instance)
    }

    /// The backup the editor is offering.
    pub fn editor_offer(&self, instance: Uuid) -> Option<EditorBackup> {
        read_backup(&self.editor_offer_file(instance))
    }

    /// Delete the editor's live backup (saved, reloaded, or no longer dirty).
    pub fn delete_editor_backup(&self, instance: Uuid) {
        remove(&self.editor_file(instance));
    }

    /// Delete the backup the editor offered (restored or discarded).
    pub fn delete_editor_offer(&self, instance: Uuid) {
        remove(&self.editor_offer_file(instance));
    }

    /// Delete both of an editor's files (closed for good).
    pub fn drop_editor(&self, instance: Uuid) {
        self.delete_editor_backup(instance);
        self.delete_editor_offer(instance);
    }

    /// Settle files whose pane no longer exists in the workspace (it closed
    /// while muxel was going down): a draft is kept as a closed draft, an
    /// editor's files are deleted, as closing an editor without saving does.
    pub fn adopt_orphans(&self, live: &std::collections::HashSet<Uuid>) {
        let mut orphans = std::collections::BTreeSet::new();
        for name in self.names() {
            if let Some((instance, _)) = split_name(&name, "draft-")
                && !live.contains(&instance)
            {
                orphans.insert(instance);
            } else if let Some((instance, _)) = split_name(&name, "editor-")
                && !live.contains(&instance)
            {
                self.drop_editor(instance);
            }
        }
        for instance in orphans {
            self.keep_closed_drafts(instance, "");
        }
    }

    /// Delete the temporaries of writes a crash cut off. Call it only while
    /// holding the workspace lock and before any of its panes start writing.
    pub fn remove_temporaries(&self) -> usize {
        let temporaries: Vec<String> = self
            .names()
            .into_iter()
            .filter(|name| name.starts_with('.') && name.ends_with(".tmp"))
            .collect();
        for name in &temporaries {
            remove(&self.dir.join(name));
        }
        temporaries.len()
    }

    /// What the directory holds.
    pub fn counts(&self) -> RescueCounts {
        let mut drafts = std::collections::BTreeSet::new();
        let mut editors = std::collections::BTreeSet::new();
        let mut counts = RescueCounts::default();
        for name in self.names() {
            if let Some((instance, _)) = split_name(&name, "draft-") {
                drafts.insert(instance);
            } else if let Some((instance, _)) = split_name(&name, "editor-") {
                editors.insert(instance);
            } else if split_name(&name, "closed-").is_some() {
                counts.closed += 1;
            }
        }
        counts.drafts = drafts.len();
        counts.editors = editors.len();
        counts
    }

    /// Delete everything kept, as turning saving off does. Returns what
    /// couldn't be listed or deleted.
    pub fn purge(&self) -> Vec<String> {
        let names = match self.try_names() {
            Ok(names) => names,
            Err(e) => return vec![format!("{}: {e}", self.dir.display())],
        };
        let mut failed = Vec::new();
        for name in names {
            if ["draft-", "closed-", "editor-", "."]
                .iter()
                .any(|prefix| name.starts_with(prefix))
            {
                let path = self.dir.join(name);
                match std::fs::remove_file(&path) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                        log::warn!("couldn't delete rescue file {}: {e}", path.display());
                        failed.push(format!("{}: {e}", path.display()));
                    }
                    _ => {}
                }
            }
        }
        failed
    }
}

fn read_backup(path: &Path) -> Option<EditorBackup> {
    let data = std::fs::read(path).ok()?;
    serde_json::from_slice(&data).ok()
}

/// A workspace held for deleting: its lock is taken before anything about
/// it changes, so one open in another muxel window stays as it is.
#[derive(Debug)]
pub struct WorkspaceDeletion {
    dir: PathBuf,
    lock: std::fs::File,
}

/// Hold workspace `workspace` for deleting. `held` is the lock this window
/// already holds on it (its current workspace); otherwise the lock is taken
/// here, and refused while another muxel window has the workspace open.
/// Even a workspace never opened is locked: a window opening it at the same
/// moment then either finds it marked or keeps the deletion from starting.
pub fn hold_workspace_for_deletion(
    workspace: Uuid,
    held: Option<std::fs::File>,
) -> Result<WorkspaceDeletion> {
    let root = crate::workspaces_dir().context("could not determine data directory")?;
    hold_in(&root, workspace, held)
}

fn hold_in(root: &Path, workspace: Uuid, held: Option<std::fs::File>) -> Result<WorkspaceDeletion> {
    let dir = root.join(workspace.to_string());
    let lock = match held {
        Some(lock) => lock,
        None => crate::try_lock_dir(&dir).context("it is open in another muxel window")?,
    };
    Ok(WorkspaceDeletion { dir, lock })
}

impl WorkspaceDeletion {
    /// Record that the workspace is being deleted, before anything else about
    /// it changes. From here on muxel never lists it when it starts, a
    /// window that still lists it can't open it, a rescue write still on its
    /// way deletes what it lands, and the sweep when a workspace next opens
    /// deletes whatever is left, so a crash before [`Self::delete`] finishes
    /// strands nothing.
    pub fn mark(&self) -> Result<()> {
        write_private(&deleted_marker(&self.dir), b"", 0, &Stops::default())
            .context("marking the workspace deleted")
    }

    /// Give back the lock this held, deleting nothing: marking it failed.
    pub fn release(self) -> std::fs::File {
        self.lock
    }

    /// Delete the workspace's directory: its document, everything it kept,
    /// and its lock file. It is marked first (see [`Self::mark`]). What
    /// couldn't be deleted is reported.
    pub fn delete(self) -> Result<()> {
        self.mark()?;
        let Self { dir, lock } = self;
        let mut failed = Vec::new();
        match std::fs::read_dir(&dir) {
            Ok(entries) => {
                for entry in entries {
                    let path = match entry {
                        Ok(entry) if entry.file_name() == "instance.lock" => continue,
                        Ok(entry) => entry.path(),
                        Err(e) => {
                            failed.push(format!("{}: {e}", dir.display()));
                            continue;
                        }
                    };
                    let removed = if path.is_dir() {
                        std::fs::remove_dir_all(&path)
                    } else {
                        std::fs::remove_file(&path)
                    };
                    if let Err(e) = removed
                        && e.kind() != std::io::ErrorKind::NotFound
                    {
                        failed.push(format!("{}: {e}", path.display()));
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => failed.push(format!("{}: {e}", dir.display())),
        }
        drop(lock);
        let _ = std::fs::remove_file(dir.join("instance.lock"));
        // A write racing this one can recreate the directory; it deletes it
        // again when it lands, and this tries once more.
        let removed = std::fs::remove_dir(&dir).or_else(|_| std::fs::remove_dir_all(&dir));
        if let Err(e) = removed
            && e.kind() != std::io::ErrorKind::NotFound
        {
            failed.push(format!("{}: {e}", dir.display()));
        }
        if !failed.is_empty() {
            bail!("couldn't delete {}", failed.join("; "));
        }
        Ok(())
    }
}

/// Delete workspace `workspace`'s directory, refused while another muxel
/// window has it open. See [`WorkspaceDeletion::delete`].
pub fn delete_workspace_dir(workspace: Uuid) -> Result<()> {
    hold_workspace_for_deletion(workspace, None)?.delete()
}

#[cfg(test)]
fn delete_workspace_dir_in(root: &Path, workspace: Uuid) -> Result<()> {
    hold_in(root, workspace, None)?.delete()
}

/// Delete what is left of deleted workspaces, each time a workspace opens: a
/// crash during a deletion, a write cut off between landing and checking for
/// the marker, or a deletion that failed leaves a directory beside its
/// marker. Until then it stays on disk; this is cleanup after the fact, not
/// a barrier that stops the write.
pub fn sweep_deleted_workspaces() {
    let Some(root) = crate::workspaces_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == DELETED) {
            remove_tree(&path.with_extension(""));
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
    if let Err(e) = write_private(
        &marker,
        stamp.as_bytes(),
        MAX_RESCUE_BYTES,
        &Stops::default(),
    ) {
        log::warn!("couldn't mark workspace running: {e:#}");
    }
    unclean
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_CLOSED_DRAFTS, RESCUE_OFF, RescueCounts, RescueDir, delete_workspace_dir_in,
        mark_running_in, purge_every_workspace_in, rescue_dir_in, settle_rescue_setting_in,
    };
    use muxel_core::Uuid;
    use muxel_core::rescue::{DraftSource, EditorBackup, MAX_RESCUE_BYTES, PromptDraft};
    use std::cell::RefCell;

    thread_local! {
        /// Run once by this thread's next write, just after it checked
        /// whether it may write: what a writer paused there would meet.
        static AFTER_CHECK: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    }

    pub(super) fn after_check() {
        if let Some(run) = AFTER_CHECK.with(|hook| hook.borrow_mut().take()) {
            run();
        }
    }

    fn pause_next_write_after_its_check(run: impl FnOnce() + 'static) {
        AFTER_CHECK.with(|hook| *hook.borrow_mut() = Some(Box::new(run)));
    }

    fn scratch_root(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "muxel-rescue-{name}-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ))
    }

    #[test]
    fn a_write_paused_past_its_check_never_brings_a_deleted_workspace_back() {
        let root = scratch_root("deleted");
        let workspaces = root.join("workspaces");
        let workspace = Uuid::new_v4();
        let dir = rescue_dir_in(&workspaces, Some(&root), workspace);
        let pane = Uuid::new_v4();
        dir.save_draft(pane, &draft("typed before the delete", 1))
            .unwrap();
        let deleted = workspaces.join(workspace.to_string());
        assert!(deleted.exists());

        // The next draft write has checked it may write when the workspace is
        // deleted; then it goes on.
        let root_for_delete = workspaces.clone();
        pause_next_write_after_its_check(move || {
            delete_workspace_dir_in(&root_for_delete, workspace).unwrap();
        });
        let _ = dir.save_draft(pane, &draft("landing after the delete", 2));
        assert!(
            !deleted.exists(),
            "the paused write left {:?}",
            std::fs::read_dir(&deleted)
                .map(|entries| entries.flatten().map(|e| e.file_name()).collect::<Vec<_>>())
        );
        // A write that starts afterwards leaves nothing either.
        let _ = dir.save_draft(pane, &draft("written later", 3));
        assert!(!deleted.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_rescue_directory_that_cant_be_listed_is_reported_not_taken_for_empty() {
        let root = scratch_root("unlisted");
        std::fs::create_dir_all(&root).unwrap();
        let workspaces = root.join("workspaces");
        // The workspaces directory itself can't be listed.
        std::fs::write(&workspaces, b"not a directory").unwrap();
        assert!(purge_every_workspace_in(&workspaces).is_err());
        std::fs::remove_file(&workspaces).unwrap();
        // One workspace's rescue directory can't be listed.
        let workspace = workspaces.join(Uuid::new_v4().to_string());
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("rescue"), b"not a directory").unwrap();
        assert!(purge_every_workspace_in(&workspaces).is_err());
        // No workspaces yet: nothing to delete, and that's not a failure.
        assert_eq!(purge_every_workspace_in(&root.join("missing")).unwrap(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

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
    fn the_marker_decides_whether_saving_is_on_and_a_config_saying_off_adds_it() {
        let data = scratch_root("settle");
        std::fs::create_dir_all(&data).unwrap();
        let marker = data.join(RESCUE_OFF);
        assert!(settle_rescue_setting_in(&data, true));
        assert!(!marker.exists());
        // Off in the config with no marker: off for every window from now on.
        assert!(!settle_rescue_setting_in(&data, false));
        assert!(marker.exists());
        // The marker wins over a config that says on.
        assert!(!settle_rescue_setting_in(&data, true));
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn a_shell_line_an_earlier_build_saved_is_deleted_when_read() {
        let dir = scratch("shell");
        let pane = Uuid::new_v4();
        let line = PromptDraft {
            source: DraftSource::Shell,
            ..draft("$env:SOME_KEY=\"x\"", 5)
        };
        dir.save_draft(pane, &line).unwrap();
        std::fs::write(
            dir.path().join(format!("closed-{pane}.5.json")),
            serde_json::to_vec(&line).unwrap(),
        )
        .unwrap();
        assert!(dir.draft(pane).is_none());
        assert!(dir.closed_drafts().is_empty());
        assert!(files(&dir).is_empty(), "left {:?}", files(&dir));
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
        assert!(dir.purge().is_empty());
        assert_eq!(dir.counts(), RescueCounts::default());
        assert!(files(&dir).is_empty());
        let _ = std::fs::remove_dir_all(dir.path());
    }

    fn backup(text: &str) -> EditorBackup {
        EditorBackup {
            path: None,
            text: text.into(),
            base: None,
            saved_at_ms: 1,
        }
    }

    #[test]
    fn a_staged_write_lands_only_when_committed() {
        let dir = scratch("staged");
        let pane = Uuid::new_v4();
        dir.save_editor_backup(pane, &backup("kept")).unwrap();
        // Abandoned: the old file stays and no temporary is left.
        drop(dir.stage_editor_backup(pane, &backup("abandoned")).unwrap());
        assert_eq!(dir.editor_backup(pane).unwrap().text, "kept");
        assert_eq!(files(&dir), vec![format!("editor-{pane}.json")]);
        let staged = dir.stage_editor_backup(pane, &backup("newer")).unwrap();
        assert_eq!(dir.editor_backup(pane).unwrap().text, "kept");
        staged.commit().unwrap();
        assert_eq!(dir.editor_backup(pane).unwrap().text, "newer");
        assert_eq!(files(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(dir.path());
    }

    #[test]
    fn once_saving_is_off_a_write_leaves_nothing_even_one_already_staged() {
        let dir = scratch("off");
        let marker = dir.path().with_extension("off");
        let dir = RescueDir {
            stops: super::Stops {
                off: Some(marker.clone()),
                deleted: None,
            },
            ..dir
        };
        let pane = Uuid::new_v4();
        let staged = dir.stage_editor_backup(pane, &backup("racing")).unwrap();
        std::fs::write(&marker, b"").unwrap();
        assert!(staged.commit().is_err());
        assert!(dir.save_draft(pane, &draft("after", 2)).is_err());
        assert!(files(&dir).is_empty(), "{:?}", files(&dir));
        let _ = std::fs::remove_file(&marker);
        let _ = std::fs::remove_dir_all(dir.path());
    }

    #[test]
    fn an_opening_editor_offers_its_backup_and_new_backups_go_beside_it() {
        let dir = scratch("editor-offer");
        let pane = Uuid::new_v4();
        dir.save_editor_backup(pane, &backup("before the crash"))
            .unwrap();
        assert_eq!(
            dir.offer_editor_backup(pane).unwrap().text,
            "before the crash"
        );
        assert!(dir.editor_backup(pane).is_none());
        dir.save_editor_backup(pane, &backup("typed while offered"))
            .unwrap();
        assert_eq!(dir.editor_offer(pane).unwrap().text, "before the crash");
        assert_eq!(dir.counts().editors, 1);
        // Discarding the offer keeps the newer backup.
        dir.delete_editor_offer(pane);
        assert_eq!(dir.editor_backup(pane).unwrap().text, "typed while offered");
        // Opening again offers the newest.
        assert_eq!(
            dir.offer_editor_backup(pane).unwrap().text,
            "typed while offered"
        );
        dir.drop_editor(pane);
        assert!(files(&dir).is_empty());
        let _ = std::fs::remove_dir_all(dir.path());
    }

    #[test]
    fn a_crash_between_write_and_rename_leaves_a_temporary_that_opening_deletes() {
        let dir = scratch("temporaries");
        std::fs::create_dir_all(dir.path()).unwrap();
        let pane = Uuid::new_v4();
        dir.save_draft(pane, &draft("whole", 1)).unwrap();
        let cut_off = dir
            .path()
            .join(format!(".draft-{pane}.json.{}.tmp", Uuid::new_v4()));
        std::fs::write(&cut_off, b"{\"text\":\"half").unwrap();
        assert_eq!(dir.remove_temporaries(), 1);
        assert_eq!(files(&dir), vec![format!("draft-{pane}.json")]);
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
