//! Differential test for the sparse consolidation path (ADR-0005).
//!
//! The completeness gate ([`epiphany_calc::safe_fed_set`]) licenses a consolidated
//! read to use the sparse union scan ([`epiphany_core::Cube::consolidate_fed`],
//! stored cells ∪ the fed leaf set) instead of the dense cartesian enumeration
//! ([`epiphany_core::Cube::consolidate_with`]) — but only where the two are
//! provably byte-identical. This test proves that property over a battery of
//! GENERATED cubes (varied dimensionality, leaf counts, weighted/multi-level
//! consolidations, sparsity, and rule shapes), seeded by `DeterministicRng`:
//!
//!   * For every cube the gate marks `Safe`, `consolidate_fed(fed) ==
//!     consolidate_with()` byte-for-byte (exact scaled `i64`) at EVERY consolidated
//!     coordinate (every cross-product of each dimension's members), for the
//!     rule-aware value function the evaluator itself uses. The `Ok`/`Err` outcome
//!     must match too, not just the value.
//!   * For every cube the gate marks `Dense` (opaque or erroring rules), the
//!     evaluator's own read still returns the correct dense value.
//!
//! If any `Safe` cube diverged, the gate would be wrong and this test would fail —
//! the signal to tighten the gate, never to loosen the equality. Dependency-free:
//! hand-authored assertions and the determinism crate's seeded RNG, no proptest.

use epiphany_calc::{compile, rules::parse, safe_fed_set, CalcEngine, CompiledModel, FedGate};
use epiphany_core::{Cube, Dimension, ElementKind, Fixed, QueryError};
use epiphany_determinism::DeterministicRng;

/// A single-cube eval registry (target at ordinal 0), like the ones the unit
/// tests use, but WITHOUT a `fed_set` override — so the `CalcEngine` inside it
/// always takes the dense path. This is deliberately the reference registry: it
/// gives the always-correct dense truth to compare the sparse scan against, and it
/// is the exact `EvalRegistry` `safe_fed_set` validates against.
struct OneCube {
    cube: Cube,
    model: CompiledModel,
}

impl epiphany_calc::EvalRegistry for OneCube {
    fn cube(&self, o: u32) -> Option<&Cube> {
        (o == 0).then_some(&self.cube)
    }
    fn compiled(&self, o: u32) -> Option<&CompiledModel> {
        (o == 0).then_some(&self.model)
    }
    fn ordinal(&self, name: &str) -> Option<u32> {
        (name == self.cube.name()).then_some(0)
    }
}

/// Wraps a `OneCube` and OVERRIDES `fed_set` to return a supplied fed set, exactly
/// as the production `PinnedRegistry` does. Used to drive the *evaluator* (not the
/// raw consolidation functions) down the sparse path, so a test can compare a full
/// `engine.value` read against a dense-only registry's read — the production seam,
/// including the rule-override-before-consolidation behavior of `C:` rules.
struct FedRegistry {
    inner: OneCube,
    fed: Vec<Box<[u32]>>,
}

impl epiphany_calc::EvalRegistry for FedRegistry {
    fn cube(&self, o: u32) -> Option<&Cube> {
        self.inner.cube(o)
    }
    fn compiled(&self, o: u32) -> Option<&CompiledModel> {
        self.inner.compiled(o)
    }
    fn ordinal(&self, name: &str) -> Option<u32> {
        self.inner.ordinal(name)
    }
    fn fed_set(&self, o: u32) -> Option<&[Box<[u32]>]> {
        (o == 0).then_some(self.fed.as_slice())
    }
}

/// A two-cube eval registry (target Sales at ordinal 0, FX at ordinal 1), for the
/// cross-cube opaque-rule case. No `fed_set` override: dense reference path.
struct TwoCubes {
    cubes: Vec<Cube>,
    models: Vec<CompiledModel>,
}

impl epiphany_calc::EvalRegistry for TwoCubes {
    fn cube(&self, o: u32) -> Option<&Cube> {
        self.cubes.get(o as usize)
    }
    fn compiled(&self, o: u32) -> Option<&CompiledModel> {
        self.models.get(o as usize)
    }
    fn ordinal(&self, name: &str) -> Option<u32> {
        self.cubes
            .iter()
            .position(|c| c.name() == name)
            .map(|i| i as u32)
    }
}

