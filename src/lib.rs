extern crate futures;
extern crate libc;
#[macro_use]
extern crate log;
extern crate mio;
extern crate regex;
extern crate tokio_core;
extern crate tokio_io;
extern crate tty;

mod core;
mod client;

// re-exports
pub use client::{Client, ExpectError};
pub use core::{InitError, Match, MatchRequest, SendError, SpawnError};

use std::process::Command;

/// A handle to spawn expect sessions.
pub struct Expect(core::ExpectHandle);

impl Expect {
    /// Create a new handle. This must be call only once, because it spawns a background thread
    /// running an event loop that manages the expect sessions. It would be wasteful to spawn
    /// multiple event loops.
    pub fn init() -> Result<Self, InitError> {
        Ok(Expect(core::ExpectHandle::new()?))
    }

    /// Start a new expect session running provided command.
    pub fn spawn(&mut self, cmd: Command) -> Result<Client, SpawnError> {
        self.0.spawn(cmd).map(From::from)
    }
}
