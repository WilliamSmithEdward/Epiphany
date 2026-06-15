//! Scale benchmark for the reconcile loop's due-selection (ADR-0013, the Phase 8
//! "scale benchmark"). It times one tick's pure `due_firings` scan over many
//! declared jobs against a populated run ledger, so the cost of waking the loop
//! is visible and bounded.
//!
//! A self-contained harness (no external bench framework, no added dependency),
//! mirroring `epiphany-core`'s `cube_ops`. Run with `cargo bench -p epiphany-flow`.
//! Numbers are best-of-N to cut scheduler noise; they are indicative, not promises.

use std::hint::black_box;
use std::time::{Duration, Instant};

use epiphany_core::{Job, Trigger};
use epiphany_flow::{due_firings, scheduled_run_id, RunLedger, RunRecord, RunState};

fn jobs(n: usize) -> Vec<(String, Job)> {
    (0..n)
        .map(|i| {
            (
                "Sales".to_string(),
                Job {
                    name: format!("job{i}"),
                    steps: vec!["load".to_string()],
                    trigger: Trigger::Interval { every_millis: 1000 },
                    enabled: true,
                },
            )
        })
        .collect()
}

/// One reconcile tick's due-scan over `N` jobs, half of which fired recently (so
/// they are not due) and half never (due now).
fn reconcile_select() {
    const N: usize = 2_000;
    const REPS: u32 = 50;
    let jobs = jobs(N);
    let mut ledger = RunLedger::in_memory();
    for i in 0..N / 2 {
        let job = format!("job{i}");
        ledger
            .append(RunRecord {
                id: scheduled_run_id("Sales", &job, 5000),
                cube: "Sales".to_string(),
                target: job,
                is_job: true,
                fire_millis: 5000,
                state: RunState::Succeeded,
                rows_read: 0,
                cells_written: 0,
                elements_added: 0,
                error: String::new(),
                principal: "scheduler".to_string(),
            })
            .unwrap();
    }

    let mut best = Duration::MAX;
    for _ in 0..REPS {
        let start = Instant::now();
        let due = due_firings(&jobs, &ledger, 5500);
        best = best.min(start.elapsed());
        black_box(due.len());
    }
    let per_sec = N as f64 / best.as_secs_f64();
    println!(
        "reconcile_select   {N} jobs in {:>7.3} ms  ->  {:>6.2} M jobs/sec  (one tick's due-scan)",
        best.as_secs_f64() * 1e3,
        per_sec / 1e6,
    );
}

fn main() {
    println!("epiphany-flow scheduler benchmarks (release):");
    reconcile_select();
}
