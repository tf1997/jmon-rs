# jmon-rs

High-performance, zero-copy JVM monitor and library for Rust, powered by `hsperfdata`.

`jmon-rs` is a lightweight tool and library that provides real-time access to JVM performance metrics without the overhead of JMX or attaching agents. It reads JVM's shared memory performance data (`hsperfdata`) directly, making it extremely fast and efficient.

## Features

- **Zero-Copy Performance**: Directly maps JVM shared memory for O(1) metric lookups.
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

## Usage

### Command Line Interface

`jmon` provides two modes: Discovery Mode and Monitor Mode.

#### 1. Discovery Mode
List all running Java processes and their basic stats (PID, Name, Uptime, Heap, Threads, GC Time).

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
jmon-rs = { path = "path/to/jmon-rs" }
```

Example code:

```rust
use jmon_rs::JvmMonitor;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let pid = 12345; // Replace with an actual JVM PID
    let monitor = JvmMonitor::connect(pid)?;

    let gc_stats = monitor.get_gc_stats();
    println!("Eden Used: {} KB", gc_stats.eu);
    println!("Old Used: {} KB", gc_stats.ou);
    println!("Total GC Time: {}s", gc_stats.gct);

    let rt_stats = monitor.get_runtime_stats();
    println!("Live Threads: {}", rt_stats.threads_live);
    
    Ok(())
}
```

## Metrics Collected

- **Garbage Collection**: Survivor (S0/S1), Eden, Old Gen, Metaspace, Compressed Class Space usage and capacities. YGC, FGC, CGC counts and times.
- **Runtime**: Thread counts (Live/Daemon/Peak), Safepoint counts and times, Application uptime.
- **Class Loading**: Loaded/Unloaded class counts and memory usage.
- **JIT Compiler**: Compilation counts, times, and failure details.
- **Code Cache**: JIT Code Cache usage and utilization.

## Permissions

Since `jmon-rs` reads files from the system's temporary directory (e.g., `/tmp/hsperfdata_<user>`), you need appropriate permissions:
- You can monitor processes owned by the same user.
- To monitor processes owned by other users (including root), you may need to run `jmon` with `sudo`.

## License

This project is licensed under the Apache License 2.0 - see the LICENSE file for details.