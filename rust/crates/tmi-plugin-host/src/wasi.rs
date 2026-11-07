//! The WASI a plugin gets (PLUGINS.md 3.2): wasmtime-wasi's p2 (the
//! synchronous bindings), in a `WasiCtx` built so that every call sees the
//! same world:
//!
//! - no preopened directory (the filesystem is empty), no environment, no
//!   arguments, stdin closed;
//! - stdout and stderr are per-call memory pipes of [`PIPE_CAPACITY`] bytes
//!   (shown with `TMI_PLUGIN_LOG`, like `log`; never part of a record). A
//!   write beyond that fails, the same way on every machine;
//! - the wall clock stands at 1970-01-01T00:00:00Z, the monotonic clock at
//!   0;
//! - `random` and `insecure-random` are ChaCha20 from fixed seeds, and the
//!   insecure seed is 0;
//! - no sockets: `wasi:sockets` is not in the linker, and a component that
//!   imports anything outside [`allowed_import`] is refused when it is
//!   loaded.

use std::time::Duration;

use rand::SeedableRng;
use wasmtime::component::{HasData, Linker, Resource, ResourceTable};
use wasmtime_wasi::cli::{WasiCli, WasiCliView as _};
use wasmtime_wasi::async_trait;
use wasmtime_wasi::clocks::{HostMonotonicClock, HostWallClock, WasiClocks, WasiClocksView as _};
use wasmtime_wasi::p2::bindings::clocks::monotonic_clock::{self, Duration as WasiDuration, Instant};
use wasmtime_wasi::p2::{subscribe, DynPollable, Pollable};
use wasmtime_wasi::filesystem::{WasiFilesystem, WasiFilesystemView as _};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::random::WasiRandom;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiView};

/// Bytes a call may write to stdout, and to stderr.
pub const PIPE_CAPACITY: usize = 1 << 20;

struct StoppedWall;

impl HostWallClock for StoppedWall {
    fn resolution(&self) -> Duration {
        Duration::from_nanos(1)
    }
    fn now(&self) -> Duration {
        Duration::ZERO
    }
}

struct StoppedMonotonic;

impl HostMonotonicClock for StoppedMonotonic {
    fn resolution(&self) -> u64 {
        1
    }
    fn now(&self) -> u64 {
        0
    }
}

/// A pollable that is ready the moment it is asked. The monotonic clock
/// stands at 0 and never moves, so "wait until `when`" and "wait `d`" are
/// answered at once: the deadline is in the past for every guest. Nothing in
/// the host sleeps, which keeps every wait inside what fuel and the epoch
/// can end (the wasm code that loops on it).
struct Immediate;

#[async_trait]
impl Pollable for Immediate {
    async fn ready(&mut self) {}
}

/// `wasi:clocks/monotonic-clock`, with [`Immediate`] pollables.
pub struct Monotonic<'a> {
    table: &'a mut ResourceTable,
}

struct HasMonotonic;

impl HasData for HasMonotonic {
    type Data<'a> = Monotonic<'a>;
}

impl monotonic_clock::Host for Monotonic<'_> {
    fn now(&mut self) -> wasmtime::Result<Instant> {
        Ok(0)
    }

    fn resolution(&mut self) -> wasmtime::Result<Instant> {
        Ok(1)
    }

    fn subscribe_instant(&mut self, _when: Instant) -> wasmtime::Result<Resource<DynPollable>> {
        let ready = self.table.push(Immediate)?;
        subscribe(self.table, ready)
    }

    fn subscribe_duration(&mut self, _duration: WasiDuration) -> wasmtime::Result<Resource<DynPollable>> {
        let ready = self.table.push(Immediate)?;
        subscribe(self.table, ready)
    }
}

/// The context of one call, and its stdout / stderr pipes.
pub fn context() -> (WasiCtx, MemoryOutputPipe, MemoryOutputPipe) {
    let stdout = MemoryOutputPipe::new(PIPE_CAPACITY);
    let stderr = MemoryOutputPipe::new(PIPE_CAPACITY);
    let mut b = WasiCtxBuilder::new();
    b.stdout(stdout.clone())
        .stderr(stderr.clone())
        .wall_clock(StoppedWall)
        .monotonic_clock(StoppedMonotonic)
        .secure_random(rand::rngs::ChaCha20Rng::from_seed([0x5a; 32]))
        .insecure_random(rand::rngs::ChaCha20Rng::from_seed([0xa5; 32]))
        .insecure_random_seed(0)
        .allow_tcp(false)
        .allow_udp(false)
        .allow_ip_name_lookup(false);
    (b.build(), stdout, stderr)
}

