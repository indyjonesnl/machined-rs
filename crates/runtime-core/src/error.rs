//! Error type for the runtime-core store and controller runtime.

use machined_resources::Key;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("resource not found: {0}")]
    NotFound(Key),

    #[error("resource already exists: {0}")]
    AlreadyExists(Key),

    #[error("version conflict on {key}: expected {expected}, found {found}")]
    Conflict { key: Key, expected: u64, found: u64 },

    #[error("cannot destroy {0}: finalizers still present")]
    HasFinalizers(Key),

    #[error("controller error: {0}")]
    Controller(String),
}

pub type Result<T> = std::result::Result<T, Error>;
