//! # JMon-rs
//!
//! `jmon-rs` is a high-performance, cross-platform JVM monitoring library.
//! It retrieves real-time JVM metrics (GC, class loading, memory, etc.) by
//! parsing `hsperfdata` memory-mapped files.
//!
//! ## Example
//!
//! ```rust,no_run
//! use jmon_rs::JvmMonitor;
//!
//! let pid = 12345;
//! let monitor = JvmMonitor::connect(pid).expect("Failed to connect");
//! let gc = monitor.get_gc_stats();
//! println!("Eden Used: {} KB", gc.eu);
//! ```

use byteorder::{BigEndian, ByteOrder, LittleEndian};
use memmap2::{Mmap, MmapOptions};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::PathBuf;

/// Error types for JVM Monitoring operations.
#[derive(Debug)]
pub enum JvmMonitorError {
    /// The specified JVM process ID was not found or access was denied.
    ProcessNotFound(u32),
    /// Standard I/O error occurred during file access or mapping.
    IoError(std::io::Error),
    /// The hsperfdata file format is invalid or corrupted.
    InvalidFormat(String),
}

impl fmt::Display for JvmMonitorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JvmMonitorError::ProcessNotFound(pid) => {
                write!(f, "JVM process {} not found or access denied", pid)
            }
            JvmMonitorError::IoError(e) => write!(f, "IO Error: {}", e),
            JvmMonitorError::InvalidFormat(msg) => write!(f, "Invalid hsperfdata format: {}", msg),
        }
    }
}

impl std::error::Error for JvmMonitorError {}

impl From<std::io::Error> for JvmMonitorError {
    fn from(err: std::io::Error) -> Self {
        JvmMonitorError::IoError(err)
    }
}

/// Represents a raw performance counter value from the JVM.
#[derive(Debug, Clone)]
pub enum PerfValue {
    /// A 64-bit integer value (e.g., counters, sizes, timestamps).
    Long(i64),
    /// A string value (e.g., version strings, GC causes).
    String(String),
}

/// Internal metadata for O(1) reads
#[derive(Debug)]
struct EntryMeta {
    data_type: u8,
    data_offset: usize,
    vector_length: usize,
}

/// Information about a discovered Java process, similar to the output of `jps`.
#[derive(Debug, Clone)]
pub struct JavaProcessInfo {
    /// The process ID (PID) of the JVM.
    pub pid: u32,
    /// Short name of the application (e.g., the Main class or JAR filename).
    pub name: String,
}

/// The main JVM Monitor instance.
///
/// Use `JvmMonitor::connect(pid)` to start monitoring a specific process,
/// or `JvmMonitor::discover_all()` to find all running JVMs.
pub struct JvmMonitor {
    mmap: Mmap,
    is_little_endian: bool,
    index: HashMap<String, EntryMeta>,
    timer_frequency: f64,
}

impl JvmMonitor {
    /// Connects to a running JVM process by its PID.
    ///
    /// This will attempt to find and memory-map the `hsperfdata` file for the given PID.
    ///
    /// # Errors
    /// Returns `JvmMonitorError::ProcessNotFound` if the process is not found.
    /// Returns `JvmMonitorError::InvalidFormat` if the data file is corrupted.
    pub fn connect(host_pid: u32) -> Result<Self, JvmMonitorError> {
        let path = Self::find_hsperfdata_file(host_pid)
            .ok_or(JvmMonitorError::ProcessNotFound(host_pid))?;

        let file = fs::File::open(&path)?;
        let mmap = unsafe { MmapOptions::new().map(&file)? };

        if mmap.len() < 32 || BigEndian::read_u32(&mmap[0..4]) != 0xcafec0c0 {
            return Err(JvmMonitorError::InvalidFormat(
                "Invalid magic number".into(),
            ));
        }

        let is_le = mmap[4] == 1;
        let entry_offset = Self::read_u32(&mmap[24..28], is_le) as usize;
        let num_entries = Self::read_u32(&mmap[28..32], is_le) as usize;

        let mut index = HashMap::with_capacity(num_entries);
        let mut cursor = entry_offset;

        for _ in 0..num_entries {
            if cursor + 20 > mmap.len() {
                break;
            }
            let entry_len = Self::read_u32(&mmap[cursor..cursor + 4], is_le) as usize;
            let name_offset = Self::read_u32(&mmap[cursor + 4..cursor + 8], is_le) as usize;
            let vector_len = Self::read_u32(&mmap[cursor + 8..cursor + 12], is_le) as usize;
            let data_type = mmap[cursor + 12];
            let data_offset = Self::read_u32(&mmap[cursor + 16..cursor + 20], is_le) as usize;

            let n_start = cursor + name_offset;
            let mut n_end = n_start;
            while n_end < mmap.len() && mmap[n_end] != 0 {
                n_end += 1;
            }
            let name = String::from_utf8_lossy(&mmap[n_start..n_end]).into_owned();

            index.insert(
                name,
                EntryMeta {
                    data_type,
                    data_offset: cursor + data_offset,
                    vector_length: vector_len,
                },
            );
            cursor += entry_len;
        }

        let mut monitor = Self {
            mmap,
            is_little_endian: is_le,
            index,
            timer_frequency: 0.0,
        };
        monitor.timer_frequency = monitor.read_long("sun.os.hrt.frequency") as f64;
        Ok(monitor)
    }

