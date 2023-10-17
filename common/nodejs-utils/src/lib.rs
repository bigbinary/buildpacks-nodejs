#![warn(unused_crate_dependencies)]
#![warn(clippy::pedantic)]
#![warn(clippy::cargo)]
#![allow(clippy::module_name_repetitions)]

pub mod application;
pub mod distribution;
pub mod inv;
mod nodejs_org;
mod npmjs_org;
pub mod package_json;
pub mod package_manager;
mod s3;
pub mod telemetry;
pub mod vrs;
