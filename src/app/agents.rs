use std::time::{Duration, Instant};

use bytes::Bytes;
use sha2::{Digest, Sha256};

use super::{terminal_targets::TerminalTargetError, App};
use crate::api::schema::AgentStartParams;

const DEFAULT_AGENT_START_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const MAX_AGENT_START_TIMEOUT: Duration = Duration::from_secs(300);
pub(crate) const AGENT_START_SETTLE_DELAY: Duration = Duration::from_secs(3);
const INVALID_AGENT_TIMEOUT_MESSAGE: &str =
    "agent start timeout must be greater than 3000ms and at most 300000ms";
const INVALID_AGENT_NAME_MESSAGE: &str = "agent name must start with a lowercase letter and contain only lowercase letters, digits, '-' or '_' (1-32 characters)";

fn valid_agent_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('a'..='z'))
        && name.len() <= 32
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '-' | '_'))
}

impl App {
    pub(super) fn collect_agent_infos(&self) -> Vec<crate::api::schema::AgentInfo> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(ws_idx, ws)| {
                ws.tabs.iter().flat_map(move |tab| {
                    tab.layout
                        .pane_ids()
                        .into_iter()
                        .filter_map(move |pane_id| self.agent_info(ws_idx, pane_id))
                })
            })
            .collect()
    }

    pub(super) fn reconcile_managed_agent_target(&mut self, target: &str) {
        let Ok(resolved) = self.resolve_agent_target(target) else {
            return;
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
            .cloned()
        else {
            return;
        };
        let changed = self
            .state
            .terminals
            .get_mut(&terminal_id)
            .is_some_and(|terminal| terminal.reconcile_managed_agent_at(Instant::now(), false));
        if changed {
            self.state.mark_session_dirty();
            self.schedule_session_save();
            self.emit_pane_updated(resolved.ws_idx, resolved.pane_id);
        }
    }

    pub(super) fn agent_info_for_target(
        &self,
        target: &str,
    ) -> Result<crate::api::schema::AgentInfo, TerminalTargetError> {
        let resolved = self.resolve_agent_target(target)?;
        self.agent_info(resolved.ws_idx, resolved.pane_id)
            .ok_or_else(|| TerminalTargetError::NotFound {
                target: target.to_string(),
            })
    }

    pub(super) fn focus_agent_target(
        &mut self,
        target: &str,
    ) -> Result<crate::api::schema::AgentInfo, TerminalTargetError> {
        let resolved = self.resolve_agent_target(target)?;
        self.state
            .focus_pane_in_workspace(resolved.ws_idx, resolved.pane_id);
        self.state.mark_active_tab_seen();
        self.state.mode = crate::app::Mode::Terminal;
        self.agent_info(resolved.ws_idx, resolved.pane_id)
            .ok_or_else(|| TerminalTargetError::NotFound {
                target: target.to_string(),
            })
    }

    pub(super) fn rename_agent_target(
        &mut self,
        target: &str,
        name: Option<String>,
    ) -> Result<crate::api::schema::AgentInfo, AgentRenameError> {
        let resolved = self
            .resolve_agent_target(target)
            .map_err(AgentRenameError::Target)?;
        let normalized_name = match name {
            Some(name) if valid_agent_name(&name) => Some(name),
            Some(_) => return Err(AgentRenameError::InvalidName),
            None => None,
        };

        if let Some(name) = normalized_name.as_deref() {
            let conflicts = self.agent_name_conflicts(name, &resolved.terminal_id);
            if !conflicts.is_empty() {
                return Err(AgentRenameError::DuplicateName {
                    name: name.to_string(),
                    candidates: conflicts,
                });
            }
        }

        let public_pane_id = self.public_pane_id(resolved.ws_idx, resolved.pane_id);
        let has_lineage = self.state.workspaces[resolved.ws_idx]
            .agent_lineage
            .values()
            .any(|record| {
                public_pane_id
                    .as_ref()
                    .is_some_and(|pane_id| record.pane_id == *pane_id)
            });
        let Some(terminal) = self
            .state
            .terminals
            .values_mut()
            .find(|terminal| terminal.id.to_string() == resolved.terminal_id)
        else {
            return Err(AgentRenameError::Target(TerminalTargetError::NotFound {
                target: target.to_string(),
            }));
        };
        if terminal.managed_agent_launch_pending() {
            return Err(AgentRenameError::PendingLaunch);
        }
        if terminal.effective_agent_label().is_none() && !has_lineage {
            return Err(AgentRenameError::NotAgent);
        }
        match normalized_name.clone() {
            Some(name) => terminal.set_agent_name(name),
            None => terminal.clear_agent_name(),
        }
        if let Some(record) = self.state.workspaces[resolved.ws_idx]
            .agent_lineage
            .values_mut()
            .find(|record| {
                public_pane_id
                    .as_ref()
                    .is_some_and(|pane_id| record.pane_id == *pane_id)
            })
        {
            if let Some(name) = normalized_name {
                record.name = name;
            }
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();
        self.emit_pane_updated(resolved.ws_idx, resolved.pane_id);
        self.agent_info(resolved.ws_idx, resolved.pane_id)
            .ok_or_else(|| {
                AgentRenameError::Target(TerminalTargetError::NotFound {
                    target: target.to_string(),
                })
            })
    }

    pub(super) fn start_agent(
        &mut self,
        params: AgentStartParams,
    ) -> Result<(crate::api::schema::AgentInfo, Vec<String>), AgentStartError> {
        let name = params.name;
        if !valid_agent_name(&name) {
            return Err(AgentStartError::InvalidName);
        }
        let Some(kind) = crate::detect::parse_agent_label(&params.kind) else {
            return Err(AgentStartError::UnsupportedKind(params.kind));
        };
        if params
            .args
            .iter()
            .any(|arg| arg.chars().any(char::is_control))
        {
            return Err(AgentStartError::InvalidArgument);
        }
        let persisted_agent_session =
            crate::agent_resume::persisted_session_from_launch_args(kind, &params.args);
        let conflicts = self.agent_name_conflicts(&name, "");
        if !conflicts.is_empty() {
            return Err(AgentStartError::DuplicateName {
                name,
                candidates: conflicts,
            });
        }
        let Some((ws_idx, pane_id)) = self.parse_current_public_pane_id(&params.pane_id) else {
            return Err(AgentStartError::TargetNotFound(params.pane_id));
        };
        let terminal_id = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.terminal_id(pane_id))
            .cloned()
            .ok_or_else(|| AgentStartError::TargetNotFound(params.pane_id.clone()))?;
        let terminal = self
            .state
            .terminals
            .get(&terminal_id)
            .ok_or_else(|| AgentStartError::TargetNotFound(params.pane_id.clone()))?;
        if terminal.is_agent_terminal() || terminal.managed_agent_kind().is_some() {
            return Err(AgentStartError::TargetBusy(params.pane_id));
        }
        let runtime = self
            .terminal_runtimes
            .get(&terminal_id)
            .ok_or_else(|| AgentStartError::TargetUnavailable(params.pane_id.clone()))?;
        let shell_name = available_shell_name(runtime)
            .ok_or_else(|| AgentStartError::TargetBusy(params.pane_id.clone()))?;

        let mut argv = vec![crate::detect::interactive_agent_executable(kind).to_string()];
        argv.extend(params.args);
        let command = crate::platform::interactive_shell_command(&argv, &shell_name)
            .ok_or(AgentStartError::InvalidArgument)?;
        let bytes = crate::app::api_helpers::encode_api_submission(runtime, &command);
        let timeout = Duration::from_millis(
            params
                .timeout_ms
                .unwrap_or(DEFAULT_AGENT_START_TIMEOUT.as_millis() as u64),
        );
        if timeout <= AGENT_START_SETTLE_DELAY || timeout > MAX_AGENT_START_TIMEOUT {
            return Err(AgentStartError::InvalidTimeout);
        }

        let now = Instant::now();
        let terminal = self
            .state
            .terminals
            .get_mut(&terminal_id)
            .ok_or_else(|| AgentStartError::TargetUnavailable(params.pane_id.clone()))?;
        terminal.begin_managed_agent(name.clone(), kind, now, AGENT_START_SETTLE_DELAY, timeout);
        if let Err(err) = runtime.try_send_bytes(Bytes::from(bytes)) {
            terminal.clear_agent_name();
            return Err(AgentStartError::InputFailed(err.to_string()));
        }
        if let Some(session) = persisted_agent_session {
            terminal.set_managed_agent_launch_session(session);
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();

        let agent = self
            .agent_info(ws_idx, pane_id)
            .ok_or(AgentStartError::TargetUnavailable(params.pane_id))?;
        Ok((agent, argv))
    }

    pub(super) fn agent_start_error_body(
        &self,
        err: AgentStartError,
    ) -> crate::api::schema::ErrorBody {
        match err {
            AgentStartError::InvalidName => crate::api::schema::ErrorBody {
                code: "invalid_agent_name".into(),
                message: INVALID_AGENT_NAME_MESSAGE.into(),
            },
            AgentStartError::UnsupportedKind(kind) => crate::api::schema::ErrorBody {
                code: "unsupported_agent_kind".into(),
                message: format!("unsupported interactive agent kind {kind}"),
            },
            AgentStartError::InvalidArgument => crate::api::schema::ErrorBody {
                code: "invalid_agent_argument".into(),
                message: "agent arguments cannot be encoded safely for the target shell".into(),
            },
            AgentStartError::InvalidTimeout => crate::api::schema::ErrorBody {
                code: "invalid_agent_timeout".into(),
                message: INVALID_AGENT_TIMEOUT_MESSAGE.into(),
            },
            AgentStartError::TargetNotFound(target) => crate::api::schema::ErrorBody {
                code: "agent_pane_not_found".into(),
                message: format!("agent target pane {target} not found"),
            },
            AgentStartError::TargetBusy(target) => crate::api::schema::ErrorBody {
                code: "agent_pane_busy".into(),
                message: format!("agent target pane {target} is not an available shell"),
            },
            AgentStartError::TargetUnavailable(target) => crate::api::schema::ErrorBody {
                code: "agent_pane_unavailable".into(),
                message: format!("agent target pane {target} has no live terminal"),
            },
            AgentStartError::InputFailed(message) => crate::api::schema::ErrorBody {
                code: "agent_start_input_failed".into(),
                message,
            },
            AgentStartError::DuplicateName { name, candidates } => crate::api::schema::ErrorBody {
                code: "agent_name_taken".into(),
                message: format!(
                    "agent name {name} is already used; candidates: {}",
                    candidates
                        .into_iter()
                        .map(|candidate| format!(
                            "terminal_id={} pane_id={} workspace_id={} tab_id={} cwd={} status={:?}",
                            candidate.terminal_id,
                            candidate.pane_id,
                            candidate.workspace_id,
                            candidate.tab_id,
                            candidate.cwd.unwrap_or_else(|| "unknown".into()),
                            candidate.agent_status,
                        ))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
            },
        }
    }

    pub(super) fn agent_target_error_body(
        &self,
        err: TerminalTargetError,
    ) -> crate::api::schema::ErrorBody {
        match err {
            TerminalTargetError::NotFound { target } => crate::api::schema::ErrorBody {
                code: "agent_not_found".into(),
                message: format!("agent target {target} not found"),
            },
            TerminalTargetError::NotAgentBacked { target } => crate::api::schema::ErrorBody {
                code: "not_agent_backed".into(),
                message: format!("pane {target} is a terminal surface without an agent instance"),
            },
            TerminalTargetError::Ambiguous { target, candidates } => {
                crate::api::schema::ErrorBody {
                    code: "agent_target_ambiguous".into(),
                    message: format!(
                        "agent target {target} is ambiguous; candidates: {}",
                        candidates
                            .into_iter()
                            .map(|candidate| format!(
                                "terminal_id={} pane_id={} workspace_id={} tab_id={} cwd={} status={:?}",
                                candidate.terminal_id,
                                candidate.pane_id,
                                candidate.workspace_id,
                                candidate.tab_id,
                                candidate.cwd.unwrap_or_else(|| "unknown".into()),
                                candidate.agent_status,
                            ))
                            .collect::<Vec<_>>()
                            .join("; ")
                    ),
                }
            }
        }
    }

    pub(super) fn agent_rename_error_body(
        &self,
        err: AgentRenameError,
    ) -> crate::api::schema::ErrorBody {
        match err {
            AgentRenameError::Target(err) => self.agent_target_error_body(err),
            AgentRenameError::InvalidName => crate::api::schema::ErrorBody {
                code: "invalid_agent_name".into(),
                message: INVALID_AGENT_NAME_MESSAGE.into(),
            },
            AgentRenameError::NotAgent => crate::api::schema::ErrorBody {
                code: "agent_not_found".into(),
                message: "agent target does not currently host an agent".into(),
            },
            AgentRenameError::PendingLaunch => crate::api::schema::ErrorBody {
                code: "agent_launch_pending".into(),
                message: "agent name cannot change while startup is pending".into(),
            },
            AgentRenameError::DuplicateName { name, candidates } => crate::api::schema::ErrorBody {
                code: "agent_name_taken".into(),
                message: format!(
                    "agent name {name} is already used; candidates: {}",
                    candidates
                        .into_iter()
                        .map(|candidate| format!(
                            "terminal_id={} pane_id={} workspace_id={} tab_id={} cwd={} status={:?}",
                            candidate.terminal_id,
                            candidate.pane_id,
                            candidate.workspace_id,
                            candidate.tab_id,
                            candidate.cwd.unwrap_or_else(|| "unknown".into()),
                            candidate.agent_status,
                        ))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
            },
        }
    }

    pub(super) fn agent_info(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<crate::api::schema::AgentInfo> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let pane_state = ws.pane_state(pane_id)?;
        let terminal = self.state.terminals.get(&pane_state.attached_terminal_id)?;
        let lineage = self.agent_lineage_for_pane(ws_idx, pane_id);
        if !terminal.is_agent_terminal() && lineage.is_none() {
            return None;
        }
        let pane = self.pane_info(ws_idx, pane_id)?;
        let ambient = self.ambient_for_pane(
            &pane_state.attached_terminal_id,
            pane.agent_session.as_ref(),
            pane.cwd.as_deref(),
        );
        Some(crate::api::schema::AgentInfo {
            agent_instance_id: lineage.map(|record| record.agent_instance_id.clone()),
            parent_agent_instance_id: lineage
                .and_then(|record| record.parent_agent_instance_id.clone()),
            spawned_from_pane_id: lineage.and_then(|record| record.spawned_from_pane_id.clone()),
            terminal_id: terminal.id.to_string(),
            name: terminal
                .agent_name
                .clone()
                .or_else(|| lineage.map(|record| record.name.clone())),
            agent: pane
                .agent
                .or_else(|| lineage.map(|record| record.kind.clone())),
            title: pane.title,
            terminal_title: pane.terminal_title,
            terminal_title_stripped: pane.terminal_title_stripped,
            display_agent: pane.display_agent,
            agent_status: pane.agent_status,
            screen_detection_skipped: terminal.full_lifecycle_hook_authority_active(),
            state_labels: pane.state_labels,
            tokens: pane.tokens,
            agent_session: pane.agent_session,
            workspace_id: pane.workspace_id,
            tab_id: pane.tab_id,
            pane_id: pane.pane_id,
            focused: pane.focused,
            launch_pending: terminal.managed_agent_launch_pending(),
            interactive_ready: terminal.managed_agent_interactive_ready(),
            state_change_seq: terminal.last_agent_state_change_seq.unwrap_or(0),
            cwd: pane.cwd,
            foreground_cwd: pane.foreground_cwd,
            revision: pane.revision,
            ambient,
        })
    }

    pub(super) fn agent_lineage_for_pane(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<&crate::workspace::AgentLineageRecord> {
        let public_pane_id = self.public_pane_id(ws_idx, pane_id)?;
        self.state
            .workspaces
            .get(ws_idx)?
            .agent_lineage
            .values()
            .find(|record| record.pane_id == public_pane_id)
    }

    pub(super) fn collect_agent_lineage(&self) -> Vec<crate::api::schema::AgentLineageInfo> {
        let mut lineage = self
            .state
            .workspaces
            .iter()
            .flat_map(|workspace| {
                workspace
                    .agent_lineage
                    .values()
                    .map(move |record| self.agent_lineage_info(workspace, record))
            })
            .collect::<Vec<_>>();
        lineage.sort_by(|left, right| left.agent_instance_id.cmp(&right.agent_instance_id));
        lineage
    }

    fn agent_lineage_info(
        &self,
        workspace: &crate::workspace::Workspace,
        record: &crate::workspace::AgentLineageRecord,
    ) -> crate::api::schema::AgentLineageInfo {
        let active = self
            .parse_pane_id(&record.pane_id)
            .is_some_and(|(ws_idx, pane_id)| {
                self.agent_info_without_lineage(ws_idx, pane_id).is_some()
            });
        let parent_exists = record
            .parent_agent_instance_id
            .as_ref()
            .is_none_or(|parent| {
                self.state
                    .workspaces
                    .iter()
                    .any(|ws| ws.agent_lineage.contains_key(parent))
            });
        crate::api::schema::AgentLineageInfo {
            agent_instance_id: record.agent_instance_id.clone(),
            idempotency_key: record.idempotency_key.clone(),
            name: record.name.clone(),
            kind: record.kind.clone(),
            host: crate::api::host_scope(),
            workspace_id: workspace.id.clone(),
            tab_id: record.tab_id.clone(),
            pane_id: record.pane_id.clone(),
            parent_agent_instance_id: record.parent_agent_instance_id.clone(),
            spawned_from_pane_id: record.spawned_from_pane_id.clone(),
            state: if !parent_exists {
                crate::api::schema::AgentLineageState::Orphaned
            } else if active {
                crate::api::schema::AgentLineageState::Active
            } else {
                crate::api::schema::AgentLineageState::Ended
            },
        }
    }

    fn agent_info_without_lineage(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<()> {
        let workspace = self.state.workspaces.get(ws_idx)?;
        let pane = workspace.pane_state(pane_id)?;
        let terminal = self.state.terminals.get(&pane.attached_terminal_id)?;
        (terminal.is_agent_terminal() || self.agent_lineage_for_pane(ws_idx, pane_id).is_some())
            .then_some(())
    }

    pub(super) fn new_agent(
        &mut self,
        params: crate::api::schema::AgentNewParams,
    ) -> Result<AgentNewResult, AgentNewError> {
        params
            .operation
            .validate()
            .map_err(|message| AgentNewError::InvalidOperation(message.to_string()))?;
        let fingerprint = agent_new_fingerprint(&params);
        for (ws_idx, workspace) in self.state.workspaces.iter().enumerate() {
            if let Some(record) = workspace
                .agent_lineage
                .values()
                .find(|record| record.idempotency_key == params.operation.idempotency_key)
            {
                if record.request_fingerprint != fingerprint {
                    return Err(AgentNewError::IdempotencyConflict);
                }
                let Some((record_ws_idx, pane_id)) = self.parse_pane_id(&record.pane_id) else {
                    return Err(AgentNewError::ReplayUnavailable(
                        record.agent_instance_id.clone(),
                    ));
                };
                if record_ws_idx != ws_idx {
                    return Err(AgentNewError::ReplayUnavailable(
                        record.agent_instance_id.clone(),
                    ));
                }
                let agent = self.agent_info(record_ws_idx, pane_id).ok_or_else(|| {
                    AgentNewError::ReplayUnavailable(record.agent_instance_id.clone())
                })?;
                let lineage = self.agent_lineage_info(workspace, record);
                return Ok(AgentNewResult {
                    ws_idx,
                    tab_idx: self.state.workspaces[ws_idx]
                        .find_tab_index_for_pane(pane_id)
                        .unwrap_or(0),
                    pane_id,
                    agent,
                    lineage,
                    argv: record.argv.clone(),
                    replayed: true,
                });
            }
        }

        if !valid_agent_name(&params.name) {
            return Err(AgentNewError::Start(AgentStartError::InvalidName));
        }
        let Some(kind) = crate::detect::parse_agent_label(&params.kind) else {
            return Err(AgentNewError::Start(AgentStartError::UnsupportedKind(
                params.kind,
            )));
        };
        if params
            .args
            .iter()
            .any(|arg| arg.chars().any(char::is_control))
        {
            return Err(AgentNewError::Start(AgentStartError::InvalidArgument));
        }
        let conflicts = self.agent_name_conflicts(&params.name, "");
        if !conflicts.is_empty() {
            return Err(AgentNewError::Start(AgentStartError::DuplicateName {
                name: params.name,
                candidates: conflicts,
            }));
        }
        let timeout = Duration::from_millis(
            params
                .timeout_ms
                .unwrap_or(DEFAULT_AGENT_START_TIMEOUT.as_millis() as u64),
        );
        if timeout <= AGENT_START_SETTLE_DELAY || timeout > MAX_AGENT_START_TIMEOUT {
            return Err(AgentNewError::Start(AgentStartError::InvalidTimeout));
        }
        let Some((ws_idx, target_pane_id)) =
            self.parse_current_public_pane_id(&params.target_pane_id)
        else {
            return Err(AgentNewError::Start(AgentStartError::TargetNotFound(
                params.target_pane_id,
            )));
        };
        let spawned_from_pane_id = params
            .spawned_from_pane_id
            .clone()
            .or_else(|| self.public_pane_id(ws_idx, target_pane_id))
            .ok_or_else(|| AgentNewError::TargetUnavailable("source pane not found".into()))?;
        let parent_agent_instance_id = self
            .state
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.agent_lineage.values())
            .find(|record| record.pane_id == spawned_from_pane_id)
            .map(|record| record.agent_instance_id.clone());

        let mut argv = vec![crate::detect::interactive_agent_executable(kind).to_string()];
        argv.extend(params.args.clone());
        let (rows, cols) = self.state.estimate_pane_size();
        let cwd = params.cwd.map(std::path::PathBuf::from).or_else(|| {
            let follow = self.launch_cwd_for_pane_in_workspace(ws_idx, target_pane_id);
            Some(self.resolve_new_terminal_cwd(follow))
        });
        let direction = match params.direction {
            crate::api::schema::SplitDirection::Right => ratatui::layout::Direction::Horizontal,
            crate::api::schema::SplitDirection::Down => ratatui::layout::Direction::Vertical,
        };
        let previous_focus = self.state.current_pane_focus_target();
        let split = self.state.workspaces[ws_idx]
            .split_pane_argv_command(
                target_pane_id,
                direction,
                rows,
                cols,
                cwd,
                &argv,
                Vec::new(),
                self.state.pane_scrollback_limit_bytes,
                self.state.host_terminal_theme,
                self.state.host_terminal_appearance,
                params.focus,
            )
            .ok_or_else(|| AgentNewError::TargetUnavailable("target pane not found".into()))?
            .map_err(|error| AgentNewError::SpawnFailed(error.to_string()))?;
        let (tab_idx, mut new_pane) = split;
        // Unlike agent.start, the argv process is the pane's direct child. A
        // successful PTY spawn is therefore the atomic start boundary; there
        // is no intermediate shell command whose foreground acquisition must
        // be polled before the lineage record can commit.
        new_pane
            .terminal
            .restore_managed_agent(params.name.clone(), kind);
        let pane_id = new_pane.pane_id;
        let terminal_id = new_pane.terminal.id.clone();
        self.terminal_runtimes
            .insert(terminal_id.clone(), new_pane.runtime);
        self.state.terminals.insert(terminal_id, new_pane.terminal);
        self.state.remove_alias_shadowed_by_new_pane(pane_id);
        if params.focus {
            self.state.switch_workspace_tab(ws_idx, tab_idx);
            self.state
                .record_pane_focus_change(previous_focus, ws_idx, pane_id);
            self.state.mode = crate::app::Mode::Terminal;
        }
        let pane_public_id = self.public_pane_id(ws_idx, pane_id).ok_or_else(|| {
            AgentNewError::TargetUnavailable("new pane identity unavailable".into())
        })?;
        let tab_id = self.public_tab_id(ws_idx, tab_idx).ok_or_else(|| {
            AgentNewError::TargetUnavailable("new tab identity unavailable".into())
        })?;
        let agent_instance_id = new_agent_instance_id();
        let record = crate::workspace::AgentLineageRecord {
            agent_instance_id: agent_instance_id.clone(),
            idempotency_key: params.operation.idempotency_key,
            request_fingerprint: fingerprint,
            name: params.name,
            kind: crate::detect::agent_label(kind).to_string(),
            tab_id,
            pane_id: pane_public_id,
            parent_agent_instance_id,
            spawned_from_pane_id: Some(spawned_from_pane_id),
            argv: argv.clone(),
        };
        self.state.workspaces[ws_idx]
            .agent_lineage
            .insert(agent_instance_id, record.clone());
        self.state.mark_session_dirty();
        self.schedule_session_save();
        let agent = self
            .agent_info(ws_idx, pane_id)
            .ok_or_else(|| AgentNewError::TargetUnavailable("new agent unavailable".into()))?;
        let lineage = self.agent_lineage_info(&self.state.workspaces[ws_idx], &record);
        Ok(AgentNewResult {
            ws_idx,
            tab_idx,
            pane_id,
            agent,
            lineage,
            argv,
            replayed: false,
        })
    }

    /// Populates `AgentInfo.ambient` when the ambient reader is enabled
    /// (`[experimental] ambient_reader`) and this pane has an identifiable
    /// Claude/Codex session. Reads only from this pane's own process HOME
    /// (never a broad account-home scan, per D-11) and never when the
    /// reader is disabled (per D-20 - no session file is opened at all).
    fn ambient_for_pane(
        &self,
        terminal_id: &crate::terminal::TerminalId,
        session: Option<&crate::api::schema::AgentSessionInfo>,
        cwd: Option<&str>,
    ) -> Option<crate::api::schema::AmbientInfo> {
        if !self.state.ambient_reader_enabled {
            return None;
        }
        let session = session?;
        if session.kind != crate::agent_resume::AgentSessionRefKind::Id {
            return None;
        }
        let shell_pid = self.terminal_runtimes.get(terminal_id)?.child_pid()?;
        let job = crate::detect::foreground_job(shell_pid)?;
        let (_, agent_pid) = crate::detect::identify_agent_pid_in_job(&job)?;
        let home = crate::platform::process_home(agent_pid)?;
        super::ambient::compute_ambient(
            &self.ambient_reader,
            &session.agent,
            &session.value,
            cwd,
            &home,
        )
    }

    fn agent_name_conflicts(
        &self,
        name: &str,
        except_terminal_id: &str,
    ) -> Vec<crate::api::schema::AgentInfo> {
        self.collect_agent_infos()
            .into_iter()
            .filter(|agent| {
                agent.name.as_deref() == Some(name) && agent.terminal_id != except_terminal_id
            })
            .collect()
    }
}

pub(super) struct AgentNewResult {
    pub ws_idx: usize,
    pub tab_idx: usize,
    pub pane_id: crate::layout::PaneId,
    pub agent: crate::api::schema::AgentInfo,
    pub lineage: crate::api::schema::AgentLineageInfo,
    pub argv: Vec<String>,
    pub replayed: bool,
}

pub(super) enum AgentNewError {
    InvalidOperation(String),
    IdempotencyConflict,
    ReplayUnavailable(String),
    TargetUnavailable(String),
    SpawnFailed(String),
    Start(AgentStartError),
}

fn agent_new_fingerprint(params: &crate::api::schema::AgentNewParams) -> String {
    let value = serde_json::json!({
        "name": params.name,
        "kind": params.kind,
        "target_pane_id": params.target_pane_id,
        "direction": params.direction,
        "focus": params.focus,
        "cwd": params.cwd,
        "spawned_from_pane_id": params.spawned_from_pane_id,
        "args": params.args,
        "timeout_ms": params.timeout_ms,
    });
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&value).unwrap_or_default())
    )
}

fn new_agent_instance_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let counter = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("agent-{nanos:x}-{counter:x}")
}

