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
use crate::flow_reader::ApiFlowReader;
use crate::flow_routes::{apply_outcome, authorize_outcome_as, resolve_flow_inputs};
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

    /// The server-global jobs (ADR-0035), each paired with an empty cube string so
    /// the unchanged `due_firings`/`Firing`/ledger plumbing (which keys by cube
    /// name) treats every job under one global namespace. The selector filters by
    /// `enabled`.
    fn gather_jobs(&self) -> Vec<(String, Job)> {
        self.state
            .automation
            .lock()
            .expect("automation store mutex")
            .automation()
            .jobs
            .values()
            .map(|job| (String::new(), job.clone()))
            .collect()
    }

    /// Execute one job firing (ADR-0035): record `Queued` then `Running`, run each
    /// step flow with the frozen fire time, apply its multi-cube outcome, then
    /// record `Succeeded` (or `Failed` on the first failing step, fail-fast).
    /// `firing_principal` is the kicker (the caller for a manual kick, the
    /// scheduler service principal for a timer firing), but each step *runs as the
    /// flow's recorded owner* and is gated by that owner's object and element
    /// security; a flow with no owner fails the run (fail-closed). The run record
    /// is stamped with the owner where there is one. Reused by the manual kick.
    pub(crate) fn execute(&self, firing: &Firing, firing_principal: &str) {
        self.record(firing, RunState::Queued, (0, 0, 0), "", firing_principal);
        self.record(firing, RunState::Running, (0, 0, 0), "", firing_principal);

        let job = {
            let store = self
                .state
                .automation
                .lock()
                .expect("automation store mutex");
            store.automation().job(&firing.job).cloned()
        };
        let Some(job) = job else {
            self.fail(firing, (0, 0, 0), "unknown job", firing_principal);
            return;
        };

        // (rows_read, cells_written, elements_added) aggregated across steps.
        let mut agg = (0u64, 0u64, 0u64);
        for step in &job.steps {
            // Resolve the step's flow from the global automation store.
            let flow = {
                let store = self
                    .state
                    .automation
                    .lock()
                    .expect("automation store mutex");
                store.automation().flow(step).cloned()
            };
            let Some(flow) = flow else {
                self.fail(
                    firing,
                    agg,
                    &format!("unknown flow '{step}'"),
                    firing_principal,
                );
                return;
            };
            // A scheduled run executes as the flow's owner (ADR-0035); without one
            // there is no principal to bound its rights, so the run fails.
            let Some(owner) = flow.owner.clone() else {
                self.fail(
                    firing,
                    agg,
                    &format!("flow '{}' has no owner", flow.name),
                    firing_principal,
                );
                return;
            };

            // Resolve the flow's declared inputs (global + local connections) to
            // rows, exactly as a manual run does; a scheduled run fetches its
            // declared sources (ADR-0035).
            let inputs = match resolve_flow_inputs(&self.state, &flow, &BTreeMap::new(), "") {
                Ok(inputs) => inputs,
                Err(e) => {
                    self.fail(firing, agg, e.message(), &owner);
                    return;
                }
            };
            let cube_names = self.state.engine.cube_names();
            let reader = ApiFlowReader::new(self.state.clone(), &owner);
            // The fire time is the frozen tick value (ADR-0013 decision 0).
            let outcome = match run_flow(
                &flow.source,
                flow.default_cube.as_deref(),
                &cube_names,
                inputs,
                &BTreeMap::new(),
                firing.fire_millis,
                Box::new(reader),
            ) {
                Ok(o) => o,
                Err(e) => {
                    self.fail(firing, agg, &flow_error_message(&e), &owner);
                    return;
                }
            };
            agg.0 += outcome.report.rows_read as u64;
            // Authorize and apply AS THE OWNER, so an unattended flow can only touch
            // what its owner could touch by hand (fail-closed).
            if let Err(e) = authorize_outcome_as(&self.state, &owner, &outcome) {
                self.fail(firing, agg, e.message(), &owner);
                return;
            }
            match apply_outcome(&self.state, &outcome) {
                Ok((elements, cells)) => {
                    agg.2 += elements as u64;
                    agg.1 += cells as u64;
                }
                Err(e) => {
                    self.fail(firing, agg, e.message(), &owner);
                    return;
                }
            }
        }
        // The run succeeded; record it under the firing principal (a job with no
        // steps records the kicker, an unusual but valid case).
        self.record(firing, RunState::Succeeded, agg, "", firing_principal);
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
            Some(&ObjectRef::global(ObjectKind::Job, &firing.job)),
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
