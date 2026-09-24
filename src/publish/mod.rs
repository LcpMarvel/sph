//! 发布流水线：upload → metadata → cover → declaration → submit。

pub mod mod_impl;
pub mod page;

pub use mod_impl::{default_options, run, validate, Options, PublishResult};

#[cfg(test)]
mod e2e_tests;
