//! A cross join on a `NON EMPTY` axis is cut down to the tuples that hold something by
//! looking at the cells, without being written out. These tests hold it to what writing it
//! out gives.

mod common;

use common::{Rng, Shape, random_model};
use epiphany_core::Scalar;
use epiphany_core::database::{CellWrite, Database};
use epiphany_core::mdx::QueryResult;
use epiphany_core::model::ModelDef;

/// 113 elements a dimension, so that two of them crossed pass the length from which a cross
/// join is left unwritten, and three pass the most tuples a set may hold.
const SHAPE: Shape = Shape {
    leaves: 100,
    groups: 12,
    fan: 10,
};

fn model(dimensions: usize) -> ModelDef {
    random_model(&mut Rng::new(7), dimensions, &SHAPE)
}

fn write(rng: &mut Rng, dimensions: usize, value: f64) -> CellWrite {
    CellWrite {
        cube: "C".to_owned(),
        at: (0..dimensions)
            .map(|_| format!("e{}", rng.below(SHAPE.leaves)))
            .collect(),
        value: Scalar::Number(value),
    }
}

/// A database of a random cube with a few hundred cells in it.
fn filled(definition: ModelDef, dimensions: usize) -> Database {
    let mut database = Database::new(definition).unwrap();
    let mut rng = Rng::new(11);
    let writes: Vec<CellWrite> = (0..300)
        .map(|index| write(&mut rng, dimensions, 1.0 + index as f64))
        // Where a test gives the cube a rule, it is these cells that the rule works out.
        .filter(|write| write.at[2] != "e0")
        .collect();
    database.write(None, &writes).unwrap();
    database
}

fn names(result: &QueryResult, axis: usize) -> Vec<Vec<String>> {
    let tuple = |tuple: &Vec<epiphany_core::mdx::MemberResult>| {
        tuple.iter().map(|member| member.name.clone()).collect()
    };
    result.axes[axis].tuples.iter().map(tuple).collect()
}

/// The same query twice: with its rows a cross join, which is cut down from the cells, and
/// with them wrapped so that the cross join is written out first.
fn both_ways(database: &Database, transaction: Option<u64>, with: &str, rest: &str) -> QueryResult {
    let rows = "[d0].Members * [d1].Members";
    let ask = |rows: &str| {
        let mdx = format!("{with} SELECT NON EMPTY {rows} ON ROWS, {rest}");
        database.query(transaction, &mdx).unwrap()
    };
    let (cut, written) = (ask(rows), ask(&format!("Head({rows}, 1000000)")));
    assert_eq!(names(&cut, 1), names(&written, 1));
    assert_eq!(names(&cut, 0), names(&written, 0));
    assert_eq!(cut.cells, written.cells);
    cut
}

#[test]
fn the_rows_that_hold_something_are_the_same_found_either_way() {
    let database = filled(model(3), 3);
    let columns = "{[d2].[all], [d2].[e0], [d2].[e1]} ON COLUMNS FROM [C]";
    let result = both_ways(&database, None, "", columns);
    let rows = names(&result, 1);
    // Some of the 12,544, and the row of both totals first among them.
    assert!(rows.len() > 300 && rows.len() < 12_769, "{}", rows.len());
    assert!(rows.contains(&vec!["all".to_owned(), "all".to_owned()]));

    // Beside one leaf only the rows with a cell at that leaf remain.
    let narrow = both_ways(&database, None, "", "{[d2].[e0]} ON COLUMNS FROM [C]");
    assert!(names(&narrow, 1).len() < rows.len());
    // And in the slicer as on an axis.
    let four = filled(model(4), 4);
    let sliced = "{[d2].[all]} ON COLUMNS FROM [C] WHERE [d3].[e5]";
    let all = both_ways(&four, None, "", "{[d2].[all]} ON COLUMNS FROM [C]");
    assert!(names(&both_ways(&four, None, "", sliced), 1).len() < names(&all, 1).len());
}

