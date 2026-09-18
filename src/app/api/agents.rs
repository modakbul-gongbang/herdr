use std::time::Duration;

use bytes::Bytes;

use crate::api::schema::{
    AgentNewParams, AgentPromptGuardedOutcome, AgentRenameParams, AgentSendKeysParams,
    AgentStartParams, AgentTarget, EventData, EventEnvelope, EventKind, PaneReadResult,
    ResponseResult,
};
use crate::app::App;

use super::responses::{encode_error, encode_error_body, encode_success};

const AGENT_PROMPT_SUBMIT_DELAY: Duration = Duration::from_millis(300);

fn agent_prompt_submit_delay(agent: crate::detect::Agent, prompt_bytes: usize) -> Duration {
    #[cfg(windows)]
    if agent == crate::detect::Agent::Codex {
        // Codex consumes Windows paste bursts at about 4 bytes/ms, then suppresses Enter briefly.
        // ponytail: best-effort ConPTY timing; remove when Codex exposes a paste-complete boundary.
        return Duration::from_millis(600 + prompt_bytes as u64 / 4);
    }
    #[cfg(not(windows))]
    let _ = (agent, prompt_bytes);
    AGENT_PROMPT_SUBMIT_DELAY
}

impl App {
    pub(super) fn handle_agent_list(&mut self, id: String) -> String {
        encode_success(
            id,
            ResponseResult::AgentList {
                agents: self.collect_agent_infos(),
            },
        )
    }

