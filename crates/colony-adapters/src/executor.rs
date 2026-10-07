// SPDX-License-Identifier: MIT
//! Mesut execution adapter. Submit admits work onto Mesut; it does not run it inline.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use colony_core::{Error, FailureKind, Result};
use colony_provider::{InferenceEnvelope, InferenceProvider, Registry};
use colony_runtime::{Dispatch, Mesut};
use mesut::prelude::{BlockingExecutor, MesuT, RuntimeConfig, TaskId, Work, WorkKind};
use mesut::{CancellationToken, RoutingHint, TaskError};

use crate::elci::{ElciEndpoint, ElciProvider};

struct Job {
    task_id: TaskId,
    token: CancellationToken,
    rx: Option<Receiver<Result<WorkerOutcome>>>,
}

type WorkerOutcome = colony_core::WorkerResult;

/// Host executor. Attempt handles are `{colony}:{unit}:{attempt}` and a repeat submit is a no-op.
pub struct MesutExecutor {
    runtime: MesuT,
    tokio: tokio::runtime::Runtime,
    endpoint: ElciEndpoint,
    registry: Registry,
    root: PathBuf,
    scratch: PathBuf,
    jobs: BTreeMap<String, Job>,
}

impl MesutExecutor {
    pub fn new(
        root: impl Into<PathBuf>,
        scratch: impl Into<PathBuf>,
        endpoint: ElciEndpoint,
        registry: Registry,
    ) -> Result<Self> {
        let tokio = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_time()
            .build()
            .map_err(|error| {
                Error::new(
                    FailureKind::InfrastructureFailure,
                    format!("mesut tokio runtime: {error}"),
                )
            })?;
        let runtime = MesuT::new(RuntimeConfig::new().with_animations(false))
            .with_blocking_executor(std::sync::Arc::new(BlockingExecutor::new(
                Default::default(),
            )));
        Ok(Self {
            runtime,
            tokio,
            endpoint,
            registry,
            root: root.into(),
            scratch: scratch.into(),
            jobs: BTreeMap::new(),
        })
    }

    pub fn join_timeout(&mut self, handle: &str, timeout: Duration) -> Result<WorkerOutcome> {
        let job = self.jobs.get_mut(handle).ok_or_else(|| {
            Error::new(
                FailureKind::ContractViolation,
                format!("unknown mesut handle {handle}"),
            )
        })?;
        let rx = job.rx.take().ok_or_else(|| {
            Error::new(
                FailureKind::ContractViolation,
                format!("mesut handle {handle} was already joined"),
            )
        })?;
        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(job) = self.jobs.get(handle) {
                    job.token.cancel();
                }
                Err(Error::new(
                    FailureKind::InfrastructureFailure,
                    format!("mesut attempt {handle} timed out"),
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if self
                    .jobs
                    .get(handle)
                    .is_some_and(|job| job.token.is_cancelled())
                {
                    Err(Error::new(
                        FailureKind::Cancelled,
                        format!("mesut cancelled attempt {handle} before it started"),
                    ))
                } else {
                    Err(Error::new(
                        FailureKind::InfrastructureFailure,
                        format!("mesut attempt {handle} ended without a result"),
                    ))
                }
            }
        }
    }
}

pub fn attempt_handle(dispatch: &Dispatch) -> String {
    format!(
        "{}:{}:{}",
        dispatch.colony_id, dispatch.unit.id, dispatch.attempt
    )
}

impl Mesut for MesutExecutor {
    fn submit(&mut self, dispatch: &Dispatch) -> Result<String> {
        let handle = attempt_handle(dispatch);
        if self.jobs.contains_key(&handle) {
            return Ok(handle);
        }
        let (tx, rx) = mpsc::channel();
        let mut work = Work::new(WorkKind::Blocking)
            .with_label(handle.clone())
            .with_hint(RoutingHint::RequireBlocking);
        let task_id = work.id;
        let token = work.cancellation.clone();
        let endpoint = self.endpoint.clone();
        let resource = self
            .registry
            .resources
            .iter()
            .find(|resource| {
                resource.provider == dispatch.assignment.provider
                    && resource.model == dispatch.assignment.model
            })
            .cloned()
            .ok_or_else(|| {
                Error::new(
                    FailureKind::ProviderUnavailable,
                    "assigned provider is not in the registry",
                )
            })?;
        let root = self.root.clone();
        let work_dir = self.scratch.join(handle.replace('/', "_"));
        let unit = dispatch.unit.clone();
        let provider = dispatch.assignment.provider.clone();
        let model = dispatch.assignment.model.clone();
        let temperature = dispatch.temperature;
        work = work.with_job(move |cancel| {
            let mut elci = ElciProvider::new(endpoint, resource, root, work_dir, cancel);
            let result = elci.infer(InferenceEnvelope {
                contract: &unit,
                temperature,
                provider: &provider,
                model: &model,
            });
            let mesut_result = match &result {
                Ok(_) => Ok(Vec::new()),
                Err(error) => Err(TaskError::ExecutionFailed(error.to_string())),
            };
            let _ = tx.send(result);
            mesut_result
        });
        self.tokio
            .block_on(self.runtime.submit(work))
            .map_err(task_error)?;
        self.jobs.insert(
            handle.clone(),
            Job {
                task_id,
                token,
                rx: Some(rx),
            },
        );
        Ok(handle)
    }

