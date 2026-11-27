"""Package the release binaries and smoke test the extracted archive on loopback."""

import argparse
from contextlib import contextmanager
import os
from pathlib import Path
from queue import Empty, Queue
import re
import stat
import subprocess
from tempfile import TemporaryDirectory
from threading import Thread
import time
import tomllib
from zipfile import ZIP_DEFLATED, ZipFile


ROOT = Path(__file__).resolve().parents[1]


def package(binaries, target, tag):
    name = f"sixm-{tag}-{target}"
    output = ROOT / "dist" / f"{name}.zip"
    output.parent.mkdir(exist_ok=True)
    suffix = ".exe" if target.endswith("-msvc") else ""
    filenames = [f"sixm-server{suffix}", f"sixm-client{suffix}"]
    with ZipFile(output, "w", ZIP_DEFLATED) as archive:
        for filename in filenames:
            archive.write(binaries / filename, f"{name}/{filename}")
        for filename in ["LICENSE", "README.md"]:
            archive.write(ROOT / filename, f"{name}/{filename}")
        example = ROOT / "examples" / "server-data"
        for path in sorted(example.rglob("*")):
            relative = path.relative_to(example)
            if path.is_file() and relative.parts[0] != "cache":
                archive.write(path, f"{name}/server-data/{relative.as_posix()}")
    return output, name, filenames


@contextmanager
def running(command, directory):
    with subprocess.Popen(
        command,
        cwd=directory,
        env={**os.environ, "RUST_LOG": "info"},
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        errors="replace",
    ) as process:
        output = Queue()

        def collect():
            for line in process.stdout:
                print(line, end="", flush=True)
                output.put(line)
            output.put(None)

        reader = Thread(target=collect, daemon=True)
        reader.start()
        try:
            yield process, output
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
            reader.join()


def wait_for(output, pattern):
    deadline = time.monotonic() + 30
    while True:
        try:
            line = output.get(timeout=max(0, deadline - time.monotonic()))
        except Empty as error:
            raise RuntimeError(f"timed out waiting for {pattern!r}") from error
        if line is None:
            raise RuntimeError(f"process exited before reporting {pattern!r}")
        match = re.search(pattern, line)
        if match:
            return match
        if time.monotonic() >= deadline:
            raise RuntimeError(f"timed out waiting for {pattern!r}")


def stop(process):
    process.stdin.write("quit\n")
    process.stdin.flush()
    exit_code = process.wait(timeout=15)
    if exit_code:
        raise RuntimeError(f"{process.args[0]} exited with status {exit_code}")


def smoke(archive_path, name, filenames, tag):
    with TemporaryDirectory(prefix="sixm-package-") as folder:
        root = Path(folder) / name
        with ZipFile(archive_path) as archive:
            archive.extractall(folder)
            if os.name != "nt":
                for filename in filenames:
                    mode = archive.getinfo(f"{name}/{filename}").external_attr >> 16
                    if not mode & stat.S_IXUSR:
                        raise RuntimeError(f"{filename} has no executable bit in the archive")
                    (root / filename).chmod(mode & 0o777)

        server_binary, client_binary = (root / filename for filename in filenames)
        for binary in (server_binary, client_binary):
            version = subprocess.check_output([binary, "--version"], text=True, timeout=10)
            if version.strip().split()[-1] != tag.removeprefix("v"):
                raise RuntimeError(f"unexpected version from {binary.name}: {version!r}")

        server_command = [server_binary, "+exec", "server.cfg", "+endpoint_add_udp", "127.0.0.1:0"]
        with running(server_command, root / "server-data") as (server, server_output):
            address = wait_for(server_output, r"Listening on (127\.0\.0\.1:\d+)")[1]
            fingerprint = wait_for(server_output, r"Server fingerprint: ([0-9a-f]{64})")[1]
            client_command = [
                client_binary, address, "PackageProbe", "--fingerprint", fingerprint,
                "+say", "Hello from the release archive",
            ]
            with running(client_command, root) as (client, client_output):
                message = r"\[chat\] PackageProbe: Hello from the release archive"
                wait_for(client_output, message)
                wait_for(server_output, message)
                stop(client)
            stop(server)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("target")
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--tag")
    args = parser.parse_args()
    with (ROOT / "Cargo.toml").open("rb") as cargo:
        version = tomllib.load(cargo)["workspace"]["package"]["version"]
    tag = f"v{version}"
    if args.tag is not None and args.tag != tag:
        parser.error(f"tag {args.tag} does not match workspace version {version}")

    output, name, filenames = package(args.binaries, args.target, tag)
    smoke(output, name, filenames, tag)
    print(f"Verified {output}")


if __name__ == "__main__":
    main()
