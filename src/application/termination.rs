use std::future::Future;
use std::io;

/// 在整个命令生命周期保留 Windows 控制信号订阅。
pub(crate) struct TerminationSignals {
    state: TerminationSignalState,
}

enum TerminationSignalState {
    Listening {
        ctrl_c: tokio::signal::windows::CtrlC,
        ctrl_break: tokio::signal::windows::CtrlBreak,
    },
    RegistrationFailed(Option<io::Error>),
}

impl TerminationSignals {
    pub(crate) fn new() -> Self {
        let state = match tokio::signal::windows::ctrl_c() {
            Ok(ctrl_c) => match tokio::signal::windows::ctrl_break() {
                Ok(ctrl_break) => TerminationSignalState::Listening { ctrl_c, ctrl_break },
                Err(error) => TerminationSignalState::RegistrationFailed(Some(error)),
            },
            Err(error) => TerminationSignalState::RegistrationFailed(Some(error)),
        };
        Self { state }
    }

    pub(crate) async fn recv(&mut self) -> io::Result<()> {
        match &mut self.state {
            TerminationSignalState::Listening { ctrl_c, ctrl_break } => {
                tokio::select! {
                    signal = ctrl_c.recv() => signal.ok_or_else(|| io::Error::other("Ctrl-C 信号源意外关闭")),
                    signal = ctrl_break.recv() => signal.ok_or_else(|| io::Error::other("Ctrl-Break 信号源意外关闭")),
                }
            }
            TerminationSignalState::RegistrationFailed(error) => Err(error
                .take()
                .unwrap_or_else(|| io::Error::other("Windows 控制信号源不可用"))),
        }
    }
}

pub(crate) enum TerminationOutcome<T> {
    Finished(T),
    Interrupted(T),
    SignalFailed { source: io::Error, result: T },
}

impl<T> TerminationOutcome<T> {
    pub(crate) fn map<U>(self, map: impl FnOnce(T) -> U) -> TerminationOutcome<U> {
        match self {
            Self::Finished(value) => TerminationOutcome::Finished(map(value)),
            Self::Interrupted(value) => TerminationOutcome::Interrupted(map(value)),
            Self::SignalFailed { source, result } => TerminationOutcome::SignalFailed {
                source,
                result: map(result),
            },
        }
    }
}

pub(crate) async fn drive_with_termination<T>(
    future: impl Future<Output = T>,
    termination_signals: &mut TerminationSignals,
    cancel_waits: impl FnOnce(),
    on_cancellation: impl FnOnce(),
) -> TerminationOutcome<T> {
    tokio::pin!(future);
    let signal = termination_signals.recv();
    tokio::pin!(signal);
    tokio::select! {
        biased;
        signal = &mut signal => match signal {
            Ok(()) => {
                cancel_waits();
                on_cancellation();
                TerminationOutcome::Interrupted(future.await)
            }
            Err(source) => {
                cancel_waits();
                let result = future.await;
                TerminationOutcome::SignalFailed { source, result }
            }
        },
        result = &mut future => TerminationOutcome::Finished(result),
    }
}
