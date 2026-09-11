//! Runtime ownership and process lifecycle. Provider adapters live above this
//! layer; they must not spawn shell commands directly.

pub mod adapters;
mod binding_store;
pub mod mapper;
mod process;
mod registry;

pub use binding_store::*;
pub use mapper::*;
pub use process::*;
pub use registry::*;
