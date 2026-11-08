//! Background launch-folder branch snapshots, shared by sidebar and ctl.
use super::{MuxelApp, integrations};
use crate::i18n::t;
use gpui::SharedString;
use muxel_core::refresh_gate::RefreshGate;
use muxel_core::session_folder::{FolderFilter, folder_key};
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
    branch: Option<String>,
    list: Option<Vec<String>>,
    host_snapshot: Option<String>,
}

pub(super) fn probe_branches(jobs: Vec<BranchJob>) -> Vec<BranchResult> {
    jobs.into_iter()
        .map(|job| {
            let remote_root = job.is_root && matches!(job.loc, integrations::RepoLoc::Remote(_));
            let (branch, list) = if remote_root {
                // One ssh for both: the branch and the list.
                let (branch, list) = integrations::read_branches(&job.loc).unwrap_or_default();
                (branch, Some(list))
            } else {
                // A local folder's branch usually comes from its git files,
                // with no git at all; only the list costs one.
                (
                    integrations::repo_current_branch(&job.loc),
                    job.is_root.then(|| integrations::list_branches(&job.loc)),
                )
            };
            BranchResult {
                key: job.key,
                branch,
                list,
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

    pub(super) fn session_matches_filter(&self, project: &Project, instance: &Instance) -> bool {
        self.folder_filters.get(&project.id).is_none_or(|filter| {
            filter.matches(
                &Self::session_folder(project, instance),
                self.session_branch(project, instance),
                self.folder_windows(project),
            )
        })
    }

    /// The row menu's filter entry. The row that started a filter offers to clear it.
    pub(super) fn folder_filter_menu_entry(
        &self,
        project: &Project,
        instance: &Instance,
    ) -> (SharedString, FolderFilter) {
        let folder = Self::session_folder(project, instance);
        let branch = self.session_branch(project, instance);
        let windows = self.folder_windows(project);
        let filtering = self
            .folder_filters
            .get(&project.id)
            .is_some_and(|f| f.is_source(&folder, windows));
        let label = match (filtering, branch) {
            (true, _) => t("Show all sessions"),
            (false, Some(_)) => t("Show only this folder and branch"),
            (false, None) => t("Show only this folder"),
        };
        (label, FolderFilter::new(&folder, branch, windows))
    }

    pub(super) fn toggle_folder_filter(&mut self, project: Uuid, clicked: FolderFilter) {
        let mut filter = self.folder_filters.remove(&project);
        FolderFilter::toggle(&mut filter, clicked);
        if let Some(filter) = filter {
            self.folder_filters.insert(project, filter);
        }
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
        self.folder_filters.retain(|id, _| project_ids.contains(id));
        for result in results {
            if current.get(&result.key) != Some(&result.host_snapshot) {
                continue;
            }
            if let Some(list) = result.list {
                self.project_branches
                    .insert(result.key.project, result.branch.clone());
                self.project_branch_lists.insert(result.key.project, list);
            }
            self.folder_branches.insert(result.key, result.branch);
        }
    }
}
