use jmon_rs::{BuiltinLongMetric, JvmMonitor, JvmMonitorError};
use std::borrow::Cow;
use std::env;
use std::io::{self, BufWriter, Write};
use std::thread;
use std::time::Duration;

enum MonitorRunError {
    Output(io::Error),
    Target(JvmMonitorError),
}

impl From<io::Error> for MonitorRunError {
    fn from(error: io::Error) -> Self {
        Self::Output(error)
    }
}

impl From<JvmMonitorError> for MonitorRunError {
    fn from(error: JvmMonitorError) -> Self {
        Self::Target(error)
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        run_discovery_mode();
        return;
    }

    let arg1 = &args[1];

    if arg1 == "--help" || arg1 == "-h" {
        print_help();
        return;
    }

    let pid: u32 = match arg1.parse() {
        Ok(n) => n,
        Err(_) => {
            eprintln!("Error: PID must be a number.");
            return;
        }
    };

    let interval: u64 = if args.len() >= 3 {
        args[2].parse().unwrap_or(1)
    } else {
        1
    };

    if interval == 0 {
        eprintln!("Error: refresh interval must be greater than zero seconds.");
        std::process::exit(2);
    }

    if let Err(error) = run_monitor_mode(pid, interval) {
        match error {
            MonitorRunError::Output(error) if error.kind() == io::ErrorKind::BrokenPipe => {}
            MonitorRunError::Output(error) => {
                eprintln!("Failed to write monitor output: {}", error);
                std::process::exit(1);
            }
            MonitorRunError::Target(error) => {
                eprintln!("Monitoring stopped: {}", error);
                std::process::exit(1);
            }
        }
    }
}

fn run_discovery_mode() {
    println!("Scanning for Java processes...");

    match JvmMonitor::discover_all() {
        Ok(processes) => {
            if processes.is_empty() {
                println!("No Java processes found (owned by current user).");
                #[cfg(unix)]
                println!("Hint: Try running with 'sudo' to see processes from other users.");
                return;
            }

            println!(
                "{:<8} | {:<25} | {:<10} | {:<10} | {:<10}",
                "PID", "Name", "Heap Used", "Live Thr", "GC Time"
            );
            println!("{}", "-".repeat(90));

            for proc in processes {
                let (heap_display, threads_live, gc_time_s) =
                    if let Ok(monitor) = JvmMonitor::connect(proc.pid) {
                        let gc = monitor.get_gc_stats();
                        let rt = monitor.get_runtime_stats();

                        // Eden + Old + Survivors)
                        let heap_used_kb = gc.eu + gc.ou + gc.s0u + gc.s1u;
                        let heap_str = format!("{:.1} MB", heap_used_kb / 1024.0);

                        (
                            heap_str,
                            rt.threads_live.to_string(),
                            format!("{:.3}s", gc.gct),
                        )
                    } else {
                        ("-".to_string(), "-".to_string(), "-".to_string())
                    };

                let name_display = truncate_for_display(&proc.name, 25, 22);

                println!(
                    "{:<8} | {:<25} | {:<10} | {:<10} | {:<10}",
                    proc.pid, name_display, heap_display, threads_live, gc_time_s
                );
            }
            println!("{}", "-".repeat(90));
            println!("* Tip: Run 'jmon <PID>' to see detailed metrics.");
        }
        Err(e) => eprintln!("Failed to discover processes: {}", e),
    }
}

