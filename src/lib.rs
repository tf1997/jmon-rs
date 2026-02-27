use byteorder::{BigEndian, ByteOrder, LittleEndian};
use memmap2::{Mmap, MmapOptions};
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::PathBuf;

/// Error types for JVM monitoring
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

/// The value of a performance metric
#[derive(Debug, Clone)]
pub enum PerfValue {
    Long(i64),
    String(String),
}

/// Internal metadata for a metric to allow O(1) live memory reads
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
}

impl JvmMonitor {
    /// Connects to a running JVM process and parses the hsperfdata index.
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

        // Parse offsets and build index (done only once)
        for _ in 0..num_entries {
            if current_offset + 20 > mmap.len() { break; }

            let entry_length = Self::read_u32(&mmap[current_offset..current_offset + 4], is_little_endian) as usize;
            let name_offset = Self::read_u32(&mmap[current_offset + 4..current_offset + 8], is_little_endian) as usize;
            let vector_length = Self::read_u32(&mmap[current_offset + 8..current_offset + 12], is_little_endian) as usize;
            let data_type = mmap[current_offset + 12];
            let data_offset = Self::read_u32(&mmap[current_offset + 16..current_offset + 20], is_little_endian) as usize;

            // Extract Name
            let name_start = current_offset + name_offset;
            let mut name_end = name_start;
            while name_end < mmap.len() && mmap[name_end] != 0 {
                name_end += 1;
            }
            let name = String::from_utf8_lossy(&mmap[name_start..name_end]).into_owned();

            // Store metadata for live reading
            index.insert(name, EntryMeta {
                data_type,
                data_offset: current_offset + data_offset,
                vector_length,
            });

            current_offset += entry_length;
        }

        Ok(Self { mmap, is_little_endian, index })
    }

    /// Reads the live value of a specific metric
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

    /// Helper to directly read a Long (f64 for math) value, defaults to 0.0
    pub fn read_f64(&self, key: &str) -> f64 {
        if let Some(PerfValue::Long(v)) = self.read_metric(key) {
            v as f64
        } else {
            0.0
        }
    }

    /// Returns a structured snapshot of current GC metrics (similar to jstat -gc)
    pub fn get_gc_metrics(&self) -> GcMetricsSnapshot {
        let to_kb = |bytes: f64| bytes / 1024.0;
        let hz = self.read_f64("sun.os.hrt.frequency");
        let to_sec = |ticks: f64| if hz > 0.0 { ticks / hz } else { 0.0 };

        GcMetricsSnapshot {
            s0c: to_kb(self.read_f64("sun.gc.generation.0.space.1.capacity")),
            s1c: to_kb(self.read_f64("sun.gc.generation.0.space.2.capacity")),
            s0u: to_kb(self.read_f64("sun.gc.generation.0.space.1.used")),
            s1u: to_kb(self.read_f64("sun.gc.generation.0.space.2.used")),
            ec: to_kb(self.read_f64("sun.gc.generation.0.space.0.capacity")),
            eu: to_kb(self.read_f64("sun.gc.generation.0.space.0.used")),
            oc: to_kb(self.read_f64("sun.gc.generation.1.space.0.capacity")),
            ou: to_kb(self.read_f64("sun.gc.generation.1.space.0.used")),
            mc: to_kb(self.read_f64("sun.gc.metaspace.capacity")),
            mu: to_kb(self.read_f64("sun.gc.metaspace.used")),
            ccsc: to_kb(self.read_f64("sun.gc.compressedclassspace.capacity")),
            ccsu: to_kb(self.read_f64("sun.gc.compressedclassspace.used")),
            ygc: self.read_f64("sun.gc.collector.0.invocations") as u64,
            ygct: to_sec(self.read_f64("sun.gc.collector.0.time")),
            fgc: self.read_f64("sun.gc.collector.1.invocations") as u64,
            fgct: to_sec(self.read_f64("sun.gc.collector.1.time")),
            gct: to_sec(self.read_f64("sun.gc.collector.0.time") + self.read_f64("sun.gc.collector.1.time")),
        }
    }

    /// Lists all available metric keys in this JVM
    pub fn get_available_keys(&self) -> Vec<String> {
        self.index.keys().cloned().collect()
    }

    #[cfg(target_os = "linux")]
    fn find_hsperfdata_file(pid: u32) -> Option<PathBuf> {
        Self::scan_temp_dir(PathBuf::from("/tmp"), pid)
    }

    #[cfg(target_os = "macos")]
    fn find_hsperfdata_file(pid: u32) -> Option<PathBuf> {
        Self::scan_temp_dir(std::env::temp_dir(), pid)
    }

    #[cfg(windows)]
    fn find_hsperfdata_file(pid: u32) -> Option<PathBuf> {
        Self::scan_temp_dir(std::env::temp_dir(), pid)
    }

    fn scan_temp_dir(base_dir: PathBuf, pid: u32) -> Option<PathBuf> {
        let pid_str = pid.to_string();
        if let Ok(entries) = fs::read_dir(base_dir) {
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

    fn read_u32(bytes: &[u8], is_le: bool) -> u32 {
        if is_le { LittleEndian::read_u32(bytes) } else { BigEndian::read_u32(bytes) }
    }

    fn read_i64(bytes: &[u8], is_le: bool) -> i64 {
        if is_le { LittleEndian::read_i64(bytes) } else { BigEndian::read_i64(bytes) }
    }
}

/// A structured snapshot of GC metrics (in KB and Seconds)
#[derive(Debug, Clone, Default)]
pub struct GcMetricsSnapshot {
    pub s0c: f64, pub s1c: f64, pub s0u: f64, pub s1u: f64,
    pub ec: f64, pub eu: f64, pub oc: f64, pub ou: f64,
    pub mc: f64, pub mu: f64, pub ccsc: f64, pub ccsu: f64,
    pub ygc: u64, pub ygct: f64,
    pub fgc: u64, pub fgct: f64,
    pub gct: f64,
}