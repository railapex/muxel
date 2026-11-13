use super::{MuxelApp, is_codex_program};
use gpui::Context;
use muxel_core::CodexReadBudget;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::SystemTime;

impl MuxelApp {
    pub(super) fn refresh_codex_background(&mut self, cx: &mut Context<Self>) {
        if self.codex_subagents_in_flight {
            return;
        }
        let requests: Vec<_> = self
            .workspace
            .instances
            .iter()
            .filter_map(|instance| {
                if !is_codex_program(instance.program.as_deref())
                    || self.workspace.project(instance.project_id)?.is_remote()
                {
                    return None;
                }
                let parent = instance.session_id.clone()?;
                let path = instance.session_path.as_deref()?;
                let store = rollout_store(path)?.to_path_buf();
                let view = self.terminal_view(instance.id)?.read(cx);
                let snapshot = view.runtime_snapshot();
                if snapshot.reason == muxel_terminal::TerminalStatusReason::ProcessExit {
                    return None;
                }
                (view.child_pid().is_some()).then(|| {
                    (
                        instance.id,
                        snapshot.launch_id,
                        snapshot.binding_rev,
                        parent,
                        view.launched_at(),
                        store,
                    )
                })
            })
            .collect();
        if requests.is_empty() {
            self.codex_subagents = Default::default();
            return;
        }
        for (pane, launch, binding, _, _, _) in &requests {
            if let Some(view) = self.terminal_view(*pane).cloned() {
                view.update(cx, |view, cx| {
                    view.begin_codex_subagents(*launch, *binding, cx);
                });
            }
        }
        let mut stores: HashMap<_, Vec<_>> = HashMap::new();
        for (_, _, _, parent, launched, store) in &requests {
            stores
                .entry(store.clone())
                .or_default()
                .push((parent.clone(), *launched));
        }
        let mut index = std::mem::take(&mut self.codex_subagents);
        self.codex_subagents_in_flight = true;
        cx.spawn(async move |this, cx| {
            let (index, running, catching_up) = cx
                .background_executor()
                .spawn(async move {
                    index.retain(|store, _| stores.contains_key(store));
                    let mut running = HashSet::new();
                    let mut catching_up = HashSet::new();
                    let mut budget = CodexReadBudget::default();
                    for (store, parents) in stores {
                        let active = index.entry(store.clone()).or_default().refresh_with_budget(
                            &store,
                            &parents,
                            SystemTime::now(),
                            &mut budget,
                        );
                        running.extend(
                            active
                                .running
                                .into_iter()
                                .map(|(parent, launched)| (store.clone(), parent, launched)),
                        );
                        catching_up.extend(
                            active
                                .catching_up
                                .into_iter()
                                .map(|(parent, launched)| (store.clone(), parent, launched)),
                        );
                    }
                    (index, running, catching_up)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.codex_subagents_in_flight = false;
                this.codex_subagents = index;
                for (pane, launch, binding, parent, launched, store) in requests {
                    if !this.workspace.instance(pane).is_some_and(|instance| {
                        instance.session_id.as_ref() == Some(&parent)
                            && instance.session_path.as_deref().and_then(rollout_store)
                                == Some(store.as_path())
                    }) {
                        continue;
                    }
                    if let Some(view) = this.terminal_view(pane).cloned() {
                        let key = (store, parent, launched);
                        let active = (!catching_up.contains(&key)).then(|| running.contains(&key));
                        view.update(cx, |view, cx| {
                            view.set_codex_subagents(launch, binding, active, cx);
                        });
                    }
                }
            });
        })
        .detach();
    }
}

fn rollout_store(path: &Path) -> Option<&Path> {
    let mut ancestors = path.ancestors().skip(1);
    for digits in [2, 2, 4] {
        let name = ancestors.next()?.file_name()?.to_str()?;
        if name.len() != digits || !name.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
    }
    ancestors.next()
}