    /// Discovers all Java processes. Supports Host and Container (Docker/K8s) PIDs.
    pub fn discover_all() -> Result<Vec<JavaProcessInfo>, JvmMonitorError> {
        let mut processes = Vec::new();
        let mut seen_host_pids = HashSet::new();

        // --- 1. Linux Specific: Container discovery via /proc ---
        #[cfg(target_os = "linux")]
        {
            if let Ok(entries) = fs::read_dir("/proc") {
                for entry_result in entries {
                    let entry = match entry_result {
                        Ok(e) => e,
                        Err(_) => continue,
                    };
                    let pid_str = entry.file_name().to_string_lossy().into_owned();
                    if pid_str.chars().all(|c| c.is_ascii_digit()) {
                        let host_pid: u32 = pid_str.parse().unwrap_or(0);
                        if host_pid == 0 || seen_host_pids.contains(&host_pid) {
                            continue;
                        }

                        if let Some(ns_pid) = Self::get_ns_pid(host_pid) {
                            let container_tmp =
                                PathBuf::from("/proc").join(&pid_str).join("root/tmp");
                            if let Some(path) = Self::find_perf_file_in_dir(&container_tmp, ns_pid)
                            {
                                if let Some(name) = Self::fast_extract_name(&path) {
                                    processes.push(JavaProcessInfo {
                                        pid: host_pid,
                                        name,
                                    });
                                    seen_host_pids.insert(host_pid);
                                }
                            }
                        }
                    }
                }
            }
        }

        // --- 2. Windows/macOS/Linux Host: Direct path and fallback scan ---
        let base_tmp = Self::get_temp_root();

        // Fast path: Target current user folder directly to avoid large directory scans (especially on Windows)
        let user_env = if cfg!(windows) { "USERNAME" } else { "USER" };
        if let Ok(user) = std::env::var(user_env) {
            let user_dir = base_tmp.join(format!("hsperfdata_{}", user));
            Self::scan_pids_in_folder(&user_dir, &mut processes, &mut seen_host_pids);
        }

        // Fallback: Scan base temp directory for other users' hsperfdata folders
        if processes.is_empty() {
            if let Ok(entries) = fs::read_dir(&base_tmp) {
                for entry_result in entries {
                    let entry = match entry_result {
                        Ok(e) => e,
                        Err(_) => continue,
                    };
                    let path = entry.path();
                    if path.is_dir()
                        && path
                            .file_name()
                            .map_or(false, |n| n.to_string_lossy().starts_with("hsperfdata_"))
                    {
                        Self::scan_pids_in_folder(&path, &mut processes, &mut seen_host_pids);
                    }
                }
            }
        }

        processes.sort_by_key(|p| p.pid);
        Ok(processes)
    }

    fn scan_pids_in_folder(
        folder: &PathBuf,
        results: &mut Vec<JavaProcessInfo>,
        seen: &mut HashSet<u32>,
    ) {
        if let Ok(p_entries) = fs::read_dir(folder) {
            for p_entry_result in p_entries {
                let p_entry = match p_entry_result {
                    Ok(entry) => entry,
                    Err(_) => continue,
                };

                if let Ok(pid) = p_entry.file_name().to_string_lossy().parse::<u32>() {
                    if !seen.contains(&pid) {
                        if let Some(name) = Self::fast_extract_name(&p_entry.path()) {
                            results.push(JavaProcessInfo { pid, name });
                            seen.insert(pid);
                        }
                    }
                }
            }
        }
    }

