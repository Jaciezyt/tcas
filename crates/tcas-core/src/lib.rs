//! Pure logic shared by `tcas` and tcasd. Nothing here does I/O.

pub mod abspath;
pub mod acquire;
pub mod brief;
pub mod config;
pub mod exempt;
pub mod guard;
pub mod human;
pub mod lease;
pub mod pathfmt;
pub mod proto;
pub mod relpath;
pub mod shell;
pub mod unmanaged;
pub mod wait;
