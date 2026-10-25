//! The page runtime (docs/ARCHITECTURE.md sections 5, 6, 13).
//!
//! A page is a package: `page.json` (identity, bounds, capabilities, quotas),
//! Luau source and WGSL shaders. Loading one runs its entry script once in a
//! sandboxed Luau realm; the script builds a computation graph whose operators
//! are its own pure functions, native operators it names, and GPU operators it
//! ships. The engine then runs that graph like any other page: scripts run
//! only when an input changed or a time they declared has come.

mod api;
pub mod data;
pub mod draw;
pub mod natives;
mod ops;
pub mod package;
pub mod page;
pub mod realm;
pub mod text;

pub use data::{Data, Res, Streamed};
pub use draw::DisplayList;
pub use natives::{
    Call, Effect, EnvSecrets, Grants, JobCtx, Natives, SecretStore, Sink, SourceCtx, SourceGuard, StreamHandle, StreamSpec,
};
pub use package::{Caps, Limits, Manifest, Package};
pub use page::{check_wgsl, LoadOptions, ScriptPage};
pub use realm::Realm;
