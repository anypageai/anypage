//! The demo page. Content, not engine: everything here goes through the same
//! `Page` interface a downloaded page would use. [`DemoPage`] builds it in
//! Rust; [`script`] loads the same page from its package (pages/demo) through
//! the Luau runtime, with the native operators it names.

pub mod bench;
pub mod feeds;
pub mod mandel;
pub mod page;
pub mod script;

pub use page::DemoPage;
