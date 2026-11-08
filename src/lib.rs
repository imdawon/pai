//! `pai`: pairs a machine with a DawonAI account and hands coding tools a day credential.
//! See `docs/PROTOCOL.md` in the privacy-ai repo for the wire formats.

pub mod app;
pub mod config;
pub mod error;
pub mod jsonc;
pub mod keystore;
pub mod pin;
pub mod proxy;
pub mod relay;
pub mod report;
pub mod sealed;
pub mod service;
pub mod setup;
pub mod update;
