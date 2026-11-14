//! Reimplementation of the Bongiovi "Digital Power Station" signal chain from
//! the public patents (US 8,284,955 family; stereo block from US 9,398,394;
//! output limiter from US 9,350,309). See docs/patent-spec-core.md and
//! docs/patent-spec-later.md.

pub mod bell;
pub mod chain;
pub mod compressor;
pub mod limiter;
pub mod profile;
pub mod shelf;
pub mod stereo;

pub use chain::Processor;
pub use profile::Profile;
