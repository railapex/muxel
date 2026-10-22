//! Recurring provider reads and process enumeration run on workers. The UI
//! claims matching validated snapshots and acknowledges only after persistence.

#[cfg(windows)]
use super::{
    GrokRegistryRead, active_grok_session_for_process, grok_switch_interval, process_parent_map,
    read_grok_active_sessions,
};
use super::{
    MuxelApp, NotifKind, home_dir, is_claude_program, is_codex_program, is_grok_program,
    launch_token_current, session_id_bound_elsewhere, validated_codex_session_id,
};
use crate::{session_binding, ui_profile};
use gpui::{App, Context};
use muxel_core::session_discovery::{
    CodexLookupEvidence, SessionDiscoveryKey, SessionDiscoveryRequest, SessionProvider,
};
use muxel_core::tmux::Socket;
use muxel_core::{AgentPreset, CodexHintMiss, CodexResumePath, CodexSession};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};
use uuid::Uuid;

// Explicit Import may search history. Restoration must never call this path.
pub(super) fn capture_codex_session_id(home: &Path, cwd: &Path) -> Option<String> {
    #[cfg(all(test, windows))]
    let injected = CODEX_LAUNCH_TEST_LOOKUP.lock().unwrap().clone();
    #[cfg(all(test, windows))]
    if let Some(discover) = injected {
        return discover(home, cwd);
    }
    muxel_core::codex_latest_session_id(home, cwd)
}

pub(super) fn resume_codex_session(
    home: &Path,
    id: &str,
    path: Option<&Path>,
) -> std::io::Result<muxel_core::CodexResumePath> {
    #[cfg(all(test, windows))]
    let injected = CODEX_RESUME_TEST_LOOKUP.lock().unwrap().clone();
    #[cfg(all(test, windows))]
    if let Some(discover) = injected {
        return discover(home, id, path);
    }
    muxel_core::codex_resume_path(home, id, path)
}

#[derive(Clone)]
pub(super) struct CodexLaunchPreflight {
    home: PathBuf,
    cwd: PathBuf,
    id: Option<String>,
}

pub(super) enum CodexLaunchResult {
    Found(CodexSession),
    Missing,
    Unknown,
}

/// Only a started local Codex pane without an exact path needs this one-time
/// recovery scan. The request copies its identity before the worker starts.
impl MuxelApp {
    pub(super) fn codex_launch_preflight_request(&self, iid: Uuid) -> Option<CodexLaunchPreflight> {
        let instance = self.workspace.instance(iid)?;
        let project = self.workspace.project(instance.project_id)?;
        if !(project.remote.is_none()
            && instance.session_started
            && instance.session_path.is_none()
            && is_codex_program(self.resume_program(iid)))
        {
            return None;
        }
        Some(CodexLaunchPreflight {
            home: home_dir()?,
            cwd: instance
                .worktree_path
                .clone()
                .unwrap_or_else(|| project.root_path.clone()),
            id: instance.session_id.clone(),
        })
    }

    pub(super) fn apply_codex_launch_preflight(
        &mut self,
        iid: Uuid,
        token: u64,
        request: &CodexLaunchPreflight,
        result: CodexLaunchResult,
    ) -> bool {
        if !launch_token_current(&self.terminal_launching, iid, token) {
            return false;
        }
        let Some(instance) = self.workspace.instance(iid) else {
            return false;
        };
        let Some(project) = self.workspace.project(instance.project_id) else {
            return false;
        };
        if !instance.session_started
            || instance.session_id != request.id
            || instance.session_path.is_some()
            || project.remote.is_some()
            || instance
                .worktree_path
                .as_ref()
                .unwrap_or(&project.root_path)
                != &request.cwd
            || !is_codex_program(self.resume_program(iid))
        {
            return false;
        }
        let idless = request.id.is_none();
        let missing = matches!(result, CodexLaunchResult::Missing);
        let found = match result {
            CodexLaunchResult::Found(session)
                if request.id.as_deref().is_none_or(|id| id == session.id)
                    && (!idless
                        || !session_id_bound_elsewhere(
                            &self.workspace.instances,
                            iid,
                            &session.id,
                        )) =>
            {
                Some(session)
            }
            _ => None,
        };
        let mut changed = false;
        if let Some(session) = found {
            let instance = self.workspace.instance_mut(iid).unwrap();
            instance.session_id = Some(session.id);
            instance.session_path = Some(session.path);
            changed = true;
        } else if idless || missing {
            let instance = self.workspace.instance_mut(iid).unwrap();
            instance.session_id = None;
            instance.session_path = None;
            instance.session_started = false;
            changed = true;
            if idless {
                self.add_event(
                    NotifKind::Success,
                    crate::i18n::t("Codex started fresh"),
                    crate::i18n::t("Use Import to find an earlier conversation.").to_string(),
                );
            }
        }
        if changed {
            self.bump_session_binding_generation(iid);
            self.persist();
        }
        true
    }
}

pub(super) fn codex_launch_preflight(request: &CodexLaunchPreflight) -> CodexLaunchResult {
    if let Some(id) = request.id.as_deref() {
        match resume_codex_session(&request.home, id, None) {
            Ok(CodexResumePath::Found(path)) => {
                return CodexLaunchResult::Found(CodexSession {
                    id: id.into(),
                    path,
                });
            }
            Ok(CodexResumePath::Missing) => return CodexLaunchResult::Missing,
            Ok(CodexResumePath::Unknown) => {}
            Err(error) => {
                log::warn!("could not check Codex rollout before resume: {error}");
                return CodexLaunchResult::Unknown;
            }
        }
    }
    #[cfg(all(test, windows))]
    let injected = CODEX_RESTORE_TEST_SCAN.lock().unwrap().clone();
    #[cfg(all(test, windows))]
    let scanned = if let Some(scan) = injected {
        scan(&request.home, &request.cwd, request.id.as_deref())
    } else {
        muxel_core::codex_restore_scan(&request.home, &request.cwd, request.id.as_deref())
    };
    #[cfg(not(all(test, windows)))]
    let scanned =
        muxel_core::codex_restore_scan(&request.home, &request.cwd, request.id.as_deref());
    match scanned {
        Ok(Some(session)) => CodexLaunchResult::Found(session),
        Ok(None) => CodexLaunchResult::Missing,
        Err(error) => {
            log::warn!("could not complete Codex rollout recovery scan: {error}");
            CodexLaunchResult::Unknown
        }
    }
}

#[cfg(all(test, windows))]
type CodexRestoreTestScan = std::sync::Arc<
    dyn Fn(&Path, &Path, Option<&str>) -> std::io::Result<Option<CodexSession>> + Send + Sync,
>;
#[cfg(all(test, windows))]
static CODEX_RESTORE_TEST_SCAN: std::sync::Mutex<Option<CodexRestoreTestScan>> =
    std::sync::Mutex::new(None);
#[cfg(all(test, windows))]
pub(super) fn inject_codex_restore_scan(scan: CodexRestoreTestScan) -> CodexLaunchTestGuard {
    *CODEX_RESTORE_TEST_SCAN.lock().unwrap() = Some(scan);
    CodexLaunchTestGuard
}

#[cfg(all(test, windows))]
type CodexResumeTestLookup = std::sync::Arc<
    dyn Fn(&Path, &str, Option<&Path>) -> std::io::Result<muxel_core::CodexResumePath>
        + Send
        + Sync,
>;
#[cfg(all(test, windows))]
static CODEX_RESUME_TEST_LOOKUP: std::sync::Mutex<Option<CodexResumeTestLookup>> =
    std::sync::Mutex::new(None);
#[cfg(all(test, windows))]
pub(super) fn inject_codex_resume_lookup(discover: CodexResumeTestLookup) -> CodexLaunchTestGuard {
    *CODEX_RESUME_TEST_LOOKUP.lock().unwrap() = Some(discover);
    CodexLaunchTestGuard
}

#[cfg(all(test, windows))]
type CodexLaunchTestLookup = std::sync::Arc<dyn Fn(&Path, &Path) -> Option<String> + Send + Sync>;
#[cfg(all(test, windows))]
static CODEX_LAUNCH_TEST_LOOKUP: std::sync::Mutex<Option<CodexLaunchTestLookup>> =
    std::sync::Mutex::new(None);

#[cfg(all(test, windows))]
pub(super) struct CodexLaunchTestGuard;
#[cfg(all(test, windows))]
impl Drop for CodexLaunchTestGuard {
    fn drop(&mut self) {
        *CODEX_LAUNCH_TEST_LOOKUP.lock().unwrap() = None;
        *CODEX_RESUME_TEST_LOOKUP.lock().unwrap() = None;
        *CODEX_RESTORE_TEST_SCAN.lock().unwrap() = None;
    }
}
#[cfg(all(test, windows))]
pub(super) fn inject_codex_launch_lookup(discover: CodexLaunchTestLookup) -> CodexLaunchTestGuard {
    *CODEX_LAUNCH_TEST_LOOKUP.lock().unwrap() = Some(discover);
    CodexLaunchTestGuard
}

#[cfg(test)]
type CodexTmuxTitleTestReader =
    std::sync::Arc<dyn Fn(&str, Socket) -> Option<String> + Send + Sync>;
#[cfg(test)]
static CODEX_TMUX_TITLE_TEST_READER: std::sync::Mutex<Option<CodexTmuxTitleTestReader>> =
    std::sync::Mutex::new(None);
#[cfg(test)]
static CODEX_TMUX_TITLE_FIXTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
struct CodexTmuxTitleTestGuard {
    _fixture_lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for CodexTmuxTitleTestGuard {
    fn drop(&mut self) {
        *CODEX_TMUX_TITLE_TEST_READER.lock().unwrap() = None;
    }
}

#[cfg(test)]
fn inject_codex_tmux_title_reader(reader: CodexTmuxTitleTestReader) -> CodexTmuxTitleTestGuard {
    let fixture_lock = CODEX_TMUX_TITLE_FIXTURE_LOCK.lock().unwrap();
    *CODEX_TMUX_TITLE_TEST_READER.lock().unwrap() = Some(reader);
    CodexTmuxTitleTestGuard {
        _fixture_lock: fixture_lock,
    }
}

/// Bound the discovery worker across process creation, output and reap. The
/// command worker kills/reaps a normally stalled child at the same deadline;
/// the caller can move on even if an OS operation itself stalls during cleanup.
fn bounded_title_command(mut command: Command, budget: Duration) -> Option<String> {
    let deadline = Instant::now() + budget;
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let result = (|| {
            let mut child = command.spawn().ok()?;
            let Some(stdout) = child.stdout.take() else {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            };
            // A title can exceed the pipe capacity, and a child can keep its
            // pipe open after exit. Neither case may block the command worker.
            let (output_sender, output_receiver) = mpsc::sync_channel(1);
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let result = stdout.take(4097).read_to_end(&mut bytes).map(|_| bytes);
                let _ = output_sender.send(result);
            });
            let status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return None;
                    }
                }
            };
            if !status.success() {
                return None;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            match output_receiver.recv_timeout(remaining) {
                Ok(Ok(bytes)) if bytes.len() <= 4096 => {
                    Some(String::from_utf8_lossy(&bytes).trim().to_string())
                }
                _ => None,
            }
        })();
        let _ = sender.send(result);
    });
    receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()
        .flatten()
}

