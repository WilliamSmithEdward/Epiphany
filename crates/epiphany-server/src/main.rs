//! Epiphany server: the daemon and composition root.
//!
//! Phase 0: a placeholder entry point that proves the workspace links and the
//! deterministic harness works. The HTTP/REST layer arrives in Phase 2.
//! See `docs/ROADMAP.md`.

use epiphany_determinism::DeterministicRng;

fn main() {
    println!(
        "Epiphany server v{} (Phase 0 skeleton)",
        env!("CARGO_PKG_VERSION")
    );

    println!("wired subsystems:");
    println!("  - {}", epiphany_api::CRATE);
    for subsystem in epiphany_api::wired_subsystems() {
        println!("    - {subsystem}");
    }

    // Determinism self-check: prints the same values on every run, every machine.
    let mut rng = DeterministicRng::new(20_200_101);
    let sample: Vec<u64> = (0..3).map(|_| rng.next_u64()).collect();
    println!("deterministic self-check (seed=20200101): {sample:?}");
}

#[cfg(test)]
mod tests {
    use epiphany_determinism::DeterministicRng;

    #[test]
    fn self_check_is_reproducible() {
        let mut a = DeterministicRng::new(20_200_101);
        let mut b = DeterministicRng::new(20_200_101);
        let xs: Vec<u64> = (0..3).map(|_| a.next_u64()).collect();
        let ys: Vec<u64> = (0..3).map(|_| b.next_u64()).collect();
        assert_eq!(xs, ys);
    }

    #[test]
    fn server_sees_all_engine_subsystems() {
        assert_eq!(epiphany_api::wired_subsystems().len(), 6);
    }
}