#[test]
fn the_functions_that_drop_empty_tuples_agree_with_the_axis() {
    let database = filled(model(3), 3);
    let crossed = "[d0].Members * [d1].Members";
    let rows = |set: &str, slicer: &str| {
        let mdx = format!("SELECT {set} ON COLUMNS FROM [C] {slicer}");
        names(&database.query(None, &mdx).unwrap(), 0)
    };
    // The cross join of all three, too long to write out, by either function.
    let three = rows(
        "NonEmptyCrossJoin([d0].Members, [d1].Members, [d2].Members)",
        "",
    );
    assert!(three.len() >= 290);
    assert_eq!(
        rows(&format!("NonEmpty({crossed} * [d2].Members)"), ""),
        three
    );
    // Each cut down from the cells, against the cross join written out and then filtered:
    // at the total of the third dimension, and at a leaf of it that holds something.
    let leaf = three
        .iter()
        .map(|row| &row[2])
        .find(|name| name.starts_with('e'));
    let leaf = format!("WHERE [d2].[{}]", leaf.expect("a leaf holds something"));
    for slicer in ["WHERE [d2].[all]", leaf.as_str()] {
        let written = rows(&format!("NonEmpty(Head({crossed}, 1000000))"), slicer);
        assert!(
            !written.is_empty() && written.len() < 12_769,
            "{slicer}: {}",
            written.len()
        );
        assert_eq!(rows(&format!("NonEmpty({crossed})"), slicer), written);
        let function = "NonEmptyCrossJoin([d0].Members, [d1].Members)";
        assert_eq!(rows(function, slicer), written);
    }
    // Beside a set of another dimension: any of its members makes a tuple count.
    let beside = "{[d2].[all], [d2].[e8]}";
    assert_eq!(
        rows(&format!("NonEmpty({crossed}, {beside})"), ""),
        rows(&format!("NonEmpty(Head({crossed}, 1000000), {beside})"), "")
    );
}
#[test]
fn a_long_set_that_is_no_cross_join_is_cut_down_as_well() {
    // Two dimensions of some twelve thousand elements each, and two hundred cells.
    let shape = Shape {
        leaves: 12_000,
        groups: 12,
        fan: 10,
    };
    let mut database = Database::new(random_model(&mut Rng::new(3), 2, &shape)).unwrap();
    let mut rng = Rng::new(5);
    let writes: Vec<CellWrite> = (0..200)
        .map(|index| CellWrite {
            cube: "C".to_owned(),
            at: vec![
                format!("e{}", rng.below(shape.leaves)),
                format!("e{}", rng.below(40)),
            ],
            value: Scalar::Number(1.0 + f64::from(index)),
        })
        .collect();
    database.write(None, &writes).unwrap();

    let ask = |rows: &str, rest: &str| {
        let mdx = format!("SELECT NON EMPTY {rows} ON ROWS, {rest} FROM [C]");
        database.query(None, &mdx).unwrap()
    };
    let columns = "{[d1].[e0], [d1].[e1], [d1].[e2]} ON COLUMNS";
    let (cut, written) = (
        ask("[d0].Members", columns),
        ask("Head([d0].Members, 1000000)", columns),
    );
    assert_eq!(names(&cut, 1), names(&written, 1));
    assert_eq!(cut.cells, written.cells);
    assert!(!cut.cells.is_empty() && names(&cut, 1).len() < 200);

    // Both axes long: twelve thousand by twelve thousand is far more cells than a query may
    // return, and what holds something is a few hundred of them.
    let both = ask("[d0].Members", "NON EMPTY [d1].Members ON COLUMNS");
    assert!(names(&both, 1).len() >= 150, "{}", names(&both, 1).len());
    assert!(names(&both, 0).len() >= 30 && names(&both, 0).len() < 100);
    let function =
        "SELECT NonEmpty([d0].Members, {[d1].[e0], [d1].[e1], [d1].[e2]}) ON COLUMNS FROM [C]";
    assert_eq!(
        names(&database.query(None, function).unwrap(), 0),
        names(&cut, 1)
    );
}

