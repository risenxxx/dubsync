pub mod gcc_phat;
pub mod offset_map;
pub mod sliding;

pub use offset_map::{OffsetMap, Segment};
pub use sliding::correlate_sliding;
