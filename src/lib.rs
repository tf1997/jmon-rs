use byteorder::{BigEndian, ByteOrder, LittleEndian};
use memmap2::{Mmap, MmapOptions};
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::PathBuf;

/// Error types
#[derive(Debug)]
pub enum JvmMonitorError {
    ProcessNotFound(u32),
    IoError(std::io::Error),
    InvalidFormat(String),
}

impl fmt::Display for JvmMonitorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JvmMonitorError::ProcessNotFound(pid) => write!(f, "JVM process {} not found or access denied", pid),
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

/// Raw value type from hsperfdata
#[derive(Debug, Clone)]
pub enum PerfValue {
    Long(i64),
    String(String),
}

/// Internal metadata for O(1) reads
#[derive(Debug)]
struct EntryMeta {
    data_type: u8,
    data_offset: usize,
    vector_length: usize,
}

/// The main JVM Monitor instance
pub struct JvmMonitor {
    mmap: Mmap,
    is_little_endian: bool,
    index: HashMap<String, EntryMeta>,
    timer_frequency: f64, // Cached frequency for time conversion
}

impl JvmMonitor {
    /// Connects to a running JVM process
    pub fn connect(pid: u32) -> Result<Self, JvmMonitorError> {
        let path = Self::find_hsperfdata_file(pid)
            .ok_or(JvmMonitorError::ProcessNotFound(pid))?;

        let file = fs::File::open(&path)?;
        let mmap = unsafe { MmapOptions::new().map(&file)? };

        if mmap.len() < 32 {
            return Err(JvmMonitorError::InvalidFormat("File too small".into()));
        }

        let magic = BigEndian::read_u32(&mmap[0..4]);
        if magic != 0xcafec0c0 {
            return Err(JvmMonitorError::InvalidFormat("Bad magic number".into()));
        }

        let is_little_endian = mmap[4] == 1;
        let entry_offset = Self::read_u32(&mmap[24..28], is_little_endian) as usize;
        let num_entries = Self::read_u32(&mmap[28..32], is_little_endian) as usize;

        let mut index = HashMap::with_capacity(num_entries);
        let mut current_offset = entry_offset;

        for _ in 0..num_entries {
            if current_offset + 20 > mmap.len() { break; }
            let entry_length = Self::read_u32(&mmap[current_offset..current_offset + 4], is_little_endian) as usize;
            let name_offset = Self::read_u32(&mmap[current_offset + 4..current_offset + 8], is_little_endian) as usize;
            let vector_length = Self::read_u32(&mmap[current_offset + 8..current_offset + 12], is_little_endian) as usize;
            let data_type = mmap[current_offset + 12];
            let data_offset = Self::read_u32(&mmap[current_offset + 16..current_offset + 20], is_little_endian) as usize;

            let name_start = current_offset + name_offset;
            let mut name_end = name_start;
            while name_end < mmap.len() && mmap[name_end] != 0 {
                name_end += 1;
            }
            let name = String::from_utf8_lossy(&mmap[name_start..name_end]).into_owned();

            index.insert(name, EntryMeta {
                data_type,
                data_offset: current_offset + data_offset,
                vector_length,
            });

            current_offset += entry_length;
        }

        let mut monitor = Self { mmap, is_little_endian, index, timer_frequency: 0.0 };
        // Cache the timer frequency for time conversions
        monitor.timer_frequency = monitor.read_long("sun.os.hrt.frequency") as f64;
        Ok(monitor)
    }

