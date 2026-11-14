//! The wasmtime engine (PLUGINS.md 3.1): every setting in [`engine`], so
//! the configuration that decides what a component computes is in one place.

use std::path::PathBuf;
use std::time::Duration;

use wasmtime::{Config, Engine, InstanceAllocationStrategy, PoolingAllocationConfig};

/// How deep wasm may recurse (3.1: an overflow always fails, whatever the
/// machine, so it never changes a record).
pub const MAX_WASM_STACK: usize = 8 << 20;
/// The host stack under it, for the host functions and wasmtime itself.
pub const HOST_STACK: usize = 8 << 20;
/// How often the epoch advances (3.1).
pub const EPOCH_TICK: Duration = Duration::from_millis(100);

/// Core instances, memories and tables one component may have in the
/// pooling allocator's slots, sized for plugins written in Rust (PLUGINS.md
/// Q14, 6.2): a `wasm32-wasip2` component on tmi-plugin-sdk instantiates 4
/// or 5 core modules (its own and wit-component's shims) with 1 memory and
/// 2 tables (`rust/plugins` and the templates, counted with `wasm-tools
/// print`; P7 had these slots too). A component over them (componentize-py's
/// have 38 core instances) is refused when it is compiled or mapped, with
/// [`crate::plugin::pooling_limit_message`]'s sentence
/// (`tests::a_component_over_the_pooling_slots_is_refused_with_a_sentence`).
pub const CORE_INSTANCES_PER_COMPONENT: u32 = 16;
pub const MEMORIES_PER_COMPONENT: u32 = 4;
pub const TABLES_PER_COMPONENT: u32 = 8;

/// What a host is configured with (PLUGINS.md 3.1 and 5; the `plugin.*`
/// config names are P6's).
#[derive(Clone, Debug)]
pub struct Settings {
    /// Fuel per call (`plugin.fuel`, 2×10¹⁰). None: no fuel metering at all
    /// (measurements only: PLUGINS.md 6.4 item 5).
    pub fuel: Option<u64>,
    /// Wall-clock limit per call (`plugin.timeout`, 120 s). None: no epoch
    /// interruption (measurements only).
    pub timeout: Option<Duration>,
    /// Linear memory per instance, all its memories together
    /// (`plugin.maxMemory`, 1 GiB).
    pub max_memory: usize,
    /// Calls that run at once: the worker threads, and the slots of the
    /// pooling allocator (PLUGINS.md 6.1's K).
    pub workers: usize,
    /// The pooling instance allocator (else on-demand allocation).
    pub pooling: bool,
    /// `memory_init_cow`.
    pub memory_init_cow: bool,
    /// Where compiled components are kept; None: compile every time.
    pub cache_dir: Option<PathBuf>,
    /// `max_wasm_stack` ([`MAX_WASM_STACK`]; `tmiki plugin test` runs the
    /// corpus with a quarter of it, PLUGINS.md 3.1). At most that.
    pub max_wasm_stack: usize,
    /// Every database the `sqlite` helper opens runs with
    /// `PRAGMA reverse_unordered_selects=ON` (`tmiki plugin test`, PLUGINS.md
    /// 3.3: an answer must not hang on the order of an unordered SELECT).
    pub sqlite_reverse_unordered_selects: bool,
}

/// K = min(cores, 8) (PLUGINS.md 6.1; the memory budget is not consulted:
/// 8 instances of 1 GiB are reserved address space, not memory in use).
pub fn default_workers() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 8)
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            fuel: Some(20_000_000_000),
            timeout: Some(Duration::from_secs(120)),
            max_memory: 1 << 30,
            workers: default_workers(),
            pooling: true,
            memory_init_cow: true,
            cache_dir: crate::cache::default_dir(),
            max_wasm_stack: MAX_WASM_STACK,
            sqlite_reverse_unordered_selects: false,
        }
    }
}

