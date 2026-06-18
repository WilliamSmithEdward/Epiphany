//! Epiphany persist: runtime durability (a fast-restart cache over the model).
//!
//! A [`Store`] makes a cube durable with two artifacts in a directory:
//! - a **snapshot** (`snapshot.model`): the canonical model-as-code text
//!   (ADR-0003) of the whole cube, the latest checkpoint;
//! - a **write-ahead log** (`wal.log`): a binary, append-only, CRC32-framed tail
//!   of leaf writes since that checkpoint (ADR-0002).
//!
//! Recovery loads the snapshot, then replays the WAL tail, discarding any record
//! torn by a crash. [`Store::checkpoint`] is the explicit full-persist command:
//! it rewrites the snapshot and clears the WAL. The text model remains the
//! source of truth; this layer is a derived cache for fast, crash-safe restart.

mod automation;
mod registry;
mod store;
mod wal;

pub use automation::{write_automation, AutomationStore};
pub use registry::{load_registry, save_registry, RegistryEntry};
pub use store::{CellWrite, DimensionEdit, PersistError, Store};

/// Stable crate identifier, reported by the server's wiring banner.
pub const CRATE: &str = "epiphany-persist";

#[cfg(test)]
mod tests {
    #[test]
    fn crate_is_named() {
        assert_eq!(super::CRATE, "epiphany-persist");
    }

    #[test]
    fn links_dependencies() {
        assert_eq!(epiphany_core::CRATE, "epiphany-core");
        let ids = epiphany_determinism::IdGen::default();
        assert_eq!(ids.next_id(), 1);
    }
}
