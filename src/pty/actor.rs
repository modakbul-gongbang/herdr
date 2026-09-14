use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;

static NEXT_AGENT_INPUT_GUARD: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct AgentInputGuardState {
    key: Option<String>,
    token: Option<String>,
    process_group_id: Option<u32>,
}

#[derive(Clone, Default)]
pub(crate) struct AgentInputGuard(Arc<Mutex<AgentInputGuardState>>);

impl AgentInputGuard {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn observe(&self, key: String) -> String {
        self.observe_owner(key, None)
    }

    pub(crate) fn observe_process(&self, agent: &str, process_group_id: u32) -> String {
        self.observe_owner(
            format!("{agent}:{process_group_id}"),
            Some(process_group_id),
        )
    }

    pub(crate) fn rotate_process(&self, agent: &str, process_group_id: u32) -> String {
        self.clear();
        self.observe_process(agent, process_group_id)
    }

    fn observe_owner(&self, key: String, process_group_id: Option<u32>) -> String {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.key.as_deref() == Some(key.as_str()) {
            if let Some(token) = state.token.clone() {
                state.process_group_id = process_group_id;
                return token;
            }
        }
        let micros = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_micros())
            .unwrap_or(0);
        let counter = NEXT_AGENT_INPUT_GUARD.fetch_add(1, Ordering::Relaxed);
        let token = format!("agent_guard_{micros:x}{counter:x}");
        state.key = Some(key);
        state.token = Some(token.clone());
        state.process_group_id = process_group_id;
        token
    }

    pub(crate) fn clear(&self) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.key = None;
        state.token = None;
        state.process_group_id = None;
    }

    pub(crate) fn current(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .token
            .clone()
    }

    #[cfg_attr(all(unix, not(test)), allow(dead_code))]
    /// Only tests compare without acting; production writes hold the lock across the write
    /// through `with_matching` / `with_matching_process`.
    #[cfg(test)]
    pub(crate) fn matches(&self, expected: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .token
            .as_deref()
            == Some(expected)
    }

    #[cfg(any(test, windows))]
    fn with_matching<R>(&self, expected: &str, action: impl FnOnce() -> R) -> Option<R> {
        let state = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (state.token.as_deref() == Some(expected)).then(action)
    }

    #[cfg(unix)]
    pub(crate) fn matches_process(&self, expected: &str, process_group_id: Option<u32>) -> bool {
        let state = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.token.as_deref() == Some(expected)
            && state
                .process_group_id
                .is_none_or(|expected_process| process_group_id == Some(expected_process))
    }

    #[cfg(unix)]
    pub(crate) fn with_matching_process<R>(
        &self,
        expected: &str,
        process_group_id: Option<u32>,
        action: impl FnOnce() -> R,
    ) -> Option<R> {
        let state = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let matches = state.token.as_deref() == Some(expected)
            && state
                .process_group_id
                .is_none_or(|expected_process| process_group_id == Some(expected_process));
        matches.then(action)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GuardedInputOutcome {
    Submitted,
    Rejected,
    Partial,
}

/// Pins a submission to one detected agent process.
///
/// `prefix` carries bytes that must share the submission's guard check, such as the focus
/// event Copilot needs before it will accept a synthetic Enter.
pub(crate) struct GuardedSubmission {
    pub(crate) expected_guard: String,
    pub(crate) prefix: Bytes,
}

#[cfg(test)]
mod guard_tests {
    use super::AgentInputGuard;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn input_guard_is_stable_for_same_process_and_rotates_for_replacement() {
        let guard = AgentInputGuard::default();
        let working = guard.observe("codex:41".into());
        let idle = guard.observe("codex:41".into());
        let replacement = guard.observe("codex:42".into());
        let same_process_replacement = guard.rotate_process("codex", 42);

        assert_eq!(working, idle);
        assert_ne!(working, replacement);
        assert_ne!(replacement, same_process_replacement);
        assert!(guard.matches(&same_process_replacement));
        assert!(!guard.matches(&working));
    }

    #[test]
    fn observed_rotation_waits_for_matching_input_boundary() {
        let guard = AgentInputGuard::default();
        let expected = guard.observe("codex:41".into());
        let guarded = guard.clone();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let input = std::thread::spawn(move || {
            guarded.with_matching(&expected, || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
        });
        entered_rx.recv().unwrap();

        let rotating = guard.clone();
        let (rotated_tx, rotated_rx) = mpsc::channel();
        let rotation = std::thread::spawn(move || {
            let token = rotating.rotate_process("codex", 42);
            rotated_tx.send(token).unwrap();
        });
        assert!(rotated_rx.recv_timeout(Duration::from_millis(50)).is_err());

        release_tx.send(()).unwrap();
        assert_eq!(input.join().unwrap(), Some(()));
        let replacement = rotated_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        rotation.join().unwrap();
        assert!(guard.matches(&replacement));
    }
}

#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(windows)]
mod windows {
    use std::io::{Read, Write};
    use std::sync::{mpsc as std_mpsc, Arc, Mutex};
    use std::time::{Duration, Instant};

    use bytes::Bytes;
    use portable_pty::{MasterPty, PtySize};
    use tokio::sync::mpsc;
    use tracing::{debug, warn};

    use super::{AgentInputGuard, GuardedInputOutcome, GuardedSubmission};

    pub(crate) struct PtyReadResult {
        pub terminal_responses: Vec<Bytes>,
    }

    type ReadCallback = Box<dyn FnMut(&[u8]) -> PtyReadResult + Send + 'static>;
    type ReaderExitCallback = Box<dyn FnOnce() + Send + 'static>;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct PtyResize {
        rows: u16,
        cols: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    }

    struct PtyResizeRequest {
        resize: PtyResize,
        terminal_responses: Vec<Bytes>,
    }

    pub(crate) struct PtyIoActorConfig {
        pub pane_id: u32,
        pub master: Box<dyn MasterPty + Send>,
        pub initially_quiesced: bool,
        pub on_read: ReadCallback,
        pub on_reader_exit: Option<ReaderExitCallback>,
    }

    enum PtyIoDataCommand {
        WriteUserInput(Bytes),
        SubmitUserInput {
            text: Bytes,
            enter: Bytes,
            delay: Duration,
            deadline: Option<Instant>,
            guard: Option<GuardedSubmission>,
            reply: std_mpsc::Sender<std::io::Result<GuardedInputOutcome>>,
        },
    }

    enum PtyIoWriteCommand {
        Write(Bytes),
        SubmissionPart {
            bytes: Bytes,
            deadline: Option<Instant>,
            /// Checked under the guard lock at the moment of the write, so a rotation cannot
            /// land between the check and the bytes reaching the PTY.
            guard: Option<(AgentInputGuard, String)>,
            reply: std_mpsc::Sender<SubmissionPartOutcome>,
        },
    }

    enum SubmissionPartOutcome {
        Completed(std::io::Result<()>),
        GuardMismatch,
    }

    enum PtyIoControlCommand {
        Resize(PtyResizeRequest),
        Shutdown,
    }

    #[derive(Clone)]
    pub(crate) struct PtyIoActorHandle {
        data_tx: mpsc::Sender<PtyIoDataCommand>,
        control_tx: std_mpsc::Sender<PtyIoControlCommand>,
        write_tx: std_mpsc::Sender<PtyIoWriteCommand>,
        response_order: Arc<Mutex<()>>,
        accepting: Arc<Mutex<bool>>,
        agent_input_guard: AgentInputGuard,
    }

    impl PtyIoActorHandle {
        pub(crate) fn try_write_user_input(
            &self,
            bytes: Bytes,
        ) -> Result<(), mpsc::error::TrySendError<Bytes>> {
            if !*self
                .accepting
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
            {
                return Err(mpsc::error::TrySendError::Closed(bytes));
            }
            self.data_tx
                .try_send(PtyIoDataCommand::WriteUserInput(bytes))
                .map_err(|err| match err {
                    mpsc::error::TrySendError::Full(command) => {
                        let PtyIoDataCommand::WriteUserInput(bytes) = command else {
                            unreachable!("queued write returned another command")
                        };
                        mpsc::error::TrySendError::Full(bytes)
                    }
                    mpsc::error::TrySendError::Closed(command) => {
                        let PtyIoDataCommand::WriteUserInput(bytes) = command else {
                            unreachable!("queued write returned another command")
                        };
                        mpsc::error::TrySendError::Closed(bytes)
                    }
                })
        }

        pub(crate) fn agent_input_guard(&self) -> AgentInputGuard {
            self.agent_input_guard.clone()
        }

        pub(crate) fn queue_user_input_submission(
            &self,
            text: Bytes,
            enter: Bytes,
            delay: Duration,
            deadline: Option<Instant>,
            guard: Option<GuardedSubmission>,
        ) -> std::io::Result<std_mpsc::Receiver<std::io::Result<GuardedInputOutcome>>> {
            let accepting = self
                .accepting
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !*accepting {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "pty actor closed",
                ));
            }
            let (reply_tx, reply_rx) = std_mpsc::channel();
            self.data_tx
                .try_send(PtyIoDataCommand::SubmitUserInput {
                    text,
                    enter,
                    delay,
                    deadline,
                    guard,
                    reply: reply_tx,
                })
                .map_err(|err| match err {
                    mpsc::error::TrySendError::Full(_) => std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "pty input queue is full",
                    ),
                    mpsc::error::TrySendError::Closed(_) => {
                        std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pty actor closed")
                    }
                })?;
            Ok(reply_rx)
        }

        pub(crate) fn write_terminal_response(&self, response: impl FnOnce() -> Option<Bytes>) {
            let _order = self
                .response_order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(bytes) = response().filter(|bytes| !bytes.is_empty()) {
                let _ = self.write_tx.send(PtyIoWriteCommand::Write(bytes));
            }
        }

        pub(crate) fn resize(
            &self,
            rows: u16,
            cols: u16,
            cell_width_px: u32,
            cell_height_px: u32,
            terminal_responses: Vec<Bytes>,
        ) {
            let _ = self
                .control_tx
                .send(PtyIoControlCommand::Resize(PtyResizeRequest {
                    resize: PtyResize {
                        rows,
                        cols,
                        cell_width_px,
                        cell_height_px,
                    },
                    terminal_responses,
                }));
        }

        pub(crate) fn shutdown(&self) {
            if let Ok(mut accepting) = self.accepting.lock() {
                *accepting = false;
            }
            let _ = self.control_tx.send(PtyIoControlCommand::Shutdown);
        }
    }

    pub(crate) struct PtyIoActor;

    impl PtyIoActor {
        pub(crate) fn spawn(config: PtyIoActorConfig) -> std::io::Result<PtyIoActorHandle> {
            let PtyIoActorConfig {
                pane_id,
                master,
                initially_quiesced,
                mut on_read,
                on_reader_exit,
            } = config;

            let mut reader = master
                .try_clone_reader()
                .map_err(|err| std::io::Error::other(err.to_string()))?;
            let mut writer = master
                .take_writer()
                .map_err(|err| std::io::Error::other(err.to_string()))?;
            let (data_tx, mut data_rx) = mpsc::channel::<PtyIoDataCommand>(1024);
            let (control_tx, control_rx) = std_mpsc::channel::<PtyIoControlCommand>();
            let (write_tx, write_rx) = std_mpsc::channel::<PtyIoWriteCommand>();
            let response_order = Arc::new(Mutex::new(()));
            let accepting = Arc::new(Mutex::new(!initially_quiesced));
            let agent_input_guard = AgentInputGuard::default();

            std::thread::spawn(move || {
                run_writer(&mut writer, write_rx);
                debug!(pane_id, "windows pty writer thread exiting");
            });

            {
                let write_tx = write_tx.clone();
                let accepting = Arc::clone(&accepting);
                let agent_input_guard = agent_input_guard.clone();
                std::thread::spawn(move || {
                    run_input_forwarder(&mut data_rx, write_tx, accepting, agent_input_guard);
                    debug!(pane_id, "windows pty input thread exiting");
                });
            }

            {
                let write_tx = write_tx.clone();
                let response_order = Arc::clone(&response_order);
                std::thread::spawn(move || {
                    let mut buf = [0u8; 8192];
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                let _order = response_order
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                                let result = on_read(&buf[..n]);
                                if result.terminal_responses.into_iter().any(|response| {
                                    write_tx.send(PtyIoWriteCommand::Write(response)).is_err()
                                }) {
                                    break;
                                }
                            }
                            Err(err) => {
                                debug!(pane_id, err = %err, "windows pty reader failed");
                                break;
                            }
                        }
                    }
                    if let Some(on_reader_exit) = on_reader_exit {
                        on_reader_exit();
                    }
                    debug!(pane_id, "windows pty reader thread exiting");
                });
            }

            {
                let write_tx = write_tx.clone();
                std::thread::spawn(move || {
                    for command in control_rx {
                        match command {
                            PtyIoControlCommand::Resize(request) => {
                                let size = request.resize;
                                if let Err(err) = master.resize(PtySize {
                                    rows: size.rows,
                                    cols: size.cols,
                                    pixel_width: size.cell_width_px.min(u16::MAX as u32) as u16,
                                    pixel_height: size.cell_height_px.min(u16::MAX as u32) as u16,
                                }) {
                                    warn!(pane_id, err = %err, "windows pty resize failed");
                                }
                                if request.terminal_responses.into_iter().any(|response| {
                                    write_tx.send(PtyIoWriteCommand::Write(response)).is_err()
                                }) {
                                    break;
                                }
                            }
                            PtyIoControlCommand::Shutdown => break,
                        }
                    }
                    debug!(pane_id, "windows pty control thread exiting");
                });
            }

            Ok(PtyIoActorHandle {
                data_tx,
                control_tx,
                write_tx,
                response_order,
                accepting,
                agent_input_guard,
            })
        }
    }

    fn run_writer(writer: &mut impl Write, write_rx: std_mpsc::Receiver<PtyIoWriteCommand>) {
        for command in write_rx {
            let result = match command {
                PtyIoWriteCommand::Write(bytes) => write_and_flush(writer, &bytes),
                PtyIoWriteCommand::SubmissionPart {
                    bytes,
                    deadline,
                    guard,
                    reply,
                } => {
                    let outcome = if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                        SubmissionPartOutcome::Completed(Err(input_submission_timed_out()))
                    } else {
                        match guard {
                            Some((agent_input_guard, expected_guard)) => {
                                match agent_input_guard.with_matching(&expected_guard, || {
                                    write_and_flush(writer, &bytes)
                                }) {
                                    Some(result) => SubmissionPartOutcome::Completed(result),
                                    None => SubmissionPartOutcome::GuardMismatch,
                                }
                            }
                            None => {
                                SubmissionPartOutcome::Completed(write_and_flush(writer, &bytes))
                            }
                        }
                    };
                    let failed = matches!(
                        &outcome,
                        SubmissionPartOutcome::Completed(Err(err))
                            if err.kind() != std::io::ErrorKind::TimedOut
                    );
                    let _ = reply.send(outcome);
                    if failed {
                        break;
                    }
                    continue;
                }
            };
            if result.is_err() {
                break;
            }
        }
    }

    fn run_input_forwarder(
        data_rx: &mut mpsc::Receiver<PtyIoDataCommand>,
        write_tx: std_mpsc::Sender<PtyIoWriteCommand>,
        accepting: Arc<Mutex<bool>>,
        agent_input_guard: AgentInputGuard,
    ) {
        while let Some(command) = data_rx.blocking_recv() {
            match command {
                PtyIoDataCommand::WriteUserInput(bytes) => {
                    if write_tx.send(PtyIoWriteCommand::Write(bytes)).is_err() {
                        break;
                    }
                }
                PtyIoDataCommand::SubmitUserInput {
                    text,
                    enter,
                    delay,
                    deadline,
                    guard,
                    reply,
                } => {
                    let (prefix, guard) = match guard {
                        Some(guard) => (
                            guard.prefix,
                            Some((agent_input_guard.clone(), guard.expected_guard)),
                        ),
                        None => (Bytes::new(), None),
                    };
                    // The focus prefix shares the submission's guard check, so a mismatch
                    // cannot leak even a focus event to the replacement process.
                    let body = if prefix.is_empty() {
                        text
                    } else {
                        let mut body = Vec::with_capacity(prefix.len() + text.len());
                        body.extend_from_slice(&prefix);
                        body.extend_from_slice(&text);
                        Bytes::from(body)
                    };
                    let wrote_any = !body.is_empty();
                    let outcome = if deadline.is_some_and(|deadline| {
                        deadline.saturating_duration_since(Instant::now()) <= delay
                    }) {
                        SubmissionPartOutcome::Completed(Err(input_submission_timed_out()))
                    } else {
                        let text_deadline =
                            deadline.and_then(|deadline| deadline.checked_sub(delay));
                        match write_submission_part(&write_tx, body, text_deadline, guard.clone()) {
                            SubmissionPartOutcome::Completed(Ok(())) => {
                                // A started text write is committed. Finish Enter even if the
                                // caller stops waiting so a timeout cannot leave a partial
                                // prompt, unless the guard says the target is already gone.
                                std::thread::sleep(delay);
                                let closed = {
                                    let accepting = accepting
                                        .lock()
                                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                                    !*accepting
                                };
                                if closed {
                                    SubmissionPartOutcome::Completed(Err(pty_actor_closed()))
                                } else {
                                    write_submission_part(&write_tx, enter, None, guard)
                                }
                            }
                            other => other,
                        }
                    };
                    let response = match outcome {
                        SubmissionPartOutcome::Completed(Ok(())) => {
                            Ok(GuardedInputOutcome::Submitted)
                        }
                        SubmissionPartOutcome::GuardMismatch if wrote_any => {
                            Ok(GuardedInputOutcome::Partial)
                        }
                        SubmissionPartOutcome::GuardMismatch => Ok(GuardedInputOutcome::Rejected),
                        SubmissionPartOutcome::Completed(Err(err)) => Err(err),
                    };
                    let failed = response
                        .as_ref()
                        .is_err_and(|err| err.kind() != std::io::ErrorKind::TimedOut);
                    let _ = reply.send(response);
                    if failed {
                        break;
                    }
                }
            }
        }
    }

    fn write_submission_part(
        write_tx: &std_mpsc::Sender<PtyIoWriteCommand>,
        bytes: Bytes,
        deadline: Option<Instant>,
        guard: Option<(AgentInputGuard, String)>,
    ) -> SubmissionPartOutcome {
        let (reply, completion) = std_mpsc::channel();
        if write_tx
            .send(PtyIoWriteCommand::SubmissionPart {
                bytes,
                deadline,
                guard,
                reply,
            })
            .is_err()
        {
            return SubmissionPartOutcome::Completed(Err(pty_actor_closed()));
        }
        completion
            .recv()
            .unwrap_or_else(|_| SubmissionPartOutcome::Completed(Err(pty_actor_closed())))
    }

    fn pty_actor_closed() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pty actor closed")
    }

    fn input_submission_timed_out() -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "agent prompt timed out before input submission",
        )
    }

    fn write_and_flush(writer: &mut impl Write, bytes: &[u8]) -> std::io::Result<()> {
        writer.write_all(bytes)?;
        writer.flush()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        struct RecordingWriter {
            writes: Vec<(Vec<u8>, Instant)>,
            flushes: Vec<Instant>,
            fail_after: Option<usize>,
            flushed: std_mpsc::Sender<()>,
        }

        impl Write for RecordingWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.fail_after == Some(self.writes.len()) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "writer closed",
                    ));
                }
                self.writes.push((bytes.to_vec(), Instant::now()));
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                self.flushes.push(Instant::now());
                let _ = self.flushed.send(());
                Ok(())
            }
        }

        fn run_recorded_submission(
            fail_after: Option<usize>,
            delay: Duration,
            deadline: Option<Instant>,
            during_delay: impl FnOnce(&std_mpsc::Sender<PtyIoWriteCommand>, &Arc<Mutex<bool>>),
        ) -> (RecordingWriter, std::io::Result<()>) {
            let (flushed_tx, flushed_rx) = std_mpsc::channel();
            let mut writer = RecordingWriter {
                writes: Vec::new(),
                flushes: Vec::new(),
                fail_after,
                flushed: flushed_tx,
            };
            let (data_tx, mut data_rx) = mpsc::channel(2);
            let (write_tx, write_rx) = std_mpsc::channel();
            let (reply_tx, reply_rx) = std_mpsc::channel();
            let accepting = Arc::new(Mutex::new(true));
            data_tx
                .try_send(PtyIoDataCommand::SubmitUserInput {
                    text: Bytes::from_static(b"prompt"),
                    enter: Bytes::from_static(b"\r"),
                    delay,
                    deadline,
                    reply: reply_tx,
                })
                .unwrap();
            data_tx
                .try_send(PtyIoDataCommand::WriteUserInput(Bytes::from_static(
                    b"user",
                )))
                .unwrap();
            let writer_thread = std::thread::spawn(move || {
                run_writer(&mut writer, write_rx);
                writer
            });
            let input_write_tx = write_tx.clone();
            let input_accepting = Arc::clone(&accepting);
            let input_thread = std::thread::spawn(move || {
                run_input_forwarder(
                    &mut data_rx,
                    input_write_tx,
                    input_accepting,
                    AgentInputGuard::default(),
                )
            });
            flushed_rx.recv().expect("prompt was flushed");
            during_delay(&write_tx, &accepting);
            let result = reply_rx.recv().expect("writer reports submission");
            drop(data_tx);
            input_thread.join().expect("input thread joins");
            drop(write_tx);
            (writer_thread.join().expect("writer thread joins"), result)
        }

        #[test]
        fn submission_sequences_user_input_but_allows_terminal_responses() {
            let delay = Duration::from_millis(30);
            let (writer, result) = run_recorded_submission(None, delay, None, |write_tx, _| {
                write_tx
                    .send(PtyIoWriteCommand::Write(Bytes::from_static(b"response")))
                    .unwrap();
            });
            result.expect("submission succeeds");

            assert_eq!(writer.writes[0].0, b"prompt");
            assert_eq!(writer.writes[1].0, b"response");
            assert_eq!(writer.writes[2].0, b"\r");
            assert_eq!(writer.writes[3].0, b"user");
            assert!(writer.writes[2].1.duration_since(writer.flushes[0]) >= delay);
        }

        #[test]
        fn submission_returns_enter_write_failure() {
            let (_writer, result) =
                run_recorded_submission(Some(1), Duration::ZERO, None, |_, _| {});
            let err = result.expect_err("enter failure reaches caller");

            assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
        }

        #[test]
        fn shutdown_during_submission_delay_cancels_enter() {
            let (writer, result) =
                run_recorded_submission(None, Duration::from_millis(30), None, |_, accepting| {
                    *accepting.lock().unwrap() = false;
                });
            let err = result.expect_err("shutdown cancels enter");
            assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
            assert_eq!(
                writer
                    .writes
                    .iter()
                    .map(|write| write.0.as_slice())
                    .collect::<Vec<_>>(),
                vec![b"prompt"]
            );
        }

        #[test]
        fn expired_queued_submission_is_not_written() {
            let (flushed_tx, _flushed_rx) = std_mpsc::channel();
            let mut writer = RecordingWriter {
                writes: Vec::new(),
                flushes: Vec::new(),
                fail_after: None,
                flushed: flushed_tx,
            };
            let (data_tx, mut data_rx) = mpsc::channel(2);
            let (write_tx, write_rx) = std_mpsc::channel();
            let (first_reply_tx, first_reply_rx) = std_mpsc::channel();
            let (expired_reply_tx, expired_reply_rx) = std_mpsc::channel();
            let accepting = Arc::new(Mutex::new(true));
            data_tx
                .try_send(PtyIoDataCommand::SubmitUserInput {
                    text: Bytes::from_static(b"first"),
                    enter: Bytes::from_static(b"\r"),
                    delay: Duration::from_millis(30),
                    deadline: None,
                    reply: first_reply_tx,
                })
                .unwrap();
            data_tx
                .try_send(PtyIoDataCommand::SubmitUserInput {
                    text: Bytes::from_static(b"expired"),
                    enter: Bytes::from_static(b"\r"),
                    delay: Duration::ZERO,
                    deadline: Some(Instant::now() + Duration::from_millis(10)),
                    reply: expired_reply_tx,
                })
                .unwrap();

            let writer_thread = std::thread::spawn(move || {
                run_writer(&mut writer, write_rx);
                writer
            });
            let input_write_tx = write_tx.clone();
            let input_thread = std::thread::spawn(move || {
                run_input_forwarder(
                    &mut data_rx,
                    input_write_tx,
                    accepting,
                    AgentInputGuard::default(),
                )
            });
            first_reply_rx.recv().unwrap().unwrap();
            let err = expired_reply_rx.recv().unwrap().unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);

            drop(data_tx);
            input_thread.join().unwrap();
            drop(write_tx);
            let writer = writer_thread.join().unwrap();
            assert_eq!(
                writer
                    .writes
                    .iter()
                    .map(|write| write.0.as_slice())
                    .collect::<Vec<_>>(),
                vec![b"first".as_slice(), b"\r".as_slice()]
            );
        }
    }
}

#[cfg(windows)]
pub(crate) use windows::*;
