//! Locks held by transactions on the server and on cubes.

use serde::{Deserialize, Serialize};

use super::TransactionId;

/// A lock a client may ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum LockMode {
    /// Others may read but not write.
    Shared,
    /// Others may neither write nor take any lock.
    Exclusive,
}

impl LockMode {
    pub(super) fn mode(self) -> Mode {
        match self {
            Self::Shared => Mode::Shared,
            Self::Exclusive => Mode::Exclusive,
        }
    }

    /// The lock to hold on the server while holding this one on a cube.
    pub(super) fn intent(self) -> Mode {
        match self {
            Self::Shared => Mode::IntentShared,
            Self::Exclusive => Mode::IntentExclusive,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    /// Holds or will take a shared lock on something inside.
    IntentShared,
    /// Is writing or will write something inside.
    IntentExclusive,
    Shared,
    Exclusive,
}

impl Mode {
    fn allows(self, other: Self) -> bool {
        use Mode::{IntentExclusive, IntentShared, Shared};
        matches!(
            (self, other),
            (IntentShared, IntentShared | IntentExclusive | Shared)
                | (IntentExclusive, IntentShared | IntentExclusive)
                | (Shared, IntentShared | Shared)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Resource {
    Server,
    /// A cube, by its position in the model.
    Cube(usize),
}

/// Every lock currently held. Transactions are few, so a list is enough.
#[derive(Debug, Default)]
pub(super) struct Locks {
    held: Vec<(Resource, TransactionId, Mode)>,
}

impl Locks {
    /// Takes a lock, unless another transaction holds one that conflicts with it.
    pub(super) fn acquire(&mut self, owner: TransactionId, resource: Resource, mode: Mode) -> bool {
        let conflicts = |&(held, holder, other): &(Resource, TransactionId, Mode)| {
            held == resource && holder != owner && !mode.allows(other)
        };
        if self.held.iter().any(conflicts) {
            return false;
        }
        if !self.held.contains(&(resource, owner, mode)) {
            self.held.push((resource, owner, mode));
        }
        true
    }

    pub(super) fn release(&mut self, owner: TransactionId) {
        self.held.retain(|&(_, holder, _)| holder != owner);
    }
}
