# jmon-rs

High-performance JVM monitor and library for Rust, powered by `hsperfdata` shared memory.

`jmon-rs` is a lightweight tool and library that provides real-time access to JVM performance metrics without the overhead of JMX or attaching agents. It reads JVM's shared memory performance data (`hsperfdata`) directly, making it extremely fast and efficient.

## Features

- **Direct Counter Reads**: Maps JVM shared memory and pre-resolves built-in numeric counter offsets.
- **Low Overhead**: No JMX, no agents, and no network communication required.
- **Rich Metrics**: Access GC stats, class loading, JIT compilation, threads, safepoints, and more.
- **Auto-Discovery**: Built-in discovery mode to list all running Java processes on the system.
- **CLI & Library**: Use it as a standalone monitoring tool or integrate it into your Rust applications.
- **Cross-Platform**: Supports Linux, macOS, and Windows.

## Installation

### From Source

```bash
git clone https://github.com/tf1997/jmon-rs.git
cd jmon-rs
cargo build --release
```

The binary will be available at `./target/release/jmon`.

### From crates.io

```bash
cargo install jmon-rs
```

## Usage

### Command Line Interface

`jmon` provides two modes: Discovery Mode and Monitor Mode.

#### 1. Discovery Mode
List running Java processes and their basic stats (PID, Name, Heap, Threads, GC Time).

```bash
jmon
```

#### 2. Monitor Mode
Monitor a specific JVM process with detailed real-time metrics.

```bash
# Monitor PID 12345 with 1-second refresh interval
jmon 12345

# Monitor PID 12345 with 5-second refresh interval
jmon 12345 5
```

### Library Usage

Add `jmon-rs` to your `Cargo.toml`:

```toml
[dependencies]
jmon-rs = "0.1.5"
```

Example code:

```rust
use jmon_rs::JvmMonitor;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let pid = 12345; // Replace with an actual JVM PID
    let monitor = JvmMonitor::connect(pid)?;
    let snapshot = monitor.sample()?;

    println!("Eden Used: {} KB", snapshot.gc.eu);
    println!("Old Used: {} KB", snapshot.gc.ou);
    println!("Total GC Time: {}s", snapshot.gc.gct);

    println!("Live Threads: {}", snapshot.runtime.threads_live);

    Ok(())
}
```

For a long-running collector, reuse one monitor per PID and use the checked
sampling API so lifecycle changes and newly-added counters can be handled
explicitly. Linux additionally checks the process start-time when `/proc` is
readable; lifecycle identity checks on macOS and Windows are best-effort.

```rust,no_run
use jmon_rs::JvmMonitor;

fn scrape(pid: u32) -> Result<(), Box<dyn std::error::Error>> {
    let monitor = JvmMonitor::connect(pid)?;
    loop {
        let snapshot = monitor.sample()?;
        println!("heap old used: {} KB", snapshot.gc.ou);
    }
}
```

Do not call `connect()` or `discover_all()` for every individual metric read.
For very high sampling rates, prefer `get_gc_numeric_stats()` and
`get_compiler_numeric_stats()` after an explicit `refresh()` and collect
diagnostic strings less frequently.

## Metrics Collected

- **Garbage Collection**: jstat-compatible generation/space values, Metaspace, Compressed Class Space, and optional collector 0/1/2 counters with their actual JVM names.
- **Runtime**: Thread counts (Live/Daemon/Peak), Safepoint counters/times, and application-time ticks.
- **Class Loading**: Loaded/Unloaded class counts and memory usage.
- **JIT Compiler**: Compilation counts, times, and failure details.
- **Code Cache**: Exact vendor counters when published; standard OpenJDK PerfData reports this capability as unavailable instead of substituting another memory pool.

## Permissions

Since `jmon-rs` reads files from the system's temporary directory (e.g., `/tmp/hsperfdata_<user>`), you need appropriate permissions:
- You can monitor processes owned by the same user.
- To monitor processes owned by other users (including root), you may need to run `jmon` with `sudo`.

## License

This project is licensed under the Apache License 2.0 - see the LICENSE file for details.
