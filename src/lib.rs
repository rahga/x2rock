//! The Sonos layer of x2rock as a library: discovery, the LAN Control API, UPnP,
//! music services, and the household state, for front ends other than the CLI.
//! The CLI, daemon and TUI are the `x2rock` binary in this same package.

pub mod art;
pub mod bookmarks;
pub mod catalogue;
pub mod credentials;
pub mod discover;
pub mod frontpage;
pub mod hint;
pub mod iheart;
pub mod itunes;
pub mod netid;
pub mod play;
pub mod rereads;
pub mod salt;
pub mod session;
pub mod sonos;
pub mod state;
pub mod stations;
pub mod store;
pub mod streams;
#[doc(hidden)]
pub mod testdir;

/// The version, as `git describe` saw it at build time.
pub const VERSION: &str = env!("X2ROCK_VERSION");