#[test]
fn random_cubes_and_queries_agree_either_way() {
    // Sets of a dimension that are long enough, two of them crossed, to be left unwritten.
    let sets = |dimension: usize, pick: usize| match pick {
        0 => format!("[d{dimension}].Members"),
        1 => format!("{{[d{dimension}].[all], TM1FilterByLevel([d{dimension}].Members, 0)}}"),
        _ => format!("Hierarchize([d{dimension}].Members)"),
    };
    for seed in 1..=12 {
        let mut rng = Rng::new(seed);
        let mut definition = random_model(&mut rng, 3, &SHAPE);
        // Every other cube has a rule, fed or not.
        if seed % 2 == 0 {
            definition.cubes[0].rules = vec!["N: [d2].[e0] = [d2].[e1] * 3".to_owned()];
        }
        if seed % 4 == 0 {
            definition.cubes[0].feeders = vec!["[d2].[e1] => [d2].[e0]".to_owned()];
        }
        let mut database = Database::new(definition).unwrap();
        // Few cells or many, with some that cancel one another under a negative weight.
        let count = [5, 40, 400, 3000][rng.below(4)];
        let writes: Vec<CellWrite> = (0..count)
            .map(|index| write(&mut rng, 3, f64::from(index % 7) - 3.0))
            .filter(|write| write.at[2] != "e0")
            .collect();
        database.write(None, &writes).unwrap();

        let (first, second) = (rng.below(3), rng.below(3));
        let rows = format!(
            "{} * {}",
            sets(first, rng.below(3)),
            sets((first + 1 + second % 2) % 3, rng.below(3))
        );
        let other = 3 - first - (first + 1 + second % 2) % 3;
        let rest = match rng.below(3) {
            0 => format!("{{[d{other}].[all]}} ON COLUMNS FROM [C]"),
            1 => {
                format!("{{[d{other}].[e0], [d{other}].[e1], [d{other}].[e5]}} ON COLUMNS FROM [C]")
            }
            _ => format!(
                "NON EMPTY {{[d{other}].[all], [d{other}].[e2], [d{other}].[e3], [d{other}].[e4]}} ON COLUMNS FROM [C]"
            ),
        };
        let ask = |rows: &str| {
            let mdx = format!("SELECT NON EMPTY {rows} ON ROWS, {rest}");
            database
                .query(None, &mdx)
                .unwrap_or_else(|error| panic!("seed {seed}: {error}\n{mdx}"))
        };
        let (cut, written) = (ask(&rows), ask(&format!("Head({rows}, 1000000)")));
        assert_eq!(
            names(&cut, 1),
            names(&written, 1),
            "seed {seed}: {rows} beside {rest}"
        );
        assert_eq!(
            names(&cut, 0),
            names(&written, 0),
            "seed {seed}: {rows} beside {rest}"
        );
        assert_eq!(
            cut.cells, written.cells,
            "seed {seed}: {rows} beside {rest}"
        );
    }
}

#[test]
fn a_cross_join_inside_hierarchize_is_cut_down_and_then_put_in_order() {
    let database = filled(model(3), 3);
    let crossed = "[d0].Members * [d1].Members";
    let ask = |rows: &str| {
        let mdx = format!("SELECT NON EMPTY {rows} ON ROWS, {{[d2].[all]}} ON COLUMNS FROM [C]");
        database.query(None, &mdx).unwrap()
    };
    for flag in ["", ", POST"] {
        let cut = ask(&format!("Hierarchize({crossed}{flag})"));
        let written = ask(&format!("Hierarchize(Head({crossed}, 1000000){flag})"));
        assert_eq!(names(&cut, 1), names(&written, 1), "{flag}");
        assert_eq!(cut.cells, written.cells, "{flag}");
        assert!(names(&cut, 1).len() > 300);
    }
    // The order is the hierarchy's and not the cross join's.
    assert_ne!(
        names(&ask(&format!("Hierarchize({crossed})")), 1),
        names(&ask(crossed), 1)
    );
    // The same of the function, and of the cross join of three that cannot be written out.
    let rows = |set: &str| {
        let mdx = format!("SELECT {set} ON COLUMNS FROM [C] WHERE [d2].[all]");
        names(&database.query(None, &mdx).unwrap(), 0)
    };
    assert_eq!(
        rows(&format!("NonEmpty(Hierarchize({crossed}))")),
        rows(&format!("NonEmpty(Hierarchize(Head({crossed}, 1000000)))"))
    );
    let three = "SELECT NON EMPTY Hierarchize([d0].Members * [d1].Members * [d2].Members) ON ROWS, \
                 {[d3].[all]} ON COLUMNS FROM [C]";
    let three = filled(model(4), 4).query(None, three).unwrap();
    assert!(names(&three, 1).len() >= 290);
    assert!(names(&three, 1).contains(&vec!["all".to_owned(); 3]));
}

