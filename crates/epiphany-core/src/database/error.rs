//! Why a database operation was refused.

use std::fmt;

use super::TransactionId;
use crate::cells::CellError;
use crate::mdx::MdxError;
use crate::model::ModelError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Cell(CellError),
    /// A write in a batch is invalid; nothing in the batch was written.
    Write {
        index: usize,
        error: CellError,
    },
    Model(ModelError),
    /// The transaction never existed, has finished, or has expired.
    UnknownTransaction(TransactionId),
    /// Another transaction holds a conflicting lock on the named thing.
    Locked(String),
    /// A model update was based on a version that is no longer current.
    StaleVersion {
        based_on: u64,
        current: u64,
    },
    /// The change could not be recorded on disk, so it was not made.
    Storage(String),
    Mdx(MdxError),
    /// A rule could not work out a cell; the text says why.
    Rule(String),
    /// Something asked for by name does not exist; the text says what.
    NotFound(String),
    /// The caller may not do this; the text says what.
    Forbidden(String),
    /// A comment cannot be kept as it is; the text says why.
    Comment(String),
    /// A sandbox cannot be made, used or published as asked; the text says why.
    Sandbox(String),
}

impl Error {
    /// A stable identifier for the kind of error.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Cell(error) | Self::Write { error, .. } => error.code(),
            Self::Model(_) => "invalid_model",
            Self::UnknownTransaction(_) => "unknown_transaction",
            Self::Locked(_) => "locked",
            Self::StaleVersion { .. } => "stale_version",
            Self::Storage(_) => "storage_failed",
            Self::Mdx(_) => "invalid_mdx",
            Self::Rule(_) => "rule_failed",
            Self::NotFound(_) => "not_found",
            Self::Forbidden(_) => "forbidden",
            Self::Comment(_) => "invalid_comment",
            Self::Sandbox(_) => "sandbox_refused",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cell(error) | Self::Write { error, .. } => error.fmt(f),
            Self::Model(error) => error.fmt(f),
            Self::UnknownTransaction(id) => write!(f, "unknown transaction {id}"),
            Self::Locked(what) => write!(f, "{what} is locked by another transaction"),
            Self::StaleVersion { based_on, current } => write!(
                f,
                "the update is based on model version {based_on}, but the current version is {current}"
            ),
            Self::Storage(reason) => write!(f, "the change could not be stored: {reason}"),
            Self::Mdx(error) => error.fmt(f),
            Self::Rule(reason) => reason.fmt(f),
            Self::NotFound(what)
            | Self::Forbidden(what)
            | Self::Comment(what)
            | Self::Sandbox(what) => what.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

impl From<MdxError> for Error {
    fn from(error: MdxError) -> Self {
        Self::Mdx(error)
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

impl From<CellError> for Error {
    fn from(error: CellError) -> Self {
        Self::Cell(error)
    }
}

impl From<ModelError> for Error {
    fn from(error: ModelError) -> Self {
        Self::Model(error)
    }
}