/// Build a "Region"-shaped dimension: `n` numeric leaves under a `Total`, with a
/// randomized (possibly multi-level, possibly negative/non-unit weighted)
/// hierarchy so `leaf_weights` is exercised with real weights and depth.
///
/// Shape variants (chosen by `rng`):
///   * a flat `Total` summing all leaves with per-edge weights from {1,1,1,2,-1}
///     (1 is favored so most rollups are plain sums, but weighted and net-zero
///     cases appear);
///   * or a two-level tree: leaves split into two `Sub` consolidations, both under
///     `Total`, so a leaf reaches `Total` via a mid-level node (nested weights).
fn region_dim(name: &str, n: u32, rng: &mut DeterministicRng) -> Dimension {
    let mut d = Dimension::new(name);
    let leaves: Vec<u32> = (0..n).map(|i| d.add_leaf(format!("{name}_l{i}"))).collect();
    let total = d.add_consolidated(format!("{name}_Total"));
    let weight_of = |rng: &mut DeterministicRng| -> i64 {
        match rng.next_below(5) {
            0 => 2,
            1 => -1,
            _ => 1,
        }
    };
    let two_level = n >= 3 && rng.next_below(2) == 0;
    if two_level {
        let sub_a = d.add_consolidated(format!("{name}_SubA"));
        let sub_b = d.add_consolidated(format!("{name}_SubB"));
        d.add_child(total, sub_a, weight_of(rng)).unwrap();
        d.add_child(total, sub_b, weight_of(rng)).unwrap();
        for (i, &leaf) in leaves.iter().enumerate() {
            let parent = if i % 2 == 0 { sub_a } else { sub_b };
            d.add_child(parent, leaf, weight_of(rng)).unwrap();
        }
    } else {
        for &leaf in &leaves {
            d.add_child(total, leaf, weight_of(rng)).unwrap();
        }
    }
    d
}

/// A measure dimension of numeric leaves by fixed names, so rules can reference
/// them by name and always compile. Always includes Sales, Cost, Margin, Net; adds
/// Ratio when `with_ratio` (used to plant an erroring rule).
fn measure_dim(with_ratio: bool) -> Dimension {
    let mut m = Dimension::new("Measure");
    m.add_leaf("Sales");
    m.add_leaf("Cost");
    m.add_leaf("Margin");
    m.add_leaf("Net");
    if with_ratio {
        m.add_leaf("Ratio");
    }
    m
}

/// The rule program a generated cube carries. Each maps to source that compiles
/// against the measure names above. The `Safe*` variants are analyzable and (with
/// complete inference) fully fed, so the gate should admit sparse; the `Dense*`
/// variants force the dense fallback (opaque / erroring), which the test checks
/// the gate rejects while the dense read stays correct.
#[derive(Clone, Copy, Debug)]
enum RuleProgram {
    /// No rules: sparse degenerates to summing stored cells (== dense).
    None,
    /// Margin = Sales - Cost (a leaf rule; fed where inputs are populated).
    Margin,
    /// Margin = Sales - Cost; Net = Margin + Sales (chained leaf rules).
    MarginAndNet,
    /// Margin = Sales - Cost + 3 (base-potent constant: whole area fed).
    MarginPlusConst,
    /// Net = value[Region:Total, Sales] (reads a consolidated input).
    NetFromConsolidated,
    /// Ratio = Sales / Cost — erroring where Cost is zero: gate -> Dense.
    RatioDivByZero,
}

impl RuleProgram {
    fn source(self) -> &'static str {
        match self {
            RuleProgram::None => "",
            RuleProgram::Margin => {
                "['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];"
            }
            RuleProgram::MarginAndNet => {
                "['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];\n\
                 ['Measure':'Net'] = value['Measure':'Margin'] + value['Measure':'Sales'];"
            }
            RuleProgram::MarginPlusConst => {
                "['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'] + 3;"
            }
            RuleProgram::NetFromConsolidated => {
                "['Measure':'Net'] = value['Region':'Region_Total', 'Measure':'Sales'];"
            }
            RuleProgram::RatioDivByZero => {
                "['Measure':'Ratio'] = value['Measure':'Sales'] / value['Measure':'Cost'];"
            }
        }
    }

    fn needs_ratio(self) -> bool {
        matches!(self, RuleProgram::RatioDivByZero)
    }
}