fn tmux_title_on(session: &str, socket: Socket) -> Option<String> {
    let mut command = Command::new("tmux");
    command.args(socket.args()).args([
        "display-message",
        "-p",
        "-t",
        &format!("={session}:"),
        "#{pane_title}",
    ]);
    bounded_title_command(command, Duration::from_secs(2))
}

/// This runs only on the discovery worker. The recorded socket prevents a
/// namesake session on the other server from supplying the wrong title.
fn codex_tmux_title(session: &str, socket: Socket) -> Option<String> {
    #[cfg(test)]
    if let Some(reader) = CODEX_TMUX_TITLE_TEST_READER.lock().unwrap().clone() {
        return reader(session, socket);
    }
    tmux_title_on(session, socket)
}

fn codex_tmux_capture_session(
    instance: &muxel_core::Instance,
    osc_hint: Option<&str>,
) -> Option<String> {
    (osc_hint.is_none()
        && instance.use_tmux
        && instance.session_started
        && instance.session_id.is_none()
        && instance.tmux_socket.is_some())
    .then(|| instance.tmux_session.clone())
    .flatten()
}

enum DiscoveryCandidate {
    Unresolved,
    Codex {
        session: Option<CodexSession>,
        miss: Option<CodexHintMiss>,
        observed_hint: Option<String>,
        cached_rejection: Option<String>,
    },
    Session {
        session_id: String,
        switched: Option<(SystemTime, SystemTime)>,
    },
    Claude {
        snapshot: session_binding::ClaudeBindingSnapshot,
        binding: Option<session_binding::ClaudeBinding>,
    },
}

impl DiscoveryCandidate {
    fn session_id(&self) -> Option<&str> {
        match self {
            Self::Unresolved => None,
            Self::Codex { session, .. } => session.as_ref().map(|session| session.id.as_str()),
            Self::Session { session_id, .. } => Some(session_id),
            Self::Claude { binding, .. } => {
                binding.as_ref().map(|binding| binding.session_id.as_str())
            }
        }
    }
}

struct DiscoveryBatch {
    results: Vec<(SessionDiscoveryRequest, DiscoveryCandidate)>,
    grok_read: Option<(SystemTime, SystemTime)>,
}

#[cfg(windows)]
pub(super) struct GrokObservation {
    launch: Instant,
    child_pid: Option<u32>,
    session_id: Option<String>,
    read: GrokRegistryRead,
}

#[cfg(windows)]
impl GrokObservation {
    fn new(key: &SessionDiscoveryKey, read: GrokRegistryRead) -> Self {
        Self {
            launch: key.launch,
            child_pid: key.child_pid,
            session_id: key.binding.clone(),
            read,
        }
    }

    fn matches(&self, key: &SessionDiscoveryKey) -> bool {
        key.provider == SessionProvider::Grok
            && self.launch == key.launch
            && self.child_pid == key.child_pid
    }
}

struct ClaudeCommit {
    record: session_binding::ClaimedClaudeRecord,
    launch: Uuid,
    identity: session_binding::ClaudeHookIdentity,
}

struct SessionBindingUpdate {
    pane: Uuid,
    previous_id: Option<String>,
    previous_path: Option<PathBuf>,
    previous_started: bool,
    previous_auto_name: Option<String>,
    switched: Option<(SystemTime, SystemTime)>,
    claude: Option<ClaudeCommit>,
}

/// A pane can reserve its saved ID and an accepted ID whose save is pending.
/// Construct once per batch, then update claims as each result is staged.
#[derive(Default)]
struct SessionOwnership {
    identities: HashMap<String, HashSet<Uuid>>,
    positions: HashMap<Uuid, usize>,
}

impl SessionOwnership {
    fn claim(&mut self, pane: Uuid, id: &str) {
        if let Some(owners) = self.identities.get_mut(id) {
            owners.insert(pane);
        } else {
            self.identities.insert(id.to_owned(), HashSet::from([pane]));
        }
    }

    fn bound_elsewhere(&self, pane: Uuid, id: &str) -> bool {
        self.identities
            .get(id)
            .is_some_and(|owners| owners.len() > 1 || !owners.contains(&pane))
    }

    fn other_owner(&self, pane: Uuid, id: &str) -> Option<Uuid> {
        self.identities
            .get(id)?
            .iter()
            .copied()
            .find(|owner| *owner != pane)
    }

    fn replace(&mut self, pane: Uuid, old: [Option<&str>; 2], new: [Option<&str>; 2]) {
        for id in old
            .into_iter()
            .flatten()
            .filter(|id| !new.contains(&Some(id)))
        {
            if let Some(owners) = self.identities.get_mut(id) {
                owners.remove(&pane);
                if owners.is_empty() {
                    self.identities.remove(id);
                }
            }
        }
        for id in new
            .into_iter()
            .flatten()
            .filter(|id| !old.contains(&Some(id)))
        {
            self.claim(pane, id);
        }
    }
}

impl MuxelApp {
    pub(super) fn bump_session_binding_generation(&mut self, iid: Uuid) {
        self.session_discoveries.binding_changed(iid);
    }

    fn discovery_terminal(&self, iid: Uuid) -> Option<&gpui::Entity<muxel_terminal::TerminalView>> {
        self.terminal_view(iid)
    }

    fn session_discovery_key(&self, iid: Uuid, cx: &App) -> Option<SessionDiscoveryKey> {
        let instance = self.workspace.instance(iid)?;
        self.session_discovery_key_for_instance(instance, cx)
    }

    fn session_discovery_key_for_instance(
        &self,
        instance: &muxel_core::Instance,
        cx: &App,
    ) -> Option<SessionDiscoveryKey> {
        let iid = instance.id;
        let project = self.workspace.project(instance.project_id)?;
        if project.remote.is_some() || self.terminal_launching.contains_key(&iid) {
            return None;
        }
        let view = self.discovery_terminal(iid)?.read(cx);
        if view.exited() {
            return None;
        }
        let preset = instance
            .preset_id
            .and_then(|id| self.presets.iter().find(|preset| preset.id == id))
            .or_else(|| {
                self.presets
                    .iter()
                    .find(|preset| preset.name == instance.preset)
            });
        let (provider, hint, tmux_session) = if let Some(preset) =
            preset.filter(|preset| is_codex_program(preset.program.as_deref()))
        {
            let hint = view
                .session_id_hint()
                .and_then(|title| muxel_core::codex_session_hint_from_title(preset, &title));
            let tmux_session = codex_tmux_capture_session(instance, hint.as_deref());
            if hint.is_none() && tmux_session.is_none() {
                return None;
            }
            if hint.as_ref().is_some_and(|hint| {
                let prefix = hint.strip_suffix("...").unwrap_or(hint);
                instance
                    .session_id
                    .as_deref()
                    .is_some_and(|id| id.starts_with(prefix))
            }) {
                return None;
            }
            (SessionProvider::Codex, hint, tmux_session)
        } else if is_claude_program(instance.program.as_deref()) {
            (SessionProvider::Claude, None, None)
        } else if cfg!(windows)
            && is_grok_program(instance.program.as_deref())
            && instance.tmux_session.is_none()
            && view.child_pid().is_some()
        {
            (SessionProvider::Grok, None, None)
        } else {
            return None;
        };
        Some(SessionDiscoveryKey {
            pane: iid,
            provider,
            launch: self.terminal_launches.get(&iid)?.0,
            child_pid: view.child_pid(),
            binding_generation: self.session_discoveries.binding_generation(iid),
            binding: instance.session_id.clone(),
            binding_path: instance.session_path.clone(),
            cwd: instance
                .worktree_path
                .as_ref()
                .unwrap_or(&project.root_path)
                .clone(),
            hint,
            tmux_socket: tmux_session.as_ref().and(instance.tmux_socket),
            tmux_session,
        })
    }

    pub(super) fn sync_provider_session_ids(&mut self, cx: &mut Context<Self>) {
        let Some(home) = home_dir() else { return };
        #[cfg(windows)]
        if !self.grok_observations.is_empty() {
            let panes: std::collections::HashSet<_> = self
                .workspace
                .instances
                .iter()
                .map(|instance| instance.id)
                .collect();
            self.grok_observations
                .retain(|pane, _| panes.contains(pane));
        }
        let data_dir = muxel_store::data_dir();
        let now = Instant::now();
        let keys: Vec<_> = self
            .workspace
            .instances
            .iter()
            .filter_map(|instance| self.session_discovery_key_for_instance(instance, cx))
            .collect();
        self.session_discoveries.reconcile(&self.workspace);
        let requests: Vec<_> = keys
            .into_iter()
            .filter_map(|key| self.session_discoveries.begin(key, now))
            .collect();
        let mut groups: [Vec<SessionDiscoveryRequest>; 3] = Default::default();
        for request in requests {
            let index = match request.key.provider {
                SessionProvider::Codex => 0,
                SessionProvider::Claude => 1,
                SessionProvider::Grok => 2,
            };
            groups[index].push(request);
        }
        for requests in groups.into_iter().filter(|requests| !requests.is_empty()) {
            let home = home.clone();
            let data_dir = data_dir.clone();
            #[cfg(windows)]
            let previous_grok_reads = requests
                .iter()
                .filter_map(|request| {
                    self.grok_read_for(&request.key)
                        .map(|read| (request.key.pane, (read.started, read.finished)))
                })
                .collect();
            #[cfg(not(windows))]
            let previous_grok_reads = HashMap::new();
            cx.spawn(async move |this, cx| {
                let (batch, data_dir) = cx
                    .background_executor()
                    .spawn(async move {
                        let batch = discover_sessions(
                            requests,
                            &home,
                            data_dir.as_deref(),
                            previous_grok_reads,
                        );
                        (batch, data_dir)
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.apply_discovery_batch(batch, data_dir.as_deref(), cx);
                });
            })
            .detach();
        }
    }