    // ==========================================
    // Internal Path and PID Resolution
    // ==========================================

    #[cfg(target_os = "linux")]
    fn get_ns_pid(host_pid: u32) -> Option<u32> {
        use std::io::{BufRead, BufReader};
        let file = fs::File::open(format!("/proc/{}/status", host_pid)).ok()?;
        let reader = BufReader::new(file);
        for line_result in reader.lines() {
            match line_result {
                Ok(line) => {
                    if line.starts_with("NSpid:") {
                        return line.split_whitespace().last().and_then(|s| s.parse().ok());
                    }
                }
                Err(_) => break,
            }
        }
        None
    }

    fn find_hsperfdata_file(host_pid: u32) -> Option<PathBuf> {
        #[cfg(target_os = "linux")]
        {
            let ns_pid = Self::get_ns_pid(host_pid).unwrap_or(host_pid);
            let container_tmp = PathBuf::from("/proc")
                .join(host_pid.to_string())
                .join("root/tmp");
            if let Some(p) = Self::find_perf_file_in_dir(&container_tmp, ns_pid) {
                return Some(p);
            }
        }

        let base_tmp = Self::get_temp_root();
        Self::find_perf_file_in_dir(&base_tmp, host_pid)
    }

    fn find_perf_file_in_dir(base_path: &PathBuf, target_pid: u32) -> Option<PathBuf> {
        let pid_str = target_pid.to_string();
        let entries = fs::read_dir(base_path).ok()?;
        for entry_result in entries {
            let entry = match entry_result {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            if path.is_dir()
                && path
                    .file_name()
                    .map_or(false, |n| n.to_string_lossy().starts_with("hsperfdata_"))
            {
                let perf_file = path.join(&pid_str);
                if perf_file.exists() {
                    return Some(perf_file);
                }
            }
        }
        None
    }

    fn fast_extract_name(path: &PathBuf) -> Option<String> {
        let file = fs::File::open(path).ok()?;
        let mmap = unsafe { MmapOptions::new().map(&file).ok()? };
        if mmap.len() < 32 || &mmap[0..4] != &[0xca, 0xfe, 0xc0, 0xc0] {
            return None;
        }

        let is_le = mmap[4] == 1;
        let entry_offset = Self::read_u32(&mmap[24..28], is_le) as usize;
        let num_entries = Self::read_u32(&mmap[28..32], is_le) as usize;
        let mut cursor = entry_offset;
        let target = b"sun.rt.javaCommand";

        for _ in 0..num_entries {
            if cursor + 20 > mmap.len() {
                break;
            }
            let entry_len = Self::read_u32(&mmap[cursor..cursor + 4], is_le) as usize;
            let name_offset = Self::read_u32(&mmap[cursor + 4..cursor + 8], is_le) as usize;
            let data_offset = Self::read_u32(&mmap[cursor + 16..cursor + 20], is_le) as usize;

            let n_start = cursor + name_offset;
            if n_start + 18 <= mmap.len() && &mmap[n_start..n_start + 18] == target {
                let d_start = cursor + data_offset;
                let mut d_end = d_start;
                while d_end < mmap.len() && mmap[d_end] != 0 && mmap[d_end] != b' ' {
                    d_end += 1;
                }
                return Some(String::from_utf8_lossy(&mmap[d_start..d_end]).into_owned());
            }
            cursor += entry_len;
        }
        None
    }

    /// Reads a raw performance counter value by its full internal name (e.g., "sun.gc.cause").
    ///
    /// Returns `None` if the key does not exist or the data type is unsupported.
    pub fn read_metric(&self, key: &str) -> Option<PerfValue> {
        let meta = self.index.get(key)?;
        let start = meta.data_offset;

        if meta.data_type == b'J' && start + 8 <= self.mmap.len() {
            let val = Self::read_i64(&self.mmap[start..start + 8], self.is_little_endian);
            Some(PerfValue::Long(val))
        } else if meta.data_type == b'B'
            && meta.vector_length > 0
            && start + meta.vector_length <= self.mmap.len()
        {
            let mut end = start;
            let limit = start + meta.vector_length;
            while end < limit && self.mmap[end] != 0 {
                end += 1;
            }
            let val = String::from_utf8_lossy(&self.mmap[start..end]).into_owned();
            Some(PerfValue::String(val))
        } else {
            None
        }
    }

    /// Reads a 64-bit integer metric. Returns `0` if the key is missing or not a long.
    pub fn read_long(&self, key: &str) -> i64 {
        if let Some(PerfValue::Long(v)) = self.read_metric(key) {
            v
        } else {
            0
        }
    }

    /// Reads a metric as a double-precision float.
    ///
    /// Note: Most JVM counters are stored as `i64`, this method casts them to `f64`.
    pub fn read_f64(&self, key: &str) -> f64 {
        self.read_long(key) as f64
    }

    /// Reads a string metric. Returns `"-"` if the key is missing or not a string.
    pub fn read_string(&self, key: &str) -> String {
        if let Some(PerfValue::String(v)) = self.read_metric(key) {
            v
        } else {
            "-".to_string()
        }
    }

    fn read_long_first_available(&self, candidates: &[&str]) -> i64 {
        for key in candidates {
            let val = self.read_long(key);
            if val > 0 {
                return val;
            }
        }
        0
    }

    // Helper: Convert Ticks to Seconds
    fn to_seconds(&self, ticks: i64) -> f64 {
        if self.timer_frequency > 0.0 {
            ticks as f64 / self.timer_frequency
        } else {
            0.0
        }
    }

    // Helper: Convert Bytes to KB
    fn to_kb(&self, bytes: i64) -> f64 {
        bytes as f64 / 1024.0
    }

    // ==========================================
    // Public API: High Level Stats
    // ==========================================

    /// Retrieves class loading statistics, equivalent to `jstat -class`.
    pub fn get_class_stats(&self) -> ClassStats {
        ClassStats {
            loaded: self.read_long("sun.cls.loadedClasses"),
            bytes: self.to_kb(self.read_long("sun.cls.loadedBytes")),
            unloaded: self.read_long("sun.cls.unloadedClasses"),
            unloaded_bytes: self.to_kb(self.read_long("sun.cls.unloadedBytes")),
            time: self.to_seconds(self.read_long("sun.cls.time")),
        }
    }

    /// Retrieves JIT compiler statistics, equivalent to `jstat -compiler`.
    pub fn get_compiler_stats(&self) -> CompilerStats {
        CompilerStats {
            compiled: self.read_long("sun.ci.totalCompilations"),
            failed: self.read_long("sun.ci.totalBailouts"),
            invalid: self.read_long("sun.ci.totalInvalidations"),
            time: self.to_seconds(self.read_long("sun.ci.totalTime")),
            failed_type: self.read_string("sun.ci.lastFailedType"),
            failed_method: self.read_string("sun.ci.lastFailedMethod"),
        }
    }

    /// Retrieves garbage collection statistics, equivalent to `jstat -gc` or `jstat -gccause`.
    pub fn get_gc_stats(&self) -> GcStats {
        let s0c = self.read_long_first_available(&[
            "sun.gc.generation.0.space.1.capacity",
            "sun.gc.generation.0.space.1.maxCapacity",
        ]);
        let s1c = self.read_long_first_available(&[
            "sun.gc.generation.0.space.2.capacity",
            "sun.gc.generation.0.space.2.maxCapacity",
        ]);
        let s0u = self.read_long("sun.gc.generation.0.space.1.used");
        let s1u = self.read_long("sun.gc.generation.0.space.2.used");
        let ec = self.read_long_first_available(&[
            "sun.gc.generation.0.space.0.capacity",
            "sun.gc.generation.0.capacity",
        ]);
        let eu = self.read_long("sun.gc.generation.0.space.0.used");

        let oc = self.read_long_first_available(&[
            "sun.gc.generation.1.space.0.capacity",
            "sun.gc.generation.1.capacity",
            "sun.gc.g1.old.capacity",
        ]);
        let ou = self.read_long_first_available(&[
            "sun.gc.generation.1.space.0.used",
            "sun.gc.generation.1.used",
            "sun.gc.g1.old.used",
        ]);

        let mc = self.read_long_first_available(&[
            "sun.gc.metaspace.capacity",
            "sun.gc.generation.2.space.0.capacity",
            "sun.gc.generation.2.capacity",
        ]);
        let mu = self.read_long_first_available(&[
            "sun.gc.metaspace.used",
            "sun.gc.generation.2.space.0.used",
            "sun.gc.generation.2.used",
        ]);
        let ccsc = self.read_long("sun.gc.compressedclassspace.capacity");
        let ccsu = self.read_long("sun.gc.compressedclassspace.used");

        let ygc = self.read_long("sun.gc.collector.0.invocations");
        let ygct = self.read_long("sun.gc.collector.0.time");
        let fgc = self.read_long("sun.gc.collector.1.invocations");
        let fgct = self.read_long("sun.gc.collector.1.time");

        // ZGC or Shenandoah usually map to collector.2
        let cgc = self.read_long("sun.gc.collector.2.invocations");
        let cgct = self.read_long("sun.gc.collector.2.time");

        GcStats {
            s0c: self.to_kb(s0c),
            s1c: self.to_kb(s1c),
            s0u: self.to_kb(s0u),
            s1u: self.to_kb(s1u),
            ec: self.to_kb(ec),
            eu: self.to_kb(eu),
            oc: self.to_kb(oc),
            ou: self.to_kb(ou),
            mc: self.to_kb(mc),
            mu: self.to_kb(mu),
            ccsc: self.to_kb(ccsc),
            ccsu: self.to_kb(ccsu),
            ygc: ygc as u64,
            ygct: self.to_seconds(ygct),
            fgc: fgc as u64,
            fgct: self.to_seconds(fgct),
            cgc: cgc as u64,
            cgct: self.to_seconds(cgct),
            gct: self.to_seconds(ygct + fgct + cgct),
            lgcc: self.read_string("sun.gc.lastCause"),
            gcc: self.read_string("sun.gc.cause"),
        }
    }

    /// Retrieves various runtime statistics including threads, code cache, and safepoints.
    pub fn get_runtime_stats(&self) -> RuntimeStats {
        // 1. Threads
        let t_live = self.read_long("java.threads.live");
        let t_daemon = self.read_long("java.threads.daemon");
        let mut t_peak = self.read_long_first_available(&[
            "java.threads.peak",
            "java.threads.livePeak",
            "java.threads.peakCount",
        ]);
        if t_peak == 0 {
            t_peak = t_live;
        }

        // 2. Code Cache
        // note：sun.ci.codeCache or sun.ci.codeCache.maxSize
        let cc_used = self.to_kb(self.read_long_first_available(&[
            "sun.ci.codeCache.used",
            "sun.gc.generation.2.space.0.used",
            "java.ci.totalCodeSize",
        ]));
        let cc_cap = self.to_kb(self.read_long_first_available(&[
            "sun.ci.codeCache.capacity",
            "sun.ci.codeCache.maxCapacity",
            "sun.ci.codeCache.maxSize",
            "sun.gc.generation.2.space.0.capacity",
        ]));

        let cc_util = if cc_cap > 0.0 { cc_used / cc_cap } else { 0.0 };

        // 3. Safepoints
        let safepoint_ticks = self
            .read_long_first_available(&["sun.rt.safepointTime", "sun.threads.vmOperationTime"]);
        let app_ticks = self.read_long("sun.rt.applicationTime");
        let safepoints =
            self.read_long_first_available(&["sun.rt.safepoints", "java.rt.safepoints"]);

        let safepoint_time_s = self.to_seconds(safepoint_ticks);
        let app_time_s = self.to_seconds(app_ticks);

        let total_ticks = safepoint_ticks + app_ticks;
        let overhead = if total_ticks > 0 {
            safepoint_ticks as f64 / total_ticks as f64
        } else {
            0.0
        };

        RuntimeStats {
            threads_live: t_live,
            threads_daemon: t_daemon,
            threads_peak: t_peak,
            code_cache_used: cc_used,
            code_cache_capacity: cc_cap,
            code_cache_utilization: cc_util,
            safepoints,
            safepoint_time_s,
            app_time_s,
            safepoint_overhead: overhead,
        }
    }

    fn read_u32(bytes: &[u8], is_le: bool) -> u32 {
        if is_le {
            LittleEndian::read_u32(bytes)
        } else {
            BigEndian::read_u32(bytes)
        }
    }

    fn read_i64(bytes: &[u8], is_le: bool) -> i64 {
        if is_le {
            LittleEndian::read_i64(bytes)
        } else {
            BigEndian::read_i64(bytes)
        }
    }

    #[cfg(target_os = "linux")]
    fn get_temp_root() -> PathBuf {
        PathBuf::from("/tmp")
    }

    #[cfg(target_os = "macos")]
    fn get_temp_root() -> PathBuf {
        std::env::temp_dir()
    }

    #[cfg(target_os = "windows")]
    fn get_temp_root() -> PathBuf {
        std::env::temp_dir()
    }
}

// ================= Data Structures =================

/// Class loading statistics.
#[derive(Debug, Clone, Default)]
pub struct ClassStats {
    /// Number of classes loaded.
    pub loaded: i64,
    /// Total size of classes loaded (KB).
    pub bytes: f64,
    /// Number of classes unloaded.
    pub unloaded: i64,
    /// Total size of classes unloaded (KB).
    pub unloaded_bytes: f64,
    /// Time spent in class loading (seconds).
    pub time: f64,
}

/// JIT compiler statistics.
#[derive(Debug, Clone, Default)]
pub struct CompilerStats {
    /// Total number of compilations performed.
    pub compiled: i64,
    /// Total number of failed compilations.
    pub failed: i64,
    /// Total number of invalidated compilations.
    pub invalid: i64,
    /// Total time spent in compilation (seconds).
    pub time: f64,
    /// Type of the last failed compilation.
    pub failed_type: String,
    /// Name of the last failed method.
    pub failed_method: String,
}

/// Garbage Collection statistics.
#[derive(Debug, Clone, Default)]
pub struct GcStats {
    /// Survivor space 0 capacity (KB).
    pub s0c: f64,
    /// Survivor space 1 capacity (KB).
    pub s1c: f64,
    /// Survivor space 0 used (KB).
    pub s0u: f64,
    /// Survivor space 1 used (KB).
    pub s1u: f64,
    /// Eden space capacity (KB).
    pub ec: f64,
    /// Eden space used (KB).
    pub eu: f64,
    /// Old space capacity (KB).
    pub oc: f64,
    /// Old space used (KB).
    pub ou: f64,
    /// Metaspace capacity (KB).
    pub mc: f64,
    /// Metaspace used (KB).
    pub mu: f64,
    /// Compressed class space capacity (KB).
    pub ccsc: f64,
    /// Compressed class space used (KB).
    pub ccsu: f64,
    /// Number of young generation GC events.
    pub ygc: u64,
    /// Total time spent in young generation GC (seconds).
    pub ygct: f64,
    /// Number of full GC events.
    pub fgc: u64,
    /// Total time spent in full GC (seconds).
    pub fgct: f64,
    /// Number of concurrent GC events (e.g., ZGC, Shenandoah).
    pub cgc: u64,
    /// Total time spent in concurrent GC (seconds).
    pub cgct: f64,
    /// Total garbage collection time (seconds).
    pub gct: f64,
    /// Last GC cause.
    pub lgcc: String,
    /// Current GC cause.
    pub gcc: String,
}

/// JVM Runtime, Threads, and Safepoint statistics.
#[derive(Debug, Clone, Default)]
pub struct RuntimeStats {
    /// Number of live threads.
    pub threads_live: i64,
    /// Number of daemon threads.
    pub threads_daemon: i64,
    /// Peak number of threads.
    pub threads_peak: i64,

    /// Code Cache memory used (KB).
    pub code_cache_used: f64,
    /// Code Cache total capacity (KB).
    pub code_cache_capacity: f64,
    /// Code Cache utilization ratio (0.0 to 1.0).
    pub code_cache_utilization: f64,

    /// Total number of safepoints reached.
    pub safepoints: i64,
    /// Total time spent in safepoints (seconds).
    pub safepoint_time_s: f64,
    /// Total time spent running the application (seconds).
    pub app_time_s: f64,
    /// Percentage of time spent in safepoints (0.0 to 1.0).
    pub safepoint_overhead: f64,
}
