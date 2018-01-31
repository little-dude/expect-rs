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

pub use client::{Client, ExpectError};
pub use core::{InitError, Match, MatchRequest, SendError, SpawnError};

use std::process::Command;

pub struct Expect(core::ExpectHandle);

impl Expect {
    pub fn new() -> Result<Self, InitError> {
        Ok(Expect(core::ExpectHandle::new()?))
    }

    pub fn spawn(&mut self, cmd: Command) -> Result<Client, SpawnError> {
        self.0.spawn(cmd).map(From::from)
    }
}
