//! Connects to a server, optionally sends one event with string arguments, then
//! prints everything the server sends for ten seconds before disconnecting.
//!
//! Usage: `cargo run -p sixm-net --example bot -- [--fingerprint <hex>] 127.0.0.1:30120 MyName [event [args...]]`
//!
//! With `--fingerprint`, the server's certificate must have that fingerprint,
//! as the server logs it at startup. Without it the connection is encrypted
//! but the server is not verified.

use std::error::Error;
use std::fmt;
use std::iter::Peekable;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use sixm_core::Value;
use sixm_net::{ClientEvent, Fingerprint, NetClient};

const LISTEN_FOR: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const FINGERPRINT_FLAG: &str = "--fingerprint";

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1).peekable();
    let fingerprint = take_fingerprint(&mut args)?;
    let addr: SocketAddr = args
        .next()
        .ok_or("usage: bot [--fingerprint <hex>] <addr> <name> [event [args...]]")?
        .parse()?;
    let name = args.next().ok_or("missing player name")?;
    let event = args.next();
    let values: Vec<Value> = args.map(Value::from).collect();

    let client = match fingerprint {
        Some(fingerprint) => NetClient::connect_pinned(addr, &name, &fingerprint, CONNECT_TIMEOUT)?,
        None => NetClient::connect(addr, &name, CONNECT_TIMEOUT)?,
    };
    println!(
        "connected as player {} to {}",
        client.player(),
        client.server_name()
    );
    if let Some(event) = event {
        client.send_event(&event, &values)?;
        println!("sent {event}({})", Values(&values));
    }

    let deadline = Instant::now() + LISTEN_FOR;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match client.recv_timeout(remaining) {
            Some(ClientEvent::Event { name, args }) => {
                println!("event {name}({})", Values(&args));
            }
            Some(ClientEvent::StateBag { bag, key, value }) => {
                println!("state {bag}.{key} = {}", Shown(&value));
            }
            Some(ClientEvent::StateBagRemoved { bag }) => {
                println!("state bag removed: {bag}");
            }
            Some(ClientEvent::Kicked { reason }) => {
                println!("kicked: {reason}");
                break;
            }
            Some(ClientEvent::Disconnected { reason }) => {
                println!("disconnected: {reason}");
                break;
            }
            None => break,
        }
    }
    client.disconnect();
    Ok(())
}

/// Takes the optional `--fingerprint <hex>` pair from the front of the arguments.
fn take_fingerprint(
    args: &mut Peekable<impl Iterator<Item = String>>,
) -> Result<Option<Fingerprint>, Box<dyn Error>> {
    if args.peek().map(String::as_str) != Some(FINGERPRINT_FLAG) {
        return Ok(None);
    }
    args.next();
    let text = args.next().ok_or("--fingerprint needs a value")?;
    Ok(Some(text.parse::<Fingerprint>()?))
}

/// Shows a value for people to read: text in quotes, arrays in brackets and
/// maps as `{key = value}`.
struct Shown<'a>(&'a Value);

impl fmt::Display for Shown<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Value::Nil => f.write_str("nil"),
            Value::Bool(flag) => write!(f, "{flag}"),
            Value::Int(number) => write!(f, "{number}"),
            Value::Float(number) => write!(f, "{number:?}"),
            Value::String(bytes) => write!(f, "\"{}\"", String::from_utf8_lossy(bytes)),
            Value::Array(items) => write!(f, "[{}]", Values(items)),
            Value::Map(entries) => {
                f.write_str("{")?;
                for (index, (key, value)) in entries.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{} = {}", Shown(key), Shown(value))?;
                }
                f.write_str("}")
            }
            Value::FuncRef(_) => f.write_str("<function>"),
        }
    }
}

/// Values separated by commas, as in an argument list.
struct Values<'a>(&'a [Value]);

impl fmt::Display for Values<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, value) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{}", Shown(value))?;
        }
        Ok(())
    }
}