    fn apply_discovery_batch(
        &mut self,
        batch: DiscoveryBatch,
        data_dir: Option<&Path>,
        cx: &mut Context<Self>,
    ) {
        #[cfg(not(windows))]
        let _ = batch.grok_read;
        let mut ownership = self.session_ownership(cx);
        let mut updates = Vec::new();
        for (request, candidate) in batch.results {
            let iid = request.key.pane;
            let current = self.session_discovery_key(iid, cx);
            // Earlier results in this batch may already have acquired the ID.
            // The same ownership snapshot also reserves accepted Grok IDs whose
            // metadata save has not succeeded yet.
            let candidate_id = candidate.session_id().or(match &candidate {
                DiscoveryCandidate::Codex {
                    cached_rejection, ..
                } => cached_rejection.as_deref(),
                _ => None,
            });
            let rejected_owner = candidate_id.and_then(|id| {
                ownership
                    .other_owner(iid, id)
                    .map(|owner| (owner, id.to_owned()))
            });
            let unowned = candidate.session_id().is_some() && rejected_owner.is_none();
            let valid = match &candidate {
                DiscoveryCandidate::Codex {
                    miss,
                    observed_hint,
                    cached_rejection,
                    ..
                } => self.session_discoveries.finish_with_codex_evidence(
                    &request,
                    current.as_ref(),
                    Instant::now(),
                    unowned,
                    CodexLookupEvidence {
                        miss: miss.clone(),
                        rejected_owner,
                        observed_hint: observed_hint.clone(),
                        cached_rejection: cached_rejection.is_some(),
                    },
                ),
                _ => self.session_discoveries.finish(
                    &request,
                    current.as_ref(),
                    Instant::now(),
                    unowned,
                ),
            };
            if !valid {
                // Stale workers never claim or acknowledge a receipt.
                continue;
            }
            let previous_id = current.as_ref().and_then(|key| key.binding.as_deref());
            #[cfg(windows)]
            let previous_grok = current
                .as_ref()
                .filter(|key| key.provider == SessionProvider::Grok)
                .and_then(|_| self.live_grok_session_id(iid, cx))
                .map(str::to_owned);
            #[cfg(not(windows))]
            let previous_grok: Option<String> = None;
            #[cfg(windows)]
            if request.key.provider == SessionProvider::Grok
                && unowned
                && let Some((started, finished)) = batch.grok_read
            {
                self.record_grok_read(&request.key, GrokRegistryRead { started, finished });
            }
            let update = match candidate {
                DiscoveryCandidate::Codex {
                    session: Some(session),
                    ..
                } if unowned => self.stage_discovered_session_with_path(
                    iid,
                    session.id,
                    Some(session.path),
                    false,
                ),
                DiscoveryCandidate::Claude { snapshot, binding } => {
                    // Only this UI owner and prelaunch recovery may claim/ack.
                    // A newer receipt must wait for its own validation.
                    let Some(record) = data_dir.and_then(|data_dir| {
                        claim_validated_claude_record(data_dir, iid, &snapshot)
                    }) else {
                        continue;
                    };
                    if unowned {
                        self.stage_discovered_claude(iid, record, binding, cx)
                    } else {
                        acknowledge_futile_claude_record(record, binding.is_some());
                        None
                    }
                }
                DiscoveryCandidate::Session {
                    session_id,
                    switched,
                } if unowned => {
                    #[cfg(windows)]
                    if request.key.provider == SessionProvider::Grok {
                        self.stage_grok_session(
                            &request.key,
                            session_id,
                            switched.expect("Grok result carries its observation interval"),
                            cx,
                        )
                    } else {
                        self.stage_discovered_session(iid, session_id, false)
                    }
                    #[cfg(not(windows))]
                    {
                        let _ = switched;
                        self.stage_discovered_session(iid, session_id, false)
                    }
                }
                _ => None,
            };
            let current_id = ownership
                .positions
                .get(&iid)
                .and_then(|index| self.workspace.instances.get(*index))
                .and_then(|instance| instance.session_id.as_deref());
            #[cfg(windows)]
            let current_grok = current
                .as_ref()
                .filter(|key| key.provider == SessionProvider::Grok)
                .and_then(|_| self.live_grok_session_id(iid, cx));
            #[cfg(not(windows))]
            let current_grok = None;
            ownership.replace(
                iid,
                [previous_id, previous_grok.as_deref()],
                [current_id, current_grok],
            );
            if let Some(update) = update {
                updates.push(update);
            }
        }
        self.finish_discovered_sessions(updates, cx);
    }

    fn session_ownership(&self, cx: &App) -> SessionOwnership {
        let mut ownership = SessionOwnership::default();
        #[cfg(windows)]
        let local_projects: HashSet<_> = self
            .workspace
            .projects
            .iter()
            .filter(|project| project.remote.is_none())
            .map(|project| project.id)
            .collect();
        #[cfg(not(windows))]
        let _ = cx;
        for (index, instance) in self.workspace.instances.iter().enumerate() {
            ownership.positions.entry(instance.id).or_insert(index);
            if let Some(id) = &instance.session_id {
                ownership.claim(instance.id, id);
            }
            #[cfg(windows)]
            if instance.kind == muxel_core::InstanceKind::Terminal
                && is_grok_program(instance.program.as_deref())
                && instance.tmux_session.is_none()
                && local_projects.contains(&instance.project_id)
                && let Some(id) = self.live_grok_session_id(instance.id, cx)
            {
                ownership.claim(instance.id, id);
            }
        }
        ownership
    }

    #[cfg(windows)]
    fn live_grok_session_id(&self, iid: Uuid, cx: &App) -> Option<&str> {
        let observation = self.grok_observations.get(&iid)?;
        if self.terminal_launches.get(&iid)?.0 != observation.launch {
            return None;
        }
        let pid = self.discovery_terminal(iid)?.read(cx).child_pid()?;
        if observation.child_pid != Some(pid) {
            return None;
        }
        observation.session_id.as_deref()
    }

    #[cfg(windows)]
    pub(super) fn reconcile_grok_before_restart(
        &mut self,
        iid: Uuid,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let Some(instance) = self.workspace.instance(iid) else {
            return Ok(());
        };
        if instance.kind != muxel_core::InstanceKind::Terminal
            || !is_grok_program(instance.program.as_deref())
            || instance.tmux_session.is_some()
            || self
                .workspace
                .project(instance.project_id)
                .is_none_or(|project| project.remote.is_some())
        {
            return Ok(());
        }
        let Some(identity) = self.live_grok_session_id(iid, cx).map(str::to_owned) else {
            return Ok(());
        };
        if instance.session_id.as_deref() == Some(identity.as_str()) {
            return Ok(());
        }
        if self.session_ownership(cx).bound_elsewhere(iid, &identity) {
            anyhow::bail!(
                crate::i18n::t("Another pane owns the current conversation.").to_string()
            );
        }
        let Some(update) = self.stage_discovered_session(iid, identity, false) else {
            return Ok(());
        };
        if !self.finish_discovered_sessions(vec![update], cx) {
            anyhow::bail!(
                crate::i18n::t("Could not save the current conversation before restart.")
                    .to_string()
            );
        }
        Ok(())
    }

    #[cfg(windows)]
    fn grok_read_for(&self, key: &SessionDiscoveryKey) -> Option<GrokRegistryRead> {
        self.grok_observations
            .get(&key.pane)
            .filter(|observation| observation.matches(key))
            .map(|observation| observation.read)
    }

    #[cfg(windows)]
    fn record_grok_read(&mut self, key: &SessionDiscoveryKey, read: GrokRegistryRead) {
        let observation = self
            .grok_observations
            .entry(key.pane)
            .or_insert_with(|| GrokObservation::new(key, read));
        if !observation.matches(key) {
            *observation = GrokObservation::new(key, read);
        } else if read.finished >= observation.read.finished {
            observation.read = read;
        }
    }

    #[cfg(windows)]
    fn stage_grok_session(
        &mut self,
        key: &SessionDiscoveryKey,
        session_id: String,
        interval: (SystemTime, SystemTime),
        cx: &App,
    ) -> Option<SessionBindingUpdate> {
        // Called only for an accepted worker result after its read is recorded.
        // Durable metadata can still say A after a failed save of live B.
        let observation = self
            .grok_observations
            .get_mut(&key.pane)
            .filter(|observation| observation.matches(key))?;
        let changed = observation.session_id.as_deref() != Some(session_id.as_str());
        if changed {
            observation.session_id = Some(session_id.clone());
        }
        if changed && let Some(view) = self.discovery_terminal(key.pane) {
            view.read(cx).provider_switched(interval.0, interval.1);
        }
        // Retrying persistence must not create another runtime boundary.
        self.stage_discovered_session(key.pane, session_id, false)
    }

    fn stage_discovered_claude(
        &mut self,
        iid: Uuid,
        record: session_binding::ClaimedClaudeRecord,
        binding: Option<session_binding::ClaudeBinding>,
        cx: &App,
    ) -> Option<SessionBindingUpdate> {
        let Some(binding) = binding else {
            acknowledge_futile_claude_record(record, false);
            return None;
        };
        let receipt_at = binding
            .stamp
            .map(|stamp| stamp.recorded_at)
            .or(record.modified);
        let (launch, confirms_launch) = {
            let view = self.discovery_terminal(iid)?;
            let view = view.read(cx);
            let terminal = view.session();
            (
                terminal.id,
                session_binding::claude_receipt_confirms_launch(
                    &binding,
                    receipt_at,
                    terminal.launched_at(),
                    terminal.launch_resumed(),
                    terminal.first_submission_at(),
                ),
            )
        };
        let current =
            session_binding::claude_hook_identity(&binding, record.modified, &record.bytes);
        let seen = self
            .claude_hook_seen
            .get(&iid)
            .filter(|(seen_launch, _)| *seen_launch == launch)
            .map(|(_, identity)| identity);
        let bound = self.workspace.instance(iid)?.session_id.as_deref();
        if !session_binding::claude_record_switches(
            &binding,
            &current,
            seen,
            bound,
            confirms_launch,
        ) {
            self.remember_claude_hook(iid, Some(launch), current);
            session_binding::acknowledge_claude_binding(record);
            return None;
        }
        let switched_at = receipt_at.unwrap_or_else(SystemTime::now);
        // A new nonce can report A -> B -> A between reads even when the final
        // UUID equals the saved one. It still needs a runtime boundary.
        let mut update = self.stage_discovered_session(iid, binding.session_id, true)?;
        update.switched = Some((switched_at, switched_at));
        update.claude = Some(ClaudeCommit {
            record,
            launch,
            identity: current,
        });
        Some(update)
    }

    fn stage_discovered_session(
        &mut self,
        iid: Uuid,
        session_id: String,
        force_boundary: bool,
    ) -> Option<SessionBindingUpdate> {
        self.stage_discovered_session_with_path(iid, session_id, None, force_boundary)
    }

