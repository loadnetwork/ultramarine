#![forbid(unsafe_code)]
#![deny(trivial_casts, trivial_numeric_casts)]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![allow(missing_docs)]

pub mod address;
pub mod context;
pub mod genesis;
pub mod height;
pub mod proposal;
pub mod proposal_part;
pub mod proto;
pub mod signing;
pub mod validator_set;
pub mod value;
pub mod vote;

pub mod aliases;
pub mod codec;
pub mod engine_api;
pub mod utils;
