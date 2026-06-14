//! The cube registry seam: resolves cube names to ordinals and back to cubes, so
//! cross-cube rule references compile and evaluate by index (no name hashing on
//! the hot path). The engine provides the production implementation over its
//! published cubes (Phase 4J); these in-memory implementations serve the
//! single-cube common case and tests.

use epiphany_core::Cube;

/// Resolves the cubes a rule set may reference.
pub trait CubeRegistry {
    /// The ordinal of a cube by name, if present.
    fn ordinal(&self, name: &str) -> Option<u32>;
    /// The cube at an ordinal, if present.
    fn cube(&self, ordinal: u32) -> Option<&Cube>;
}

/// A registry containing exactly one cube (ordinal 0): the common single-cube
/// model.
#[derive(Debug)]
pub struct SingleCube<'a>(&'a Cube);

impl<'a> SingleCube<'a> {
    /// Wrap a single cube as a registry.
    pub fn new(cube: &'a Cube) -> Self {
        Self(cube)
    }
}

impl CubeRegistry for SingleCube<'_> {
    fn ordinal(&self, name: &str) -> Option<u32> {
        (name == self.0.name()).then_some(0)
    }

    fn cube(&self, ordinal: u32) -> Option<&Cube> {
        (ordinal == 0).then_some(self.0)
    }
}

/// A registry over an owned, ordered set of cubes (ordinal = position). Useful
/// for cross-cube tests.
#[derive(Debug)]
pub struct VecRegistry {
    cubes: Vec<Cube>,
}

impl VecRegistry {
    /// Build a registry from cubes; their ordinals are their positions.
    pub fn new(cubes: Vec<Cube>) -> Self {
        Self { cubes }
    }

    /// The cubes, in ordinal order.
    pub fn cubes(&self) -> &[Cube] {
        &self.cubes
    }
}

impl CubeRegistry for VecRegistry {
    fn ordinal(&self, name: &str) -> Option<u32> {
        self.cubes
            .iter()
            .position(|c| c.name() == name)
            .map(|i| i as u32)
    }

    fn cube(&self, ordinal: u32) -> Option<&Cube> {
        self.cubes.get(ordinal as usize)
    }
}
