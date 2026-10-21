use std::time::{Duration, Instant};

use collections::{HashMap, HashSet};
use gpui::{AppContext as _, Context, Task};
use lsp::LanguageServerId;
use settings::Settings as _;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::{
    LanguageServerSelector,
    lsp_store::{LanguageServerState, LspStore},
    project_settings::ProjectSettings,
};

const MEMORY_CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// Long enough that a restart doesn't interrupt someone who is still editing the server's files.
const IDLE_BEFORE_RESTART: Duration = Duration::from_secs(5 * 60);
const BYTES_PER_MEGABYTE: u64 = 1024 * 1024;

#[derive(Default)]
pub(super) struct MemoryLimitState {
    last_document_change: HashMap<LanguageServerId, Instant>,
    check: Option<Task<()>>,
}

impl MemoryLimitState {
    pub(super) fn document_changed(&mut self, server_id: LanguageServerId, now: Instant) {
        self.last_document_change.insert(server_id, now);
    }
}

impl LspStore {
    /// Runs while language servers do, so an idle editor doesn't wake up for it.
    pub(super) fn ensure_memory_limit_check(&mut self, cx: &mut Context<Self>) {
        let Some(local) = self.as_local_mut() else {
            return;
        };
        if local.memory_limit.check.is_some() {
            return;
        }
        local.memory_limit.check = Some(cx.spawn(async move |this, cx| {
            let mut system = System::new();
            loop {
                cx.background_executor().timer(MEMORY_CHECK_INTERVAL).await;
                let Ok(Some((limit_bytes, candidates))) =
                    this.update(cx, |this, cx| this.idle_servers_to_measure(cx))
                else {
                    return;
                };
                if candidates.is_empty() {
                    continue;
                }
                let (returned_system, over_limit) = cx
                    .background_spawn(async move {
                        let over_limit = servers_over_limit(&mut system, &candidates, limit_bytes);
                        (system, over_limit)
                    })
                    .await;
                system = returned_system;
                if over_limit.is_empty() {
                    continue;
                }
                this.update(cx, |this, cx| {
                    for (server_id, bytes) in over_limit {
                        this.restart_server_over_memory_limit(server_id, bytes, cx);
                    }
                })
                .ok();
            }
        }));
    }

    /// The servers that went unchanged long enough to be restarted, with their process IDs. `None`
    /// stops the check, once no language servers run.
    fn idle_servers_to_measure(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<(u64, Vec<(LanguageServerId, u32)>)> {
        let limit_megabytes = ProjectSettings::get_global(cx)
            .global_lsp_settings
            .restart_idle_server_above_mb;
        let now = cx.background_executor().now();
        let local = self.as_local_mut()?;
        if local.language_servers.is_empty() {
            local.memory_limit.check = None;
            return None;
        }
        let live_servers = local
            .language_servers
            .keys()
            .copied()
            .collect::<HashSet<_>>();
        local
            .memory_limit
            .last_document_change
            .retain(|server_id, _| live_servers.contains(server_id));
        if limit_megabytes == 0 {
            return Some((0, Vec::new()));
        }
        let candidates = local
            .language_servers
            .iter()
            .filter_map(|(server_id, state)| {
                let LanguageServerState::Running { server, .. } = state else {
                    return None;
                };
                let is_idle = local
                    .memory_limit
                    .last_document_change
                    .get(server_id)
                    .is_none_or(|changed_at| {
                        now.duration_since(*changed_at) >= IDLE_BEFORE_RESTART
                    });
                is_idle.then_some((*server_id, server.process_id()?))
            })
            .collect();
        Some((limit_megabytes * BYTES_PER_MEGABYTE, candidates))
    }

    fn restart_server_over_memory_limit(
        &mut self,
        server_id: LanguageServerId,
        bytes: u64,
        cx: &mut Context<Self>,
    ) {
        log::info!(
            "Restarting language server {server_id} after it grew to {} MB while idle",
            bytes / BYTES_PER_MEGABYTE
        );
        let buffers = self.buffer_store.read(cx).buffers().collect();
        self.restart_language_servers_for_buffers(
            buffers,
            HashSet::from_iter([LanguageServerSelector::Id(server_id)]),
            cx,
        );
    }
}

/// Servers such as vtsls do their work in child processes, so each server is measured with its
/// process tree.
fn servers_over_limit(
    system: &mut System,
    candidates: &[(LanguageServerId, u32)],
    limit_bytes: u64,
) -> Vec<(LanguageServerId, u64)> {
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );
    let mut children: HashMap<Pid, Vec<Pid>> = HashMap::default();
    for (pid, process) in system.processes() {
        if let Some(parent) = process.parent() {
            children.entry(parent).or_default().push(*pid);
        }
    }
    candidates
        .iter()
        .filter_map(|(server_id, process_id)| {
            let mut total_bytes = 0;
            let mut pending = vec![Pid::from_u32(*process_id)];
            while let Some(pid) = pending.pop() {
                if let Some(process) = system.process(pid) {
                    total_bytes += process.memory();
                }
                pending.extend(children.get(&pid).into_iter().flatten().copied());
            }
            (total_bytes > limit_bytes).then_some((*server_id, total_bytes))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_measures_server_with_its_children() {
        let mut system = System::new();
        let current_pid = std::process::id();
        let server_id = LanguageServerId(0);
        let over_limit = servers_over_limit(&mut system, &[(server_id, current_pid)], 1);
        assert_eq!(over_limit.len(), 1);
        assert!(over_limit[0].1 > 1);
        let under_limit = servers_over_limit(&mut system, &[(server_id, current_pid)], u64::MAX);
        assert!(under_limit.is_empty());
    }
}
