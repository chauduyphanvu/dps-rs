//! Audio processing chain whose core is reimplemented from Bongiovi Acoustics'
//! public patents (US 8,284,955 family; stereo block from US 9,398,394;
//! output limiter from US 9,350,309). See docs/patent-spec-core.md and
//! docs/patent-spec-later.md.

pub mod bass;
pub mod bell;
pub mod biquad;
pub mod calibrate;
pub mod chain;
pub mod compressor;
pub mod contour;
pub mod correction;
pub mod crossfeed;
pub mod leveler;
pub mod limiter;
pub mod loudness;
pub mod profile;
pub mod response;
pub mod shelf;
pub mod stereo;

pub use chain::{Meters, Processor};
pub use profile::Profile;