/// Deterministically build one generated cube for `(seed, program)`, returning the
/// single-cube dense-reference registry. The generator never writes a stored cell
/// over a rule-computed measure leaf (which the write path forbids anyway), so the
/// stored data and the rule output stay disjoint, as in a real model.
fn generate(seed: u64, program: RuleProgram) -> OneCube {
    let mut rng = DeterministicRng::new(seed);
    // 1..=2 extra "region-like" dims (so rank is 2 or 3), each 2..=5 leaves.
    let extra_dims = 1 + rng.next_below(2) as usize; // 1 or 2 region dims
    let mut dims: Vec<Dimension> = Vec::new();
    for k in 0..extra_dims {
        let n = 2 + rng.next_below(4) as u32; // 2..=5 leaves
        dims.push(region_dim(
            if k == 0 { "Region" } else { "Period" },
            n,
            &mut rng,
        ));
    }
    dims.push(measure_dim(program.needs_ratio()));
    let measure_pos = dims.len() - 1;
    let mut cube = Cube::new("Gen", dims).unwrap();

    // Which measures a rule computes (so the generator avoids storing over them).
    let computed_names: &[&str] = match program {
        RuleProgram::None => &[],
        RuleProgram::Margin | RuleProgram::MarginPlusConst => &["Margin"],
        RuleProgram::MarginAndNet => &["Margin", "Net"],
        RuleProgram::NetFromConsolidated => &["Net"],
        RuleProgram::RatioDivByZero => &["Ratio"],
    };
    let computed_measures: Vec<u32> = computed_names
        .iter()
        .map(|n| cube.dimension(measure_pos).resolve(n).unwrap())
        .collect();

    // Populate a random sparse subset of NON-computed leaf coordinates with random
    // (possibly negative) values. Iterate the dense leaf-coordinate space of the
    // region dims × the non-computed measure leaves.
    let leaf_lists: Vec<Vec<u32>> = (0..cube.rank())
        .map(|d| {
            (0..cube.dimension(d).len())
                .filter(|&i| cube.dimension(d).element(i).unwrap().kind == ElementKind::Leaf)
                .filter(|&i| {
                    // Exclude computed measures in the measure dim.
                    d != measure_pos || !computed_measures.contains(&i)
                })
                .collect()
        })
        .collect();
    let mut coord = vec![0u32; cube.rank()];
    fill_leaves(&mut cube, &leaf_lists, 0, &mut coord, &mut rng);

    let source = program.source();
    let model = compile(
        &cube,
        &epiphany_calc::SingleCube::new(&cube),
        &parse(source).unwrap(),
        1,
    )
    .unwrap();
    OneCube { cube, model }
}

/// Recursively walk the cartesian product of `leaf_lists`, writing a random value
/// at roughly half the coordinates (deterministic).
fn fill_leaves(
    cube: &mut Cube,
    leaf_lists: &[Vec<u32>],
    d: usize,
    coord: &mut Vec<u32>,
    rng: &mut DeterministicRng,
) {
    if d == leaf_lists.len() {
        // ~50% populated; values in -500..=499 scaled, so negatives and zero occur.
        if rng.next_below(2) == 0 {
            let raw = rng.next_below(1000) as i64 - 500;
            if raw != 0 {
                cube.set_leaf(coord, Fixed::from_scaled(raw)).unwrap();
            }
        }
        return;
    }
    for &leaf in &leaf_lists[d] {
        coord[d] = leaf;
        fill_leaves(cube, leaf_lists, d + 1, coord, rng);
    }
}

/// Enumerate EVERY coordinate of the cube (the full cartesian product of all
/// members of every dimension, leaves and consolidations alike).
fn all_coords(cube: &Cube) -> Vec<Vec<u32>> {
    let per_dim: Vec<Vec<u32>> = (0..cube.rank())
        .map(|d| (0..cube.dimension(d).len()).collect())
        .collect();
    let mut out = Vec::new();
    let mut coord = vec![0u32; cube.rank()];
    fn walk(per_dim: &[Vec<u32>], d: usize, coord: &mut Vec<u32>, out: &mut Vec<Vec<u32>>) {
        if d == per_dim.len() {
            out.push(coord.clone());
            return;
        }
        for &e in &per_dim[d] {
            coord[d] = e;
            walk(per_dim, d + 1, coord, out);
        }
    }
    walk(&per_dim, 0, &mut coord, &mut out);
    out
}