fn available_shell_name(runtime: &crate::terminal::TerminalRuntime) -> Option<String> {
    #[cfg(test)]
    if runtime.child_pid().is_none() {
        return Some("sh".into());
    }
    crate::platform::available_pane_shell(runtime.child_pid()?)
}

pub(super) fn runtime_hosts_agent(
    runtime: &crate::terminal::TerminalRuntime,
    expected: crate::detect::Agent,
) -> bool {
    #[cfg(test)]
    if runtime.child_pid().is_none() {
        return true;
    }
    live_runtime_agent(runtime) == Some(expected)
}

fn live_runtime_agent(runtime: &crate::terminal::TerminalRuntime) -> Option<crate::detect::Agent> {
    let job = crate::detect::foreground_job(runtime.child_pid()?)?;
    crate::detect::identify_agent_in_job(&job)
        .map(|(agent, _)| agent)
        .or_else(|| {
            job.processes
                .iter()
                .find_map(|process| crate::platform::process_agent_hint(process.pid))
        })
}

pub(super) enum AgentStartError {
    InvalidName,
    UnsupportedKind(String),
    InvalidArgument,
    InvalidTimeout,
    TargetNotFound(String),
    TargetBusy(String),
    TargetUnavailable(String),
    InputFailed(String),
    DuplicateName {
        name: String,
        candidates: Vec<crate::api::schema::AgentInfo>,
    },
}

pub(super) enum AgentRenameError {
    Target(TerminalTargetError),
    InvalidName,
    NotAgent,
    PendingLaunch,
    DuplicateName {
        name: String,
        candidates: Vec<crate::api::schema::AgentInfo>,
    },
}

#[cfg(test)]
mod tests {
    use super::valid_agent_name;

    #[test]
    fn agent_names_use_a_small_cli_safe_grammar() {
        for name in ["a", "reviewer-one", "reviewer_2", &"a".repeat(32)] {
            assert!(valid_agent_name(name), "expected {name:?} to be valid");
        }
        for name in [
            "",
            " reviewer",
            "reviewer ",
            "reviewer one",
            "Reviewer",
            "1reviewer",
            "reviewer.one",
            &"a".repeat(33),
        ] {
            assert!(!valid_agent_name(name), "expected {name:?} to be invalid");
        }
    }
}
