use jmon_rs::JvmMonitor;
use std::env;
use std::io::{self, Write};
use std::thread;
use std::time::Duration;

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

    let interval = if args.len() >= 3 {
        args[2].parse().unwrap_or(1)
    } else {
        1
    };

    run_monitor_mode(pid, interval);
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

            println!("{:<8} | {:<25} | {:<10} | {:<10} | {:<10}", 
                "PID", "Name", "Heap Used", "Live Thr", "GC Time");
            println!("{}", "-".repeat(90));

            for proc in processes {
                let (heap_display, threads_live, gc_time_s) = if let Ok(monitor) = JvmMonitor::connect(proc.pid) {
                    let gc = monitor.get_gc_stats();
                    let rt = monitor.get_runtime_stats();
                    
                    // Eden + Old + Survivors)
                    let heap_used_kb = gc.eu + gc.ou + gc.s0u + gc.s1u;
                    let heap_str = format!("{:.1} MB", heap_used_kb / 1024.0);
                    
                    (heap_str, rt.threads_live.to_string(), format!("{:.3}s", gc.gct))
                } else {
                    ("-".to_string(), "-".to_string(), "-".to_string())
                };

                let name_display = if proc.name.len() > 25 { 
                    format!("{}...", &proc.name[..22]) 
                } else { 
                    proc.name.clone() 
                };

                println!("{:<8} | {:<25} | {:<10} | {:<10} | {:<10}", 
                    proc.pid, name_display, heap_display, threads_live, gc_time_s);
            }
            println!("{}", "-".repeat(90));
            println!("* Tip: Run 'jmon <PID>' to see detailed metrics.");
        },
        Err(e) => eprintln!("Failed to discover processes: {}", e),
    }
}

fn run_monitor_mode(pid: u32, interval: u64) {
    println!("Connecting to JVM PID: {} ...", pid);
    
    let monitor = match JvmMonitor::connect(pid) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Failed to connect: {}", e);
            eprintln!("Hint: Check if the PID exists and you have permissions (try sudo).");
            std::process::exit(1);
        }
    };

    let vm_name = monitor.read_string("java.property.java.vm.name");
    let vm_version = monitor.read_string("java.property.java.vm.version");
    let cmd_line = monitor.read_string("sun.rt.javaCommand");

    loop {
        let gc = monitor.get_gc_stats();
        let rt = monitor.get_runtime_stats();
        let cls = monitor.get_class_stats();
        let comp = monitor.get_compiler_stats();
        print!("\x1B[2J\x1B[1;1H");
        
        println!("===============================================================================");
        println!(" JMON - JVM Monitor | PID: {} | Interval: {}s", pid, interval);
        println!(" VM: {} ({})", vm_name, vm_version);
        let display_cmd = if cmd_line.len() > 75 { format!("{}...", &cmd_line[..75]) } else { cmd_line.clone() };
        println!(" CMD: {}", display_cmd);
        println!("===============================================================================");

        println!("\n[Runtime & Threads]");
        println!("{:<8} {:<8} {:<8} | {:<10} {:<10} | {:<10} {:<12}", 
            "Live", "Daemon", "Peak", "Safepoints", "SP Time(s)", "AppTime(s)", "Overhead%");
        println!("{:<8} {:<8} {:<8} | {:<10} {:<10.3} | {:<10.3} {:<12.3}%",
            rt.threads_live, rt.threads_daemon, rt.threads_peak,
            rt.safepoints, rt.safepoint_time_s, rt.app_time_s, rt.safepoint_overhead * 100.0
        );

        println!("\n[Garbage Collection (KB)]");
        println!("{:<8} {:<8} {:<8} {:<8} | {:<8} {:<8} | {:<8} {:<8} | {:<8}", 
            "S0U", "S1U", "EU", "EC", "OU", "OC", "MU", "MC", "CCSU");
        println!("{:<8.0} {:<8.0} {:<8.0} {:<8.0} | {:<8.0} {:<8.0} | {:<8.0} {:<8.0} | {:<8.0}",
            gc.s0u, gc.s1u, gc.eu, gc.ec, gc.ou, gc.oc, gc.mu, gc.mc, gc.ccsu
        );

        println!("\n{:<6} {:<8} | {:<6} {:<8} | {:<8} | {:<20}", 
            "YGC", "YGCT", "FGC", "FGCT", "GCT", "Last Cause");
        println!("{:<6} {:<8.3} | {:<6} {:<8.3} | {:<8.3} | {:<20}", 
            gc.ygc, gc.ygct, gc.fgc, gc.fgct, gc.gct, gc.lgcc
        );

        println!("\n[Class & JIT]");
        println!("{:<10} {:<10} | {:<10} {:<10} {:<10} | {:<10} {:<8}", 
            "Loaded", "Unloaded", "Compiled", "Failed", "Invalid", "CodeCache", "Util%");
        println!("{:<10} {:<10} | {:<10} {:<10} {:<10} | {:<10.0} {:<8.1}%", 
            cls.loaded, cls.unloaded, comp.compiled, comp.failed, comp.invalid, rt.code_cache_used, rt.code_cache_utilization * 100.0
        );

        println!("===============================================================================");
        
        io::stdout().flush().unwrap();
        thread::sleep(Duration::from_secs(interval));
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