    fn cancel(&mut self, handle: &str) -> Result<()> {
        let job = self.jobs.get(handle).ok_or_else(|| {
            Error::new(
                FailureKind::ContractViolation,
                format!("unknown mesut handle {handle}"),
            )
        })?;
        job.token.cancel();
        let task_id = job.task_id;
        self.tokio
            .block_on(self.runtime.cancel(task_id))
            .map_err(task_error)?;
        Ok(())
    }
}

fn task_error(error: TaskError) -> Error {
    let kind = match error {
        TaskError::Cancelled | TaskError::Timeout(_) => FailureKind::Cancelled,
        TaskError::QueueFull => FailureKind::ProviderThrottled,
        TaskError::NoExecutor | TaskError::ExecutorUnavailable => FailureKind::ProviderUnavailable,
        _ => FailureKind::InfrastructureFailure,
    };
    Error::new(kind, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elci::ElciEndpoint;
    use colony_allocator::Assignment;
    use colony_core::{FailureKind, WorkGraph};
    use colony_provider::Registry;
    use colony_runtime::{Dispatch, Mesut};
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    const DEFAULT_JOIN: Duration = Duration::from_secs(120);

    #[derive(serde::Deserialize)]
    struct Request {
        graph: WorkGraph,
        registry: Registry,
    }

    #[test]
    fn cancel_stops_a_running_process() {
        let request: Request = serde_json::from_str(include_str!("../../../examples/request.json"))
            .expect("example request");
        let scratch = std::env::temp_dir().join(format!("colony-mesut-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).unwrap();
        let mut executor = MesutExecutor::new(
            &scratch,
            &scratch,
            ElciEndpoint::Command {
                program: "/bin/sleep".into(),
                args: vec!["30".into()],
            },
            request.registry,
        )
        .unwrap();
        let unit = request.graph.nodes[0].clone();
        let dispatch = Dispatch {
            colony_id: request.graph.colony_id.clone(),
            attempt: 1,
            unit: unit.clone(),
            assignment: Assignment {
                work_unit: unit.id,
                provider: "example-local".into(),
                model: "research".into(),
                candidates: Vec::new(),
            },
            deadline_ms: request.graph.policy.deadline_ms,
            temperature: 0.2,
            dependency_results: Vec::new(),
            repair_result: None,
            repair_events: Vec::new(),
        };
        let handle = Mesut::submit(&mut executor, &dispatch).unwrap();
        Mesut::cancel(&mut executor, &handle).unwrap();
        let error = executor
            .join_timeout(&handle, Duration::from_secs(5))
            .unwrap_err();
        assert_eq!(error.kind, FailureKind::Cancelled);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn repeated_submit_uses_the_same_attempt() {
        let request: Request = serde_json::from_str(include_str!("../../../examples/request.json"))
            .expect("example request");
        let scratch =
            std::env::temp_dir().join(format!("colony-mesut-once-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).unwrap();
        let script = scratch.join("once.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf x >> \"$COLONY_OUT/runs\"\nprintf '%s\\n' \"$COLONY_OUTPUTS\" | while IFS= read -r name; do\n  [ -n \"$name\" ] || continue\n  path=$(printf '%s\\n' \"$COLONY_PATHS\" | head -n 1)\n  printf '{\"findings\":[{\"reference\":\"%s\",\"detail\":\"observed\"}]}\\n' \"$path\" > \"$COLONY_OUT/$name\"\ndone\n",
        )
        .unwrap();
        let mut mode = std::fs::metadata(&script).unwrap().permissions();
        mode.set_mode(0o755);
        std::fs::set_permissions(&script, mode).unwrap();
        let mut executor = MesutExecutor::new(
            &scratch,
            scratch.join("work"),
            ElciEndpoint::Command {
                program: script,
                args: Vec::new(),
            },
            request.registry,
        )
        .unwrap();
        let unit = request.graph.nodes[0].clone();
        let dispatch = Dispatch {
            colony_id: request.graph.colony_id,
            attempt: 7,
            unit: unit.clone(),
            assignment: Assignment {
                work_unit: unit.id,
                provider: "example-local".into(),
                model: "research".into(),
                candidates: Vec::new(),
            },
            deadline_ms: request.graph.policy.deadline_ms,
            temperature: 0.2,
            dependency_results: Vec::new(),
            repair_result: None,
            repair_events: Vec::new(),
        };
        let first = Mesut::submit(&mut executor, &dispatch).unwrap();
        let second = Mesut::submit(&mut executor, &dispatch).unwrap();
        assert_eq!(first, second);
        executor.join_timeout(&first, DEFAULT_JOIN).unwrap();
        let marker = scratch
            .join("work")
            .join(first.replace('/', "_"))
            .join("runs");
        let runs = std::fs::read_to_string(&marker).unwrap_or_default();
        assert_eq!(runs, "x");
        let _ = std::fs::remove_dir_all(&scratch);
    }
}
