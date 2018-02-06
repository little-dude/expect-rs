//! This example illustrates how Client.expect() works.
extern crate env_logger;
extern crate expect;
extern crate futures;
extern crate tokio_core;

use expect::{Expect, Match};
use std::process::Command;
use std::time::Duration;

fn main() {
    env_logger::init();
    let mut expect = Expect::init().unwrap();

    // spawn python and wait for the python prompt
    let mut client = expect.spawn(Command::new("python")).unwrap();
    client.match_string(">>>".into()).unwrap();

    // import the random module and wait for the python prompt
    client.send_line("import random".into()).unwrap();
    client.match_string(">>>".into()).unwrap();

    // import the random time module and wait for the python prompt
    client.send_line("import time".into()).unwrap();
    client.match_string(">>>".into()).unwrap();

    // set a timeout of 0.5 seconds (the default on is 10)
    client.set_timeout(Some(Duration::from_millis(500)));

    // run `eval(random.choice(["time.sleep(1)", "exit()", "pass"]))`, which will cause python to
    // randomly execute "time.sleep(1)", or "exit()" or "pass"
    let cmd = "eval(random.choice([\"time.sleep(1)\", \"exit()\", \"pass\"]))".into();
    client.send_line(cmd).unwrap();

    // try to match against 3 possible patterns: a string (">>>"), a timeout and EOF.
    // depending on which function python picks and runs, the pattern that matches will vary.
    let matches = vec![Match::Utf8(">>>".into()), Match::Timeout, Match::Eof];
    match client.expect(matches).unwrap() {
        (0, _) => println!("matched \">>>\": `pass` got picked"),
        (1, _) => println!("matched timeout: `time.sleep(1)` got picked"),
        (2, _) => println!("matched eof: `exit()` got picked"),
        (..,) => unreachable!(),
    }
}
