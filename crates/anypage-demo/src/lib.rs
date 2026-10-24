//! The built-in demo page. Content, not engine: everything here goes through
//! the same `Page` interface a downloaded page would use.

pub mod bench;
pub mod feeds;
pub mod mandel;
pub mod page;

pub use page::DemoPage;
