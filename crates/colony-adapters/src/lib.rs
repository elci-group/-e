// SPDX-License-Identifier: MIT
//! Host boundary for Colony. The control-plane crates stay free of I/O.
//! This crate is the only place that talks to Mesut, Padagonia, the filesystem
//! and an ELCI model endpoint.
mod elci;
mod executor;
mod hash;
mod preflight;
mod run;
mod semantic;
mod verifier;

pub use elci::{ElciEndpoint, ElciProvider};
pub use executor::MesutExecutor;
pub use preflight::LucidPreflight;
pub use run::{execute, ColonyRequest, Host, Report};
pub use semantic::PadagoniaSource;
pub use verifier::HostVerifier;