/// The seeds and programs the battery runs over.
const SEEDS: [u64; 12] = [1, 2, 3, 5, 7, 11, 13, 17, 42, 99, 1234, 0xDEAD_BEEF];
const PROGRAMS: [RuleProgram; 6] = [
    RuleProgram::None,
    RuleProgram::Margin,
    RuleProgram::MarginAndNet,
    RuleProgram::MarginPlusConst,
    RuleProgram::NetFromConsolidated,
    RuleProgram::RatioDivByZero,
];

/// THE differential test. For every generated cube:
///   * compute the completeness gate;
///   * if Safe, assert `consolidate_fed(fed)` == `consolidate_with()` byte-for-byte
///     (same value AND same Ok/Err) at EVERY coordinate, using the rule-aware value
///     function the evaluator uses;
///   * if Dense, assert the evaluator's dense read succeeds/behaves as its own
///     consolidate_with (the fallback path) — i.e. it does not silently take a
///     sparse shortcut.
#[test]
fn sparse_fed_equals_dense_wherever_the_gate_is_safe() {
    let mut safe_cubes = 0usize;
    let mut dense_cubes = 0usize;
    let mut safe_coords_checked = 0usize;
    let mut consolidated_coords_checked = 0usize;
    let mut erroring_dense_seen = false;

    for &seed in &SEEDS {
        for &program in &PROGRAMS {
            let reg = generate(seed, program);
            let cube = &reg.cube;
            // The reference engine ALWAYS takes the dense path (OneCube has no
            // fed_set override), so `value` here is the dense truth.
            let engine = CalcEngine::new(&reg);

            let gate = safe_fed_set(&reg, 0).expect("gate evaluates");
            let coords = all_coords(cube);

            match gate {
                FedGate::Safe(fed) => {
                    safe_cubes += 1;
                    // Note: even the div-by-zero PROGRAM can be gated Safe for a
                    // particular random dataset — if that dataset never actually hits
                    // a zero divisor at any Ratio leaf, there is no erroring leaf and
                    // sparse genuinely equals dense. The gate is a property of the
                    // data+rules, not the rule text; the per-coordinate equality below
                    // is the real proof, so no rule-shape is force-asserted here.
                    for c in &coords {
                        // Dense reference value at this coordinate (the always-correct
                        // path). Both closures pull leaves back through the SAME dense
                        // engine, so the only difference under test is the enumeration
                        // strategy (dense product vs stored ∪ fed union).
                        let dense: Result<Fixed, QueryError> = cube
                            .consolidate_with::<QueryError, _>(c, |lc| Ok(engine.value(0, lc)?));
                        let sparse: Result<Fixed, QueryError> = cube
                            .consolidate_fed::<QueryError, _>(c, &fed, |lc| {
                                Ok(engine.value(0, lc)?)
                            });
                        // Byte-identical: same Ok/Err, and on Ok the exact scaled i64.
                        match (&dense, &sparse) {
                            (Ok(d), Ok(s)) => assert_eq!(
                                d.to_scaled(),
                                s.to_scaled(),
                                "sparse != dense at {c:?} (seed {seed}, {program:?}): \
                                 dense={d:?} sparse={s:?}"
                            ),
                            (Err(_), Err(_)) => {}
                            _ => panic!(
                                "Ok/Err outcome differs at {c:?} (seed {seed}, {program:?}): \
                                 dense={dense:?} sparse={sparse:?}"
                            ),
                        }
                        let is_consolidated = c
                            .iter()
                            .enumerate()
                            .any(|(d, &i)| !cube.dimension(d).element(i).unwrap().kind.is_leaf());
                        if is_consolidated {
                            consolidated_coords_checked += 1;
                        }
                        safe_coords_checked += 1;
                    }
                }
                FedGate::Dense(reason) => {
                    dense_cubes += 1;
                    // In this battery the only rule shape that drives Dense is the
                    // erroring one (div-by-zero), and only "erroring"/"under-fed"
                    // could be its reason; a "safe" program reaching Dense would mean
                    // the gate got MORE conservative than expected, which is
                    // acceptable (never wrong). Record when the erroring reason fires
                    // so the battery's coverage of that branch is asserted globally.
                    if reason == "erroring" {
                        erroring_dense_seen = true;
                    }
                    // Fallback correctness: the evaluator's own read equals its dense
                    // consolidate_with at every coordinate (it did not shortcut).
                    for c in &coords {
                        let via_engine = engine.value(0, c);
                        let via_dense: Result<Fixed, QueryError> = cube
                            .consolidate_with::<QueryError, _>(c, |lc| Ok(engine.value(0, lc)?));
                        match (via_engine, via_dense) {
                            (Ok(a), Ok(b)) => assert_eq!(a.to_scaled(), b.to_scaled()),
                            (Err(_), Err(_)) => {}
                            // A leaf coordinate: engine.value reads the leaf directly
                            // while consolidate_with also short-circuits to the same
                            // single leaf read, so these agree; a consolidated coord
                            // goes through consolidate_with in the engine too.
                            (a, b) => {
                                // Reconcile the leaf fast-path: for a pure leaf coord
                                // both are a single value; any mismatch is a real bug.
                                panic!(
                                    "engine vs dense mismatch at {c:?} (seed {seed}, {program:?}): \
                                     {a:?} vs {b:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    // Sanity on coverage: the battery actually exercised both branches and a
    // non-trivial number of consolidated coordinates on the sparse path.
    assert!(safe_cubes > 0, "some cubes were gated Safe");
    assert!(dense_cubes > 0, "some cubes fell back to Dense");
    assert!(
        consolidated_coords_checked > 50,
        "checked many consolidated coordinates on the sparse path, got {consolidated_coords_checked}"
    );
    assert!(
        erroring_dense_seen,
        "the battery exercised the erroring -> Dense fallback"
    );
    let _ = safe_coords_checked;
}

/// A deterministic, guaranteed erroring case (independent of the random battery's
/// luck): a `Ratio = Sales / Cost` rule over data where a Ratio target leaf has a
/// non-zero Sales and a ZERO Cost, so the rule divides by zero. The gate MUST
/// refuse the sparse path (`Dense("erroring")`), and the evaluator's dense read of
/// the erroring consolidation must surface the error rather than a wrong number.
#[test]
fn a_guaranteed_div_by_zero_cube_falls_back_to_dense() {
    let mut region = Dimension::new("Region");
    let n = region.add_leaf("North");
    let s = region.add_leaf("South");
    let total = region.add_consolidated("Total");
    region.add_child(total, n, 1).unwrap();
    region.add_child(total, s, 1).unwrap();
    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Sales");
    measure.add_leaf("Cost");
    measure.add_leaf("Ratio");
    let mut cube = Cube::new("DZ", vec![region, measure]).unwrap();
    let sales = cube.dimension(1).resolve("Sales").unwrap();
    let cost = cube.dimension(1).resolve("Cost").unwrap();
    // North has Sales but NO Cost -> Ratio[North] divides by zero.
    cube.set_leaf(&[n, sales], Fixed::from(100)).unwrap();
    cube.set_leaf(&[s, sales], Fixed::from(200)).unwrap();
    cube.set_leaf(&[s, cost], Fixed::from(50)).unwrap();
    let model = compile(
        &cube,
        &epiphany_calc::SingleCube::new(&cube),
        &parse("['Measure':'Ratio'] = value['Measure':'Sales'] / value['Measure':'Cost'];")
            .unwrap(),
        1,
    )
    .unwrap();
    let reg = OneCube { cube, model };

    // Gate: must be Dense with the erroring reason (a Ratio leaf divides by zero).
    match safe_fed_set(&reg, 0).unwrap() {
        FedGate::Dense(reason) => assert_eq!(reason, "erroring"),
        FedGate::Safe(_) => panic!("a div-by-zero cube must not be gated Safe"),
    }

    // The evaluator's dense read of the Total Ratio consolidation surfaces the
    // div-by-zero (it enumerates North/Ratio, which errors) — proof the fallback
    // path does not paper over the error with a sparse shortcut.
    let engine = CalcEngine::new(&reg);
    let ratio = reg.cube.dimension(1).resolve("Ratio").unwrap();
    let total = reg.cube.dimension(0).resolve("Total").unwrap();
    assert!(
        engine.value(0, &[total, ratio]).is_err(),
        "the dense rollup surfaces the div-by-zero"
    );
    // And a healthy stored rollup on the SAME cube is still correct dense:
    // Total Sales = 100 + 200 = 300.
    assert_eq!(engine.value(0, &[total, sales]).unwrap(), Fixed::from(300));
}

/// A focused, hand-authored companion: a weighted two-level hierarchy where a
/// rule-derived leaf feeds a nested rollup. Proves the sparse scan includes a fed
/// leaf through a multi-level, non-unit-weighted path exactly as the dense path
/// does — the case a flat single-level fixture would not catch.
#[test]
fn sparse_matches_dense_through_a_weighted_two_level_rollup() {
    // Region: l0,l1,l2 leaves; SubA={l0,l1} w2/w1, SubB={l2} w3; Total=SubA(w1)+SubB(-1).
    let mut region = Dimension::new("Region");
    let l0 = region.add_leaf("l0");
    let l1 = region.add_leaf("l1");
    let l2 = region.add_leaf("l2");
    let sub_a = region.add_consolidated("SubA");
    let sub_b = region.add_consolidated("SubB");
    let total = region.add_consolidated("Total");
    region.add_child(sub_a, l0, 2).unwrap();
    region.add_child(sub_a, l1, 1).unwrap();
    region.add_child(sub_b, l2, 3).unwrap();
    region.add_child(total, sub_a, 1).unwrap();
    region.add_child(total, sub_b, -1).unwrap();

    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Sales");
    measure.add_leaf("Cost");
    measure.add_leaf("Margin");

    let mut cube = Cube::new("W", vec![region, measure]).unwrap();
    let sales = cube.dimension(1).resolve("Sales").unwrap();
    let cost = cube.dimension(1).resolve("Cost").unwrap();
    // Populate Sales/Cost sparsely across the three region leaves.
    cube.set_leaf(&[l0, sales], Fixed::from(100)).unwrap();
    cube.set_leaf(&[l0, cost], Fixed::from(40)).unwrap();
    cube.set_leaf(&[l1, sales], Fixed::from(70)).unwrap();
    cube.set_leaf(&[l2, cost], Fixed::from(15)).unwrap();
    // Margin = Sales - Cost: a rule-derived leaf (fed where inputs are populated).
    let model = compile(
        &cube,
        &epiphany_calc::SingleCube::new(&cube),
        &parse("['Measure':'Margin'] = value['Measure':'Sales'] - value['Measure':'Cost'];")
            .unwrap(),
        1,
    )
    .unwrap();
    let reg = OneCube { cube, model };
    let engine = CalcEngine::new(&reg);

    let gate = safe_fed_set(&reg, 0).unwrap();
    let fed = match gate {
        FedGate::Safe(fed) => fed,
        FedGate::Dense(r) => panic!("expected Safe, got Dense({r})"),
    };

    // Check the Margin rollups at every consolidated Region node (Sub/Total) —
    // these mix a rule-derived leaf, non-unit weights, and a negative edge.
    let margin = reg.cube.dimension(1).resolve("Margin").unwrap();
    for &node in &[sub_a, sub_b, total] {
        let c = [node, margin];
        let dense = reg
            .cube
            .consolidate_with::<QueryError, _>(&c, |lc| Ok(engine.value(0, lc)?))
            .unwrap();
        let sparse = reg
            .cube
            .consolidate_fed::<QueryError, _>(&c, &fed, |lc| Ok(engine.value(0, lc)?))
            .unwrap();
        assert_eq!(
            dense.to_scaled(),
            sparse.to_scaled(),
            "weighted rollup mismatch at Region node {node}"
        );
    }
    // Concretely: Margin l0=60, l1=70, l2=-15.
    // SubA = 2*60 + 1*70 = 190; SubB = 3*(-15) = -45; Total = 1*190 + (-1)*(-45) = 235.
    let total_margin = reg
        .cube
        .consolidate_fed::<QueryError, _>(&[total, margin], &fed, |lc| Ok(engine.value(0, lc)?))
        .unwrap();
    assert_eq!(total_margin, Fixed::from(235));
}

/// An OPAQUE-rule cube falls back to dense and still returns the correct dense
/// value. `Rev = FX!Rate` is driven ONLY by a cross-cube scalar, so feeder
/// inference cannot localize which Rev leaves are non-zero (ADR-0005 decision 3):
/// the rule is reported opaque, and the gate must therefore refuse the sparse path
/// (`Dense("opaque-rules")`). The dense read then correctly rolls up the
/// rule-derived Rev leaves (Rate is non-zero at every region), which a sparse scan
/// over stored ∪ (empty) fed would have UNDER-counted — the exact silent-wrong-zero
/// the gate exists to prevent.
#[test]
fn an_opaque_cross_cube_rule_cube_falls_back_to_dense_and_stays_correct() {
    // Sales: Region(North,South,Total) x Measure(Units, Rev). Only Units stored.
    let mut region = Dimension::new("Region");
    let north = region.add_leaf("North");
    let south = region.add_leaf("South");
    let total = region.add_consolidated("Total");
    region.add_child(total, north, 1).unwrap();
    region.add_child(total, south, 1).unwrap();
    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Units");
    measure.add_leaf("Rev");
    let mut sales = Cube::new("Sales", vec![region, measure]).unwrap();
    let units = sales.dimension(1).resolve("Units").unwrap();
    sales.set_leaf(&[north, units], Fixed::from(10)).unwrap();
    sales.set_leaf(&[south, units], Fixed::from(20)).unwrap();

    // FX: a single cross-cube scalar Rate = 3.
    let mut pair = Dimension::new("Pair");
    let rate = pair.add_leaf("Rate");
    let mut fx = Cube::new("FX", vec![pair]).unwrap();
    fx.set_leaf(&[rate], Fixed::from(3)).unwrap();

    let reg_for_compile = epiphany_calc::VecRegistry::new(vec![sales.clone(), fx.clone()]);
    let sales_model = compile(
        &sales,
        &reg_for_compile,
        &parse("['Measure':'Rev'] = 'FX'!['Pair':'Rate'];").unwrap(),
        1,
    )
    .unwrap();
    let fx_model = compile(&fx, &reg_for_compile, &parse("").unwrap(), 1).unwrap();
    let reg = TwoCubes {
        cubes: vec![sales, fx],
        models: vec![sales_model, fx_model],
    };

    // Gate: the opaque cross-cube-only rule forces the dense path.
    match safe_fed_set(&reg, 0).unwrap() {
        FedGate::Dense(reason) => assert_eq!(reason, "opaque-rules"),
        FedGate::Safe(_) => panic!("an opaque-rule cube must not be gated Safe"),
    }

    // Dense read is correct: Rev is 3 at BOTH North and South (the cross-cube
    // scalar), so Total Rev = 3 + 3 = 6. A sparse scan over stored ∪ empty-fed
    // would see NO Rev leaf (none stored, none fed) and wrongly return 0 — which
    // is exactly why the gate refused sparse here.
    let engine = CalcEngine::new(&reg);
    let rev = reg.cubes[0].dimension(1).resolve("Rev").unwrap();
    let total_i = reg.cubes[0].dimension(0).resolve("Total").unwrap();
    let north_i = reg.cubes[0].dimension(0).resolve("North").unwrap();
    assert_eq!(engine.value(0, &[north_i, rev]).unwrap(), Fixed::from(3));
    assert_eq!(engine.value(0, &[total_i, rev]).unwrap(), Fixed::from(6));

    // Demonstrate the trap the gate avoids: an (unsafe) sparse scan with the empty
    // fed set genuinely under-counts here, proving the gate's refusal was load-bearing.
    let empty_fed: Vec<Box<[u32]>> = Vec::new();
    let unsafe_sparse =
        reg.cubes[0]
            .consolidate_fed::<QueryError, _>(&[total_i, rev], &empty_fed, |lc| {
                Ok(engine.value(0, lc)?)
            })
            .unwrap();
    assert_eq!(
        unsafe_sparse,
        Fixed::ZERO,
        "an unfed sparse scan under-counts the opaque rollup — the gate must forbid it"
    );
}

/// End-to-end at the EVALUATOR seam (not the raw consolidation functions): drive a
/// full `engine.value` read through a registry whose `fed_set` licenses sparse, and
/// assert it equals a dense-only registry's read at EVERY coordinate — over the
/// same generated battery. This exercises exactly what production does: a
/// consolidated coordinate with a matching rule is overridden (never consolidated),
/// and only a genuine rollup takes the sparse-vs-dense path. Any divergence here is
/// a wrong value a real read would return.
#[test]
fn evaluator_sparse_seam_equals_dense_evaluator_over_the_battery() {
    for &seed in &SEEDS {
        for &program in &PROGRAMS {
            let dense_reg = generate(seed, program);
            let gate = safe_fed_set(&dense_reg, 0).expect("gate evaluates");
            let fed = match gate {
                FedGate::Safe(fed) => fed,
                // Only the sparse-licensed cubes have a seam to compare; a Dense cube
                // stays on the dense path in production, already covered elsewhere.
                FedGate::Dense(_) => continue,
            };
            let coords = all_coords(&dense_reg.cube);
            // Two registries over the SAME cube+model: one dense (no fed_set), one
            // sparse-licensed. `FedRegistry` moves the cube in, so rebuild the dense
            // twin first for the reference reads.
            let dense_engine_reg = generate(seed, program);
            let dense_engine = CalcEngine::new(&dense_engine_reg);
            let sparse_reg = FedRegistry {
                inner: dense_reg,
                fed,
            };
            let sparse_engine = CalcEngine::new(&sparse_reg);
            for c in &coords {
                let dense = dense_engine.value(0, c);
                let sparse = sparse_engine.value(0, c);
                match (&dense, &sparse) {
                    (Ok(d), Ok(s)) => assert_eq!(
                        d.to_scaled(),
                        s.to_scaled(),
                        "evaluator sparse != dense at {c:?} (seed {seed}, {program:?})"
                    ),
                    (Err(_), Err(_)) => {}
                    _ => panic!(
                        "evaluator Ok/Err differs at {c:?} (seed {seed}, {program:?}): \
                         dense={dense:?} sparse={sparse:?}"
                    ),
                }
            }
        }
    }
}

/// A `C:`-scoped rule (ADR-0041) recomputes AT the consolidated coordinate rather
/// than summing children. Such a rule fires as an override (via `matching_rule`)
/// before either consolidation path runs, so the sparse seam must not change its
/// value: the evaluator read through a sparse-licensed registry must equal the
/// dense evaluator at every coordinate — including the `C:` total, where the rule,
/// not the rollup, decides the value.
#[test]
fn c_scoped_rule_is_unchanged_by_the_sparse_seam() {
    // Region(North,South,Total) x Measure(Sales,Cost,Margin); Sales/Cost populated.
    let mut region = Dimension::new("Region");
    let n = region.add_leaf("North");
    let s = region.add_leaf("South");
    let total = region.add_consolidated("Total");
    region.add_child(total, n, 1).unwrap();
    region.add_child(total, s, 1).unwrap();
    let mut measure = Dimension::new("Measure");
    measure.add_leaf("Sales");
    measure.add_leaf("Cost");
    measure.add_leaf("Margin");
    let mut cube = Cube::new("C", vec![region, measure]).unwrap();
    let sales = cube.dimension(1).resolve("Sales").unwrap();
    let cost = cube.dimension(1).resolve("Cost").unwrap();
    cube.set_leaf(&[n, sales], Fixed::from(120)).unwrap();
    cube.set_leaf(&[n, cost], Fixed::from(60)).unwrap();
    cube.set_leaf(&[s, sales], Fixed::from(200)).unwrap();
    cube.set_leaf(&[s, cost], Fixed::from(50)).unwrap();
    // A C: ratio at the Total, plus an N: ratio at the leaves (a common idiom).
    let src = "N:['Measure':'Margin'] = value['Measure':'Sales'] / value['Measure':'Cost'];\n\
               C:['Measure':'Margin'] = value['Measure':'Sales'] / value['Measure':'Cost'];";
    let model = compile(
        &cube,
        &epiphany_calc::SingleCube::new(&cube),
        &parse(src).unwrap(),
        1,
    )
    .unwrap();
    let dense_reg = OneCube {
        cube: cube.clone(),
        model: model.clone(),
    };
    let gate = safe_fed_set(&dense_reg, 0).unwrap();
    let fed = match gate {
        FedGate::Safe(fed) => fed,
        FedGate::Dense(r) => panic!("expected Safe, got Dense({r})"),
    };
    let dense_engine = CalcEngine::new(&dense_reg);
    let sparse_reg = FedRegistry {
        inner: OneCube { cube, model },
        fed,
    };
    let sparse_engine = CalcEngine::new(&sparse_reg);

    // Every coordinate agrees, and the C: total is the recomputed ratio of totals
    // (320/110 = 2.9091), NOT the summed child ratios — proving the sparse seam did
    // not turn the C: override into a rollup.
    for c in &all_coords(&dense_reg.cube) {
        assert_eq!(
            dense_engine.value(0, c).map(|v| v.to_scaled()),
            sparse_engine.value(0, c).map(|v| v.to_scaled()),
            "C: seam mismatch at {c:?}"
        );
    }
    let total = dense_reg.cube.dimension(0).resolve("Total").unwrap();
    let margin = dense_reg.cube.dimension(1).resolve("Margin").unwrap();
    assert_eq!(
        sparse_engine.value(0, &[total, margin]).unwrap(),
        "2.9091".parse::<Fixed>().unwrap()
    );
}
