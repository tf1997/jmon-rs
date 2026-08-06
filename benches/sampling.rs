use jmon_rs::{BuiltinLongMetric, JvmMonitor};
use std::hint::black_box;
use std::time::Instant;

fn main() {
    let Some(pid) = std::env::args()
        .skip(1)
        .find_map(|value| value.parse::<u32>().ok())
    else {
        eprintln!("sampling benchmark skipped; usage: cargo bench --bench sampling -- <pid>");
        return;
    };
    let iterations = std::env::var("JMON_BENCH_ITERS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1_000_000);

    let connect_started = Instant::now();
    let monitor = JvmMonitor::connect(pid).expect("connect target JVM");
    println!(
        "initial_connect_ns={}",
        connect_started.elapsed().as_nanos()
    );
    let ticks = monitor
        .metric_handle("sun.os.hrt.ticks")
        .expect("resolve hrt ticks");

    measure("read_long", iterations, || {
        black_box(monitor.read_long("sun.os.hrt.ticks"));
    });
    measure("read_long_handle", iterations, || {
        black_box(monitor.read_long_handle(ticks));
    });
    measure("read_builtin_long", iterations, || {
        black_box(monitor.read_builtin_long(BuiltinLongMetric::OldUsed));
    });

    measure("numeric_getters", iterations, || {
        black_box(monitor.get_gc_numeric_stats());
        black_box(monitor.get_runtime_stats());
        black_box(monitor.get_class_stats());
        black_box(monitor.get_compiler_numeric_stats());
    });

    measure("all_getters", iterations, || {
        black_box(monitor.get_gc_stats());
        black_box(monitor.get_runtime_stats());
        black_box(monitor.get_class_stats());
        black_box(monitor.get_compiler_stats());
    });

    let checked_iterations = (iterations / 100).max(1_000);
    measure("checked_sample", checked_iterations, || {
        black_box(monitor.sample().expect("checked sample"));
    });

    let reconnect_iterations = (iterations / 1_000).clamp(100, 10_000);
    measure("connect_drop", reconnect_iterations, || {
        black_box(JvmMonitor::connect(pid).expect("reconnect target JVM"));
    });
}

fn measure(name: &str, iterations: u64, mut operation: impl FnMut()) {
    let started = Instant::now();
    for _ in 0..iterations {
        operation();
    }
    let elapsed = started.elapsed();
    let nanos_per_operation = elapsed.as_nanos() as f64 / iterations as f64;
    println!("{name}_iterations={iterations} {name}_ns_per_op={nanos_per_operation:.2}");
}
