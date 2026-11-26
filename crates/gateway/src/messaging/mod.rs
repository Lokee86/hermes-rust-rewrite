//! Native gateway contract for messaging adapters.
mod runtime;
mod sessions;
mod store;
mod types;

#[cfg(test)]
mod conformance;

pub(super) use runtime::Host;
pub use runtime::Ingress;
pub use types::*;
