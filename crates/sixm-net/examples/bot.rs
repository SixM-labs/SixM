//! Connects to a server, optionally sends one event with string arguments, then
//! prints everything the server sends for ten seconds before disconnecting.
//!
//! Usage: `cargo run -p sixm-net --example bot -- 127.0.0.1:30120 MyName [event [args...]]`

use std::error::Error;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use sixm_core::Value;
use sixm_net::{ClientEvent, NetClient};

const LISTEN_FOR: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let addr: SocketAddr = args
        .next()
        .ok_or("usage: bot <addr> <name> [event [args...]]")?
        .parse()?;
    let name = args.next().ok_or("missing player name")?;
    let event = args.next();
    let values: Vec<Value> = args.map(Value::from).collect();

    let client = NetClient::connect(addr, &name, CONNECT_TIMEOUT)?;
    println!(
        "connected as player {} to {}",
        client.player(),
        client.server_name()
    );
    if let Some(event) = event {
        client.send_event(&event, &values)?;
        println!("sent {event} {values:?}");
    }

    let deadline = Instant::now() + LISTEN_FOR;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match client.recv_timeout(remaining) {
            Some(ClientEvent::Event { name, args }) => println!("event {name} {args:?}"),
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
