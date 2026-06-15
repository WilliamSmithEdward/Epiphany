//! The reconcile-loop driver (ADR-0013): the composition-root half of the
//! scheduler. The pure due-selection lives in `epiphany-flow`; this drives it on
//! the injected clock and executes due job runs through the same apply path the
//! HTTP flow-run handler uses, recording each run in the durable ledger and
//! auditing it (ADR-0010).
//!
//! [`Scheduler::tick`] is synchronous and deterministic (drive it from a test
//! under a `ManualClock`); [`Scheduler::spawn`] runs it on a real timer in
//! production, each tick on a blocking thread because the flow engine (boa) is
//! blocking. Real wall-clock time enters only at the tick's single
//! `clock.now_millis()` read, which is frozen as `fire_millis` and passed down to
//! `run_flow` and the run record -- the whole determinism reconciliation
//! (ADR-0013 decision 0).

use std::collections::BTreeMap;
use std::time::Duration;

use epiphany_core::Job;
use epiphany_flow::{due_firings, run_flow, Firing, FlowError, RunRecord, RunState};
use epiphany_security::{AuditAction, ObjectKind, ObjectRef};

use crate::authz::audit_at;
use crate::flow_routes::apply_outcome;
use crate::AppState;

/// The service principal recorded for timer-fired runs (ADR-0013 decision 9).
///
/// A scheduled run is a *system* action over an admin/modeler-defined job (a
/// secured cube object that took cube `Write` to create, enable, or delete), not
/// a user impersonation, so the reconcile loop does not re-resolve a user's
/// access per firing -- exactly as a rule or flow, once defined, is evaluated by
/// the system for everyone. Control over a job is its `enabled` flag and its
/// lifecycle (both cube-`Write`-gated, ADR-0015 decision 2a): to stop a job, an
/// admin disables or deletes it. A per-run user-authz model (and whose access it
/// would check) is deferred with the other scheduler extensions in ADR-0013.
pub(crate) const SCHEDULER_PRINCIPAL: &str = "scheduler";

/// The reconcile-loop driver over an [`AppState`].
#[derive(Clone, Debug)]
pub struct Scheduler {
    state: AppState,
}

impl Scheduler {
    /// Build a scheduler over the application state.
    pub fn new(state: AppState) -> Self {
        Self { state }
    }

    /// Run one reconcile tick: read the clock once, fire every due job
    /// synchronously, recording and auditing each. Returns the number of runs
    /// dispatched (not counting coalesced single-flight skips). Deterministic
    /// under a `ManualClock`, so the Phase 8 acceptance drives it directly.
    pub fn tick(&self) -> usize {
        let tick_now = self.state.clock.now_millis();
        let jobs = self.gather_jobs();
        let firings = {
            let ledger = self.state.runs.lock().expect("run ledger mutex");
            due_firings(&jobs, &ledger, tick_now)
        };
        let mut dispatched = 0;
        for firing in firings {
            if firing.coalesced {
                // Single-flight: observable via audit but no run (decision 6).
                self.audit_job(&firing, false);
                continue;
            }
            // Dedup a re-derived firing (decision 3): same id already recorded.
            if self
                .state
                .runs
                .lock()
                .expect("run ledger mutex")
                .contains(&firing.run_id)
            {
                continue;
            }
            self.execute(&firing, SCHEDULER_PRINCIPAL);
            dispatched += 1;
        }
        dispatched
    }

    /// Every cube's jobs, from live snapshots, paired with the owning cube. The
    /// selector filters by `enabled`.
    fn gather_jobs(&self) -> Vec<(String, Job)> {
        let mut jobs = Vec::new();
        for cube in self.state.engine.cube_names() {
            if let Some(snap) = self.state.engine.snapshot(&cube) {
                for job in snap.model().jobs.values() {
                    jobs.push((cube.clone(), job.clone()));
                }
            }
        }
        jobs
    }

