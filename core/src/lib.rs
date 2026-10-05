mod database;
mod entries_db;
mod run;
mod state_db;
mod state_db_mem;

#[cfg(feature = "backend-libmdbx")]
mod entries_db_mdbx;
#[cfg(feature = "backend-libmdbx")]
mod state_db_mdbx;

#[cfg(feature = "backend-redb")]
mod entries_db_redb;
#[cfg(feature = "backend-redb")]
mod state_db_redb;

pub use database::{ReadTransaction, Row, WriteTransaction};
pub use entries_db::{EntriesStore, Query};
pub use run::{DEFAULT_FUEL_PER_ENTRY, DEFAULT_MAX_MEMORY, RunEnvironment, RunError};
pub use state_db::StateDatabase;
pub use state_db_mem::StateMemDatabase;

#[cfg(feature = "backend-libmdbx")]
pub use entries_db_mdbx::EntriesMdbxDatabase;
#[cfg(feature = "backend-libmdbx")]
pub use state_db_mdbx::StateLMDBDatabase;

#[cfg(feature = "backend-redb")]
pub use entries_db_redb::EntriesRedbDatabase;
#[cfg(feature = "backend-redb")]
pub use state_db_redb::StateRedbDatabase;
