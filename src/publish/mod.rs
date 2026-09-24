//! 发布流水线：upload → metadata → cover → declaration → submit。

pub mod mod_impl;
pub mod page;
pub mod patches;
pub mod recovery;

pub use mod_impl::{default_options, run, validate, Options, PublishResult};

#[cfg(test)]
mod e2e_tests;
