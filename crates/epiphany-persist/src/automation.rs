//! Durable, server-global automation store (ADR-0035).
//!
//! Flows, flow tests, connections, and jobs are no longer per-cube; they live in
//! one server-global [`Automation`] model, persisted here as a single canonical
//! model-as-code file `<dir>/automation.model` (via [`Automation::to_model_text`]).
//! Writes are atomic (write a `.tmp`, then rename), so a crash between writes
//! leaves the previous consistent file, mirroring the snapshot and shared-dimension
//! registry durability pattern. There is no WAL: automation is desired state edited
//! infrequently, so each mutation rewrites the whole file (the same way structural
//! cube mutations checkpoint the snapshot).

use std::path::{Path, PathBuf};

use epiphany_core::{Automation, Connection, Flow, FlowTest, Job};

use crate::store::PersistError;

const AUTOMATION_FILE: &str = "automation.model";
const AUTOMATION_TMP: &str = "automation.model.tmp";

/// The durable global automation store: owns the in-memory [`Automation`] and
/// keeps `automation.model` in sync after every mutation.
#[derive(Debug)]
pub struct AutomationStore {
    dir: PathBuf,
    automation: Automation,
}

impl AutomationStore {
    /// Open the store at `dir`, loading `automation.model` if present, else an
    /// empty model. Creates `dir` if missing.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, PersistError> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(AUTOMATION_FILE);
        let automation = if path.exists() {
            Automation::load_from_path(&path)?
        } else {
            Automation::new()
        };
        Ok(Self { dir, automation })
    }

    /// The current automation model (read-only).
    pub fn automation(&self) -> &Automation {
        &self.automation
    }

    /// Replace the whole automation model and persist it atomically. Used by boot
    /// migration (lifting per-cube automation) and bulk edits.
    pub fn replace(&mut self, automation: Automation) -> Result<(), PersistError> {
        self.automation = automation;
        self.persist()
    }

    /// Define (create or replace) a flow, then persist.
    pub fn define_flow(&mut self, flow: Flow) -> Result<(), PersistError> {
        self.automation.flows.insert(flow.name.clone(), flow);
        self.persist()
    }

    /// Delete a flow by name. Returns whether one was removed; persists only when
    /// something changed.
    pub fn delete_flow(&mut self, name: &str) -> Result<bool, PersistError> {
        let removed = self.automation.flows.remove(name).is_some();
        if removed {
            self.persist()?;
        }
        Ok(removed)
    }

    /// Define (create or replace) a flow unit test, then persist.
    pub fn define_flow_test(&mut self, test: FlowTest) -> Result<(), PersistError> {
        self.automation.flow_tests.insert(test.name.clone(), test);
        self.persist()
    }

    /// Delete a flow test by name. Returns whether one was removed.
    pub fn delete_flow_test(&mut self, name: &str) -> Result<bool, PersistError> {
        let removed = self.automation.flow_tests.remove(name).is_some();
        if removed {
            self.persist()?;
        }
        Ok(removed)
    }

    /// Define (create or replace) a global connection, then persist.
    pub fn define_connection(&mut self, connection: Connection) -> Result<(), PersistError> {
        self.automation
            .connections
            .insert(connection.name.clone(), connection);
        self.persist()
    }

    /// Delete a connection by name. Returns whether one was removed.
    pub fn delete_connection(&mut self, name: &str) -> Result<bool, PersistError> {
        let removed = self.automation.connections.remove(name).is_some();
        if removed {
            self.persist()?;
        }
        Ok(removed)
    }

    /// Define (create or replace) a scheduled job, then persist.
    pub fn define_job(&mut self, job: Job) -> Result<(), PersistError> {
        self.automation.jobs.insert(job.name.clone(), job);
        self.persist()
    }

    /// Delete a job by name. Returns whether one was removed.
    pub fn delete_job(&mut self, name: &str) -> Result<bool, PersistError> {
        let removed = self.automation.jobs.remove(name).is_some();
        if removed {
            self.persist()?;
        }
        Ok(removed)
    }

    /// Atomically write the current model to `automation.model`.
    fn persist(&self) -> Result<(), PersistError> {
        write_automation(&self.dir, &self.automation)
    }
}

/// Atomically write `automation` to `<dir>/automation.model` (write a temp file,
/// then rename). Public so boot migration can write the file before opening the
/// store.
pub fn write_automation(dir: &Path, automation: &Automation) -> Result<(), PersistError> {
    std::fs::create_dir_all(dir)?;
    let text = automation.to_model_text()?;
    let tmp = dir.join(AUTOMATION_TMP);
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, dir.join(AUTOMATION_FILE))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("epiphany-automation-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn flow(name: &str) -> Flow {
        Flow {
            name: name.to_string(),
            source: "function rows(ctx) {}".to_string(),
            owner: Some("ann".to_string()),
            default_cube: Some("Sales".to_string()),
            inputs: Vec::new(),
        }
    }

    #[test]
    fn defines_persists_and_reloads() {
        let dir = temp_dir("reload");
        {
            let mut store = AutomationStore::open(&dir).unwrap();
            store.define_flow(flow("load")).unwrap();
            assert_eq!(store.automation().flows.len(), 1);
        }
        // Reopen: the flow survives.
        let store = AutomationStore::open(&dir).unwrap();
        assert_eq!(store.automation().flows.len(), 1);
        assert_eq!(
            store.automation().flows["load"].owner.as_deref(),
            Some("ann")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_reports_change_and_persists() {
        let dir = temp_dir("delete");
        let mut store = AutomationStore::open(&dir).unwrap();
        store.define_flow(flow("load")).unwrap();
        assert!(store.delete_flow("load").unwrap());
        assert!(!store.delete_flow("load").unwrap());
        assert!(store.automation().flows.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
