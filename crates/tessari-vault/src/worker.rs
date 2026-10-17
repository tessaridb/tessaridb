//! CPU-bound work run from a task without holding the worker the task is on.

/// Run CPU-bound `work` (an Argon2id hash or derivation) so it does not hold a
/// runtime worker every other connection on it is waiting for.
///
/// On the multi-thread runtime the worker hands its other tasks on before
/// `work` starts (`block_in_place`); anywhere else (a current-thread runtime, a
/// caller with no runtime) there is no worker to free and `work` runs where it
/// is.
pub fn off_the_worker<T>(work: impl FnOnce() -> T) -> T {
    let shared = tokio::runtime::Handle::try_current().is_ok_and(|runtime| {
        runtime.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread
    });
    if shared {
        tokio::task::block_in_place(work)
    } else {
        work()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::mpsc;
    use std::time::Duration;

    use super::off_the_worker;

    #[test]
    fn blocking_work_hands_the_only_worker_to_the_other_tasks() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .expect("a one-worker runtime");
        let (started, has_started) = mpsc::channel();
        let (ran, has_run) = mpsc::channel();
        // The work holds its thread until a task spawned after it starts has
        // run: possible only when the one worker was handed on.
        let blocked = runtime.spawn(async move {
            started.send(()).unwrap();
            off_the_worker(|| has_run.recv_timeout(Duration::from_secs(2)).is_ok())
        });
        has_started.recv().unwrap();
        let other = runtime.spawn(async move { ran.send(()).unwrap() });
        assert!(
            runtime.block_on(blocked).unwrap(),
            "nothing else ran while the work held the worker"
        );
        runtime.block_on(other).unwrap();
    }

    #[test]
    fn with_no_runtime_the_work_runs_where_it_is() {
        assert_eq!(off_the_worker(|| 7), 7);
    }
}
