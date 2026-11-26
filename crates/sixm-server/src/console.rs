//! Reads console lines from standard input on a background thread.

use std::io::{self, BufRead};
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};

use tracing::warn;

use crate::server::Input;

/// Starts a thread that sends each line of standard input to `sink`.
///
/// The thread ends at end of input, which is normal when the server runs
/// without a terminal. The server keeps running without console input.
///
/// # Errors
///
/// Returns an error if the thread cannot be created.
pub fn spawn_stdin_reader(sink: Sender<Input>) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("console".to_owned())
        .spawn(move || {
            for line in io::stdin().lock().lines() {
                let line = match line {
                    Ok(line) => line,
                    Err(err) => {
                        warn!("console input closed: {err}");
                        break;
                    }
                };
                if sink.send(Input::Line(line)).is_err() {
                    break;
                }
            }
        })
}
