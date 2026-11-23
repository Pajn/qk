//! What qk keeps from one run for the runs that follow, and how a later run
//! tells whether it still holds: the stamps of files and directories, how
//! long a stamp must have been left alone before it can be trusted, and the
//! saved files themselves.

pub mod saved;
pub mod settle;
pub mod stamp;

pub use stamp::{Stamp, look};
