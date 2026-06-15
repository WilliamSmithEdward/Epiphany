//! The pure scheduling core (ADR-0013): given the declared jobs, the durable run
//! ledger, and a single clock reading frozen at the loop's wake (`tick_now`),
//! compute which firings are due. This module reads no clock and does no I/O; the
//! async driver that wakes it and dispatches runs lives at the composition root
//! (`epiphany-api`/`epiphany-server`), so the scheduling *logic* is deterministic
//! and unit-testable in isolation, exactly the property ADR-0013 requires.
//!
//! Determinism (ADR-0013 decisions 0, 1): `fire_millis` is the frozen `tick_now`,
//! never a fresh clock read; the run id is a pure function of the firing; and the
//! due-scan is ordered by `(next_due, cube, job)` so dispatch is observable-
//! deterministic, never a `HashMap` iteration.

use epiphany_core::Job;

use crate::ledger::RunLedger;

/// A firing the loop selected for one job at one tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Firing {
    /// The cube the job writes.
    pub cube: String,
    /// The job name.
    pub job: String,
    /// The frozen clock value for this tick (ADR-0013 decision 0).
    pub fire_millis: u64,
    /// The deterministic run id derived from the firing.
    pub run_id: String,
    /// `true` when a prior run of the same job is still active, so this firing is
    /// coalesced to a logged-and-audited skip (single-flight, decision 6) rather
    /// than dispatched.
    pub coalesced: bool,
}

/// The deterministic run id for a scheduled firing (ADR-0013 decision 3): a pure
/// function of `(cube, job, fire_millis)`, so a firing re-derived after a restart
/// reuses the same id and the ledger dedupes it.
pub fn scheduled_run_id(cube: &str, job: &str, fire_millis: u64) -> String {
    format!("sched:{cube}:{job}:{fire_millis}")
}

/// Select the firings due at `tick_now`, as a pure function of the declared jobs
/// (each paired with its owning cube), the run ledger, and `tick_now`.
///
/// A job fires when it is enabled and `trigger.next_due(last_succeeded_fire) <=
/// tick_now`. Because `last_fired` advances only on a successful run, an
/// interrupted or never-run job re-derives as due. A due job whose prior run is
/// still active is returned `coalesced` (single-flight). Firings are ordered by
/// `(next_due, cube, job)` for deterministic dispatch.
pub fn due_firings(jobs: &[(String, Job)], ledger: &RunLedger, tick_now: u64) -> Vec<Firing> {
    let mut due: Vec<(u64, &str, &Job)> = jobs
        .iter()
        .filter(|(_, job)| job.enabled)
        .filter_map(|(cube, job)| {
            let last = ledger.last_succeeded_fire(cube, &job.name);
            let next_due = job.trigger.next_due(last);
            (next_due <= tick_now).then_some((next_due, cube.as_str(), job))
        })
        .collect();
    due.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(b.1))
            .then_with(|| a.2.name.cmp(&b.2.name))
    });
    due.into_iter()
        .map(|(_, cube, job)| Firing {
            cube: cube.to_string(),
            job: job.name.clone(),
            fire_millis: tick_now,
            run_id: scheduled_run_id(cube, &job.name, tick_now),
            coalesced: ledger.job_in_flight(cube, &job.name),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{RunRecord, RunState};
    use epiphany_core::Trigger;

    fn job(name: &str, every: u64, enabled: bool) -> (String, Job) {
        (
            "Sales".to_string(),
            Job {
                name: name.to_string(),
                steps: vec!["load".to_string()],
                trigger: Trigger::Interval {
                    every_millis: every,
                },
                enabled,
            },
        )
    }

    fn succeeded(cube: &str, job: &str, fire: u64) -> RunRecord {
        RunRecord {
            id: scheduled_run_id(cube, job, fire),
            cube: cube.to_string(),
            target: job.to_string(),
            is_job: true,
            fire_millis: fire,
            state: RunState::Succeeded,
            rows_read: 0,
            cells_written: 0,
            elements_added: 0,
            error: String::new(),
            principal: "scheduler".to_string(),
        }
    }

    #[test]
    fn never_fired_is_due_immediately_then_respects_the_interval() {
        let jobs = vec![job("nightly", 1000, true)];
        let mut ledger = RunLedger::in_memory();
        // First tick: due (never fired).
        let f = due_firings(&jobs, &ledger, 5000);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].fire_millis, 5000);
        assert_eq!(f[0].run_id, "sched:Sales:nightly:5000");
        assert!(!f[0].coalesced);

        // Record success at 5000: next due is 6000.
        ledger.append(succeeded("Sales", "nightly", 5000)).unwrap();
        assert!(due_firings(&jobs, &ledger, 5999).is_empty());
        assert_eq!(due_firings(&jobs, &ledger, 6000).len(), 1);
    }

    #[test]
    fn disabled_jobs_never_fire() {
        let jobs = vec![job("off", 1000, false)];
        assert!(due_firings(&jobs, &RunLedger::in_memory(), 1_000_000).is_empty());
    }

    #[test]
    fn an_in_flight_job_coalesces() {
        let jobs = vec![job("slow", 1000, true)];
        let mut ledger = RunLedger::in_memory();
        // A run is already active.
        ledger
            .append(RunRecord {
                state: RunState::Running,
                ..succeeded("Sales", "slow", 0)
            })
            .unwrap();
        let f = due_firings(&jobs, &ledger, 5000);
        assert_eq!(f.len(), 1);
        assert!(f[0].coalesced, "an overlapping firing must coalesce");
    }

    #[test]
    fn firings_are_ordered_deterministically() {
        let jobs = vec![
            ("Sales".to_string(), job("zebra", 1000, true).1),
            ("Sales".to_string(), job("alpha", 1000, true).1),
        ];
        let f = due_firings(&jobs, &RunLedger::in_memory(), 5000);
        // Same next_due (both never fired -> 0), so ordered by job name.
        assert_eq!(f[0].job, "alpha");
        assert_eq!(f[1].job, "zebra");
    }
}
