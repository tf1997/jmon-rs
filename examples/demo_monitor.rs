use jmon_rs::{JvmMonitor, PerfValue};
use std::env;
use std::thread;
use std::time::Duration;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: cargo run --example demo_monitor <pid>");
        return;
    }
    
    let pid: u32 = args[1].parse().expect("PID must be a number");

    // 1. Initialize connection (Only parses offsets once!)
    let monitor = match JvmMonitor::connect(pid) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Failed to connect to JVM: {}", e);
            return;
        }
    };

    // (Optional) Read some basic JVM info
    if let Some(PerfValue::String(cmd)) = monitor.read_metric("sun.rt.javaCommand") {
        println!("Connected to JVM Command: {}", cmd);
    }
    
    println!("{:<8} {:<8} {:<8} {:<8}", "EU (KB)", "OU (KB)", "YGC", "FGC");
    println!("-----------------------------------------");

    // 2. Continuous Live Monitoring loop
    for _ in 0..10 { // Monitor for 10 seconds as an example
        // Fetch structured GC metrics instantly via zero-copy read
        let gc = monitor.get_gc_stats();
        
        println!("{:<8.1} {:<8.1} {:<8} {:<8}", 
            gc.eu, gc.ou, gc.ygc, gc.fgc
        );

        thread::sleep(Duration::from_secs(1));
    }
}