    /// Execute one job firing: record `Queued` then `Running`, run each step flow
    /// with the frozen fire time, apply its outcome, then record `Succeeded` (or
    /// `Failed` on the first failing step, fail-fast). Reused by the manual kick.
    pub(crate) fn execute(&self, firing: &Firing, principal: &str) {
        self.record(firing, RunState::Queued, (0, 0, 0), "", principal);
        self.record(firing, RunState::Running, (0, 0, 0), "", principal);

        let Some(snap) = self.state.engine.snapshot(&firing.cube) else {
            self.fail(firing, (0, 0, 0), "unknown cube", principal);
            return;
        };
        let Some(job) = snap.model().job(&firing.job).cloned() else {
            self.fail(firing, (0, 0, 0), "unknown job", principal);
            return;
        };

        // (rows_read, cells_written, elements_added) aggregated across steps.
        let mut agg = (0u64, 0u64, 0u64);
        for step in &job.steps {
            let Some(flow) = snap.model().flows.get(step) else {
                self.fail(firing, agg, &format!("unknown flow '{step}'"), principal);
                return;
            };
            // A scheduled step runs the flow with no external input (binding a
            // connection to a job step is a deferred increment). The fire time is
            // the frozen tick value (ADR-0013 decision 0), never a fresh read.
            let outcome = match run_flow(
                &flow.source,
                &firing.cube,
                Vec::new(),
                &BTreeMap::new(),
                firing.fire_millis,
            ) {
                Ok(o) => o,
                Err(e) => {
                    self.fail(firing, agg, &flow_error_message(&e), principal);
                    return;
                }
            };
            agg.0 += outcome.report.rows_read as u64;
            match apply_outcome(&self.state, &firing.cube, &outcome) {
                Ok((elements, cells)) => {
                    agg.2 += elements as u64;
                    agg.1 += cells as u64;
                }
                Err(e) => {
                    self.fail(firing, agg, e.message(), principal);
                    return;
                }
            }
        }
        self.record(firing, RunState::Succeeded, agg, "", principal);
        self.audit_job(firing, true);
    }

    /// Record a terminal failure and audit it.
    fn fail(&self, firing: &Firing, agg: (u64, u64, u64), error: &str, principal: &str) {
        self.record(firing, RunState::Failed, agg, error, principal);
        self.audit_job(firing, false);
    }

    /// Append a run-state record to the durable ledger (best-effort: a full disk
    /// must not panic the scheduler).
    fn record(
        &self,
        firing: &Firing,
        state: RunState,
        agg: (u64, u64, u64),
        error: &str,
        principal: &str,
    ) {
        let record = RunRecord {
            id: firing.run_id.clone(),
            cube: firing.cube.clone(),
            target: firing.job.clone(),
            is_job: true,
            fire_millis: firing.fire_millis,
            state,
            rows_read: agg.0,
            cells_written: agg.1,
            elements_added: agg.2,
            error: error.to_string(),
            principal: principal.to_string(),
        };
        if let Ok(mut ledger) = self.state.runs.lock() {
            let _ = ledger.append(record);
        }
    }

    /// Audit a job firing (ADR-0010): target by identity (`Job` in its cube), no
    /// secrets, timestamped with the frozen `fire_millis` (never a fresh clock
    /// read) so the record is reproducible under a `ManualClock` (ADR-0013
    /// decisions 0 and 9).
    fn audit_job(&self, firing: &Firing, allowed: bool) {
        audit_at(
            &self.state,
            SCHEDULER_PRINCIPAL,
            AuditAction::JobExec,
            Some(&ObjectRef::in_cube(
                ObjectKind::Job,
                &firing.cube,
                &firing.job,
            )),
            allowed,
            firing.fire_millis,
        );
    }

    /// Spawn the production reconcile loop: tick every `tick_millis` on the
    /// injected (real) clock, each tick on a blocking thread (the flow engine
    /// blocks). The task is detached; durability never depends on a clean stop (an
    /// interrupted run recovers on restart), so it is cancelled on shutdown.
    pub fn spawn(state: AppState, tick_millis: u64) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(tick_millis.max(1)));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let scheduler = Scheduler::new(state.clone());
                // The flow engine is blocking, so each reconcile runs off the
                // async worker threads; a panicked tick is logged, not fatal.
                let _ = tokio::task::spawn_blocking(move || scheduler.tick()).await;
            }
        })
    }
}

/// A client-safe message for a flow run failure (no internal cause).
fn flow_error_message(err: &FlowError) -> String {
    match err {
        FlowError::Strip(e) => e.message.clone(),
        FlowError::Runtime { message } => message.clone(),
    }
}
