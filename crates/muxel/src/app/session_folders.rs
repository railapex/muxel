//! Background launch-folder branch snapshots, shared by sidebar and ctl.
use super::{MuxelApp, integrations};
use crate::i18n::t;
use gpui::{App, Context, SharedString, Window};
use muxel_core::refresh_gate::RefreshGate;
use muxel_core::session_folder::{FilterRow, RowFilter, folder_key};
use muxel_core::{Instance, Project, RemoteOs};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct FolderKey {
    project: Uuid,
    root: String,
    host: Option<Uuid>,
    folder: String,
}

pub(super) struct BranchJob {
    key: FolderKey,
    loc: integrations::RepoLoc,
    host_snapshot: Option<String>,
    is_root: bool,
}

pub(super) struct BranchResult {
    key: FolderKey,
    /// Another folder's branch.
    branch: Option<String>,
    host_snapshot: Option<String>,
    /// A project root's read of its branch and list. A remote one that fails
    /// keeps what it showed.
    root_read: Option<integrations::BranchRead>,
}

pub(super) fn probe_branches(jobs: Vec<BranchJob>) -> Vec<BranchResult> {
    jobs.into_iter()
        .map(|job| {
            // A project's root reads its branch and list with one git (one ssh
            // for a remote one). Another folder's branch usually comes from its
            // git files, with no git at all.
            let (branch, root_read) = if job.is_root {
                (None, Some(integrations::read_branches(&job.loc)))
            } else {
                (integrations::repo_current_branch(&job.loc), None)
            };
            BranchResult {
                branch,
                root_read,
                key: job.key,
                host_snapshot: job.host_snapshot,
            }
        })
        .collect()
}

/// The local jobs whose HEAD or branches moved, or whose backstop passed. Reading
/// the stamp costs file reads; the git processes it avoids are the expensive part.
pub(super) fn due_local_jobs(
    jobs: Vec<BranchJob>,
    gate: &mut RefreshGate<FolderKey, integrations::GitRefStamp>,
    now: Instant,
) -> Vec<BranchJob> {
    let keys: HashSet<FolderKey> = jobs.iter().map(|job| job.key.clone()).collect();
    gate.retain(|key| keys.contains(key));
    jobs.into_iter()
        .filter(|job| {
            let integrations::RepoLoc::Local(dir) = &job.loc else {
                return true;
            };
            let stamp = integrations::git_ref_stamp(dir);
            if !gate.due(&job.key, &stamp, now) {
                return false;
            }
            gate.record(job.key.clone(), stamp, now);
            true
        })
        .collect()
}

impl MuxelApp {
    pub(super) fn folder_windows(&self, project: &Project) -> bool {
        match &project.remote {
            None => cfg!(windows),
            Some(r) => self
                .remotes
                .iter()
                .find(|h| h.id == r.host_id)
                .is_some_and(|h| h.os == RemoteOs::Windows),
        }
    }

    pub(super) fn project_folder(project: &Project) -> String {
        project
            .remote
            .as_ref()
            .map(|r| r.remote_root.clone())
            .unwrap_or_else(|| project.root_path.to_string_lossy().into_owned())
    }