/// The engine for `s` (PLUGINS.md 3.1's table, row for row).
pub fn engine(s: &Settings) -> wasmtime::Result<Engine> {
    let mut c = Config::new();
    c.cranelift_nan_canonicalization(true)
        .wasm_relaxed_simd(false)
        .wasm_simd(true)
        // wasm_threads and wasm_component_model_async (with async in
        // general): off by construction, wasmtime's `threads` and
        // `component-model-async` features are not built (the methods do
        // not exist without them)
        .wasm_shared_everything_threads(false)
        .wasm_component_model(true)
        .wasm_gc(false)
        // wasm_exceptions: off ([`EXCEPTIONS`]); without the `gc` feature
        // wasmtime has no such method and leaves exceptions off
        .wasm_memory64(false)
        .wasm_stack_switching(false)
        .consume_fuel(s.fuel.is_some())
        .epoch_interruption(s.timeout.is_some())
        .max_wasm_stack(s.max_wasm_stack.clamp(64 << 10, MAX_WASM_STACK))
        // wasmtime-wasi's p2 builds wasmtime's `async` feature in, which
        // requires the (unused) async stack to hold the wasm stack
        .async_stack_size(s.max_wasm_stack.clamp(64 << 10, MAX_WASM_STACK) + (1 << 20))
        .memory_init_cow(s.memory_init_cow)
        .parallel_compilation(true);
    if s.pooling {
        let n = s.workers.max(1) as u32;
        let mut p = PoolingAllocationConfig::default();
        p.total_component_instances(n)
            .total_core_instances(n * CORE_INSTANCES_PER_COMPONENT)
            .total_memories(n * MEMORIES_PER_COMPONENT)
            .total_tables(n * TABLES_PER_COMPONENT)
            .max_core_instances_per_component(CORE_INSTANCES_PER_COMPONENT)
            .max_memories_per_component(MEMORIES_PER_COMPONENT)
            .max_tables_per_component(TABLES_PER_COMPONENT)
            .max_memory_size(s.max_memory);
        c.allocation_strategy(InstanceAllocationStrategy::Pooling(p));
    }
    Engine::new(&c)
}

/// Whether wasm exceptions are on. PLUGINS.md 3.1 says on (deterministic,
/// for C++ and later languages), but wasmtime implements them on its GC
/// heap, which this build leaves out (3.1: `wasm_gc` off, and the `gc`
/// feature costs size); see "P5 の結果".
pub const EXCEPTIONS: bool = false;

/// The text the compiled-component cache key holds for `s`: the settings
/// that change compiled code (wasmtime's own compatibility hash covers its
/// version and compiler flags; this adds what tmi chose).
pub fn fingerprint(s: &Settings) -> String {
    format!(
        "tmi-plugin-host {} wasmtime 49.0.5 fuel={} epoch={} stack={} pooling={} cow={} maxmem={} exceptions={}",
        env!("CARGO_PKG_VERSION"),
        s.fuel.is_some(),
        s.timeout.is_some(),
        s.max_wasm_stack.clamp(64 << 10, MAX_WASM_STACK),
        s.pooling,
        s.memory_init_cow,
        s.max_memory,
        EXCEPTIONS
    )
}

/// Advances `engine`'s epoch every [`EPOCH_TICK`] until the engine is gone.
pub fn start_epoch_ticker(engine: &Engine) {
    let weak = engine.weak();
    std::thread::Builder::new()
        .name("tmi-plugin-epoch".into())
        .spawn(move || loop {
            std::thread::sleep(EPOCH_TICK);
            match weak.upgrade() {
                Some(e) => e.increment_epoch(),
                None => return,
            }
        })
        .expect("spawn the epoch thread");
}

/// The deadline in epoch ticks for `timeout`: one more than it takes, as
/// the first tick may come at once, so a call is stopped between `timeout`
/// and `timeout` + one tick after it starts.
pub fn deadline_ticks(timeout: Duration) -> u64 {
    timeout.as_millis().div_ceil(EPOCH_TICK.as_millis()) as u64 + 1
}
