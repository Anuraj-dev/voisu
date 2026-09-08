//! FIFO off-actor queue for ASR persist and capture admission.
//!
//! SetAsrMode and Start share one worker so a Start the actor received after
//! SetAsrMode observes that commit, without holding the actor on flock.

use std::sync::mpsc::{self, Sender};
use std::thread;

use tokio::sync::oneshot;
use voisu_core::{AsrMode, DaemonState, Response};

use crate::asr_mode::{
    self, AsrModeCommit, CaptureKind, PersistError, persist_asr_mode, set_asr_mode_response,
};

type PersistFn = Box<dyn Fn(AsrMode) -> Result<AsrModeCommit, PersistError> + Send>;
type AdmitFn =
    Box<dyn Fn(CaptureKind, DaemonState, Option<AsrMode>) -> Result<AsrMode, Box<Response>> + Send>;

pub struct AsrModeQueue {
    tx: Sender<AsrModeJob>,
}

enum AsrModeJob {
    Persist {
        mode: AsrMode,
        daemon_state: DaemonState,
        active: Option<AsrMode>,
        reply: oneshot::Sender<Response>,
    },
    Admit {
        kind: CaptureKind,
        daemon_state: DaemonState,
        active: Option<AsrMode>,
        complete: Box<dyn FnOnce(Result<AsrMode, Box<Response>>) + Send>,
    },
}

impl AsrModeQueue {
    pub fn spawn() -> Self {
        Self::spawn_with(
            Box::new(persist_asr_mode),
            Box::new(asr_mode::admit_capture),
        )
    }

    fn spawn_with(persist: PersistFn, admit: AdmitFn) -> Self {
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("voisu-asr-mode".to_owned())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    match job {
                        AsrModeJob::Persist {
                            mode,
                            daemon_state,
                            active,
                            reply,
                        } => {
                            let _ = reply.send(set_asr_mode_response(
                                persist(mode),
                                daemon_state,
                                active,
                            ));
                        }
                        AsrModeJob::Admit {
                            kind,
                            daemon_state,
                            active,
                            complete,
                        } => complete(admit(kind, daemon_state, active)),
                    }
                }
            })
            .expect("asr-mode queue thread");
        Self { tx }
    }

    pub fn persist(
        &self,
        mode: AsrMode,
        daemon_state: DaemonState,
        active: Option<AsrMode>,
        reply: oneshot::Sender<Response>,
    ) {
        let job = AsrModeJob::Persist {
            mode,
            daemon_state,
            active,
            reply,
        };
        if let Err(error) = self.tx.send(job) {
            match error.0 {
                AsrModeJob::Persist {
                    reply,
                    daemon_state,
                    ..
                } => {
                    let _ = reply.send(Response::rejected(
                        Some(daemon_state),
                        "ASR mode queue is unavailable",
                    ));
                }
                AsrModeJob::Admit { .. } => unreachable!("persist job"),
            }
        }
    }

    pub fn admit(
        &self,
        kind: CaptureKind,
        daemon_state: DaemonState,
        active: Option<AsrMode>,
        complete: impl FnOnce(Result<AsrMode, Box<Response>>) + Send + 'static,
    ) {
        let job = AsrModeJob::Admit {
            kind,
            daemon_state,
            active,
            complete: Box::new(complete),
        };
        if let Err(error) = self.tx.send(job) {
            match error.0 {
                AsrModeJob::Admit {
                    complete,
                    daemon_state,
                    ..
                } => complete(Err(Box::new(Response::rejected(
                    Some(daemon_state),
                    "ASR mode queue is unavailable",
                )))),
                AsrModeJob::Persist { .. } => unreachable!("admit job"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    use crate::asr_mode::{AsrPathResources, RecordingAdmission};

    fn admit_at(
        config_path: &std::path::Path,
        state_dir: &std::path::Path,
        daemon_state: DaemonState,
    ) -> Result<AsrMode, Box<Response>> {
        match asr_mode::admit_recording_at(config_path, state_dir) {
            Ok(RecordingAdmission::Cloud { mode, .. }) => match asr_mode::select_resources(mode) {
                AsrPathResources::Cloud => Ok(mode),
                AsrPathResources::Local { .. } => Err(Box::new(Response::rejected(
                    Some(daemon_state),
                    "Local ASR is unavailable; Start refused before capture",
                ))),
            },
            Ok(RecordingAdmission::Local { mode, .. }) => Ok(mode),
            Ok(RecordingAdmission::LocalUnavailable { .. }) => Err(Box::new(Response::rejected(
                Some(daemon_state),
                "Local ASR is unavailable; Start refused before capture",
            ))),
            Err(error) => Err(Box::new(Response::rejected(
                Some(daemon_state),
                error.message(),
            ))),
        }
    }

    #[test]
    fn persist_then_admit_on_the_queue_sees_local() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("config.toml");
        let state = home.path().join("state");
        fs::create_dir_all(&state).unwrap();
        let queue = {
            let persist_config = config.clone();
            let persist_state = state.clone();
            let admit_config = config.clone();
            let admit_state = state.clone();
            AsrModeQueue::spawn_with(
                Box::new(move |mode| {
                    asr_mode::persist_asr_mode_at(&persist_config, &persist_state, mode)
                }),
                Box::new(move |_kind, daemon_state, _active| {
                    admit_at(&admit_config, &admit_state, daemon_state)
                }),
            )
        };

        let (persist_tx, persist_rx) = oneshot::channel();
        queue.persist(AsrMode::Local, DaemonState::Idle, None, persist_tx);
        let (admit_tx, admit_rx) = std::sync::mpsc::channel();
        queue.admit(CaptureKind::Start, DaemonState::Idle, None, move |result| {
            let _ = admit_tx.send(result);
        });

        let persisted = persist_rx.blocking_recv().expect("persist reply");
        assert!(persisted.ok, "{}", persisted.message);

        let admitted = admit_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("admit result");
        match admitted {
            Ok(AsrMode::Cloud) => panic!("Start after SetAsrMode(Local) must not admit Cloud"),
            Ok(AsrMode::Local) => panic!("Local must not admit capture"),
            Err(response) => {
                assert!(!response.ok, "{}", response.message);
                assert!(
                    response.message.contains("refused before capture")
                        || response.message.contains("unavailable"),
                    "{}",
                    response.message
                );
            }
        }
    }
}