#[test]
fn a_cell_written_in_a_transaction_brings_its_rows() {
    let mut database = filled(model(3), 3);
    let columns = "{[d2].[e99]} ON COLUMNS FROM [C]";
    let before = names(&both_ways(&database, None, "", columns), 1).len();
    let transaction = database.begin();
    let cell = CellWrite {
        cube: "C".to_owned(),
        at: ["e98", "e97", "e99"].map(str::to_owned).to_vec(),
        value: Scalar::Number(5.0),
    };
    database.write(Some(transaction), &[cell]).unwrap();
    let within = names(&both_ways(&database, Some(transaction), "", columns), 1);
    assert!(within.len() > before);
    assert!(within.contains(&vec!["e98".to_owned(), "e97".to_owned()]));
    assert_eq!(
        names(&both_ways(&database, None, "", columns), 1).len(),
        before
    );
}

#[test]
fn rules_and_calculated_members_change_nothing() {
    let columns = "{[d2].[e0], [d2].[e1], [d2].[all]} ON COLUMNS FROM [C]";
    for feeders in [vec![], vec!["[d2].[e1] => [d2].[e0]".to_owned()]] {
        let mut definition = model(3);
        definition.cubes[0].rules = vec!["N: [d2].[e0] = [d2].[e1] * 2".to_owned()];
        definition.cubes[0].feeders = feeders;
        both_ways(&filled(definition, 3), None, "", columns);
    }
    // A calculated member may hold something where no cell does, so the rows are written out.
    let database = filled(model(3), 3);
    let with = "WITH MEMBER [d2].[one] AS 1";
    let result = both_ways(&database, None, with, "{[d2].[one]} ON COLUMNS FROM [C]");
    assert_eq!(names(&result, 1).len(), 12_769);
}

#[test]
fn a_cross_join_too_long_to_write_out_still_gives_its_rows() {
    let database = filled(model(4), 4);
    let rows = "CrossJoin([d0].Members, [d1].Members, [d2].Members)";
    let refused = database
        .query(
            None,
            &format!("SELECT Head({rows}, 10) ON ROWS, {{[d3].[all]}} ON COLUMNS FROM [C]"),
        )
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("a set has more than 1000000 tuples"),
        "{refused}"
    );

    let mdx = format!("SELECT NON EMPTY {rows} ON ROWS, {{[d3].[all]}} ON COLUMNS FROM [C]");
    let result = database.query(None, &mdx).unwrap();
    let rows = names(&result, 1);
    // Every stored cell brings at least its own row, and the row of all three totals is there.
    assert!(rows.len() >= 290, "{}", rows.len());
    assert!(rows.contains(&vec!["all".to_owned(); 3]));
    assert!(result.cells.iter().all(Option::is_some));
    // No row twice, and in the order of the cross join: the first set's members slowest.
    let first: Vec<&String> = rows.iter().map(|row| &row[0]).collect();
    let mut runs = first.clone();
    runs.dedup();
    let mut distinct = runs.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(runs.len(), distinct.len());
}