    pub(super) fn handle_agent_get(&mut self, id: String, target: AgentTarget) -> String {
        self.reconcile_managed_agent_target(&target.target);
        let agent = match self.agent_info_for_target(&target.target) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_focus(&mut self, id: String, target: AgentTarget) -> String {
        let agent = match self.focus_agent_target(&target.target) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_rename(&mut self, id: String, params: AgentRenameParams) -> String {
        let agent = match self.rename_agent_target(&params.target, params.name) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_rename_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_start(&mut self, id: String, params: AgentStartParams) -> String {
        let (agent, argv) = match self.start_agent(params) {
            Ok(started) => started,
            Err(err) => return encode_error_body(id, self.agent_start_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentStarted { agent, argv })
    }

    pub(super) fn handle_agent_new(&mut self, id: String, params: AgentNewParams) -> String {
        let created = match self.new_agent(params) {
            Ok(created) => created,
            Err(err) => {
                let body = match err {
                    super::super::agents::AgentNewError::InvalidOperation(message) => {
                        crate::api::schema::ErrorBody {
                            code: "invalid_idempotency_key".into(),
                            message,
                        }
                    }
                    super::super::agents::AgentNewError::IdempotencyConflict => {
                        crate::api::schema::ErrorBody {
                            code: "idempotency_conflict".into(),
                            message: "idempotency key was already used for a different agent.new request".into(),
                        }
                    }
                    super::super::agents::AgentNewError::ReplayUnavailable(agent_instance_id) => {
                        crate::api::schema::ErrorBody {
                            code: "idempotency_result_ended".into(),
                            message: format!("agent.new result {agent_instance_id} exists but its pane is no longer active"),
                        }
                    }
                    super::super::agents::AgentNewError::TargetUnavailable(message) => {
                        crate::api::schema::ErrorBody {
                            code: "agent_new_target_unavailable".into(),
                            message,
                        }
                    }
                    super::super::agents::AgentNewError::SpawnFailed(message) => {
                        crate::api::schema::ErrorBody {
                            code: "agent_new_spawn_failed".into(),
                            message,
                        }
                    }
                    super::super::agents::AgentNewError::Start(err) => {
                        self.agent_start_error_body(err)
                    }
                };
                return encode_error_body(id, body);
            }
        };
        if !created.replayed {
            if let Some(pane) = self.pane_info(created.ws_idx, created.pane_id) {
                self.emit_event(EventEnvelope {
                    event: EventKind::PaneCreated,
                    data: EventData::PaneCreated { pane },
                });
            }
            self.emit_layout_updated_event(created.ws_idx, created.tab_idx);
            self.emit_event(EventEnvelope {
                event: EventKind::AgentLineageChanged,
                data: EventData::AgentLineageChanged {
                    lineage: created.lineage.clone(),
                },
            });
        }
        encode_success(
            id,
            ResponseResult::AgentCreated {
                agent: created.agent,
                lineage: created.lineage,
                argv: created.argv,
                replayed: created.replayed,
            },
        )
    }

    pub(crate) fn handle_deferred_agent_api_request(
        &mut self,
        request: crate::api::schema::Request,
        respond_to: std::sync::mpsc::Sender<String>,
    ) -> bool {
        let submission = match request.method {
            crate::api::schema::Method::AgentPrompt(params) => PromptSubmission {
                target: params.target,
                text: params.text,
                wait: params.wait,
                expected_input_guard: None,
            },
            crate::api::schema::Method::AgentPromptGuarded(params) => PromptSubmission {
                target: params.target,
                text: params.text,
                wait: params.wait,
                expected_input_guard: Some(params.expected_input_guard),
            },
            _ => return false,
        };
        let expected_input_guard = submission.expected_input_guard.clone();
        let target = submission.target.clone();
        match self.queue_agent_prompt(request.id, submission) {
            Ok((id, agent, completion)) => {
                std::thread::spawn(move || {
                    let outcome = completion.recv();
                    let response = match expected_input_guard {
                        None => match outcome {
                            Ok(Ok(_)) => {
                                encode_success(id, ResponseResult::AgentPrompted { agent })
                            }
                            Ok(Err(err)) if err.kind() == std::io::ErrorKind::TimedOut => {
                                encode_error(id, "timeout", err.to_string())
                            }
                            Ok(Err(err)) => {
                                encode_error(id, "agent_prompt_failed", err.to_string())
                            }
                            Err(_) => encode_error(id, "agent_prompt_failed", "pty actor closed"),
                        },
                        Some(expected_input_guard) => encode_guarded_prompt_outcome(
                            id,
                            target,
                            expected_input_guard,
                            agent,
                            outcome,
                        ),
                    };
                    let _ = respond_to.send(response);
                });
            }
            Err(response) => {
                let _ = respond_to.send(response);
            }
        }
        true
    }

    fn queue_agent_prompt(
        &mut self,
        id: String,
        submission: PromptSubmission,
    ) -> Result<
        (
            String,
            crate::api::schema::AgentInfo,
            std::sync::mpsc::Receiver<std::io::Result<crate::pty::actor::GuardedInputOutcome>>,
        ),
        String,
    > {
        let PromptSubmission {
            target,
            text,
            wait,
            expected_input_guard,
        } = submission;
        if text.is_empty() {
            return Err(encode_error(
                id,
                "empty_agent_prompt",
                "agent prompt must not be empty",
            ));
        }
        if expected_input_guard
            .as_ref()
            .is_some_and(|guard| guard.is_empty())
        {
            return Err(encode_error(
                id,
                "empty_agent_input_guard",
                "expected agent input guard must not be empty",
            ));
        }
        let resolved = match self.resolve_agent_target(&target) {
            Ok(resolved) => resolved,
            Err(err) => return Err(encode_error_body(id, self.agent_target_error_body(err))),
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
            .cloned()
        else {
            return Err(agent_not_found(id, &target));
        };
        let Some(terminal) = self.state.terminals.get(&terminal_id) else {
            return Err(agent_not_found(id, &target));
        };
        if terminal.state == crate::detect::AgentState::Blocked {
            return Err(encode_error(
                id,
                "agent_blocked",
                format!("agent {target} is blocked and requires interactive input"),
            ));
        }
        let Some(expected_agent) = terminal.effective_known_agent() else {
            return Err(agent_not_ready(id, &target));
        };
        if terminal.managed_agent_launch_pending() {
            return Err(agent_not_ready(id, &target));
        }
        let Some(runtime) = self.lookup_runtime_sender(resolved.ws_idx, resolved.pane_id) else {
            return Err(agent_not_found(id, &target));
        };
        if !super::super::agents::runtime_hosts_agent(runtime, expected_agent) {
            return Err(encode_error(
                id,
                "agent_not_ready",
                format!("agent {target} is no longer the pane foreground process"),
            ));
        }
        let submit_delay = agent_prompt_submit_delay(expected_agent, text.len());
        #[cfg(windows)]
        let submit_deadline = wait.as_ref().and_then(|wait| wait.submission_deadline);
        #[cfg(not(windows))]
        let submit_deadline = {
            let _ = &wait;
            None
        };
        // Copilot ignores synthetic Enter after focus loss until it receives focus gained.
        let focus = if expected_agent == crate::detect::Agent::GithubCopilot {
            match crate::ghostty::encode_focus(crate::ghostty::FocusEvent::Gained) {
                Ok(focus) => Bytes::from(focus),
                Err(err) => {
                    return Err(encode_error(id, "agent_prompt_failed", err.to_string()));
                }
            }
        } else {
            Bytes::new()
        };
        let guard = match expected_input_guard {
            // A guarded submission carries the focus bytes so the guard check covers them too.
            Some(expected_guard) => Some(crate::pty::actor::GuardedSubmission {
                expected_guard,
                prefix: focus,
            }),
            None => {
                if !focus.is_empty() {
                    if let Err(err) = runtime.try_send_bytes(focus) {
                        return Err(encode_error(id, "agent_prompt_failed", err.to_string()));
                    }
                }
                None
            }
        };
        let (text, enter) = crate::app::api_helpers::encode_api_submission_parts(runtime, &text);
        let Some(agent) = self.agent_info(resolved.ws_idx, resolved.pane_id) else {
            return Err(agent_not_found(id, &target));
        };
        let completion = runtime
            .queue_user_input_submission(
                Bytes::from(text),
                Bytes::from(enter),
                submit_delay,
                submit_deadline,
                guard,
            )
            .map_err(|err| encode_error(id.clone(), "agent_prompt_failed", err.to_string()))?;
        Ok((id, agent, completion))
    }

    pub(super) fn handle_agent_read(
        &mut self,
        id: String,
        params: crate::api::schema::AgentReadParams,
    ) -> String {
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some((pane, workspace_id)) = self.lookup_runtime(resolved.ws_idx, resolved.pane_id)
        else {
            return agent_not_found(id, &params.target);
        };
        let snapshot = crate::app::api_helpers::read_terminal_snapshot(
            pane,
            params.source,
            params.format,
            params.lines,
        );

        encode_success(
            id,
            ResponseResult::PaneRead {
                read: PaneReadResult {
                    pane_id: self
                        .public_pane_id(resolved.ws_idx, resolved.pane_id)
                        .unwrap_or_else(|| params.target.clone()),
                    workspace_id,
                    tab_id: self
                        .public_tab_id(resolved.ws_idx, resolved.tab_idx)
                        .unwrap(),
                    source: params.source,
                    format: params.format,
                    text: snapshot.text,
                    revision: 0,
                    truncated: snapshot.truncated,
                },
            },
        )
    }

    pub(super) fn handle_agent_explain(&mut self, id: String, target: AgentTarget) -> String {
        let resolved = match self.resolve_agent_target(&target.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some((pane, _workspace_id)) = self.lookup_runtime(resolved.ws_idx, resolved.pane_id)
        else {
            return agent_not_found(id, &target.target);
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
        else {
            return agent_not_found(id, &target.target);
        };
        let Some(terminal) = self.state.terminals.get(terminal_id) else {
            return agent_not_found(id, &target.target);
        };
        if terminal.full_lifecycle_hook_authority_active() {
            let explain = serde_json::json!({
                "agent": terminal.effective_agent_label().unwrap_or("unknown"),
                "state": crate::detect::manifest::agent_state_label(terminal.state),
                "manifest_source": null,
                "manifest_version": null,
                "cached_remote_version": null,
                "local_override_shadowing_remote": false,
                "remote_update_status": null,
                "remote_update_error": null,
                "matched_rule": null,
                "visible_idle": false,
                "visible_blocker": false,
                "visible_working": false,
                "screen_detection_skipped": true,
                "screen_detection_skip_reason": "full_lifecycle_hook_authority",
                "skip_state_update": false,
                "skipped_update_reason": null,
                "fallback_reason": null,
                "warning": null,
                "evaluated_rules": [],
            });
            return encode_success(id, ResponseResult::AgentExplain { explain });
        }
        let Some(agent) = terminal.effective_known_agent().or(terminal.detected_agent) else {
            return encode_error(
                id,
                "agent_explain_unavailable",
                format!(
                    "agent target {} does not have a detected agent label",
                    target.target
                ),
            );
        };

        let screen = pane.detection_text();
        let osc_title = pane.agent_osc_title();
        let osc_progress = pane.agent_osc_progress();
        let explain = crate::detect::manifest::explain_with_input(
            agent,
            crate::detect::manifest::DetectionInput {
                screen: &screen,
                osc_title: &osc_title,
                osc_progress: &osc_progress,
            },
        );
        let value = crate::detect::manifest::explain_to_json_value(&explain);

        encode_success(id, ResponseResult::AgentExplain { explain: value })
    }

    pub(super) fn handle_agent_send_keys(
        &mut self,
        id: String,
        params: AgentSendKeysParams,
    ) -> String {
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
        else {
            return agent_not_found(id, &params.target);
        };
        let Some(expected_agent) = self
            .state
            .terminals
            .get(terminal_id)
            .and_then(|terminal| terminal.effective_known_agent())
        else {
            return agent_not_ready(id, &params.target);
        };
        let Some(runtime) = self.lookup_runtime_sender(resolved.ws_idx, resolved.pane_id) else {
            return agent_not_found(id, &params.target);
        };
        if !super::super::agents::runtime_hosts_agent(runtime, expected_agent) {
            return agent_not_ready(id, &params.target);
        }
        let encoded = match super::super::api_helpers::encode_api_keys(runtime, &params.keys) {
            Ok(encoded) => encoded,
            Err(key) => {
                return encode_error(id, "invalid_key", format!("unsupported key {key}"));
            }
        };
        let bytes: Vec<u8> = encoded.into_iter().flatten().collect();
        if let Err(err) = runtime.try_send_bytes(Bytes::from(bytes)) {
            return encode_error(id, "agent_send_keys_failed", err.to_string());
        }

        encode_success(id, ResponseResult::Ok {})
    }
}

fn agent_not_ready(id: String, target: &str) -> String {
    encode_error(
        id,
        "agent_not_ready",
        format!("agent {target} is not an active named agent"),
    )
}

fn agent_not_found(id: String, target: &str) -> String {
    encode_error(
        id,
        "agent_not_found",
        format!("agent target {target} not found"),
    )
}

struct PromptSubmission {
    target: String,
    text: String,
    wait: Option<crate::api::schema::AgentPromptWaitOptions>,
    /// `Some` pins delivery to one detected agent process; `None` is an unguarded prompt.
    expected_input_guard: Option<String>,
}

/// Maps a guarded submission's PTY outcome onto the wire response.
///
/// `agent` is reported only while the pane still carries the guard the caller pinned, so a
/// success never describes the process that replaced the intended target.
fn encode_guarded_prompt_outcome(
    id: String,
    target: String,
    expected_input_guard: String,
    agent: crate::api::schema::AgentInfo,
    outcome: Result<
        std::io::Result<crate::pty::actor::GuardedInputOutcome>,
        std::sync::mpsc::RecvError,
    >,
) -> String {
    let outcome = match outcome {
        Ok(Ok(crate::pty::actor::GuardedInputOutcome::Submitted)) => {
            AgentPromptGuardedOutcome::Submitted
        }
        Ok(Ok(crate::pty::actor::GuardedInputOutcome::Partial)) => {
            AgentPromptGuardedOutcome::Partial
        }
        Ok(Ok(crate::pty::actor::GuardedInputOutcome::Rejected)) => {
            return encode_error(
                id,
                "agent_input_guard_mismatch",
                format!("agent {target} changed before guarded prompt submission"),
            );
        }
        Ok(Err(err)) => {
            return encode_error(
                id,
                "agent_prompt_outcome_unknown",
                format!("guarded prompt outcome for agent {target} is unknown: {err}"),
            );
        }
        Err(_) => {
            return encode_error(
                id,
                "agent_prompt_outcome_unknown",
                format!("guarded prompt outcome for agent {target} is unknown: pty actor closed"),
            );
        }
    };
    let agent = Some(agent)
        .filter(|agent| agent.input_guard.as_deref() == Some(expected_input_guard.as_str()));
    encode_success(
        id,
        ResponseResult::AgentPromptGuarded {
            target,
            expected_input_guard,
            outcome,
            agent,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::schema::{AgentPromptGuardedParams, AgentPromptParams, AgentStatus, SuccessResponse},
        app::Mode,
        config::Config,
        detect::{Agent, AgentState},
        workspace::Workspace,
    };

    fn app_with_agent() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("agent")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        app
    }

    fn start_deferred_agent_prompt(
        app: &mut App,
        id: &str,
        params: AgentPromptParams,
    ) -> std::sync::mpsc::Receiver<String> {
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        assert!(app.handle_deferred_agent_api_request(
            crate::api::schema::Request {
                id: id.into(),
                method: crate::api::schema::Method::AgentPrompt(params),
            },
            respond_to,
        ));
        response_rx
    }

    fn run_deferred_guarded_agent_prompt(
        app: &mut App,
        id: &str,
        params: AgentPromptGuardedParams,
    ) -> String {
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        assert!(app.handle_deferred_agent_api_request(
            crate::api::schema::Request {
                id: id.into(),
                method: crate::api::schema::Method::AgentPromptGuarded(params),
            },
            respond_to,
        ));
        response_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("guarded agent prompt responds after submission")
    }

    fn run_deferred_agent_prompt(app: &mut App, id: &str, params: AgentPromptParams) -> String {
        start_deferred_agent_prompt(app, id, params)
            .recv_timeout(Duration::from_secs(1))
            .expect("agent prompt responds after submission")
    }

    #[test]
    fn prompt_delay_only_scales_for_windows_codex() {
        let codex_delay = agent_prompt_submit_delay(Agent::Codex, 4_096);
        #[cfg(windows)]
        assert_eq!(codex_delay, Duration::from_millis(1_624));
        #[cfg(not(windows))]
        assert_eq!(codex_delay, AGENT_PROMPT_SUBMIT_DELAY);
        assert_eq!(
            agent_prompt_submit_delay(Agent::OpenCode, 4_096),
            AGENT_PROMPT_SUBMIT_DELAY
        );
    }

    #[tokio::test]
    async fn agent_prompt_sends_text_then_delays_enter() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Working);
        let (runtime, mut rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80, 24, 0, b"", 2,
            );
        runtime.test_process_pty_bytes(b"\x1b[?2004h");
        app.state.insert_test_runtime(pane_id, runtime);

        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();
        let bracketed_started = std::time::Instant::now();
        let response_rx = start_deferred_agent_prompt(
            &mut app,
            "req",
            AgentPromptParams {
                target: public_pane_id,
                text: "A != B".into(),
                wait: None,
            },
        );
        assert!(response_rx.try_recv().is_err());
        let response = response_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("agent prompt responds after submission");
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentPrompted { agent, .. } = success.result else {
            panic!("expected prompted response");
        };
        assert_eq!(agent.name.as_deref(), Some("reviewer"));
        assert_eq!(
            rx.try_recv().unwrap(),
            Bytes::from_static(b"\x1b[200~A != B\x1b[201~")
        );
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
        assert!(bracketed_started.elapsed() >= AGENT_PROMPT_SUBMIT_DELAY);

        app.lookup_runtime_sender(0, pane_id)
            .unwrap()
            .test_process_pty_bytes(b"\x1b[?2004l");
        let raw_started = std::time::Instant::now();
        let raw = run_deferred_agent_prompt(
            &mut app,
            "req-raw",
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
            },
        );
        let raw: SuccessResponse = serde_json::from_str(&raw).unwrap();
        assert!(matches!(raw.result, ResponseResult::AgentPrompted { .. }));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"A != B"));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
        assert!(raw_started.elapsed() >= AGENT_PROMPT_SUBMIT_DELAY);

        let rejected = run_deferred_agent_prompt(
            &mut app,
            "req-label",
            AgentPromptParams {
                target: "opencode".into(),
                text: "wrong target".into(),
                wait: None,
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "agent_not_found");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn agent_prompt_rejects_blocked_agent_without_writing() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::GithubCopilot), AgentState::Blocked);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let response = run_deferred_agent_prompt(
            &mut app,
            "req",
            AgentPromptParams {
                target: "reviewer".into(),
                text: "unrelated prompt".into(),
                wait: None,
            },
        );

        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "agent_blocked");
        assert!(
            tokio::time::timeout(
                AGENT_PROMPT_SUBMIT_DELAY + Duration::from_millis(100),
                rx.recv()
            )
            .await
            .is_err(),
            "blocked prompt wrote or scheduled terminal input"
        );
    }

