//! The bundled demo model, materialized on first run so the app is non-empty
//! (dead-simple onboarding, the section 1 mandate).

use epiphany_core::{Cube, Dimension, Fixed};

/// The demo cubes as `(name, cube)` pairs, in deterministic order.
pub fn demo_cubes() -> Vec<(String, Cube)> {
    vec![("Sales".to_string(), sales_cube())]
}

/// A small `Region x Period x Measure` planning cube with consolidations, an
/// alternate rollup, a weighted variance, a string measure, and seeded data.
fn sales_cube() -> Cube {
    let mut region = Dimension::new("Region");
    let north = region.add_leaf("North");
    let south = region.add_leaf("South");
    let east = region.add_leaf("East");
    let total = region.add_consolidated("Total");
    let coastal = region.add_consolidated("Coastal");
    for leaf in [north, south, east] {
        region.add_child(total, leaf, 1).unwrap();
    }
    region.add_child(coastal, north, 1).unwrap();
    region.add_child(coastal, east, 1).unwrap();

    let mut period = Dimension::new("Period");
    let jan = period.add_leaf("Jan");
    let feb = period.add_leaf("Feb");
    let mar = period.add_leaf("Mar");
    let q1 = period.add_consolidated("Q1");
    for leaf in [jan, feb, mar] {
        period.add_child(q1, leaf, 1).unwrap();
    }

    let mut measure = Dimension::new("Measure");
    let actual = measure.add_leaf("Actual");
    let budget = measure.add_leaf("Budget");
    let variance = measure.add_consolidated("Variance");
    measure.add_child(variance, actual, 1).unwrap();
    measure.add_child(variance, budget, -1).unwrap();
    let comment = measure.add_string("Comment");

    let mut cube = Cube::new("Sales", vec![region, period, measure]).unwrap();
    let seed: &[(u32, u32, u32, i32)] = &[
        (north, jan, actual, 100),
        (north, jan, budget, 90),
        (south, jan, actual, 80),
        (east, jan, actual, 60),
        (north, feb, actual, 110),
        (north, mar, actual, 120),
    ];
    for &(r, p, m, v) in seed {
        cube.set_leaf(&[r, p, m], Fixed::from(v)).unwrap();
    }
    cube.set_string(&[north, jan, comment], "strong start")
        .unwrap();
    cube
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_cube_consolidates() {
        let cubes = demo_cubes();
        assert_eq!(cubes.len(), 1);
        let (name, cube) = &cubes[0];
        assert_eq!(name, "Sales");
        let region = cube.dimension(0);
        let period = cube.dimension(1);
        let measure = cube.dimension(2);
        let total = region.index_of("Total").unwrap();
        let q1 = period.index_of("Q1").unwrap();
        let actual = measure.index_of("Actual").unwrap();
        // Total Actual over Q1 = 100 + 80 + 60 + 110 + 120 = 470.
        assert_eq!(cube.get(&[total, q1, actual]).unwrap(), Fixed::from(470));
    }
}
