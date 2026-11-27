#![allow(dead_code)]
//! Builds resource trees in a temporary folder and points a server at them.

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

use sixm_core::Host;
use sixm_server::{ResourceManager, Server};
use tempfile::TempDir;

pub mod net;

/// A server whose resource folder is a fresh temporary directory.
pub struct Fixture {
    dir: TempDir,
    pub server: Server,
}

impl Fixture {
    /// A server with `sixm_resourcesPath` pointing at an empty `resources` folder.
    /// Its network uses an ephemeral certificate, so no identity file is written.
    pub fn new() -> Self {
        let fixture = Self {
            dir: tempfile::tempdir().expect("temp dir"),
            server: Server::new(),
        };
        let line = format!(
            "set sixm_resourcesPath \"{}\"",
            fixture.resources().display()
        );
        fixture.server.execute(&line);
        fixture.server.execute("set sixm_identityFile \"\"");
        fixture
    }

    /// The folder scanned for resources.
    pub fn resources(&self) -> PathBuf {
        self.dir.path().join("resources")
    }

    /// The temporary folder that holds everything the fixture writes.
    pub fn root(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    /// The resource manager.
    pub fn manager(&self) -> &ResourceManager {
        self.server.manager()
    }

    /// Writes a resource called `name` with `manifest` and the given files,
    /// relative to its folder.
    pub fn add_resource(&self, name: &str, manifest: &str, files: &[(&str, &str)]) {
        let root = self.resources().join(name);
        write_file(&root.join("fxmanifest.lua"), manifest);
        for (file, body) in files {
            write_file(&root.join(file), body);
        }
    }

    /// Calls export `export` of `resource` and returns its first result as text.
    pub fn text(&self, resource: &str, export: &str) -> String {
        let values = self
            .manager()
            .call_export(resource, export, &[], None)
            .unwrap_or_else(|err| panic!("export `{export}` of `{resource}`: {err}"));
        match values.first() {
            Some(sixm_core::Value::String(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
            other => panic!("export `{export}` returned {other:?}, not a string"),
        }
    }
}

/// A `fxmanifest.lua` with the given server scripts, exports and dependencies.
pub fn manifest(scripts: &[&str], exports: &[&str], dependencies: &[&str]) -> String {
    let mut out = String::from("fx_version 'cerulean'\ngame 'common'\n");
    for script in scripts {
        let _ = writeln!(out, "server_script '{script}'");
    }
    for export in exports {
        let _ = writeln!(out, "server_export '{export}'");
    }
    for dependency in dependencies {
        let _ = writeln!(out, "dependency '{dependency}'");
    }
    out
}

/// Writes `body` to `path`, creating parent folders.
pub fn write_file(path: &std::path::Path, body: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create folder");
    }
    fs::write(path, body).expect("write file");
}
