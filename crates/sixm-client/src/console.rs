//! Console commands of the client: the built-in set, convars, and the commands
//! that scripts register with `RegisterCommand`.

use sixm_core::{Host, Value, tokenize};
use tracing::{error, info, warn};

use crate::runtime::ClientRuntime;

/// Most queued commands that run in one drain. Beyond this, scripts are
/// assumed to be looping and the rest of the queue is dropped.
const MAX_QUEUED_COMMANDS: usize = 1000;

/// Built-in command names, as listed by `help`.
pub(crate) const BUILT_IN: [&str; 5] = ["help", "quit", "set", "setr", "sets"];

impl ClientRuntime {
    /// Runs one console line, then every command it or the scripts queued.
    pub fn execute(&self, line: &str) {
        self.run_line(line);
        self.drain_commands();
    }

    /// Runs one command given as words, name first, then every queued command.
    /// This is how `+command` arguments run.
    pub fn execute_words(&self, words: &[String]) {
        self.run_words(words, &words.join(" "));
        self.drain_commands();
    }

    /// Tokenizes `line` and runs it. A line that does not tokenize is logged.
    fn run_line(&self, line: &str) {
        match tokenize(line) {
            Ok(words) => self.run_words(&words, line.trim()),
            Err(err) => error!("{}: {err}", line.trim()),
        }
    }

    /// Runs one command given as words. `raw` is the line as a script command
    /// handler sees it.
    fn run_words(&self, words: &[String], raw: &str) {
        let Some((name, args)) = words.split_first() else {
            return;
        };
        match name.to_ascii_lowercase().as_str() {
            "set" | "sets" | "setr" => self.set_convar(args),
            "quit" => {
                let reason = if args.is_empty() {
                    "requested from the console".to_owned()
                } else {
                    args.join(" ")
                };
                info!("Shutting down: {reason}");
                self.quit.set(true);
            }
            "help" => self.help(),
            _ => self.run_script_or_convar(name, args, raw),
        }
    }

    /// Calls a script command or prints or sets a convar. A name neither claims
    /// is sent to the server.
    fn run_script_or_convar(&self, name: &str, args: &[String], raw: &str) {
        let command = self
            .commands
            .borrow()
            .get(&name.to_ascii_lowercase())
            .cloned();
        if let Some(command) = command {
            let call_args = [
                Value::Int(0),
                Value::Array(args.iter().map(|arg| Value::from(arg.as_str())).collect()),
                Value::from(raw),
            ];
            if let Err(err) = self.call_ref(&command.handler, &call_args) {
                error!("command `{name}` failed: {err}");
            }
            return;
        }
        let value = self.convars.borrow().get(name).map(str::to_owned);
        if let Some(value) = value {
            if args.is_empty() {
                info!("{name} = \"{value}\"");
            } else {
                self.store_convar(name, &args.join(" "));
            }
            return;
        }
        self.send_to_server(name, raw);
    }

    /// Sets a local convar. A convar the server replicates is refused with a
    /// warning, and keeps the value the server sent.
    fn store_convar(&self, name: &str, value: &str) {
        if self.convars.borrow().is_replicated(name) {
            warn!("{name} is replicated by the server and cannot be set here");
            return;
        }
        let changed = {
            let mut convars = self.convars.borrow_mut();
            let prev = convars.get(name).map(str::to_owned);
            convars.set(name, value);
            prev.as_deref() != Some(value)
        };
        if changed {
            self.deliver_convar_change(name, Some(value), false);
        }
    }

    /// Sends a line no client command or convar claims to the server, which
    /// may know it.
    fn send_to_server(&self, name: &str, raw: &str) {
        let sent = self.client.borrow().send_command(raw);
        if let Err(err) = sent {
            error!("{name}: the server did not get the command: {err}");
        }
    }

    /// `set`, `sets` and `setr`: stores a convar. The value is the rest of the
    /// line, joined by single spaces.
    fn set_convar(&self, args: &[String]) {
        let Some((name, value)) = args.split_first() else {
            warn!("usage: set <name> <value>");
            return;
        };
        self.store_convar(name, &value.join(" "));
    }

    /// `help`: lists the built-in and script commands.
    fn help(&self) {
        info!("Built-in commands: {}", BUILT_IN.join(", "));
        let listing: Vec<String> = self
            .commands
            .borrow()
            .iter()
            .map(|(name, command)| {
                if command.restricted {
                    format!("{name} (restricted)")
                } else {
                    name.clone()
                }
            })
            .collect();
        if listing.is_empty() {
            info!("No script commands are registered");
        } else {
            info!("Script commands: {}", listing.join(", "));
        }
    }

    /// Runs queued commands until the queue is empty, up to
    /// [`MAX_QUEUED_COMMANDS`] of them.
    pub(crate) fn drain_commands(&self) {
        let mut ran = 0;
        loop {
            let next = self.queued.borrow_mut().pop_front();
            let Some(line) = next else {
                return;
            };
            if ran == MAX_QUEUED_COMMANDS {
                let dropped = {
                    let mut queued = self.queued.borrow_mut();
                    let count = queued.len() + 1;
                    queued.clear();
                    count
                };
                warn!("more than {MAX_QUEUED_COMMANDS} commands queued at once; dropped {dropped}");
                return;
            }
            ran += 1;
            self.run_line(&line);
        }
    }
}
