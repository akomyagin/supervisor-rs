//! supervisor-rs library crate — exposes config parsing and process spawning
//! so both the binary and integration tests can use them.

pub mod clock;
pub mod config;
pub mod process;
pub mod signal;
pub mod supervise;
