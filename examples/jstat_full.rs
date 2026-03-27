use jmon_rs::{JvmMonitor, PerfValue};
use std::env;
use std::thread;
use std::time::Duration;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: cargo run --example jstat_full <pid>");
        return;
    }
    
    let pid: u32 = args[1].parse().expect("PID must be a number");

    // Connect
    let monitor = match JvmMonitor::connect(pid) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Error: {}", e);
            return;
        }
    };

    println!("Target JVM PID: {}", pid);
    if let Some(PerfValue::String(vm_name)) = monitor.read_metric("java.property.java.vm.name") {
        println!("VM: {}", vm_name);
    }
    println!("--------------------------------------------------");

    loop {

        let cls = monitor.get_class_stats();
        let comp = monitor.get_compiler_stats();
        let gc = monitor.get_gc_stats();
        let rt = monitor.get_runtime_stats();

        println!("\n[Threads & CodeCache]");
        println!("{:<10} {:<10} {:<10} | {:<12} {:<12} {:<10}", 
            "Live", "Daemon", "Peak", "CC Used(KB)", "CC Max(KB)", "Util%");
        println!("{:<10} {:<10} {:<10} | {:<12.1} {:<12.1} {:<10.1}%", 
            rt.threads_live, rt.threads_daemon, rt.threads_peak,
            rt.code_cache_used, rt.code_cache_capacity, rt.code_cache_utilization * 100.0);

        println!("\n[Safepoints (STW Analysis)]");
        println!("{:<10} {:<12} {:<12} {:<12}", 
            "Count", "SP Time(s)", "App Time(s)", "Overhead%");
        println!("{:<10} {:<12.3} {:<12.3} {:<12.4}%", 
            rt.safepoints, rt.safepoint_time_s, rt.app_time_s, rt.safepoint_overhead * 100.0);

        println!("\n[Class Loading]");
        println!("{:<10} {:<10} {:<10} {:<10} {:<10}", "Loaded", "Bytes(KB)", "Unloaded", "Bytes(KB)", "Time(s)");
        println!("{:<10} {:<10.1} {:<10} {:<10.1} {:<10.3}", 
            cls.loaded, cls.bytes, cls.unloaded, cls.unloaded_bytes, cls.time);

        println!("\n[JIT Compiler]");
        println!("{:<10} {:<10} {:<10} {:<10} {:<20}", "Compiled", "Failed", "Invalid", "Time(s)", "LastFailed");
        println!("{:<10} {:<10} {:<10} {:<10.3} {:<20}", 
            comp.compiled, comp.failed, comp.invalid, comp.time, comp.failed_type);

        println!("\n[Garbage Collection]");
        println!("{:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8}", 
            "S0C", "S1C", "S0U", "S1U", "EC", "EU", "OC", "OU", "MC");
        println!("{:<8.1} {:<8.1} {:<8.1} {:<8.1} {:<8.1} {:<8.1} {:<8.1} {:<8.1} {:<8.1}",
            gc.s0c, gc.s1c, gc.s0u, gc.s1u, gc.ec, gc.eu, gc.oc, gc.ou, gc.mc);
            
        println!("\n{:<6} {:<8} {:<6} {:<8} {:<8} {:<20} {:<20}", 
            "YGC", "YGCT", "FGC", "FGCT", "GCT", "LGCC", "GCC");
        println!("{:<6} {:<8.3} {:<6} {:<8.3} {:<8.3} {:<20} {:<20}", 
            gc.ygc, gc.ygct, gc.fgc, gc.fgct, gc.gct, gc.lgcc, gc.gcc);

        println!("--------------------------------------------------");
        thread::sleep(Duration::from_secs(2));
    }
}