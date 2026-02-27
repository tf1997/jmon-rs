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
        // 1. 获取 Class 指标
        let cls = monitor.get_class_stats();
        // 2. 获取 Compiler 指标
        let comp = monitor.get_compiler_stats();
        // 3. 获取 GC 指标
        let gc = monitor.get_gc_stats();

        // 模拟 jstat -class 输出
        println!("\n[Class Loading]");
        println!("{:<10} {:<10} {:<10} {:<10} {:<10}", "Loaded", "Bytes(KB)", "Unloaded", "Bytes(KB)", "Time(s)");
        println!("{:<10} {:<10.1} {:<10} {:<10.1} {:<10.3}", 
            cls.loaded, cls.bytes, cls.unloaded, cls.unloaded_bytes, cls.time);

        // 模拟 jstat -compiler 输出
        println!("\n[JIT Compiler]");
        println!("{:<10} {:<10} {:<10} {:<10} {:<20}", "Compiled", "Failed", "Invalid", "Time(s)", "LastFailed");
        println!("{:<10} {:<10} {:<10} {:<10.3} {:<20}", 
            comp.compiled, comp.failed, comp.invalid, comp.time, comp.failed_type);

        // 模拟 jstat -gc / -gccause 输出
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