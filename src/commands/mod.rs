//! One module per family of commands. `run` in `main.rs` parses, resolves the
//! room and hands off here; nothing in this tree parses arguments and nothing
//! in `main.rs` talks to a speaker. Each file is named for what the person is
//! doing - installing, playing, adjusting a speaker - not for a Sonos API.

pub mod admin;
