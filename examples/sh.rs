extern crate env_logger;
extern crate expect;
extern crate futures;
extern crate tokio_core;

use std::time::Duration;
use std::process::Command;
use expect::Client;

fn main() {
    env_logger::init();
    let cmd = Command::new("sh");
    let mut client = Client::spawn(cmd).unwrap();
    client.match_string("$".into()).unwrap();
    client.send_line("whoami".into()).unwrap();
    client.match_string("$".into()).unwrap();
    client.send_line("sleep 7".into()).unwrap();
    client.match_timeout(Duration::from_millis(5000)).unwrap();
    client.match_string("$".into()).unwrap();
    client.send_line("exit".into()).unwrap();
    client.match_eof().unwrap();
}
