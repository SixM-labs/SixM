//! The SixM dedicated server: command line, console, resource manager and main loop.
//!
//! [`Server`] owns a [`ResourceManager`] and runs console commands against it.
//! The binary parses the arguments, reads standard input and calls
//! [`Server::run`]. Everything else is usable from tests without a process.

mod access;
mod commands;
mod config;
mod console;
mod convars;
mod entities;
mod host_functions;
mod manager;
mod network;
mod resource_files;
mod scope;
mod server;
mod state;

pub use config::{ArgError, Invocation, TokenError, parse_args, tokenize};
pub use console::spawn_stdin_reader;
pub use manager::{ManagerError, ResourceManager};
pub use server::{Input, Server};
