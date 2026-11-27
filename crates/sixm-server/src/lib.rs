//! The SixM dedicated server: command line, console, resource manager and main loop.
//!
//! [`Server`] owns a [`ResourceManager`] and runs console commands against it.
//! The binary parses the arguments, reads standard input and calls
//! [`Server::run`]. Everything else is usable from tests without a process.

mod access;
mod buckets;
mod client_entities;
mod commands;
mod config;
mod console;
mod convars;
mod entities;
mod host_functions;
mod http;
mod manager;
mod network;
mod perf;
mod profiler;
mod resource_files;
mod scope;
mod server;
mod state;
mod watch;
mod web;

pub use config::{
    ArgError, CliInvocation, CliOptions, Invocation, TokenError, parse_args, parse_cli, tokenize,
};
pub use console::spawn_stdin_reader;
pub use manager::{ManagerError, ResourceManager};
pub use server::{Input, Server};