fn run_monitor_mode(pid: u32, interval: u64) -> Result<(), MonitorRunError> {
    let stdout = io::stdout();
    {
        let mut output = BufWriter::new(stdout.lock());
        writeln!(output, "Connecting to JVM PID: {} ...", pid)?;
        output.flush()?;
    }

    let monitor = JvmMonitor::connect(pid)?;

    let vm_name = monitor.read_string("java.property.java.vm.name");
    let vm_version = monitor.read_string("java.property.java.vm.version");
    let cmd_line = monitor.read_string("sun.rt.javaCommand");
    let display_cmd = truncate_for_display(&cmd_line, 75, 75);
    let compatibility = monitor.compatibility();
    let collector_display = compatibility
        .collector_names
        .iter()
        .enumerate()
        .filter_map(|(ordinal, name)| name.as_ref().map(|name| format!("{ordinal}={name}")))
        .collect::<Vec<_>>()
        .join(" | ");

    loop {
        let snapshot = monitor.sample()?;
        let gc = snapshot.gc;
        let rt = snapshot.runtime;
        let cls = snapshot.classes;
        let comp = snapshot.compiler;

        // Buffer and lock one complete frame so terminal output needs as few writes as possible.
        {
            let mut output = BufWriter::new(stdout.lock());
            write!(output, "\x1B[2J\x1B[1;1H")?;

            writeln!(
                output,
                "==============================================================================="
            )?;
            writeln!(
                output,
                " JMON - JVM Monitor | PID: {} | Interval: {}s",
                pid, interval
            )?;
            writeln!(output, " VM: {} ({})", vm_name, vm_version)?;
            writeln!(output, " CMD: {}", display_cmd)?;
            if !collector_display.is_empty() {
                writeln!(output, " GC slots: {}", collector_display)?;
            }
            writeln!(
                output,
                "==============================================================================="
            )?;

            writeln!(output, "\n[Runtime & Threads]")?;
            writeln!(
                output,
                "{:<8} {:<8} {:<8} | {:<10} {:<10} | {:<10} {:<12}",
                "Live", "Daemon", "Peak", "Safepoints", "SP Time(s)", "AppTime(s)", "Overhead%"
            )?;
            writeln!(
                output,
                "{:<8} {:<8} {:<8} | {:<10} {:<10.3} | {:<10.3} {:<12.3}%",
                rt.threads_live,
                rt.threads_daemon,
                rt.threads_peak,
                rt.safepoints,
                rt.safepoint_time_s,
                rt.app_time_s,
                rt.safepoint_overhead * 100.0
            )?;

            writeln!(output, "\n[Garbage Collection (KB)]")?;
            writeln!(
                output,
                "{:<8} {:<8} {:<8} {:<8} | {:<8} {:<8} | {:<8} {:<8} | {:<8}",
                "S0U", "S1U", "EU", "EC", "OU", "OC", "MU", "MC", "CCSU"
            )?;
            writeln!(
                output,
                "{:<8.0} {:<8.0} {:<8.0} {:<8.0} | {:<8.0} {:<8.0} | {:<8.0} {:<8.0} | {:<8.0}",
                gc.s0u, gc.s1u, gc.eu, gc.ec, gc.ou, gc.oc, gc.mu, gc.mc, gc.ccsu
            )?;

            writeln!(
                output,
                "\n{:<6} {:<8} | {:<6} {:<8} | {:<6} {:<8} | {:<8} | {:<20}",
                "C0", "C0T", "C1", "C1T", "C2", "C2T", "GCT", "Last Cause"
            )?;
            writeln!(
                output,
                "{:<6} {:<8.3} | {:<6} {:<8.3} | {:<6} {:<8.3} | {:<8.3} | {:<20}",
                gc.ygc, gc.ygct, gc.fgc, gc.fgct, gc.cgc, gc.cgct, gc.gct, gc.lgcc
            )?;

            writeln!(output, "\n[Class & JIT]")?;
            writeln!(
                output,
                "{:<10} {:<10} | {:<10} {:<10} {:<10} | {:<10} {:<8}",
                "Loaded", "Unloaded", "Compiled", "Failed", "Invalid", "CodeCache", "Util%"
            )?;
            let code_cache_used = monitor
                .read_builtin_long(BuiltinLongMetric::CodeCacheUsed)
                .map(|bytes| format!("{:.0}", bytes as f64 / 1024.0))
                .unwrap_or_else(|| "-".to_owned());
            let code_cache_utilization = monitor
                .read_builtin_long(BuiltinLongMetric::CodeCacheCapacity)
                .filter(|capacity| *capacity > 0)
                .and_then(|capacity| {
                    monitor
                        .read_builtin_long(BuiltinLongMetric::CodeCacheUsed)
                        .map(|used| format!("{:.1}%", used as f64 / capacity as f64 * 100.0))
                })
                .unwrap_or_else(|| "-".to_owned());
            writeln!(
                output,
                "{:<10} {:<10} | {:<10} {:<10} {:<10} | {:<10} {:<8}",
                cls.loaded,
                cls.unloaded,
                comp.compiled,
                comp.failed,
                comp.invalid,
                code_cache_used,
                code_cache_utilization
            )?;

            writeln!(
                output,
                "==============================================================================="
            )?;
            output.flush()?;
        }

        thread::sleep(Duration::from_secs(interval));
    }
}

fn truncate_for_display<'a>(value: &'a str, threshold: usize, keep: usize) -> Cow<'a, str> {
    if value.chars().nth(threshold).is_none() {
        Cow::Borrowed(value)
    } else {
        let end = value
            .char_indices()
            .nth(keep)
            .map_or(value.len(), |(index, _)| index);
        Cow::Owned(format!("{}...", &value[..end]))
    }
}

fn print_help() {
    println!("JMon - High Performance JVM Monitor (Rust)");
    println!("------------------------------------------");
    println!("Usage:");
    println!("  jmon              List all Java processes (Discovery Mode)");
    println!("  jmon <pid> [sec]  Monitor specific PID (Monitor Mode)");
    println!();
    println!("Examples:");
    println!("  jmon              # Show summary of all Java apps");
    println!("  jmon 12345        # Monitor PID 12345 (refresh 1s)");
    println!("  jmon 12345 5      # Monitor PID 12345 (refresh 5s)");
}

#[cfg(test)]
mod tests {
    use super::truncate_for_display;

    #[test]
    fn display_truncation_preserves_ascii_layout() {
        assert_eq!(
            truncate_for_display("abcdefghijklmnopqrstuvwxyz", 25, 22),
            "abcdefghijklmnopqrstuv..."
        );
        assert_eq!(
            truncate_for_display("abcdefghijklmnopqrstuvwxy", 25, 22),
            "abcdefghijklmnopqrstuvwxy"
        );
    }

    #[test]
    fn display_truncation_respects_utf8_boundaries() {
        assert_eq!(truncate_for_display("你好世界", 3, 2), "你好...");
        assert_eq!(truncate_for_display("你好世界", 4, 2), "你好世界");
    }
}