    fn stage_discovered_session_with_path(
        &mut self,
        iid: Uuid,
        session_id: String,
        path: Option<PathBuf>,
        force_boundary: bool,
    ) -> Option<SessionBindingUpdate> {
        let instance = self.workspace.instance_mut(iid)?;
        let same_id = instance.session_id.as_deref() == Some(session_id.as_str());
        let path = path.or_else(|| same_id.then(|| instance.session_path.clone()).flatten());
        if !force_boundary && same_id && instance.session_path == path {
            return None;
        }
        let update = SessionBindingUpdate {
            pane: iid,
            previous_id: instance.session_id.clone(),
            previous_path: instance.session_path.clone(),
            previous_started: instance.session_started,
            previous_auto_name: instance.auto_name.clone(),
            switched: None,
            claude: None,
        };
        instance.session_id = Some(session_id);
        instance.session_path = path;
        instance.session_started = true;
        if !same_id {
            instance.auto_name = None;
        }
        self.bump_session_binding_generation(iid);
        Some(update)
    }

    fn finish_discovered_sessions(
        &mut self,
        updates: Vec<SessionBindingUpdate>,
        cx: &mut Context<Self>,
    ) -> bool {
        if updates.is_empty() {
            return true;
        }
        if self.try_persist() {
            for update in updates {
                if let Some(claude) = update.claude {
                    self.remember_claude_hook(update.pane, Some(claude.launch), claude.identity);
                    session_binding::acknowledge_claude_binding(claude.record);
                }
                if let Some((from, to)) = update.switched
                    && let Some(view) = self.discovery_terminal(update.pane)
                {
                    view.read(cx).provider_switched(from, to);
                }
            }
            cx.notify();
            true
        } else {
            for update in updates {
                let instance = self
                    .workspace
                    .instance_mut(update.pane)
                    .expect("discovery owner exists during synchronous persistence");
                instance.session_id = update.previous_id;
                instance.session_path = update.previous_path;
                instance.session_started = update.previous_started;
                instance.auto_name = update.previous_auto_name;
                self.bump_session_binding_generation(update.pane);
                // Claimed Claude files are intentionally not acknowledged, and
                // their nonce is not remembered until the save succeeds.
            }
            false
        }
    }
}

fn discover_sessions(
    requests: Vec<SessionDiscoveryRequest>,
    home: &Path,
    data_dir: Option<&Path>,
    previous_grok_reads: HashMap<Uuid, (SystemTime, SystemTime)>,
) -> DiscoveryBatch {
    #[cfg(not(windows))]
    let _ = previous_grok_reads;
    #[cfg(windows)]
    let mut grok_snapshot = None;
    let results = requests
        .into_iter()
        .map(|request| {
            let key = &request.key;
            let started = Instant::now();
            let candidate = match key.provider {
                SessionProvider::Codex => {
                    let mut miss = None;
                    let mut cached_rejection = None;
                    let hint = key.hint.clone().or_else(|| {
                        key.tmux_session
                            .as_deref()
                            .zip(key.tmux_socket)
                            .and_then(|(session, socket)| codex_tmux_title(session, socket))
                            .and_then(|title| {
                                muxel_core::codex_session_hint_from_title(
                                    &AgentPreset::codex(),
                                    &title,
                                )
                            })
                    });
                    let observed_hint = hint.clone();
                    let session = hint.and_then(|hint| {
                        if let Some((cached_hint, cached_id)) = &request.codex_rejected_hint
                            && *cached_hint == hint
                        {
                            // A sibling still owns this exact title. Read the
                            // title again next time, but do not rescan a rollout
                            // that was already validated for this hint.
                            cached_rejection = Some(cached_id.clone());
                            return None;
                        }
                        let stamp = CodexHintMiss::capture(home, &hint, started);
                        if let Some(cached) = request
                            .codex_miss
                            .as_ref()
                            .filter(|cached| cached.reusable_for(&stamp))
                        {
                            miss = Some(cached.clone());
                            return None;
                        }
                        let candidate = validated_codex_session_id(
                            &AgentPreset::codex(),
                            key.binding.as_deref(),
                            Some(&hint),
                            home,
                            &key.cwd,
                            Some(key.pane),
                        );
                        if candidate.is_none() {
                            miss = Some(stamp);
                        }
                        candidate
                    });
                    DiscoveryCandidate::Codex {
                        session,
                        miss,
                        observed_hint,
                        cached_rejection,
                    }
                }
                SessionProvider::Claude => data_dir
                    .and_then(|data_dir| {
                        session_binding::read_pending_claude_binding(data_dir, key.pane)
                    })
                    .map_or(DiscoveryCandidate::Unresolved, |snapshot| {
                        let binding =
                            session_binding::claude_binding_from_record(&snapshot.bytes, home);
                        DiscoveryCandidate::Claude { snapshot, binding }
                    }),
                SessionProvider::Grok => {
                    #[cfg(windows)]
                    {
                        let (read, active, parents) = grok_snapshot.get_or_insert_with(|| {
                            let started = SystemTime::now();
                            let active = read_grok_active_sessions();
                            let read = GrokRegistryRead {
                                started,
                                finished: SystemTime::now(),
                            };
                            (read, active, process_parent_map())
                        });
                        let previous = previous_grok_reads
                            .get(&key.pane)
                            .copied()
                            .map(|(started, finished)| GrokRegistryRead { started, finished });
                        key.child_pid
                            .and_then(|pid| active_grok_session_for_process(pid, active, parents))
                            .map_or(DiscoveryCandidate::Unresolved, |session| {
                                DiscoveryCandidate::Session {
                                    session_id: session.session_id.clone(),
                                    switched: Some(grok_switch_interval(session, previous, *read)),
                                }
                            })
                    }
                    #[cfg(not(windows))]
                    {
                        DiscoveryCandidate::Unresolved
                    }
                }
            };
            ui_profile::session_lookup_finished(
                ui_profile::SessionLookupKind::Discovery,
                Some(key.provider),
                key.pane,
                started.elapsed(),
                if candidate.session_id().is_some() {
                    ui_profile::SessionLookupOutcome::Found
                } else {
                    ui_profile::SessionLookupOutcome::Unresolved
                },
            );
            (request, candidate)
        })
        .collect();
    #[cfg(windows)]
    let grok_read = grok_snapshot.map(|(read, _, _)| (read.started, read.finished));
    #[cfg(not(windows))]
    let grok_read = None;
    DiscoveryBatch { results, grok_read }
}

/// Called only after the UI has validated the worker key and launch owner.
/// A mismatching claim stays pending for the next worker; dropping it never acks.
fn claim_validated_claude_record(
    data_dir: &Path,
    pane: Uuid,
    snapshot: &session_binding::ClaudeBindingSnapshot,
) -> Option<session_binding::ClaimedClaudeRecord> {
    let record = session_binding::claim_claude_binding(data_dir, pane)?;
    if record.bytes != snapshot.bytes || record.modified != snapshot.modified {
        return None;
    }
    Some(record)
}

/// A valid record whose transcript or project tree is temporarily unavailable
/// remains claimed for the next discovery pass. Malformed records cannot heal.
fn acknowledge_futile_claude_record(
    record: session_binding::ClaimedClaudeRecord,
    binding_valid: bool,
) {
    if binding_valid || !session_binding::claude_record_structurally_valid(&record.bytes) {
        session_binding::acknowledge_claude_binding(record);
    }
}