    #[tokio::test]
    async fn guarded_agent_prompt_submits_only_for_current_input_guard() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Working);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        let current_guard = runtime.observe_agent_input_guard_for_test("opencode:41");
        app.state.insert_test_runtime(pane_id, runtime);

        let rejected = run_deferred_guarded_agent_prompt(
            &mut app,
            "req-stale",
            AgentPromptGuardedParams {
                target: "reviewer".into(),
                text: "must not be delivered".into(),
                expected_input_guard: "stale".into(),
                wait: None,
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "agent_input_guard_mismatch");
        assert!(rx.try_recv().is_err());

        let submitted = run_deferred_guarded_agent_prompt(
            &mut app,
            "req-current",
            AgentPromptGuardedParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                expected_input_guard: current_guard.clone(),
                wait: None,
            },
        );
        let success: SuccessResponse = serde_json::from_str(&submitted).unwrap();
        let ResponseResult::AgentPromptGuarded {
            target,
            expected_input_guard,
            outcome,
            agent,
        } = success.result
        else {
            panic!("expected guarded prompt response");
        };
        assert_eq!(target, "reviewer");
        assert_eq!(expected_input_guard, current_guard);
        assert_eq!(outcome, AgentPromptGuardedOutcome::Submitted);
        assert_eq!(
            agent.unwrap().input_guard.as_deref(),
            Some(current_guard.as_str())
        );
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"A != B"));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn agent_prompt_focuses_copilot_before_submitting() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::GithubCopilot), AgentState::Idle);
        let (runtime, mut rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80, 24, 0, b"", 3,
            );
        runtime.test_process_pty_bytes(b"\x1b[?2004h");
        app.state.insert_test_runtime(pane_id, runtime);

        let response = run_deferred_agent_prompt(
            &mut app,
            "req",
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::AgentPrompted { .. }
        ));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\x1b[I"));
        assert_eq!(
            rx.try_recv().unwrap(),
            Bytes::from_static(b"\x1b[200~A != B\x1b[201~")
        );
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
    }

    #[tokio::test]
    async fn agent_send_keys_validates_every_key_before_writing() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let rejected = app.handle_agent_send_keys(
            "req-invalid".into(),
            AgentSendKeysParams {
                target: "reviewer".into(),
                keys: vec!["enter".into(), "not-a-key".into()],
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "invalid_key");
        assert!(rx.try_recv().is_err());

        let sent = app.handle_agent_send_keys(
            "req-valid".into(),
            AgentSendKeysParams {
                target: "reviewer".into(),
                keys: vec!["up".into(), "enter".into()],
            },
        );
        let success: SuccessResponse = serde_json::from_str(&sent).unwrap();
        assert!(matches!(success.result, ResponseResult::Ok {}));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\x1b[A\r"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn agent_prompt_rejects_managed_agent_while_startup_is_pending() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        let now = std::time::Instant::now();
        terminal.begin_managed_agent(
            "reviewer".into(),
            Agent::OpenCode,
            now,
            std::time::Duration::from_secs(3),
            std::time::Duration::from_secs(10),
        );
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let response = run_deferred_agent_prompt(
            &mut app,
            "req-pending",
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "agent_not_ready");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn agent_focus_marks_already_focused_done_agent_seen() {
        let mut app = app_with_agent();
        app.state.outer_terminal_focus = Some(false);

        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(Some(Agent::Pi), AgentState::Idle);
        app.state.workspaces[0].tabs[0]
            .panes
            .get_mut(&pane_id)
            .unwrap()
            .seen = false;
        app.state.workspaces[0].tabs[0].layout.focus_pane(pane_id);

        let response = app.handle_agent_focus(
            "req".into(),
            AgentTarget {
                target: app.public_pane_id(0, pane_id).unwrap(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentInfo { agent } = success.result else {
            panic!("expected agent info response");
        };
        assert_eq!(agent.agent_status, AgentStatus::Idle);
    }

    #[test]
    fn agent_rename_does_not_replace_the_pane_label() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_manual_label("shell-pane".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let target = app.public_pane_id(0, pane_id).unwrap();

        for name in [Some("reviewer".to_string()), None] {
            let response = app.handle_agent_rename(
                "req".into(),
                AgentRenameParams {
                    target: target.clone(),
                    name,
                },
            );
            let success: SuccessResponse = serde_json::from_str(&response).unwrap();
            assert!(matches!(success.result, ResponseResult::AgentInfo { .. }));
            assert_eq!(
                app.state.terminals[&terminal_id].manual_label.as_deref(),
                Some("shell-pane")
            );
        }
    }
}