/// The WASI interfaces a plugin may import (version 0.2.x), and nothing
/// else outside `tmi:format`.
const WASI: [&str; 20] = [
    "wasi:cli/environment",
    "wasi:cli/exit",
    "wasi:cli/stdin",
    "wasi:cli/stdout",
    "wasi:cli/stderr",
    "wasi:cli/terminal-input",
    "wasi:cli/terminal-output",
    "wasi:cli/terminal-stdin",
    "wasi:cli/terminal-stdout",
    "wasi:cli/terminal-stderr",
    "wasi:io/error",
    "wasi:io/poll",
    "wasi:io/streams",
    "wasi:clocks/wall-clock",
    "wasi:clocks/monotonic-clock",
    "wasi:filesystem/types",
    "wasi:filesystem/preopens",
    "wasi:random/random",
    "wasi:random/insecure",
    "wasi:random/insecure-seed",
];

/// The `tmi:format` interfaces of the `format-reader` world, and `edit`
/// (the `format-plugin` world, P7).
const TMI: [&str; 7] = ["types", "source", "codecs", "sqlite", "xml", "log", "edit"];

/// Whether a component may import `name` (`wasi:cli/environment@0.2.6`).
pub fn allowed_import(name: &str) -> bool {
    let (path, version) = name.split_once('@').unwrap_or((name, ""));
    if let Some(iface) = path.strip_prefix("tmi:format/") {
        return TMI.contains(&iface) && (version == "0.1" || version.starts_with("0.1."));
    }
    WASI.contains(&path) && version.starts_with("0.2.")
}

struct HasIo;

impl HasData for HasIo {
    type Data<'a> = &'a mut ResourceTable;
}

/// The allowed WASI interfaces, synchronous (wasmtime-wasi's
/// `add_to_linker_sync` without the sockets).
pub fn add_to_linker<T: WasiView>(l: &mut Linker<T>) -> wasmtime::Result<()> {
    use wasmtime_wasi::p2::bindings::{cli, clocks, filesystem, random, sync};
    wasmtime_wasi::p2::bindings::io::error::add_to_linker::<T, HasIo>(l, |t| t.ctx().table)?;
    sync::io::poll::add_to_linker::<T, HasIo>(l, |t| t.ctx().table)?;
    sync::io::streams::add_to_linker::<T, HasIo>(l, |t| t.ctx().table)?;
    clocks::wall_clock::add_to_linker::<T, WasiClocks>(l, T::clocks)?;
    // not wasmtime-wasi's: its pollables really wait (see `Immediate`)
    clocks::monotonic_clock::add_to_linker::<T, HasMonotonic>(l, |t| Monotonic { table: t.ctx().table })?;
    filesystem::preopens::add_to_linker::<T, WasiFilesystem>(l, T::filesystem)?;
    sync::filesystem::types::add_to_linker::<T, WasiFilesystem>(l, T::filesystem)?;
    random::random::add_to_linker::<T, WasiRandom>(l, |t| t.ctx().ctx.random())?;
    random::insecure::add_to_linker::<T, WasiRandom>(l, |t| t.ctx().ctx.random())?;
    random::insecure_seed::add_to_linker::<T, WasiRandom>(l, |t| t.ctx().ctx.random())?;
    cli::exit::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::environment::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::stdin::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::stdout::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::stderr::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::terminal_input::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::terminal_output::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::terminal_stdin::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::terminal_stdout::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::terminal_stderr::add_to_linker::<T, WasiCli>(l, T::cli)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::allowed_import;

    #[test]
    fn the_import_list() {
        assert!(allowed_import("wasi:cli/environment@0.2.6"));
        assert!(allowed_import("wasi:random/random@0.2.0"));
        assert!(allowed_import("tmi:format/source@0.1.0"));
        assert!(allowed_import("tmi:format/codecs@0.1.3"));
        assert!(!allowed_import("tmi:format/reader@0.1.0"), "an export, never an import");
        assert!(!allowed_import("tmi:format/source@1.0.0"));
        assert!(!allowed_import("wasi:sockets/tcp@0.2.6"));
        assert!(!allowed_import("wasi:sockets/instance-network@0.2.6"));
        assert!(!allowed_import("wasi:http/outgoing-handler@0.2.6"));
        assert!(!allowed_import("wasi:cli/environment@0.3.0"));
        assert!(!allowed_import("wasi:cli/environment"));
    }
}