    /// Read raw metric value
    pub fn read_metric(&self, key: &str) -> Option<PerfValue> {
        let meta = self.index.get(key)?;
        let start = meta.data_offset;

        if meta.data_type == b'J' && start + 8 <= self.mmap.len() {
            let val = Self::read_i64(&self.mmap[start..start + 8], self.is_little_endian);
            Some(PerfValue::Long(val))
        } else if meta.data_type == b'B' && meta.vector_length > 0 && start + meta.vector_length <= self.mmap.len() {
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

    // Helper: Read Long (i64), return 0 if missing
    pub fn read_long(&self, key: &str) -> i64 {
        if let Some(PerfValue::Long(v)) = self.read_metric(key) { v } else { 0 }
    }

    // Helper: Read Float (f64), return 0.0 if missing
    pub fn read_f64(&self, key: &str) -> f64 {
        self.read_long(key) as f64
    }

    // Helper: Read String, return "-" if missing
    pub fn read_string(&self, key: &str) -> String {
        if let Some(PerfValue::String(v)) = self.read_metric(key) { v } else { "-".to_string() }
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

    /// Get Class Loading Statistics (jstat -class)
    pub fn get_class_stats(&self) -> ClassStats {
        ClassStats {
            loaded: self.read_long("java.cls.loadedClasses"),
            bytes: self.to_kb(self.read_long("java.cls.loadedBytes")),
            unloaded: self.read_long("java.cls.unloadedClasses"),
            unloaded_bytes: self.to_kb(self.read_long("java.cls.unloadedBytes")),
            time: self.to_seconds(self.read_long("sun.cls.time")),
        }
    }

    /// Get JIT Compiler Statistics (jstat -compiler)
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

    /// Get Full GC Statistics (jstat -gc / -gccause / -gcutil)
    pub fn get_gc_stats(&self) -> GcStats {
        let s0c = self.read_long("sun.gc.generation.0.space.1.capacity");
        let s1c = self.read_long("sun.gc.generation.0.space.2.capacity");
        let s0u = self.read_long("sun.gc.generation.0.space.1.used");
        let s1u = self.read_long("sun.gc.generation.0.space.2.used");
        let ec = self.read_long("sun.gc.generation.0.space.0.capacity");
        let eu = self.read_long("sun.gc.generation.0.space.0.used");
        
        let oc = self.read_long("sun.gc.generation.1.space.0.capacity");
        let ou = self.read_long("sun.gc.generation.1.space.0.used");

        let mc = self.read_long("sun.gc.metaspace.capacity");
        let mu = self.read_long("sun.gc.metaspace.used");
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
    

    fn read_u32(bytes: &[u8], is_le: bool) -> u32 {
        if is_le { LittleEndian::read_u32(bytes) } else { BigEndian::read_u32(bytes) }
    }

    fn read_i64(bytes: &[u8], is_le: bool) -> i64 {
        if is_le { LittleEndian::read_i64(bytes) } else { BigEndian::read_i64(bytes) }
    }

    // Platform Specific Logic
    #[cfg(target_os = "linux")]
    fn find_hsperfdata_file(pid: u32) -> Option<PathBuf> {
        Self::scan_temp_dir(PathBuf::from("/tmp"), pid)
    }

    #[cfg(target_os = "macos")]
    fn find_hsperfdata_file(pid: u32) -> Option<PathBuf> {
        Self::scan_temp_dir(std::env::temp_dir(), pid)
    }

    #[cfg(target_os = "windows")]
    fn find_hsperfdata_file(pid: u32) -> Option<PathBuf> {
        Self::scan_temp_dir(std::env::temp_dir(), pid)
    }

    fn scan_temp_dir(base_dir: PathBuf, pid: u32) -> Option<PathBuf> {
        let pid_str = pid.to_string();
        if let Ok(entries) = fs::read_dir(&base_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if let Some(folder_name) = path.file_name().and_then(|n| n.to_str()) {
                        if folder_name.starts_with("hsperfdata_") {
                            let target_file = path.join(&pid_str);
                            if target_file.exists() {
                                return Some(target_file);
                            }
                        }
                    }
                }
            }
        }
        None
    }
}

// ================= Data Structures =================

/// jstat -class
#[derive(Debug, Clone, Default)]
pub struct ClassStats {
    pub loaded: i64,
    pub bytes: f64, // KB
    pub unloaded: i64,
    pub unloaded_bytes: f64, // KB
    pub time: f64, // Seconds
}

/// jstat -compiler
#[derive(Debug, Clone, Default)]
pub struct CompilerStats {
    pub compiled: i64,
    pub failed: i64,
    pub invalid: i64,
    pub time: f64, // Seconds
    pub failed_type: String,
    pub failed_method: String,
}

/// jstat -gc / -gccause
#[derive(Debug, Clone, Default)]
pub struct GcStats {
    // Survivor
    pub s0c: f64, pub s1c: f64, pub s0u: f64, pub s1u: f64,
    // Eden
    pub ec: f64, pub eu: f64,
    // Old
    pub oc: f64, pub ou: f64,
    // Metaspace / Compressed Class
    pub mc: f64, pub mu: f64, pub ccsc: f64, pub ccsu: f64,
    // Events
    pub ygc: u64, pub ygct: f64,
    pub fgc: u64, pub fgct: f64,
    pub cgc: u64, pub cgct: f64, // Concurrent GC (ZGC/Shenandoah)
    pub gct: f64,
    // Causes
    pub lgcc: String, // Last GC Cause
    pub gcc: String,  // Current GC Cause
}