#[cfg(all(test, windows))]
mod grok_regressions {
    use super::super::{
        AgentStatus, GrokActiveSession, GrokRegistryRead, LoopCleanup, LoopRun, MuxelApp,
        PostRunAction, grok_switch_interval, popout_lifecycle_tests::with_app,
    };
    use super::{DiscoveryBatch, DiscoveryCandidate};
    use gpui::{AppContext as _, Context, TestAppContext, Window, WindowHandle};
    use muxel_core::{AgentPreset, CodexSession, InjectionMode, Instance, Project};
    use muxel_terminal::{
        CommandSpec, TerminalLaunch, TerminalRuntimeSnapshot, TerminalSession,
        TerminalStatusCoverage, TerminalStatusReason, TerminalView,
    };
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime};
    use uuid::Uuid;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    struct StopChild(Arc<TerminalSession>);
    impl Drop for StopChild {
        fn drop(&mut self) {
            self.0.kill();
        }
    }

    struct ChildHandle(HANDLE);
    impl ChildHandle {
        fn new(session: &TerminalSession) -> Self {
            // SAFETY: this is the PTY created by this fixture. Retain the handle
            // before Restart so PID reuse cannot satisfy a liveness assertion.
            Self(unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    false,
                    session.child_pid().unwrap(),
                )
                .unwrap()
            })
        }

        fn running(&self) -> bool {
            // SAFETY: the retained handle stays open until this guard drops.
            let status = unsafe { WaitForSingleObject(self.0, 0) };
            if status == WAIT_TIMEOUT {
                true
            } else {
                assert_eq!(status, WAIT_OBJECT_0);
                false
            }
        }
    }
    impl Drop for ChildHandle {
        fn drop(&mut self) {
            // SAFETY: this guard exclusively owns the retained process handle.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    fn checked_update<R>(
        window: WindowHandle<MuxelApp>,
        cx: &mut TestAppContext,
        run: impl FnOnce(&mut MuxelApp, &mut Window, &mut Context<MuxelApp>) -> R,
    ) -> R {
        // Restore GPUI's app lease before reporting a failed assertion. Owned
        // PTY guards still unwind inside the callback before cleanup runs.
        let result = window
            .update(cx, |app, window, cx| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(app, window, cx)))
            })
            .unwrap();
        match result {
            Ok(value) => value,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    fn pane(
        app: &mut MuxelApp,
        window: &mut Window,
        cx: &mut Context<MuxelApp>,
    ) -> (Uuid, Arc<TerminalSession>) {
        let cwd = muxel_store::data_dir()
            .unwrap()
            .join(Uuid::new_v4().to_string());
        std::fs::create_dir_all(&cwd).unwrap();
        let project = app.workspace.add_project(Project::new("Grok fixture", cwd));
        let mut instance = Instance::shell(project);
        instance.program = Some("grok.exe".into());
        instance.session_id = Some(Uuid::new_v4().to_string());
        instance.session_started = true;
        let id = instance.id;
        app.workspace.add_instance(instance);
        let launch = TerminalLaunch::spawn(
            CommandSpec::program(
                "cmd.exe",
                vec!["/d".into(), "/q".into(), "/c".into(), "more".into()],
            ),
            (80, 24),
        )
        .unwrap();
        let view = cx.new(|cx| TerminalView::new(launch, id, Instant::now(), window, cx));
        let session = view.read(cx).session().clone();
        app.terminals.insert(id, view);
        app.terminal_launches.insert(id, (Instant::now(), false));
        (id, session)
    }

    fn complete_grok(app: &mut MuxelApp, id: Uuid, identity: &str, cx: &mut Context<MuxelApp>) {
        let key = app.session_discovery_key(id, cx).unwrap();
        let request = app.session_discoveries.begin(key, Instant::now()).unwrap();
        let now = SystemTime::now();
        app.apply_discovery_batch(
            DiscoveryBatch {
                results: vec![(
                    request,
                    DiscoveryCandidate::Session {
                        session_id: identity.into(),
                        switched: Some((now, now)),
                    },
                )],
                grok_read: Some((now, now)),
            },
            muxel_store::data_dir().as_deref(),
            cx,
        );
    }

    #[test]
    fn codex_batch_rechecks_a_released_owner_then_saves_the_rollout_path() {
        with_app(|cx, window| {
            checked_update(window, cx, |app, window, cx| {
                app.current_workspace = Some(Uuid::new_v4());
                let (target, target_session) = pane(app, window, cx);
                let _stop_target = StopChild(target_session);
                let (owner, owner_session) = pane(app, window, cx);
                let _stop_owner = StopChild(owner_session);
                let identity = Uuid::new_v4().to_string();
                let path = muxel_store::data_dir()
                    .unwrap()
                    .join(format!("rollout-{identity}.jsonl"));
                std::fs::write(&path, b"rollout fixture").unwrap();
                let preset = AgentPreset::codex();
                let instance = app.workspace.instance_mut(target).unwrap();
                instance.preset_id = Some(preset.id);
                instance.preset = preset.name.clone();
                instance.program = preset.program.clone();
                instance.session_id = None;
                instance.session_started = true;
                instance.use_tmux = true;
                instance.tmux_session = Some("muxel_codex_batch".into());
                instance.tmux_socket = Some(muxel_core::tmux::Socket::Default);
                app.presets.push(preset);
                app.workspace.instance_mut(owner).unwrap().session_id = Some(identity.clone());
                let doc = muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap();
                std::fs::create_dir_all(doc.parent().unwrap()).unwrap();

                let key = app.session_discovery_key(target, cx).unwrap();
                app.session_discoveries.reconcile(&app.workspace);
                let first = app
                    .session_discoveries
                    .begin(key.clone(), Instant::now())
                    .unwrap();
                let candidate = || DiscoveryCandidate::Codex {
                    session: Some(CodexSession {
                        id: identity.clone(),
                        path: path.clone(),
                    }),
                    miss: None,
                    observed_hint: Some(identity.clone()),
                    cached_rejection: None,
                };
                app.apply_discovery_batch(
                    DiscoveryBatch {
                        results: vec![(first, candidate())],
                        grok_read: None,
                    },
                    None,
                    cx,
                );
                assert!(app.workspace.instance(target).unwrap().session_id.is_none());
                assert!(
                    app.workspace
                        .instance(target)
                        .unwrap()
                        .session_path
                        .is_none()
                );

                app.workspace.instance_mut(owner).unwrap().session_id = None;
                app.bump_session_binding_generation(owner);
                app.session_discoveries.reconcile(&app.workspace);
                let retry = app
                    .session_discoveries
                    .begin(key, Instant::now() + Duration::from_millis(1))
                    .expect("released owner must allow immediate revalidation");
                app.apply_discovery_batch(
                    DiscoveryBatch {
                        results: vec![(retry, candidate())],
                        grok_read: None,
                    },
                    None,
                    cx,
                );
                let bound = app.workspace.instance(target).unwrap();
                assert_eq!(bound.session_id.as_deref(), Some(identity.as_str()));
                assert_eq!(bound.session_path.as_ref(), Some(&path));
                let saved = muxel_store::load_workspace_from(&doc).unwrap();
                let bound = saved.instance(target).unwrap();
                assert_eq!(bound.session_id.as_deref(), Some(identity.as_str()));
                assert_eq!(bound.session_path.as_ref(), Some(&path));
            });
        });
    }

    #[test]
    fn gasq_restart_preserves_unsaved_grok_and_then_resumes_its_accepted_identity() {
        with_app(|cx, window| {
            let (old_process, replacement, report, identity) = checked_update(
                window,
                cx,
                |app, window, cx| {
                    app.current_workspace = Some(Uuid::new_v4());
                    app.use_tmux = false;
                    app.use_worktree = false;
                    let (id, session) = pane(app, window, cx);
                    let _stop_old = StopChild(session.clone());
                    let old_process = ChildHandle::new(&session);
                    let root = muxel_store::data_dir()
                        .unwrap()
                        .join("restart-grok-fixture");
                    std::fs::create_dir_all(&root).unwrap();
                    let program = root.join("grok.cmd");
                    let report = root.join("resume.txt");
                    std::fs::write(
                        &program,
                        "@echo off\r\n> \"%~dp0resume.txt\" echo %~1\r\n>> \"%~dp0resume.txt\" echo %~2\r\nmore >nul\r\n",
                    )
                    .unwrap();
                    let mut preset = AgentPreset::grok();
                    // The store's canonical test root has a Windows verbatim
                    // prefix. cmd cannot execute a batch file under that prefix.
                    let program = program.to_string_lossy();
                    preset.program =
                        Some(program.strip_prefix(r"\\?\").unwrap_or(&program).to_owned());
                    preset.injection = InjectionMode::None;
                    let instance = app.workspace.instance_mut(id).unwrap();
                    instance.preset_id = Some(preset.id);
                    instance.preset = preset.name.clone();
                    instance.program = preset.program.clone();
                    instance.injection = InjectionMode::None;
                    app.presets.push(preset);
                    let old = app.workspace.instance(id).unwrap().session_id.clone();
                    let doc =
                        muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap();
                    std::fs::create_dir_all(&doc).unwrap();
                    let identity = Uuid::new_v4().to_string();
                    complete_grok(app, id, &identity, cx);
                    assert_eq!(app.workspace.instance(id).unwrap().session_id, old);
                    let revision = session.binding_revision();
                    app.restart_agent(id, window, cx);
                    let current = app.terminal_view(id).unwrap().read(cx).session().clone();
                    let _stop_current = StopChild(current.clone());
                    assert_eq!(
                        current.id, session.id,
                        "failed-save Restart replaced the accepted live pane"
                    );
                    assert!(
                        old_process.running(),
                        "failed-save Restart killed the accepted conversation"
                    );
                    assert_eq!(session.binding_revision(), revision);
                    assert_eq!(app.workspace.instance(id).unwrap().session_id, old);
                    assert!(
                        !report.exists(),
                        "refused Restart still launched its fixture"
                    );
                    std::fs::remove_dir(&doc).unwrap();
                    app.restart_agent(id, window, cx);
                    let replacement =
                        StopChild(app.terminal_view(id).unwrap().read(cx).session().clone());
                    assert_ne!(replacement.0.id, session.id);
                    assert_eq!(
                        app.workspace.instance(id).unwrap().session_id.as_deref(),
                        Some(identity.as_str())
                    );
                    let saved = muxel_store::load_workspace_from(&doc).unwrap();
                    assert_eq!(
                        saved.instance(id).unwrap().session_id.as_deref(),
                        Some(identity.as_str())
                    );
                    (old_process, replacement, report, identity)
                },
            );
            let cap = Instant::now() + Duration::from_secs(10);
            let mut cursor_reply = Instant::now() - Duration::from_secs(1);
            loop {
                let selected = std::fs::read_to_string(&report).unwrap_or_default();
                if !old_process.running()
                    && selected.lines().collect::<Vec<_>>() == ["--resume", identity.as_str()]
                {
                    break;
                }
                // Headless ConPTY can wait for a cursor-position response before
                // starting cmd. Supply the same reply as the batch-runner test
                // without pumping the app's provider-registry polling tasks.
                if selected.is_empty() && cursor_reply.elapsed() >= Duration::from_millis(100) {
                    replacement.0.write_input(b"\x1b[1;1R");
                    cursor_reply = Instant::now();
                }
                assert!(
                    Instant::now() < cap,
                    "Restart fixture did not select the accepted identity at {}: {selected:?}",
                    report.display()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(replacement.0.child_pid().is_some());
        });
    }

    #[test]
    fn gasq_sibling_guard_reserves_matching_unsaved_grok_identity() {
        for change in ["matching", "launch", "pid", "removed"] {
            with_app(|cx, window| {
                checked_update(window, cx, |app, window, cx| {
                    app.current_workspace = Some(Uuid::new_v4());
                    let (owner, owner_session) = pane(app, window, cx);
                    let _stop_owner = StopChild(owner_session.clone());
                    let (other, other_session) = pane(app, window, cx);
                    let _stop_other = StopChild(other_session.clone());
                    let old_owner = app.workspace.instance(owner).unwrap().session_id.clone();
                    let old_other = app.workspace.instance(other).unwrap().session_id.clone();
                    let doc =
                        muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap();
                    std::fs::create_dir_all(&doc).unwrap();
                    let identity = Uuid::new_v4().to_string();
                    complete_grok(app, owner, &identity, cx);
                    assert_eq!(app.workspace.instance(owner).unwrap().session_id, old_owner);
                    let revision = owner_session.binding_revision();
                    std::fs::remove_dir(&doc).unwrap();
                    match change {
                        "matching" => {}
                        "launch" => {
                            app.terminal_launches.get_mut(&owner).unwrap().0 +=
                                Duration::from_secs(1)
                        }
                        "pid" => app.grok_observations.get_mut(&owner).unwrap().child_pid = Some(0),
                        "removed" => app.workspace.instances.retain(|row| row.id != owner),
                        _ => unreachable!(),
                    }
                    complete_grok(app, other, &identity, cx);
                    if change != "matching" {
                        assert_eq!(
                            app.workspace.instance(other).unwrap().session_id.as_deref(),
                            Some(identity.as_str()),
                            "stale {change} observation blocked admission"
                        );
                        return;
                    }
                    assert_eq!(
                        app.workspace.instance(other).unwrap().session_id,
                        old_other,
                        "sibling adopted an accepted but unsaved Grok identity"
                    );
                    assert!(
                        !app.grok_observations.contains_key(&other),
                        "rejected candidate advanced its read bound"
                    );
                    complete_grok(app, owner, &identity, cx);
                    assert_eq!(
                        app.workspace.instance(owner).unwrap().session_id.as_deref(),
                        Some(identity.as_str())
                    );
                    assert_eq!(
                        owner_session.binding_revision(),
                        revision,
                        "save retry repeated the accepted switch"
                    );
                    let saved = muxel_store::load_workspace_from(&doc).unwrap();
                    assert_eq!(
                        saved
                            .instances
                            .iter()
                            .filter(|row| row.session_id.as_deref() == Some(identity.as_str()))
                            .count(),
                        1
                    );
                });
            });
        }
    }

    #[test]
    fn gasq_batch_releases_unsaved_identity_when_owner_switches_back_without_metadata_change() {
        with_app(|cx, window| {
            checked_update(window, cx, |app, window, cx| {
                app.current_workspace = Some(Uuid::new_v4());
                let (owner, owner_session) = pane(app, window, cx);
                let _stop_owner = StopChild(owner_session.clone());
                let (other, other_session) = pane(app, window, cx);
                let _stop_other = StopChild(other_session);
                let original = app
                    .workspace
                    .instance(owner)
                    .unwrap()
                    .session_id
                    .clone()
                    .unwrap();
                let doc = muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap();
                std::fs::create_dir_all(&doc).unwrap();
                let accepted = Uuid::new_v4().to_string();
                complete_grok(app, owner, &accepted, cx);
                assert_eq!(
                    app.workspace.instance(owner).unwrap().session_id.as_deref(),
                    Some(original.as_str())
                );
                let revision = owner_session.binding_revision();
                std::fs::remove_dir(&doc).unwrap();
                let owner_key = app.session_discovery_key(owner, cx).unwrap();
                let other_key = app.session_discovery_key(other, cx).unwrap();
                let now = Instant::now();
                let owner_request = app.session_discoveries.begin(owner_key, now).unwrap();
                let other_request = app.session_discoveries.begin(other_key, now).unwrap();
                let read = SystemTime::now();
                app.apply_discovery_batch(
                    DiscoveryBatch {
                        results: vec![
                            (
                                owner_request,
                                DiscoveryCandidate::Session {
                                    session_id: original.clone(),
                                    switched: Some((read, read)),
                                },
                            ),
                            (
                                other_request,
                                DiscoveryCandidate::Session {
                                    session_id: accepted.clone(),
                                    switched: Some((read, read)),
                                },
                            ),
                        ],
                        grok_read: Some((read, read)),
                    },
                    muxel_store::data_dir().as_deref(),
                    cx,
                );
                assert_ne!(owner_session.binding_revision(), revision);
                assert_eq!(
                    app.workspace.instance(owner).unwrap().session_id.as_deref(),
                    Some(original.as_str())
                );
                assert_eq!(
                    app.grok_observations[&owner].session_id.as_deref(),
                    Some(original.as_str())
                );
                assert_eq!(
                    app.workspace.instance(other).unwrap().session_id.as_deref(),
                    Some(accepted.as_str()),
                    "unchanged metadata left the old live identity reserved for the rest of the batch"
                );
            });
        });
    }

    #[test]
    fn dtz5_grok_failed_save_abandons_the_old_exit_loop_and_dedupes_retries() {
        with_app(|cx, window| {
            window
                .update(cx, |app, window, cx| {
                    app.current_workspace = Some(Uuid::new_v4());
                    let (id, session) = pane(app, window, cx);
                    let _stop = StopChild(session.clone());
                    let old = app
                        .workspace
                        .instance(id)
                        .unwrap()
                        .session_id
                        .clone()
                        .unwrap();
                    session.process_probe_output(format!("\x1b]0;{old} | Ready\x07").as_bytes());
                    session.mark_turn_submitted();
                    let revision = session.binding_revision();
                    let cached_done = TerminalRuntimeSnapshot {
                        status: AgentStatus::Done,
                        attributable: true,
                        reason: TerminalStatusReason::Quiet,
                        coverage: TerminalStatusCoverage::ProviderVisible,
                        launch_id: session.id,
                        binding_rev: revision,
                        snapshot_seq: 1,
                        input_rev: 0,
                        draft_latched: false,
                        completion_ready: true,
                    };
                    let mut run = LoopRun {
                        loop_id: Uuid::new_v4(),
                        launch: Some(session.id),
                        seen_working: true,
                        started: Instant::now(),
                        post_run: PostRunAction::Exit,
                    };
                    let doc =
                        muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap();
                    std::fs::create_dir_all(&doc).unwrap();
                    let new = Uuid::new_v4().to_string();
                    let accept =
                        |app: &mut MuxelApp, cx: &mut Context<MuxelApp>, identity: &str| {
                            let key = app.session_discovery_key(id, cx).unwrap();
                            let now = SystemTime::now();
                            app.record_grok_read(
                                &key,
                                GrokRegistryRead {
                                    started: now,
                                    finished: now,
                                },
                            );
                            let update = app
                                .stage_grok_session(&key, identity.to_owned(), (now, now), cx)
                                .unwrap();
                            assert_eq!(
                                app.workspace.instance(id).unwrap().session_id.as_deref(),
                                Some(identity)
                            );
                            app.finish_discovered_sessions(vec![update], cx);
                        };
                    accept(app, cx, &new);
                    assert_eq!(
                        app.workspace.instance(id).unwrap().session_id.as_deref(),
                        Some(old.as_str()),
                        "fixture must fail the real workspace save"
                    );
                    assert_eq!(
                        run.cleanup(
                            cached_done,
                            session.binding_revision(),
                            session.first_submission_binding(),
                            Duration::ZERO
                        ),
                        LoopCleanup::Abandon,
                        "failed persistence left A's Exit loop owning B"
                    );
                    let observed = session.binding_revision();
                    assert_ne!(observed, revision);
                    accept(app, cx, &new);
                    assert_eq!(
                        session.binding_revision(),
                        observed,
                        "retrying the same live identity invalidated it again"
                    );
                    assert_eq!(
                        app.workspace.instance(id).unwrap().session_id.as_deref(),
                        Some(old.as_str())
                    );
                    std::fs::remove_dir(&doc).unwrap();
                    accept(app, cx, &new);
                    assert_eq!(
                        app.workspace.instance(id).unwrap().session_id.as_deref(),
                        Some(new.as_str())
                    );
                    assert_eq!(
                        session.binding_revision(),
                        observed,
                        "successful save repeated the accepted switch"
                    );
                    accept(app, cx, &Uuid::new_v4().to_string());
                    assert_ne!(
                        session.binding_revision(),
                        observed,
                        "a later genuine switch was suppressed"
                    );
                })
                .unwrap();
        });
    }

    #[test]
    fn dtz5_overlapping_grok_panes_keep_the_target_legacy_title_and_read_bound() {
        with_app(|cx, window| {
            window
                .update(cx, |app, window, cx| {
                    app.current_workspace = Some(Uuid::new_v4());
                    let (target, session) = pane(app, window, cx);
                    let _stop_target = StopChild(session.clone());
                    let (other, other_session) = pane(app, window, cx);
                    let _stop_other = StopChild(other_session);
                    let key = app.session_discovery_key(target, cx).unwrap();
                    let foreign = app.session_discovery_key(other, cx).unwrap();
                    // The target worker reads A, then stalls. B's title arrives
                    // before an unrelated pane completes a later registry read.
                    let old_read = GrokRegistryRead {
                        started: SystemTime::now(),
                        finished: SystemTime::now(),
                    };
                    let title = "Current Grok B | Ready";
                    session.process_probe_output(format!("\x1b]0;{title}\x07").as_bytes());
                    assert_eq!(session.title().as_deref(), Some(title));
                    let title_at = SystemTime::now();
                    let cap = Instant::now() + Duration::from_secs(10);
                    while SystemTime::now()
                        .duration_since(title_at)
                        .unwrap_or_default()
                        < Duration::from_secs(3)
                    {
                        assert!(
                            Instant::now() < cap,
                            "wall clock did not advance past title slack"
                        );
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    let foreign_read = GrokRegistryRead {
                        started: SystemTime::now(),
                        finished: SystemTime::now(),
                    };
                    app.record_grok_read(&foreign, foreign_read);
                    // The target's old A result completes after the other pane.
                    app.record_grok_read(&key, old_read);
                    let previous = app.grok_read_for(&key);
                    let read = GrokRegistryRead {
                        started: SystemTime::now(),
                        finished: SystemTime::now(),
                    };
                    let new = GrokActiveSession {
                        pid: key.child_pid.unwrap(),
                        session_id: Uuid::new_v4().to_string(),
                        opened_at: chrono::DateTime::UNIX_EPOCH,
                    };
                    let interval = grok_switch_interval(&new, previous, read);
                    app.record_grok_read(&key, read);
                    let update = app
                        .stage_grok_session(&key, new.session_id, interval, cx)
                        .unwrap();
                    app.finish_discovered_sessions(vec![update], cx);
                    assert_eq!(
                        session.title().as_deref(),
                        Some(title),
                        "a different pane's newer read cleared the target's current title"
                    );
                    assert_eq!(interval.0, old_read.started);
                    let mut replaced = key.clone();
                    replaced.launch += Duration::from_secs(1);
                    assert!(
                        app.grok_read_for(&replaced).is_none(),
                        "another launch reused the observation"
                    );
                    replaced = key;
                    replaced.child_pid = replaced.child_pid.map(|pid| pid + 1);
                    assert!(
                        app.grok_read_for(&replaced).is_none(),
                        "another process reused the observation"
                    );
                })
                .unwrap();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DiscoveryCandidate, SessionOwnership, acknowledge_futile_claude_record,
        claim_validated_claude_record, discover_sessions,
    };
    use crate::session_binding;
    use muxel_core::session_discovery::{SessionDiscovery, SessionDiscoveryKey, SessionProvider};
    use serde_json::json;
    use std::path::PathBuf;
    use std::time::{Duration, Instant, SystemTime};
    use uuid::Uuid;

    #[test]
    fn gasq_ownership_index_preserves_duplicate_owners_and_both_pending_claims() {
        let owner = Uuid::new_v4();
        let sibling = Uuid::new_v4();
        let mut ownership = SessionOwnership::default();
        ownership.claim(owner, "saved");
        ownership.claim(owner, "accepted");
        ownership.claim(sibling, "accepted");
        assert!(ownership.bound_elsewhere(owner, "accepted"));
        assert!(ownership.bound_elsewhere(sibling, "accepted"));
        ownership.replace(
            owner,
            [Some("saved"), Some("accepted")],
            [Some("saved"), Some("next")],
        );
        assert!(!ownership.bound_elsewhere(sibling, "accepted"));
        assert!(ownership.bound_elsewhere(owner, "accepted"));
        assert!(ownership.bound_elsewhere(sibling, "saved"));
        assert!(ownership.bound_elsewhere(sibling, "next"));
        ownership.replace(sibling, [Some("accepted"), Some("accepted")], [None, None]);
        assert!(!ownership.bound_elsewhere(owner, "accepted"));
    }

    struct ClaudeFixture {
        root: PathBuf,
        home: PathBuf,
        data: PathBuf,
        pane: Uuid,
        request: muxel_core::session_discovery::SessionDiscoveryRequest,
    }

    impl ClaudeFixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("muxel-discovery-{}", Uuid::new_v4()));
            let home = root.join("home");
            let data = root.join("data");
            std::fs::create_dir_all(home.join(".claude/projects/fixture")).unwrap();
            let pane = Uuid::new_v4();
            let request = SessionDiscovery::default()
                .begin(
                    SessionDiscoveryKey {
                        pane,
                        provider: SessionProvider::Claude,
                        launch: Instant::now(),
                        child_pid: None,
                        binding_generation: 0,
                        binding: None,
                        binding_path: None,
                        cwd: home.join(".claude/projects/fixture"),
                        hint: None,
                        tmux_session: None,
                        tmux_socket: None,
                    },
                    Instant::now(),
                )
                .unwrap();
            Self {
                root,
                home,
                data,
                pane,
                request,
            }
        }

        fn shared(&self) -> PathBuf {
            session_binding::claude_binding_path(&self.data, self.pane)
        }

        fn hook(&self, identity: Uuid, nonce: Uuid) -> Vec<u8> {
            self.hook_source(identity, nonce, "resume")
        }

        fn hook_source(&self, identity: Uuid, nonce: Uuid, source: &str) -> Vec<u8> {
            let project = self.home.join(".claude/projects/fixture");
            let transcript = project.join(format!("{identity}.jsonl"));
            std::fs::write(&transcript, "{}\n").unwrap();
            let bytes = json!({
                "hook_event_name":"SessionStart", "source":source, "session_id":identity,
                "cwd":project, "transcript_path":transcript,
                "muxel_hook":{"nonce":nonce, "recorded_at_us":123456000_u64}
            })
            .to_string()
            .into_bytes();
            let shared = self.shared();
            std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
            std::fs::write(shared, &bytes).unwrap();
            bytes
        }

        fn candidate(
            &self,
        ) -> (
            session_binding::ClaudeBindingSnapshot,
            session_binding::ClaudeBinding,
        ) {
            let (home, data, request) =
                (self.home.clone(), self.data.clone(), self.request.clone());
            let mut batch = std::thread::spawn(move || {
                discover_sessions(vec![request], &home, Some(&data), Default::default())
            })
            .join()
            .unwrap();
            match batch.results.pop().unwrap().1 {
                DiscoveryCandidate::Claude {
                    snapshot,
                    binding: Some(binding),
                } => (snapshot, binding),
                _ => panic!("expected validated pending Claude snapshot"),
            }
        }

        fn claim(
            &self,
            snapshot: &session_binding::ClaudeBindingSnapshot,
        ) -> Option<session_binding::ClaimedClaudeRecord> {
            claim_validated_claude_record(&self.data, self.pane, snapshot)
        }
    }

    impl Drop for ClaudeFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn claude_fixture_removes_its_temp_tree() {
        let root = {
            let fixture = ClaudeFixture::new();
            assert!(fixture.root.starts_with(std::env::temp_dir()));
            fixture.root.clone()
        };
        assert!(!root.exists());
    }

    #[test]
    fn claude_worker_snapshot_preserves_nonce_and_leaves_hook_unclaimed() {
        let fixture = ClaudeFixture::new();
        let identity = Uuid::new_v4();
        let nonce = Uuid::new_v4();
        let original = fixture.hook(identity, nonce);
        let (snapshot, binding) = fixture.candidate();
        assert_eq!(snapshot.bytes, original);
        assert!(snapshot.modified.is_some());
        assert_eq!(binding.session_id, identity.to_string());
        let stamp = binding.stamp.unwrap();
        assert_eq!(stamp.nonce, nonce);
        assert_eq!(
            stamp.recorded_at,
            SystemTime::UNIX_EPOCH + Duration::from_micros(123456000)
        );
        assert_eq!(std::fs::read(fixture.shared()).unwrap(), original);
        assert_eq!(
            std::fs::read_dir(fixture.shared().parent().unwrap())
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn claude_worker_stale_snapshot_cannot_consume_a_newer_hook() {
        let fixture = ClaudeFixture::new();
        fixture.hook(Uuid::new_v4(), Uuid::new_v4());
        let (old_snapshot, _) = fixture.candidate();
        let newer_identity = Uuid::new_v4();
        let newer = fixture.hook(newer_identity, Uuid::new_v4());
        assert!(
            fixture.claim(&old_snapshot).is_none(),
            "stale validation accepted a newer record"
        );
        assert!(!fixture.shared().exists());
        assert_eq!(
            session_binding::read_pending_claude_binding(&fixture.data, fixture.pane)
                .unwrap()
                .bytes,
            newer
        );
        let (retry, binding) = fixture.candidate();
        assert_eq!(binding.session_id, newer_identity.to_string());
        session_binding::acknowledge_claude_binding(fixture.claim(&retry).unwrap());
        assert!(
            session_binding::read_pending_claude_binding(&fixture.data, fixture.pane).is_none()
        );
    }

    #[test]
    fn claude_worker_retries_pending_claim_before_newer_shared_hook() {
        let fixture = ClaudeFixture::new();
        let first = Uuid::new_v4();
        let nonce = Uuid::new_v4();
        fixture.hook(first, nonce);
        let (snapshot, _) = fixture.candidate();
        // A stale result or failed persistence leaves the UI's claim unacked.
        drop(fixture.claim(&snapshot).unwrap());
        let second = Uuid::new_v4();
        let newer = fixture.hook(second, Uuid::new_v4());
        let (retry, binding) = fixture.candidate();
        assert_eq!(binding.session_id, first.to_string());
        assert_eq!(binding.stamp.unwrap().nonce, nonce);
        session_binding::acknowledge_claude_binding(fixture.claim(&retry).unwrap());
        assert_eq!(std::fs::read(fixture.shared()).unwrap(), newer);
        let (latest, binding) = fixture.candidate();
        assert_eq!(binding.session_id, second.to_string());
        session_binding::acknowledge_claude_binding(fixture.claim(&latest).unwrap());
        assert!(
            session_binding::read_pending_claude_binding(&fixture.data, fixture.pane).is_none()
        );
    }

    #[test]
    fn clear_record_retries_when_claude_project_tree_appears_later() {
        let fixture = ClaudeFixture::new();
        let identity = Uuid::new_v4();
        let original = fixture.hook_source(identity, Uuid::new_v4(), "clear");
        std::fs::remove_dir_all(fixture.home.join(".claude/projects")).unwrap();
        let mut batch = discover_sessions(
            vec![fixture.request.clone()],
            &fixture.home,
            Some(&fixture.data),
            Default::default(),
        );
        let snapshot = match batch.results.pop().unwrap().1 {
            DiscoveryCandidate::Claude {
                snapshot,
                binding: None,
            } => snapshot,
            _ => panic!("missing project tree must defer validation"),
        };
        let claim = fixture.claim(&snapshot).unwrap();
        acknowledge_futile_claude_record(claim, false);
        assert_eq!(
            session_binding::read_pending_claude_binding(&fixture.data, fixture.pane)
                .unwrap()
                .bytes,
            original
        );
        let project = fixture.home.join(".claude/projects/fixture");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(format!("{identity}.jsonl")), "{}\n").unwrap();
        let (retry, binding) = fixture.candidate();
        assert_eq!(binding.session_id, identity.to_string());
        session_binding::acknowledge_claude_binding(fixture.claim(&retry).unwrap());
    }

    #[test]
    fn malformed_claude_record_is_acknowledged() {
        let fixture = ClaudeFixture::new();
        let shared = fixture.shared();
        std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
        std::fs::write(&shared, b"not json").unwrap();
        let snapshot =
            session_binding::read_pending_claude_binding(&fixture.data, fixture.pane).unwrap();
        acknowledge_futile_claude_record(fixture.claim(&snapshot).unwrap(), false);
        assert!(
            session_binding::read_pending_claude_binding(&fixture.data, fixture.pane).is_none()
        );
    }
}

#[cfg(test)]
mod w8lm_tests {
    use super::{
        DiscoveryCandidate, bounded_title_command, codex_tmux_capture_session, discover_sessions,
        inject_codex_tmux_title_reader,
    };
    use muxel_core::session_discovery::{
        CodexLookupEvidence, SessionDiscoveries, SessionDiscoveryKey, SessionDiscoveryRequest,
        SessionProvider,
    };
    use muxel_core::tmux::Socket;
    use muxel_core::{AgentPreset, CodexHintMiss, CodexSession, Instance, Project, Workspace};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    fn validated(discovered: DiscoveryCandidate) -> Option<CodexSession> {
        match discovered {
            DiscoveryCandidate::Codex {
                session,
                cached_rejection: None,
                ..
            } => session,
            DiscoveryCandidate::Codex {
                cached_rejection: Some(id),
                ..
            } => panic!("cached sibling rejection is not a validated candidate: {id}"),
            _ => panic!("expected a Codex result"),
        }
    }

    fn discover_codex(
        request: SessionDiscoveryRequest,
        root: &Path,
    ) -> (
        SessionDiscoveryRequest,
        DiscoveryCandidate,
        Option<CodexHintMiss>,
        Option<String>,
    ) {
        let (request, candidate) = discover_sessions(vec![request], root, None, Default::default())
            .results
            .pop()
            .unwrap();
        let (miss, observed_hint) = match &candidate {
            DiscoveryCandidate::Codex {
                miss,
                observed_hint,
                ..
            } => (miss.clone(), observed_hint.clone()),
            _ => panic!("expected a Codex result"),
        };
        (request, candidate, miss, observed_hint)
    }

    #[test]
    #[ignore = "invoked as the child of bounded_title_command_bounds_a_stalled_process"]
    fn sleeping_title_child() {
        if let Some(path) = std::env::var_os("MUXEL_TEST_SLEEPY_TITLE_READY") {
            std::fs::write(path, "ready").unwrap();
            std::thread::sleep(Duration::from_secs(30));
        }
    }

    #[test]
    fn bounded_title_command_bounds_a_stalled_process() {
        let ready = std::env::temp_dir().join(format!("muxel-title-ready-{}", Uuid::new_v4()));
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--ignored",
                "--exact",
                "app::session_discovery::w8lm_tests::sleeping_title_child",
            ])
            .env("MUXEL_TEST_SLEEPY_TITLE_READY", &ready);
        let task =
            std::thread::spawn(move || bounded_title_command(command, Duration::from_secs(10)));
        let launch_deadline = Instant::now() + Duration::from_secs(8);
        while !ready.exists() && Instant::now() < launch_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            ready.exists(),
            "sleeping child did not start before the launch deadline"
        );
        let started = Instant::now();
        assert!(task.join().unwrap().is_none());
        assert!(started.elapsed() < Duration::from_secs(11));
        std::fs::remove_file(ready).unwrap();
    }

    fn key(root: &std::path::Path) -> SessionDiscoveryKey {
        SessionDiscoveryKey {
            pane: Uuid::new_v4(),
            provider: SessionProvider::Codex,
            launch: Instant::now(),
            child_pid: Some(1),
            binding_generation: 0,
            binding: None,
            binding_path: None,
            cwd: root.join("project"),
            hint: Some("01a1543c-9700-7000-8000-000000000001".into()),
            tmux_session: None,
            tmux_socket: None,
        }
    }

    fn write(root: &std::path::Path, key: &SessionDiscoveryKey, subagent: bool) -> PathBuf {
        std::fs::create_dir_all(&key.cwd).unwrap();
        let day = root.join(".codex/sessions/2026/10/19");
        std::fs::create_dir_all(&day).unwrap();
        let id = key.hint.as_ref().unwrap();
        let path = day.join(format!("rollout-{id}.jsonl"));
        let source = if subagent {
            serde_json::json!({"subagent":{"thread_spawn":{"parent_thread_id":"parent"}}})
        } else {
            serde_json::json!("cli")
        };
        std::fs::write(&path,serde_json::json!({"type":"session_meta","payload":{"id":id,"session_id":id,"source":source,"cwd":key.cwd}}).to_string()).unwrap();
        path
    }

    #[test]
    fn w8lm_worker_captures_a_rollout_that_appears_after_a_cached_miss() {
        let root = std::env::temp_dir().join(format!("muxel-w8lm-worker-{}", Uuid::new_v4()));
        let key = key(&root);
        let now = Instant::now();
        let mut discovery = SessionDiscoveries::default();
        for seconds in [0, 1] {
            let at = now + Duration::from_secs(seconds);
            let request = discovery.begin(key.clone(), at).unwrap();
            let (request, candidate, miss, _) = discover_codex(request, &root);
            assert!(validated(candidate).is_none());
            assert!(miss.is_some());
            assert!(discovery.finish_with_codex_miss(&request, Some(&key), at, false, miss));
        }
        let path = write(&root, &key, false);
        let request = discovery
            .begin(key.clone(), now + Duration::from_secs(3))
            .unwrap();
        assert!(request.codex_miss.is_some());
        let (_, candidate, miss, _) = discover_codex(request, &root);
        let candidate = validated(candidate).expect("new day directory must invalidate the miss");
        assert_eq!(candidate.id, key.hint.unwrap());
        assert_eq!(candidate.path, path);
        assert!(miss.is_none());
    }

    #[test]
    fn w8lm_worker_rejects_a_subagent_without_changing_the_existing_binding() {
        let root = std::env::temp_dir().join(format!("muxel-w8lm-subagent-{}", Uuid::new_v4()));
        let mut key = key(&root);
        key.binding = Some(Uuid::new_v4().to_string());
        write(&root, &key, true);
        let old = key.binding.clone();
        let request = SessionDiscoveries::default()
            .begin(key, Instant::now())
            .unwrap();
        let (request, candidate, _, _) = discover_codex(request, &root);
        assert!(validated(candidate).is_none());
        assert_eq!(request.key.binding, old);
    }

    #[test]
    fn tmux_codex_capture_reads_its_recorded_socket_then_stops_when_bound() {
        let root = std::env::temp_dir().join(format!("muxel-tmux-codex-{}", Uuid::new_v4()));
        let mut key = key(&root);
        let id = key.hint.clone().unwrap();
        let path = write(&root, &key, false);
        let mut instance = Instance::from_preset(Uuid::new_v4(), &AgentPreset::codex());
        instance.use_tmux = true;
        instance.session_started = true;
        instance.tmux_session = Some("muxel_test_owned".into());
        instance.tmux_socket = Some(Socket::Default);
        key.hint = None;
        key.tmux_session = codex_tmux_capture_session(&instance, key.hint.as_deref());
        key.tmux_socket = instance.tmux_socket;
        assert_eq!(key.tmux_session.as_deref(), Some("muxel_test_owned"));
        let _reader = inject_codex_tmux_title_reader(Arc::new(move |session, socket| {
            assert_eq!(session, "muxel_test_owned");
            assert_eq!(socket, Socket::Default);
            Some(format!("{id} | Ready"))
        }));
        let now = Instant::now();
        let mut discovery = SessionDiscoveries::default();
        let request = discovery.begin(key.clone(), now).unwrap();
        let (request, candidate, miss, _) = discover_codex(request, &root);
        let candidate = validated(candidate).expect("tmux title must resolve the rollout");
        assert_eq!(candidate.path, path);
        assert!(miss.is_none());
        assert!(discovery.finish_with_codex_miss(&request, Some(&key), now, true, miss));
        instance.session_id = Some(candidate.id);
        assert!(codex_tmux_capture_session(&instance, None).is_none());
        instance.session_id = None;
        instance.tmux_socket = None;
        assert!(codex_tmux_capture_session(&instance, None).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sibling_owned_tmux_title_keeps_polling_until_a_new_hint_is_unowned() {
        let root = std::env::temp_dir().join(format!("muxel-tmux-rebind-{}", Uuid::new_v4()));
        let mut key = key(&root);
        let owned_id = key.hint.clone().unwrap();
        let owned_path = write(&root, &key, false);
        let new_id = "01a1543c-9700-7000-8000-000000000002";
        key.hint = Some(new_id.into());
        let new_path = write(&root, &key, false);
        key.hint = None;
        key.tmux_session = Some("muxel_switching".into());
        key.tmux_socket = Some(Socket::Private);

        let project = Project::new("capture", key.cwd.clone());
        let mut owner = Instance::from_preset(project.id, &AgentPreset::codex());
        owner.session_id = Some(owned_id.clone());
        let owner_id = owner.id;
        let mut target = Instance::from_preset(project.id, &AgentPreset::codex());
        target.id = key.pane;
        target.use_tmux = true;
        target.session_started = true;
        target.tmux_session = key.tmux_session.clone();
        target.tmux_socket = key.tmux_socket;
        let workspace = Workspace {
            projects: vec![project],
            instances: vec![owner, target],
            ..Workspace::default()
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let owned_title = owned_id.clone();
        let _reader = inject_codex_tmux_title_reader(Arc::new(move |session, socket| {
            assert_eq!(session, "muxel_switching");
            assert_eq!(socket, Socket::Private);
            let id = if observed.fetch_add(1, Ordering::Relaxed) < 2 {
                &owned_title
            } else {
                new_id
            };
            Some(format!("{id} | Ready"))
        }));
        let now = Instant::now();
        let mut discoveries = SessionDiscoveries::default();
        discoveries.reconcile(&workspace);
        for seconds in [0, 1] {
            let at = now + Duration::from_secs(seconds);
            let request = discoveries.begin(key.clone(), at).unwrap();
            let (request, candidate, miss, observed_hint) = discover_codex(request, &root);
            let cached_rejection = seconds == 1;
            match candidate {
                DiscoveryCandidate::Codex {
                    session: Some(candidate),
                    ..
                } if !cached_rejection => {
                    assert_eq!(candidate.id, owned_id)
                }
                DiscoveryCandidate::Codex {
                    cached_rejection: Some(id),
                    ..
                } if cached_rejection => {
                    assert_eq!(id, owned_id)
                }
                _ => panic!("unexpected candidate state for sibling-owned title"),
            }
            assert!(miss.is_none());
            assert!(discoveries.finish_with_codex_evidence(
                &request,
                Some(&key),
                at,
                false,
                CodexLookupEvidence {
                    miss,
                    rejected_owner: Some((owner_id, owned_id.clone())),
                    observed_hint,
                    cached_rejection,
                }
            ));
            if seconds == 0 {
                std::fs::remove_file(&owned_path).unwrap();
            }
        }
        let at = now + Duration::from_secs(3);
        let request = discoveries.begin(key.clone(), at).unwrap();
        let (request, candidate, miss, observed_hint) = discover_codex(request, &root);
        let candidate = validated(candidate).expect("new title should be validated and captured");
        assert_eq!(candidate.id, new_id);
        assert_eq!(candidate.path, new_path);
        assert!(discoveries.finish_with_codex_evidence(
            &request,
            Some(&key),
            at,
            true,
            CodexLookupEvidence {
                miss,
                rejected_owner: None,
                observed_hint,
                cached_rejection: false,
            }
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cached_owned_title_cannot_bind_after_its_owner_releases_mid_lookup() {
        let root = std::env::temp_dir().join(format!("muxel-tmux-release-{}", Uuid::new_v4()));
        let mut key = key(&root);
        let owned_id = key.hint.clone().unwrap();
        let owned_path = write(&root, &key, false);
        key.hint = None;
        key.tmux_session = Some("muxel_released".into());
        key.tmux_socket = Some(Socket::Private);

        let project = Project::new("capture", key.cwd.clone());
        let mut owner = Instance::from_preset(project.id, &AgentPreset::codex());
        owner.session_id = Some(owned_id.clone());
        let owner_id = owner.id;
        let mut target = Instance::from_preset(project.id, &AgentPreset::codex());
        target.id = key.pane;
        let mut workspace = Workspace {
            projects: vec![project],
            instances: vec![owner, target],
            ..Workspace::default()
        };
        let title_id = owned_id.clone();
        let _reader = inject_codex_tmux_title_reader(Arc::new(move |session, socket| {
            assert_eq!((session, socket), ("muxel_released", Socket::Private));
            Some(format!("{title_id} | Ready"))
        }));
        let now = Instant::now();
        let mut discoveries = SessionDiscoveries::default();
        discoveries.reconcile(&workspace);

        let first = discoveries.begin(key.clone(), now).unwrap();
        let (first, discovered, miss, observed_hint) = discover_codex(first, &root);
        assert_eq!(validated(discovered).unwrap().path, owned_path.clone());
        assert!(discoveries.finish_with_codex_evidence(
            &first,
            Some(&key),
            now,
            false,
            CodexLookupEvidence {
                miss,
                rejected_owner: Some((owner_id, owned_id.clone())),
                observed_hint,
                cached_rejection: false,
            }
        ));

        std::fs::remove_file(&owned_path).unwrap();
        let at = now + Duration::from_secs(1);
        let cached = discoveries.begin(key.clone(), at).unwrap();
        let (cached, discovered, miss, observed_hint) = discover_codex(cached, &root);
        assert!(matches!(
            discovered,
            DiscoveryCandidate::Codex { cached_rejection: Some(ref id), .. } if id == &owned_id
        ));
        // Ownership changes after the worker reused A, before completion.
        workspace.instances[0].session_id = None;
        discoveries.reconcile(&workspace);
        assert!(discoveries.finish_with_codex_evidence(
            &cached,
            Some(&key),
            at,
            false,
            CodexLookupEvidence {
                miss,
                rejected_owner: None,
                observed_hint,
                cached_rejection: true,
            }
        ));
        assert!(workspace.instances[1].session_id.is_none());
        let retry = discoveries
            .begin(key.clone(), at + Duration::from_millis(1))
            .expect("released cached owner must trigger immediate revalidation");
        assert!(retry.codex_rejected_hint.is_none());
        let (_, discovered, _, _) = discover_codex(retry, &root);
        assert!(validated(discovered).is_none(), "deleted A was rebound");
        std::fs::remove_dir_all(root).unwrap();
    }
}