    pub(super) fn session_folder(project: &Project, instance: &Instance) -> String {
        instance
            .working_directory()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| Self::project_folder(project))
    }

    fn folder_key(&self, project: &Project, folder: &str) -> FolderKey {
        let windows = self.folder_windows(project);
        FolderKey {
            project: project.id,
            root: folder_key(&Self::project_folder(project), windows),
            host: project.remote.as_ref().map(|r| r.host_id),
            folder: folder_key(folder, windows),
        }
    }

    pub(super) fn session_branch(&self, project: &Project, instance: &Instance) -> Option<&str> {
        self.folder_branches
            .get(&self.folder_key(project, &Self::session_folder(project, instance)))
            .and_then(|b| b.as_deref())
    }

    /// The sidebar's filter box, read.
    pub(super) fn row_filter(&self, cx: &App) -> RowFilter {
        RowFilter::parse(&self.sidebar_filter.read(cx).value())
    }

    /// Whether `filter` leaves row `iid`, `instance` in `project`: by the name
    /// the row shows, its launch folder and that folder's cached branch.
    pub(super) fn row_matches(
        &self,
        filter: &RowFilter,
        project: &Project,
        iid: Uuid,
        instance: &Instance,
        cx: &App,
    ) -> bool {
        filter.is_empty()
            || filter.matches(FilterRow {
                project: &project.name,
                name: &self.sidebar_row_title(iid, Some(instance), cx),
                folder: &Self::session_folder(project, instance),
                branch: self.session_branch(project, instance),
                windows: self.folder_windows(project),
            })
    }

    /// The row menu's filter entry: its label, and the text it puts in the
    /// box. The row whose folder the box names offers to clear it (`None`).
    pub(super) fn folder_filter_menu_entry(
        &self,
        filter: &RowFilter,
        project: &Project,
        instance: &Instance,
    ) -> (SharedString, Option<String>) {
        let folder = Self::session_folder(project, instance);
        let branch = self.session_branch(project, instance);
        if filter.names_folder(&project.name, &folder, self.folder_windows(project)) {
            return (t("Show all sessions"), None);
        }
        let label = match branch {
            Some(_) => t("Show only this folder and branch"),
            None => t("Show only this folder"),
        };
        (
            label,
            Some(RowFilter::for_folder(&project.name, &folder, branch)),
        )
    }

    /// Put `text` in the sidebar's filter box, or empty it.
    pub(super) fn set_sidebar_filter(
        &mut self,
        text: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_filter.update(cx, |input, cx| {
            input.set_value(text.unwrap_or_default(), window, cx)
        });
        cx.notify();
    }

    /// One probe per project/folder context, with the root sharing its existing read.
    pub(super) fn branch_jobs(&self, remote: bool) -> Vec<BranchJob> {
        let mut jobs = Vec::new();
        let mut instances: HashMap<Uuid, Vec<&Instance>> = HashMap::new();
        for instance in &self.workspace.instances {
            if instance.kind == muxel_core::InstanceKind::Terminal {
                instances
                    .entry(instance.project_id)
                    .or_default()
                    .push(instance);
            }
        }
        for project in self
            .workspace
            .projects
            .iter()
            .filter(|p| p.is_remote() == remote)
        {
            let host = match &project.remote {
                Some(r) => {
                    let Some(host) = self.remotes.iter().find(|h| h.id == r.host_id) else {
                        continue;
                    };
                    Some(host.effective(&self.identities))
                }
                None => None,
            };
            let host_snapshot = host
                .as_ref()
                .map(|h| serde_json::to_string(h).expect("serializable host"));
            let root = Self::project_folder(project);
            let mut seen = HashSet::new();
            for folder in std::iter::once(root).chain(
                instances
                    .remove(&project.id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|i| Self::session_folder(project, i)),
            ) {
                let key = self.folder_key(project, &folder);
                if !seen.insert(key.clone()) {
                    continue;
                }
                let loc = if let Some(host) = &host {
                    integrations::RepoLoc::remote(
                        host.clone(),
                        folder,
                        Self::commands_control_path(host.id),
                        None,
                    )
                } else {
                    integrations::RepoLoc::Local(PathBuf::from(folder))
                };
                jobs.push(BranchJob {
                    key,
                    loc,
                    host_snapshot: host_snapshot.clone(),
                    is_root: seen.len() == 1,
                });
            }
        }
        jobs
    }

    pub(super) fn apply_folder_branches(&mut self, results: Vec<BranchResult>, remote: bool) {
        // Revalidate host, root and folder membership after asynchronous I/O.
        let current: HashMap<_, _> = self
            .branch_jobs(remote)
            .into_iter()
            .map(|job| (job.key, job.host_snapshot))
            .collect();
        let project_ids: HashSet<_> = self.workspace.projects.iter().map(|p| p.id).collect();
        self.folder_branches.retain(|key, _| {
            project_ids.contains(&key.project)
                && (key.host.is_some() != remote || current.contains_key(key))
        });
        let mut root_reads = Vec::new();
        for result in results {
            if current.get(&result.key) != Some(&result.host_snapshot) {
                continue;
            }
            if let Some(read) = result.root_read {
                if let integrations::BranchRead::Read(branch, _) = &read {
                    self.folder_branches
                        .insert(result.key.clone(), branch.clone());
                }
                match result.key.host {
                    Some(host) => {
                        let name = self
                            .remotes
                            .iter()
                            .find(|h| h.id == host)
                            .map(|h| h.name.clone())
                            .unwrap_or_default();
                        root_reads.push((result.key.project, host, name, read));
                    }
                    None => {
                        // Only a remote read comes back without a branch.
                        let (branch, list) = match read {
                            integrations::BranchRead::Read(branch, list) => (branch, list),
                            _ => (None, Vec::new()),
                        };
                        self.project_branches.insert(result.key.project, branch);
                        self.project_branch_lists.insert(result.key.project, list);
                    }
                }
                continue;
            }
            self.folder_branches.insert(result.key, result.branch);
        }
        let notices = super::land_branch_reads(
            &mut self.project_branches,
            &mut self.project_branch_lists,
            &mut self.branch_read_failures,
            root_reads,
        );
        for (title, detail) in notices {
            self.add_event(super::NotifKind::Error, title, detail);
        }
    }
}
