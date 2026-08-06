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
//!
//! Long-running collectors should reuse the monitor and call
//! [`JvmMonitor::sample`] or [`JvmMonitor::refresh`] once per sampling cycle so
//! target exit and newly-published counters are handled explicitly.

mod perfdata;
#[cfg(test)]
mod tests;

use perfdata::{EntryMeta, PerfHeader, PerfMemory, parse_snapshot};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, Metadata};
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, RwLock};
#[cfg(not(unix))]
use std::time::SystemTime;
use std::time::{Duration, Instant};

const ACCESSIBLE_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const ACCESSIBLE_POLL_INTERVAL: Duration = Duration::from_millis(20);
const MISSING_OFFSET: usize = usize::MAX;
const MAX_DISCOVERY_FILE_SIZE: u64 = 64 * 1024 * 1024;
static NEXT_MONITOR_ID: AtomicUsize = AtomicUsize::new(1);

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

/// Pre-resolved metric metadata for lock-free repeated reads.
///
/// Handles remain valid for the lifetime of the monitor that created them,
/// including after append-only index refreshes. Passing a handle to another
/// monitor safely returns `None`.
#[derive(Debug, Clone, Copy)]
pub struct MetricHandle {
    monitor_id: usize,
    metadata: EntryMeta,
}

/// Information about a discovered Java process, similar to the output of `jps`.
#[derive(Debug, Clone)]
pub struct JavaProcessInfo {
    /// The process ID (PID) of the JVM.
    pub pid: u32,
    /// Short name of the application (e.g., the Main class or JAR filename).
    pub name: String,
}

/// Health information read from the hsperfdata prologue.
///
/// Call [`JvmMonitor::refresh`] once per sampling cycle (or at a lower health
/// cadence) to validate the target identity and discover counters added after
/// the initial connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorHealth {
    /// Target host PID used for this connection.
    pub pid: u32,
    /// Bytes that did not fit in the target's perfdata buffer.
    pub overflow_bytes: u32,
    /// Number of published performance entries.
    pub num_entries: usize,
    /// Timestamp of the last structural modification, in target JVM ticks.
    pub modification_timestamp: i64,
}

/// A checked, coherent-at-the-sampling-boundary collection of the library's
/// high-level metric groups.
///
/// Individual JVM counters are still updated independently by HotSpot, so this
/// is not a transactional snapshot of all target values.
#[derive(Debug, Clone)]
pub struct JvmStatsSnapshot {
    /// Target and perfdata structure health observed before reading values.
    pub health: MonitorHealth,
    /// Garbage collection statistics.
    pub gc: GcStats,
    /// Runtime, thread, code-cache, and safepoint statistics.
    pub runtime: RuntimeStats,
    /// Class loading statistics.
    pub classes: ClassStats,
    /// JIT compiler statistics.
    pub compiler: CompilerStats,
}

#[derive(Debug, Clone)]
struct FileIdentity {
    len: u64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    created: Option<SystemTime>,
}

impl FileIdentity {
    fn from_metadata(metadata: &Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self {
                len: metadata.len(),
                device: metadata.dev(),
                inode: metadata.ino(),
            }
        }
        #[cfg(not(unix))]
        {
            Self {
                len: metadata.len(),
                created: metadata.created().ok(),
            }
        }
    }

    fn matches(&self, metadata: &Metadata) -> bool {
        if self.len != metadata.len() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            self.device == metadata.dev() && self.inode == metadata.ino()
        }
        #[cfg(not(unix))]
        {
            self.created.is_none() || self.created == metadata.created().ok()
        }
    }
}

#[derive(Debug)]
struct RefreshState {
    used: usize,
    entry_offset: usize,
    num_entries: usize,
    modification_timestamp: i64,
    overflow: u32,
    transient_failures: u8,
}

/// Logical 64-bit counters used by the high-level APIs.
///
/// HotSpot counter names are not a stable public JVM specification. This enum
/// identifies the metric's meaning; [`JvmMonitor::read_builtin_long`] resolves
/// the best compatible key once at connect/refresh time. Use
/// [`JvmMonitor::metric_resolution`] when diagnosing a particular JVM build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(usize)]
#[non_exhaustive]
pub enum BuiltinLongMetric {
    LoadedClasses,
    SharedLoadedClasses,
    LoadedBytes,
    SharedLoadedBytes,
    UnloadedClasses,
    SharedUnloadedClasses,
    UnloadedBytes,
    SharedUnloadedBytes,
    ClassLoadTime,
    Compilations,
    CompilerBailouts,
    CompilerInvalidations,
    CompilationTime,
    /// Historical jstat `generation.0.space.1.capacity` slot.
    Survivor0Capacity,
    /// Historical jstat `generation.0.space.2.capacity` slot.
    Survivor1Capacity,
    /// Historical jstat `generation.0.space.1.used` slot.
    Survivor0Used,
    /// Historical jstat `generation.0.space.2.used` slot.
    Survivor1Used,
    /// Historical jstat `generation.0.space.0.capacity` slot.
    EdenCapacity,
    /// Historical jstat `generation.0.space.0.used` slot.
    EdenUsed,
    /// Historical jstat generation-1 space-0 capacity slot.
    ///
    /// It is the whole heap for non-generational ZGC and Shenandoah, not an
    /// independently meaningful old generation.
    OldCapacity,
    /// Historical jstat generation-1 space-0 used slot; see `OldCapacity`.
    OldUsed,
    MetaspaceCapacity,
    MetaspaceUsed,
    CompressedClassCapacity,
    CompressedClassUsed,
    /// Legacy HotSpot PermGen committed capacity (not Metaspace).
    PermGenCapacity,
    /// Legacy HotSpot PermGen used bytes (not Metaspace).
    PermGenUsed,
    /// `sun.gc.collector.0.invocations`; the meaning is collector-specific.
    Collector0Invocations,
    /// `sun.gc.collector.0.time`; the meaning is collector-specific.
    Collector0Time,
    /// `sun.gc.collector.1.invocations`; the meaning is collector-specific.
    Collector1Invocations,
    /// `sun.gc.collector.1.time`; the meaning is collector-specific.
    Collector1Time,
    /// `sun.gc.collector.2.invocations`; optional since JDK 11.
    Collector2Invocations,
    /// `sun.gc.collector.2.time`; optional since JDK 11.
    Collector2Time,
    ThreadsLive,
    ThreadsDaemon,
    ThreadsPeak,
    CodeCacheUsed,
    CodeCacheCapacity,
    SafepointTime,
    SafepointSyncTime,
    ApplicationTime,
    Safepoints,
}

impl BuiltinLongMetric {
    /// All built-in logical counters in stable display order.
    pub const ALL: &'static [Self] = &[
        Self::LoadedClasses,
        Self::SharedLoadedClasses,
        Self::LoadedBytes,
        Self::SharedLoadedBytes,
        Self::UnloadedClasses,
        Self::SharedUnloadedClasses,
        Self::UnloadedBytes,
        Self::SharedUnloadedBytes,
        Self::ClassLoadTime,
        Self::Compilations,
        Self::CompilerBailouts,
        Self::CompilerInvalidations,
        Self::CompilationTime,
        Self::Survivor0Capacity,
        Self::Survivor1Capacity,
        Self::Survivor0Used,
        Self::Survivor1Used,
        Self::EdenCapacity,
        Self::EdenUsed,
        Self::OldCapacity,
        Self::OldUsed,
        Self::MetaspaceCapacity,
        Self::MetaspaceUsed,
        Self::CompressedClassCapacity,
        Self::CompressedClassUsed,
        Self::PermGenCapacity,
        Self::PermGenUsed,
        Self::Collector0Invocations,
        Self::Collector0Time,
        Self::Collector1Invocations,
        Self::Collector1Time,
        Self::Collector2Invocations,
        Self::Collector2Time,
        Self::ThreadsLive,
        Self::ThreadsDaemon,
        Self::ThreadsPeak,
        Self::CodeCacheUsed,
        Self::CodeCacheCapacity,
        Self::SafepointTime,
        Self::SafepointSyncTime,
        Self::ApplicationTime,
        Self::Safepoints,
    ];
}

/// Origin of a resolved internal HotSpot key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MetricKeySource {
    /// Canonical OpenJDK HotSpot PerfData key in supported releases.
    OpenJdk,
    /// Semantically equivalent non-canonical/vendor-style alias.
    VendorAlias,
    /// Pre-Metaspace HotSpot layout (for example, JDK 7 PermGen).
    LegacyHotSpot,
}

/// Resolution result for one logical built-in metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct MetricResolution {
    /// Logical counter requested by the caller.
    pub metric: BuiltinLongMetric,
    /// Actual internal key, or `None` when this JVM does not publish it.
    pub key: Option<&'static str>,
    /// Origin of `key`, or `None` when unavailable.
    pub source: Option<MetricKeySource>,
}

impl MetricResolution {
    /// Returns whether the target JVM publishes this metric.
    pub fn is_available(self) -> bool {
        self.key.is_some()
    }
}

/// Low-frequency diagnostic report describing the target JVM's PerfData layout.
///
/// Collector ordinals are deliberately not renamed to "young/full/concurrent":
/// their meanings differ between G1, ZGC, Shenandoah, CMS, and JDK versions.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct JvmCompatibility {
    /// `java.property.java.version`, when published.
    pub java_version: Option<String>,
    /// `java.property.java.vm.version`, when published.
    pub vm_version: Option<String>,
    /// `java.property.java.vm.name`, when published.
    pub vm_name: Option<String>,
    /// `sun.gc.policy.name`, when the collector publishes it.
    pub gc_policy: Option<String>,
    /// High-resolution timer frequency used to convert counter ticks.
    pub timer_frequency_hz: Option<usize>,
    /// `sun.gc.collector.N.name` for collector ordinals 0 through 2.
    pub collector_names: [Option<String>; 3],
    /// `sun.gc.generation.N.name` for generation ordinals 0 through 2.
    pub generation_names: [Option<String>; 3],
    /// `sun.gc.generation.N.space.M.name` for ordinals 0 through 2.
    pub space_names: [[Option<String>; 3]; 3],
    /// Resolution of every logical 64-bit metric.
    pub metrics: Vec<MetricResolution>,
}

#[derive(Debug, Clone, Copy)]
struct MetricCandidate {
    key: &'static str,
    source: MetricKeySource,
}

const fn openjdk(key: &'static str) -> MetricCandidate {
    MetricCandidate {
        key,
        source: MetricKeySource::OpenJdk,
    }
}

const fn vendor_alias(key: &'static str) -> MetricCandidate {
    MetricCandidate {
        key,
        source: MetricKeySource::VendorAlias,
    }
}

const fn legacy_hotspot(key: &'static str) -> MetricCandidate {
    MetricCandidate {
        key,
        source: MetricKeySource::LegacyHotSpot,
    }
}

const BUILTIN_LONG_METRIC_COUNT: usize = BuiltinLongMetric::Safepoints as usize + 1;

const fn builtin_metric_order_is_complete() -> bool {
    let mut index = 0;
    while index < BUILTIN_LONG_METRIC_COUNT {
        if BuiltinLongMetric::ALL[index] as usize != index {
            return false;
        }
        index += 1;
    }
    true
}

const _: () = assert!(BuiltinLongMetric::ALL.len() == BUILTIN_LONG_METRIC_COUNT);
const _: () = assert!(builtin_metric_order_is_complete());

macro_rules! static_metric_candidates {
    ($($candidate:expr),+ $(,)?) => {{
        const CANDIDATES: &[MetricCandidate] = &[$($candidate),+];
        CANDIDATES
    }};
}

fn builtin_long_candidates(metric: BuiltinLongMetric) -> &'static [MetricCandidate] {
    use BuiltinLongMetric::*;
    match metric {
        LoadedClasses => static_metric_candidates![
            openjdk("java.cls.loadedClasses"),
            vendor_alias("sun.cls.loadedClasses"),
        ],
        SharedLoadedClasses => static_metric_candidates![openjdk("java.cls.sharedLoadedClasses")],
        LoadedBytes => static_metric_candidates![openjdk("sun.cls.loadedBytes")],
        SharedLoadedBytes => static_metric_candidates![openjdk("sun.cls.sharedLoadedBytes")],
        UnloadedClasses => static_metric_candidates![
            openjdk("java.cls.unloadedClasses"),
            vendor_alias("sun.cls.unloadedClasses"),
        ],
        SharedUnloadedClasses => {
            static_metric_candidates![openjdk("java.cls.sharedUnloadedClasses")]
        }
        UnloadedBytes => static_metric_candidates![openjdk("sun.cls.unloadedBytes")],
        SharedUnloadedBytes => static_metric_candidates![openjdk("sun.cls.sharedUnloadedBytes")],
        ClassLoadTime => static_metric_candidates![openjdk("sun.cls.time")],
        Compilations => static_metric_candidates![
            openjdk("sun.ci.totalCompiles"),
            vendor_alias("sun.ci.totalCompilations"),
        ],
        CompilerBailouts => static_metric_candidates![openjdk("sun.ci.totalBailouts")],
        CompilerInvalidations => static_metric_candidates![
            openjdk("sun.ci.totalInvalidates"),
            vendor_alias("sun.ci.totalInvalidations"),
        ],
        CompilationTime => static_metric_candidates![
            openjdk("java.ci.totalTime"),
            vendor_alias("sun.ci.totalTime"),
        ],
        Survivor0Capacity => {
            static_metric_candidates![openjdk("sun.gc.generation.0.space.1.capacity")]
        }
        Survivor1Capacity => {
            static_metric_candidates![openjdk("sun.gc.generation.0.space.2.capacity")]
        }
        Survivor0Used => static_metric_candidates![openjdk("sun.gc.generation.0.space.1.used")],
        Survivor1Used => static_metric_candidates![openjdk("sun.gc.generation.0.space.2.used")],
        EdenCapacity => static_metric_candidates![openjdk("sun.gc.generation.0.space.0.capacity")],
        EdenUsed => static_metric_candidates![openjdk("sun.gc.generation.0.space.0.used")],
        OldCapacity => static_metric_candidates![openjdk("sun.gc.generation.1.space.0.capacity")],
        OldUsed => static_metric_candidates![openjdk("sun.gc.generation.1.space.0.used")],
        MetaspaceCapacity => static_metric_candidates![openjdk("sun.gc.metaspace.capacity")],
        MetaspaceUsed => static_metric_candidates![openjdk("sun.gc.metaspace.used")],
        CompressedClassCapacity => {
            static_metric_candidates![openjdk("sun.gc.compressedclassspace.capacity")]
        }
        CompressedClassUsed => {
            static_metric_candidates![openjdk("sun.gc.compressedclassspace.used")]
        }
        PermGenCapacity => {
            static_metric_candidates![legacy_hotspot("sun.gc.generation.2.space.0.capacity")]
        }
        PermGenUsed => {
            static_metric_candidates![legacy_hotspot("sun.gc.generation.2.space.0.used")]
        }
        Collector0Invocations => {
            static_metric_candidates![openjdk("sun.gc.collector.0.invocations")]
        }
        Collector0Time => static_metric_candidates![openjdk("sun.gc.collector.0.time")],
        Collector1Invocations => {
            static_metric_candidates![openjdk("sun.gc.collector.1.invocations")]
        }
        Collector1Time => static_metric_candidates![openjdk("sun.gc.collector.1.time")],
        Collector2Invocations => {
            static_metric_candidates![openjdk("sun.gc.collector.2.invocations")]
        }
        Collector2Time => static_metric_candidates![openjdk("sun.gc.collector.2.time")],
        ThreadsLive => static_metric_candidates![openjdk("java.threads.live")],
        ThreadsDaemon => static_metric_candidates![openjdk("java.threads.daemon")],
        ThreadsPeak => static_metric_candidates![
            openjdk("java.threads.livePeak"),
            vendor_alias("java.threads.peak"),
            vendor_alias("java.threads.peakCount"),
        ],
        CodeCacheUsed => static_metric_candidates![vendor_alias("sun.ci.codeCache.used")],
        CodeCacheCapacity => static_metric_candidates![vendor_alias("sun.ci.codeCache.capacity")],
        SafepointTime => static_metric_candidates![openjdk("sun.rt.safepointTime")],
        SafepointSyncTime => static_metric_candidates![openjdk("sun.rt.safepointSyncTime")],
        ApplicationTime => static_metric_candidates![openjdk("sun.rt.applicationTime")],
        Safepoints => static_metric_candidates![openjdk("sun.rt.safepoints")],
    }
}

/// The main JVM Monitor instance.
///
/// Use `JvmMonitor::connect(pid)` to start monitoring a specific process,
/// or `JvmMonitor::discover_all()` to find all running JVMs.
pub struct JvmMonitor {
    monitor_id: usize,
    host_pid: u32,
    #[cfg(target_os = "linux")]
    process_start_time: Option<u64>,
    #[cfg(target_os = "linux")]
    process_stat_path: Option<PathBuf>,
    path: PathBuf,
    file: File,
    identity: FileIdentity,
    memory: PerfMemory,
    is_little_endian: bool,
    index: HashMap<String, EntryMeta>,
    dynamic_index: RwLock<HashMap<String, EntryMeta>>,
    builtin_long_offsets: [AtomicUsize; BUILTIN_LONG_METRIC_COUNT],
    refresh_state: Mutex<RefreshState>,
    timer_frequency_hz: AtomicUsize,
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
        #[cfg(target_os = "linux")]
        let process_start_time = Self::get_process_start_time(host_pid);
        #[cfg(not(target_os = "linux"))]
        let process_start_time = None;

        let path = Self::find_hsperfdata_file(host_pid)
            .ok_or(JvmMonitorError::ProcessNotFound(host_pid))?;

        Self::connect_path_with_start_time(host_pid, path, process_start_time)
    }

    #[cfg(test)]
    fn connect_path(host_pid: u32, path: PathBuf) -> Result<Self, JvmMonitorError> {
        Self::connect_path_with_start_time(host_pid, path, None)
    }

    fn connect_path_with_start_time(
        host_pid: u32,
        path: PathBuf,
        _process_start_time: Option<u64>,
    ) -> Result<Self, JvmMonitorError> {
        let file = File::open(&path)?;
        let metadata = file.metadata()?;
        let identity = FileIdentity::from_metadata(&metadata);
        let memory = PerfMemory::map(&file)?;

        let deadline = Instant::now() + ACCESSIBLE_WAIT_TIMEOUT;
        loop {
            if memory.is_accessible() == Some(true) {
                break;
            }
            let current_metadata =
                fs::metadata(&path).map_err(|_| JvmMonitorError::ProcessNotFound(host_pid))?;
            if !identity.matches(&current_metadata) {
                return Err(JvmMonitorError::ProcessNotFound(host_pid));
            }
            if Instant::now() >= deadline {
                return Err(JvmMonitorError::InvalidFormat(format!(
                    "JVM {host_pid} perfdata did not become accessible within {} seconds",
                    ACCESSIBLE_WAIT_TIMEOUT.as_secs()
                )));
            }
            std::thread::sleep(ACCESSIBLE_POLL_INTERVAL);
        }

        let (header, index) = memory.capture_index()?;
        let current_metadata =
            fs::metadata(&path).map_err(|_| JvmMonitorError::ProcessNotFound(host_pid))?;
        let current_file_metadata = file.metadata()?;
        if !identity.matches(&current_metadata)
            || !identity.matches(&current_file_metadata)
            || current_file_metadata.len() != memory.len() as u64
        {
            return Err(JvmMonitorError::ProcessNotFound(host_pid));
        }
        #[cfg(target_os = "linux")]
        let process_start_time =
            _process_start_time.or_else(|| Self::get_process_start_time(host_pid));
        #[cfg(target_os = "linux")]
        if let Some(expected_start_time) = process_start_time
            && Self::get_process_start_time(host_pid) != Some(expected_start_time)
        {
            return Err(JvmMonitorError::ProcessNotFound(host_pid));
        }
        let builtin_long_offsets = std::array::from_fn(|_| AtomicUsize::new(MISSING_OFFSET));
        update_builtin_offsets(&builtin_long_offsets, &index);
        let timer_frequency_hz =
            timer_frequency_from_index(&memory, &index, header.is_little_endian);

        let monitor = Self {
            monitor_id: NEXT_MONITOR_ID.fetch_add(1, Ordering::Relaxed),
            host_pid,
            #[cfg(target_os = "linux")]
            process_start_time,
            #[cfg(target_os = "linux")]
            process_stat_path: process_start_time.map(|_| Self::process_stat_path(host_pid)),
            path,
            file,
            identity,
            memory,
            is_little_endian: header.is_little_endian,
            index,
            dynamic_index: RwLock::new(HashMap::new()),
            builtin_long_offsets,
            refresh_state: Mutex::new(RefreshState {
                used: header.used,
                entry_offset: header.entry_offset,
                num_entries: header.num_entries,
                modification_timestamp: header.modification_timestamp,
                overflow: header.overflow,
                transient_failures: 0,
            }),
            timer_frequency_hz: AtomicUsize::new(timer_frequency_hz),
        };
        Ok(monitor)
    }

    /// Returns the host PID associated with this monitor.
    pub fn pid(&self) -> u32 {
        self.host_pid
    }

    /// Validates that the mapped file still belongs to the connected JVM and
    /// returns the current hsperfdata prologue health.
    ///
    /// Unlike the fast `read_*` methods, this method performs filesystem
    /// metadata checks. Call it once per sampling cycle or on a slower health
    /// cadence, not once per individual metric.
    pub fn check_health(&self) -> Result<MonitorHealth, JvmMonitorError> {
        self.validate_target_identity()?;
        let state = self
            .refresh_state
            .lock()
            .map_err(|_| JvmMonitorError::InvalidFormat("refresh state lock poisoned".into()))?;
        let header = self.memory.header()?;
        header.validate_layout(self.memory.len())?;
        if !header.accessible {
            return Err(JvmMonitorError::ProcessNotFound(self.host_pid));
        }
        if header.is_little_endian != self.is_little_endian {
            return Err(JvmMonitorError::InvalidFormat(
                "perfdata byte order changed after connection".into(),
            ));
        }
        if header.entry_offset != state.entry_offset
            || header.used < state.used
            || header.num_entries < state.num_entries
        {
            return Err(JvmMonitorError::InvalidFormat(
                "perfdata structure moved backwards after connection".into(),
            ));
        }
        Ok(self.health_from_header(header))
    }

    /// Checks target health and publishes counters appended since `connect`.
    ///
    /// Returns the number of newly indexed counters. Existing HotSpot entries
    /// are append-only; a removed or relocated entry is treated as corruption
    /// rather than silently replacing a live offset.
    pub fn refresh(&self) -> Result<usize, JvmMonitorError> {
        self.validate_target_identity()?;
        let mut state = self
            .refresh_state
            .lock()
            .map_err(|_| JvmMonitorError::InvalidFormat("refresh state lock poisoned".into()))?;
        let mut current_header = self.memory.header()?;
        current_header.validate_layout(self.memory.len())?;
        if !current_header.accessible {
            return Err(JvmMonitorError::ProcessNotFound(self.host_pid));
        }
        if current_header.entry_offset != state.entry_offset
            || current_header.used < state.used
            || current_header.num_entries < state.num_entries
        {
            return Err(JvmMonitorError::InvalidFormat(
                "perfdata structure moved backwards after connection".into(),
            ));
        }

        // HotSpot publishes used/num_entries before it finishes writing a new
        // entry, then updates the modification timestamp last. Do not parse or
        // publish the early state.
        if (current_header.used != state.used || current_header.num_entries != state.num_entries)
            && current_header.modification_timestamp == state.modification_timestamp
        {
            for _ in 0..5 {
                std::thread::yield_now();
                current_header = self.memory.header()?;
                current_header.validate_layout(self.memory.len())?;
                if current_header.modification_timestamp != state.modification_timestamp {
                    break;
                }
            }
        }

        if current_header.entry_offset != state.entry_offset
            || current_header.used < state.used
            || current_header.num_entries < state.num_entries
        {
            return Err(JvmMonitorError::InvalidFormat(
                "perfdata structure moved backwards during refresh".into(),
            ));
        }

        if current_header.used == state.used
            && current_header.num_entries == state.num_entries
            && current_header.modification_timestamp == state.modification_timestamp
        {
            if self.timer_frequency_hz.load(Ordering::Relaxed) == 0
                && let Some(metadata) = self.lookup_meta("sun.os.hrt.frequency")
                && metadata.data_type == b'J'
                && metadata.vector_length == 0
                && let Some(frequency) = self
                    .memory
                    .read_i64(metadata.data_offset, self.is_little_endian)
                    .and_then(|frequency| usize::try_from(frequency).ok())
            {
                self.timer_frequency_hz.store(frequency, Ordering::Relaxed);
            }
            state.overflow = current_header.overflow;
            state.transient_failures = 0;
            return Ok(0);
        }

        let (header, complete_index) = match self.memory.capture_index() {
            Ok(captured) => captured,
            Err(error) => {
                let after = self.memory.header()?;
                let structurally_forward = after.entry_offset == state.entry_offset
                    && after.used >= state.used
                    && after.num_entries >= state.num_entries
                    && (after.used != state.used
                        || after.num_entries != state.num_entries
                        || after.modification_timestamp != state.modification_timestamp);
                if structurally_forward && state.transient_failures < 3 {
                    state.overflow = after.overflow;
                    state.transient_failures += 1;
                    return Ok(0);
                }
                return Err(error);
            }
        };
        if header.modification_timestamp == current_header.modification_timestamp
            && (header.used != current_header.used
                || header.num_entries != current_header.num_entries)
        {
            state.overflow = header.overflow;
            return Ok(0);
        }
        if header.is_little_endian != self.is_little_endian {
            return Err(JvmMonitorError::InvalidFormat(
                "perfdata byte order changed after connection".into(),
            ));
        }

        for (name, metadata) in &self.index {
            if complete_index.get(name) != Some(metadata) {
                return Err(JvmMonitorError::InvalidFormat(format!(
                    "existing perfdata entry {name} was removed or relocated"
                )));
            }
        }

        let mut dynamic = self
            .dynamic_index
            .write()
            .map_err(|_| JvmMonitorError::InvalidFormat("dynamic index lock poisoned".into()))?;
        for (name, metadata) in dynamic.iter() {
            if complete_index.get(name) != Some(metadata) {
                return Err(JvmMonitorError::InvalidFormat(format!(
                    "dynamic perfdata entry {name} was removed or relocated"
                )));
            }
        }

        let mut added = 0;
        for (name, metadata) in &complete_index {
            if !self.index.contains_key(name) && !dynamic.contains_key(name) {
                dynamic.insert(name.clone(), *metadata);
                added += 1;
            }
        }
        update_builtin_offsets(&self.builtin_long_offsets, &complete_index);
        self.timer_frequency_hz.store(
            timer_frequency_from_index(&self.memory, &complete_index, self.is_little_endian),
            Ordering::Relaxed,
        );

        state.used = header.used;
        state.entry_offset = header.entry_offset;
        state.num_entries = header.num_entries;
        state.modification_timestamp = header.modification_timestamp;
        state.overflow = header.overflow;
        state.transient_failures = 0;
        Ok(added)
    }

    /// Validates the target, refreshes structural metadata, and reads all
    /// high-level metric groups.
    ///
    /// Long-running collectors should prefer this fallible API over calling
    /// the compatibility `get_*` methods without a periodic health check.
    pub fn sample(&self) -> Result<JvmStatsSnapshot, JvmMonitorError> {
        self.refresh()?;
        let state = self
            .refresh_state
            .lock()
            .map_err(|_| JvmMonitorError::InvalidFormat("refresh state lock poisoned".into()))?;
        let health = MonitorHealth {
            pid: self.host_pid,
            overflow_bytes: state.overflow,
            num_entries: state.num_entries,
            modification_timestamp: state.modification_timestamp,
        };
        drop(state);

        Ok(JvmStatsSnapshot {
            health,
            gc: self.get_gc_stats(),
            runtime: self.get_runtime_stats(),
            classes: self.get_class_stats(),
            compiler: self.get_compiler_stats(),
        })
    }

    fn validate_target_identity(&self) -> Result<(), JvmMonitorError> {
        #[cfg(target_os = "linux")]
        if let Some(expected_start_time) = self.process_start_time
            && self
                .process_stat_path
                .as_deref()
                .and_then(Self::read_process_start_time)
                != Some(expected_start_time)
        {
            return Err(JvmMonitorError::ProcessNotFound(self.host_pid));
        }

        let path_metadata = fs::metadata(&self.path)
            .map_err(|_| JvmMonitorError::ProcessNotFound(self.host_pid))?;
        if !self.identity.matches(&path_metadata) {
            return Err(JvmMonitorError::ProcessNotFound(self.host_pid));
        }
        let file_metadata = self.file.metadata()?;
        if !self.identity.matches(&file_metadata) || file_metadata.len() != self.memory.len() as u64
        {
            return Err(JvmMonitorError::ProcessNotFound(self.host_pid));
        }
        Ok(())
    }

    fn health_from_header(&self, header: PerfHeader) -> MonitorHealth {
        MonitorHealth {
            pid: self.host_pid,
            overflow_bytes: header.overflow,
            num_entries: header.num_entries,
            modification_timestamp: header.modification_timestamp,
        }
    }

    /// Discovers all Java processes. Supports Host and Container (Docker/K8s) PIDs.
    pub fn discover_all() -> Result<Vec<JavaProcessInfo>, JvmMonitorError> {
        let mut processes = Vec::new();
        let mut seen_host_pids = HashSet::new();

        // --- 1. Linux Specific: Container discovery via /proc ---
        #[cfg(target_os = "linux")]
        {
            use std::collections::hash_map::Entry;
            use std::ffi::OsString;
            use std::os::unix::fs::MetadataExt;

            // Most host processes share the same /tmp. Cache the hsperfdata
            // directory names by the underlying tmp directory identity so it
            // is scanned once per mount/chroot rather than once per PID.
            let mut perf_dirs_by_tmp: HashMap<(u64, u64), Vec<OsString>> = HashMap::new();
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
                            let Ok(metadata) = fs::metadata(&container_tmp) else {
                                continue;
                            };
                            let directory_key = (metadata.dev(), metadata.ino());
                            let directory_names = match perf_dirs_by_tmp.entry(directory_key) {
                                Entry::Occupied(entry) => entry.into_mut(),
                                Entry::Vacant(entry) => {
                                    let Some(names) =
                                        Self::perfdata_directory_names(&container_tmp)
                                    else {
                                        continue;
                                    };
                                    entry.insert(names)
                                }
                            };

                            let pid_name = ns_pid.to_string();
                            let perf_file = directory_names
                                .iter()
                                .map(|directory| container_tmp.join(directory).join(&pid_name))
                                .find(|candidate| candidate.is_file());
                            if let Some(path) = perf_file
                                && let Some(name) = Self::fast_extract_name(&path)
                            {
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

        // --- 2. Windows/macOS/Linux Host: Direct path and fallback scan ---
        let base_tmp = Self::get_temp_root();

        // Fast path: Target current user folder directly to avoid large directory scans (especially on Windows)
        let user_env = if cfg!(windows) { "USERNAME" } else { "USER" };
        let current_user_dir = std::env::var(user_env)
            .ok()
            .map(|user| base_tmp.join(format!("hsperfdata_{user}")));
        if let Some(user_dir) = &current_user_dir {
            Self::scan_pids_in_folder(user_dir, &mut processes, &mut seen_host_pids);
        }

        // Scan other users as well; `seen_host_pids` keeps this additive scan
        // from duplicating current-user/container results.
        if let Ok(entries) = fs::read_dir(&base_tmp) {
            for entry_result in entries {
                let entry = match entry_result {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                let name = entry.file_name();
                let path = entry.path();
                if name.to_string_lossy().starts_with("hsperfdata_")
                    && entry.file_type().is_ok_and(|kind| kind.is_dir())
                    && current_user_dir.as_ref() != Some(&path)
                {
                    Self::scan_pids_in_folder(&path, &mut processes, &mut seen_host_pids);
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

                if let Ok(pid) = p_entry.file_name().to_string_lossy().parse::<u32>()
                    && !seen.contains(&pid)
                    && let Some(name) = Self::fast_extract_name(&p_entry.path())
                {
                    results.push(JavaProcessInfo { pid, name });
                    seen.insert(pid);
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

    #[cfg(target_os = "linux")]
    fn get_process_start_time(host_pid: u32) -> Option<u64> {
        // /proc/<pid>/stat field 2 (comm) may contain spaces and parentheses,
        // so split only after its final closing parenthesis. The remaining
        // fields begin at field 3; starttime is field 22 (index 19 here).
        Self::read_process_start_time(&Self::process_stat_path(host_pid))
    }

    #[cfg(target_os = "linux")]
    fn process_stat_path(host_pid: u32) -> PathBuf {
        PathBuf::from("/proc")
            .join(host_pid.to_string())
            .join("stat")
    }

    #[cfg(target_os = "linux")]
    fn read_process_start_time(path: &std::path::Path) -> Option<u64> {
        let mut file = File::open(path).ok()?;
        let mut buffer = [0_u8; 4096];
        let mut bytes_read = 0;
        while bytes_read < buffer.len() {
            let read = file.read(&mut buffer[bytes_read..]).ok()?;
            if read == 0 {
                break;
            }
            bytes_read += read;
        }
        if bytes_read == buffer.len() {
            return None;
        }
        let stat = std::str::from_utf8(&buffer[..bytes_read]).ok()?;
        Self::parse_process_start_time(stat)
    }

    #[cfg(any(target_os = "linux", test))]
    fn parse_process_start_time(stat: &str) -> Option<u64> {
        let command_end = stat.rfind(')')?;
        stat.get(command_end + 2..)?
            .split_whitespace()
            .nth(19)?
            .parse()
            .ok()
    }

    fn find_hsperfdata_file(host_pid: u32) -> Option<PathBuf> {
        // Most callers monitor a host JVM. Try the host temp directory before
        // paying the Linux namespace/status scan cost.
        let base_tmp = Self::get_temp_root();
        let user_env = if cfg!(windows) { "USERNAME" } else { "USER" };
        if let Ok(user) = std::env::var(user_env) {
            let direct = base_tmp
                .join(format!("hsperfdata_{user}"))
                .join(host_pid.to_string());
            if direct.is_file() {
                return Some(direct);
            }
        }
        if let Some(path) = Self::find_perf_file_in_dir(&base_tmp, host_pid) {
            return Some(path);
        }

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

        None
    }

    fn find_perf_file_in_dir(base_path: &PathBuf, target_pid: u32) -> Option<PathBuf> {
        let pid_str = target_pid.to_string();
        Self::perfdata_directory_names(base_path)?
            .into_iter()
            .map(|directory| base_path.join(directory).join(&pid_str))
            .find(|candidate| candidate.is_file())
    }

    fn perfdata_directory_names(base_path: &PathBuf) -> Option<Vec<std::ffi::OsString>> {
        let entries = fs::read_dir(base_path).ok()?;
        let mut names = Vec::new();
        for entry_result in entries {
            let entry = match entry_result {
                Ok(e) => e,
                Err(_) => continue,
            };
            let name = entry.file_name();
            if name.to_string_lossy().starts_with("hsperfdata_")
                && entry.file_type().is_ok_and(|kind| kind.is_dir())
            {
                names.push(name);
            }
        }
        Some(names)
    }

    fn fast_extract_name(path: &PathBuf) -> Option<String> {
        // Discovery is not a hot metric path. An owned read avoids exposing
        // references into a file the target JVM is concurrently modifying.
        let file = File::open(path).ok()?;
        let file_len = file.metadata().ok()?.len();
        if !(perfdata::PROLOGUE_SIZE as u64..=MAX_DISCOVERY_FILE_SIZE).contains(&file_len) {
            return None;
        }
        let mut snapshot = Vec::new();
        snapshot.try_reserve_exact(file_len as usize).ok()?;
        file.take(MAX_DISCOVERY_FILE_SIZE + 1)
            .read_to_end(&mut snapshot)
            .ok()?;
        if snapshot.len() as u64 > MAX_DISCOVERY_FILE_SIZE {
            return None;
        }
        let (_, index) = parse_snapshot(&snapshot).ok()?;
        let metadata = index.get("sun.rt.javaCommand")?;
        if metadata.data_type != b'B' || metadata.vector_length == 0 {
            return None;
        }
        let end = metadata.data_offset.checked_add(metadata.vector_length)?;
        let data = snapshot.get(metadata.data_offset..end)?;
        let name_len = data
            .iter()
            .position(|byte| *byte == 0 || *byte == b' ')
            .unwrap_or(data.len());
        Some(String::from_utf8_lossy(&data[..name_len]).into_owned())
    }

    /// Reads a raw performance counter value by its full internal name (e.g., "sun.gc.cause").
    ///
    /// Returns `None` if the key does not exist or the data type is unsupported.
    pub fn read_metric(&self, key: &str) -> Option<PerfValue> {
        let metadata = self.lookup_meta(key)?;
        self.read_resolved_metric(metadata)
    }

    /// Resolves a metric name once for repeated reads without further hashing
    /// or dynamic-index locking.
    pub fn metric_handle(&self, key: &str) -> Option<MetricHandle> {
        self.lookup_meta(key).map(|metadata| MetricHandle {
            monitor_id: self.monitor_id,
            metadata,
        })
    }

    /// Reads a previously resolved metric handle.
    pub fn read_metric_handle(&self, handle: MetricHandle) -> Option<PerfValue> {
        if handle.monitor_id != self.monitor_id {
            return None;
        }
        self.read_resolved_metric(handle.metadata)
    }

    /// Reads a previously resolved long metric without allocation.
    pub fn read_long_handle(&self, handle: MetricHandle) -> Option<i64> {
        if handle.monitor_id != self.monitor_id
            || handle.metadata.data_type != b'J'
            || handle.metadata.vector_length != 0
        {
            return None;
        }
        self.memory
            .read_i64(handle.metadata.data_offset, self.is_little_endian)
    }

    fn read_resolved_metric(&self, metadata: EntryMeta) -> Option<PerfValue> {
        match metadata.data_type {
            b'J' if metadata.vector_length == 0 => self
                .memory
                .read_i64(metadata.data_offset, self.is_little_endian)
                .map(PerfValue::Long),
            b'B' if metadata.vector_length > 0 => self
                .memory
                .read_string(metadata.data_offset, metadata.vector_length)
                .map(PerfValue::String),
            _ => None,
        }
    }

    fn lookup_meta(&self, key: &str) -> Option<EntryMeta> {
        if let Some(metadata) = self.index.get(key) {
            return Some(*metadata);
        }
        self.dynamic_index.read().ok()?.get(key).copied()
    }

    /// Reads a logical built-in counter after version/vendor key resolution.
    ///
    /// Unlike [`JvmMonitor::read_long`], this method distinguishes an
    /// unsupported counter (`None`) from a real counter whose value is zero.
    /// The hot path is allocation-free and performs no hash lookup or lock.
    #[inline]
    pub fn read_builtin_long(&self, metric: BuiltinLongMetric) -> Option<i64> {
        let offset = self.builtin_long_offsets[metric as usize].load(Ordering::Relaxed);
        if offset == MISSING_OFFSET {
            return None;
        }
        self.memory.read_i64(offset, self.is_little_endian)
    }

    /// Reports the exact key selected for a logical built-in counter.
    ///
    /// This is a diagnostic path and may take the dynamic-index read lock. It
    /// should not be called for every sample.
    pub fn metric_resolution(&self, metric: BuiltinLongMetric) -> MetricResolution {
        let dynamic = self.dynamic_index.read().ok();
        // refresh holds the dynamic-index write lock while publishing a newly
        // resolved offset, so this load and lookup describe the same binding.
        let selected_offset = self.builtin_long_offsets[metric as usize].load(Ordering::Relaxed);
        let resolved = if selected_offset == MISSING_OFFSET {
            None
        } else {
            builtin_long_candidates(metric).iter().find(|candidate| {
                self.index
                    .get(candidate.key)
                    .or_else(|| {
                        dynamic
                            .as_ref()
                            .and_then(|dynamic| dynamic.get(candidate.key))
                    })
                    .is_some_and(|metadata| {
                        metadata.data_type == b'J'
                            && metadata.vector_length == 0
                            && metadata.data_offset == selected_offset
                    })
            })
        };
        MetricResolution {
            metric,
            key: resolved.map(|candidate| candidate.key),
            source: resolved.map(|candidate| candidate.source),
        }
    }

    /// Builds a low-frequency compatibility report for the connected JVM.
    ///
    /// The report identifies the actual collector/generation names and every
    /// selected metric key. Building it allocates owned strings and a vector;
    /// metric sampling itself remains allocation-free.
    pub fn compatibility(&self) -> JvmCompatibility {
        let collector_names = std::array::from_fn(|ordinal| {
            self.read_optional_string(match ordinal {
                0 => "sun.gc.collector.0.name",
                1 => "sun.gc.collector.1.name",
                _ => "sun.gc.collector.2.name",
            })
        });
        let generation_names = std::array::from_fn(|ordinal| {
            self.read_optional_string(match ordinal {
                0 => "sun.gc.generation.0.name",
                1 => "sun.gc.generation.1.name",
                _ => "sun.gc.generation.2.name",
            })
        });
        const SPACE_NAME_KEYS: [[&str; 3]; 3] = [
            [
                "sun.gc.generation.0.space.0.name",
                "sun.gc.generation.0.space.1.name",
                "sun.gc.generation.0.space.2.name",
            ],
            [
                "sun.gc.generation.1.space.0.name",
                "sun.gc.generation.1.space.1.name",
                "sun.gc.generation.1.space.2.name",
            ],
            [
                "sun.gc.generation.2.space.0.name",
                "sun.gc.generation.2.space.1.name",
                "sun.gc.generation.2.space.2.name",
            ],
        ];
        let space_names = std::array::from_fn(|generation| {
            std::array::from_fn(|space| {
                self.read_optional_string(SPACE_NAME_KEYS[generation][space])
            })
        });

        JvmCompatibility {
            java_version: self.read_optional_string("java.property.java.version"),
            vm_version: self.read_optional_string("java.property.java.vm.version"),
            vm_name: self.read_optional_string("java.property.java.vm.name"),
            gc_policy: self.read_optional_string("sun.gc.policy.name"),
            timer_frequency_hz: match self.timer_frequency_hz.load(Ordering::Relaxed) {
                0 => None,
                frequency => Some(frequency),
            },
            collector_names,
            generation_names,
            space_names,
            metrics: BuiltinLongMetric::ALL
                .iter()
                .copied()
                .map(|metric| self.metric_resolution(metric))
                .collect(),
        }
    }

    #[inline]
    fn read_known_long(&self, metric: BuiltinLongMetric) -> i64 {
        self.read_builtin_long(metric).unwrap_or(0)
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
        self.read_optional_string(key)
            .unwrap_or_else(|| "-".to_string())
    }

    fn read_optional_string(&self, key: &str) -> Option<String> {
        match self.read_metric(key) {
            Some(PerfValue::String(value)) => Some(value),
            _ => None,
        }
    }

    fn read_display_value(&self, key: &str) -> String {
        match self.read_metric(key) {
            Some(PerfValue::String(value)) => value,
            Some(PerfValue::Long(value)) => value.to_string(),
            None => "-".to_owned(),
        }
    }

    // Helper: Convert Ticks to Seconds
    fn to_seconds(&self, ticks: i64) -> f64 {
        let timer_frequency_hz = self.timer_frequency_hz.load(Ordering::Relaxed);
        if timer_frequency_hz > 0 {
            ticks as f64 / timer_frequency_hz as f64
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
        let loaded = self
            .read_known_long(BuiltinLongMetric::LoadedClasses)
            .saturating_add(self.read_known_long(BuiltinLongMetric::SharedLoadedClasses));
        let loaded_bytes = self
            .read_known_long(BuiltinLongMetric::LoadedBytes)
            .saturating_add(self.read_known_long(BuiltinLongMetric::SharedLoadedBytes));
        let unloaded = self
            .read_known_long(BuiltinLongMetric::UnloadedClasses)
            .saturating_add(self.read_known_long(BuiltinLongMetric::SharedUnloadedClasses));
        let unloaded_bytes = self
            .read_known_long(BuiltinLongMetric::UnloadedBytes)
            .saturating_add(self.read_known_long(BuiltinLongMetric::SharedUnloadedBytes));
        ClassStats {
            loaded,
            bytes: self.to_kb(loaded_bytes),
            unloaded,
            unloaded_bytes: self.to_kb(unloaded_bytes),
            time: self.to_seconds(self.read_known_long(BuiltinLongMetric::ClassLoadTime)),
        }
    }

    /// Retrieves allocation-free numeric JIT compiler statistics.
    pub fn get_compiler_numeric_stats(&self) -> CompilerNumericStats {
        CompilerNumericStats {
            compiled: self.read_known_long(BuiltinLongMetric::Compilations),
            failed: self.read_known_long(BuiltinLongMetric::CompilerBailouts),
            invalid: self.read_known_long(BuiltinLongMetric::CompilerInvalidations),
            time: self.to_seconds(self.read_known_long(BuiltinLongMetric::CompilationTime)),
        }
    }

    /// Retrieves JIT compiler statistics, equivalent to `jstat -compiler`.
    pub fn get_compiler_stats(&self) -> CompilerStats {
        let numeric = self.get_compiler_numeric_stats();
        CompilerStats {
            compiled: numeric.compiled,
            failed: numeric.failed,
            invalid: numeric.invalid,
            time: numeric.time,
            failed_type: self.read_display_value("sun.ci.lastFailedType"),
            failed_method: self.read_string("sun.ci.lastFailedMethod"),
        }
    }

    /// Retrieves allocation-free, jstat-compatible numeric GC statistics.
    ///
    /// For source compatibility, an unavailable JVM counter is represented as
    /// zero. Use [`JvmMonitor::read_builtin_long`] or
    /// [`JvmMonitor::get_gc_collector_numeric_stats`] when missing-versus-zero
    /// must be preserved.
    pub fn get_gc_numeric_stats(&self) -> GcNumericStats {
        let s0c = self.read_known_long(BuiltinLongMetric::Survivor0Capacity);
        let s1c = self.read_known_long(BuiltinLongMetric::Survivor1Capacity);
        let s0u = self.read_known_long(BuiltinLongMetric::Survivor0Used);
        let s1u = self.read_known_long(BuiltinLongMetric::Survivor1Used);
        let ec = self.read_known_long(BuiltinLongMetric::EdenCapacity);
        let eu = self.read_known_long(BuiltinLongMetric::EdenUsed);
        let oc = self.read_known_long(BuiltinLongMetric::OldCapacity);
        let ou = self.read_known_long(BuiltinLongMetric::OldUsed);
        let mc = self.read_known_long(BuiltinLongMetric::MetaspaceCapacity);
        let mu = self.read_known_long(BuiltinLongMetric::MetaspaceUsed);
        let ccsc = self.read_known_long(BuiltinLongMetric::CompressedClassCapacity);
        let ccsu = self.read_known_long(BuiltinLongMetric::CompressedClassUsed);

        // These retain jstat's historical column names for API compatibility,
        // but are raw collector ordinals. Their exact meanings are available
        // through compatibility().collector_names.
        let ygc = self.read_known_long(BuiltinLongMetric::Collector0Invocations);
        let ygct = self.read_known_long(BuiltinLongMetric::Collector0Time);
        let fgc = self.read_known_long(BuiltinLongMetric::Collector1Invocations);
        let fgct = self.read_known_long(BuiltinLongMetric::Collector1Time);
        let cgc = self.read_known_long(BuiltinLongMetric::Collector2Invocations);
        let cgct = self.read_known_long(BuiltinLongMetric::Collector2Time);
        let total_gc_ticks = ygct.saturating_add(fgct).saturating_add(cgct);

        GcNumericStats {
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
            ygc: ygc.max(0) as u64,
            ygct: self.to_seconds(ygct),
            fgc: fgc.max(0) as u64,
            fgct: self.to_seconds(fgct),
            cgc: cgc.max(0) as u64,
            cgct: self.to_seconds(cgct),
            gct: self.to_seconds(total_gc_ticks),
        }
    }

    /// Retrieves the three HotSpot collector slots without assigning a
    /// collector-independent meaning to their ordinals.
    ///
    /// This is the preferred high-frequency API for collector counts/times on
    /// mixed JDK and GC fleets. A missing slot remains `None`, while a
    /// published zero remains `Some(0)`. Call [`JvmMonitor::compatibility`]
    /// once to obtain each slot's collector name.
    pub fn get_gc_collector_numeric_stats(&self) -> [GcCollectorNumericStats; 3] {
        const COUNT_METRICS: [BuiltinLongMetric; 3] = [
            BuiltinLongMetric::Collector0Invocations,
            BuiltinLongMetric::Collector1Invocations,
            BuiltinLongMetric::Collector2Invocations,
        ];
        const TIME_METRICS: [BuiltinLongMetric; 3] = [
            BuiltinLongMetric::Collector0Time,
            BuiltinLongMetric::Collector1Time,
            BuiltinLongMetric::Collector2Time,
        ];
        let timer_frequency_hz = self.timer_frequency_hz.load(Ordering::Relaxed);
        std::array::from_fn(|ordinal| GcCollectorNumericStats {
            ordinal: ordinal as u8,
            invocations: self
                .read_builtin_long(COUNT_METRICS[ordinal])
                .map(|value| value.max(0) as u64),
            time_s: if timer_frequency_hz == 0 {
                None
            } else {
                self.read_builtin_long(TIME_METRICS[ordinal])
                    .map(|ticks| ticks as f64 / timer_frequency_hz as f64)
            },
        })
    }

    /// Retrieves garbage collection statistics, equivalent to `jstat -gc` or `jstat -gccause`.
    pub fn get_gc_stats(&self) -> GcStats {
        let numeric = self.get_gc_numeric_stats();
        GcStats {
            s0c: numeric.s0c,
            s1c: numeric.s1c,
            s0u: numeric.s0u,
            s1u: numeric.s1u,
            ec: numeric.ec,
            eu: numeric.eu,
            oc: numeric.oc,
            ou: numeric.ou,
            mc: numeric.mc,
            mu: numeric.mu,
            ccsc: numeric.ccsc,
            ccsu: numeric.ccsu,
            ygc: numeric.ygc,
            ygct: numeric.ygct,
            fgc: numeric.fgc,
            fgct: numeric.fgct,
            cgc: numeric.cgc,
            cgct: numeric.cgct,
            gct: numeric.gct,
            lgcc: self.read_string("sun.gc.lastCause"),
            gcc: self.read_string("sun.gc.cause"),
        }
    }

    /// Retrieves various runtime statistics including threads, code cache, and safepoints.
    ///
    /// Standard OpenJDK PerfData does not expose exact current code-cache
    /// occupancy/capacity. Those fields remain zero unless a vendor publishes
    /// the explicitly compatible `sun.ci.codeCache.*` keys; use
    /// [`JvmMonitor::metric_resolution`] to test availability.
    pub fn get_runtime_stats(&self) -> RuntimeStats {
        // 1. Threads
        let t_live = self.read_known_long(BuiltinLongMetric::ThreadsLive);
        let t_daemon = self.read_known_long(BuiltinLongMetric::ThreadsDaemon);
        let mut t_peak = self.read_known_long(BuiltinLongMetric::ThreadsPeak);
        if t_peak == 0 {
            t_peak = t_live;
        }

        // 2. Optional vendor Code Cache counters. Standard OpenJDK PerfData
        // does not publish exact current occupancy/capacity values.
        let cc_used = self.to_kb(self.read_known_long(BuiltinLongMetric::CodeCacheUsed));
        let cc_cap = self.to_kb(self.read_known_long(BuiltinLongMetric::CodeCacheCapacity));

        let cc_util = if cc_cap > 0.0 { cc_used / cc_cap } else { 0.0 };

        // 3. Safepoints
        let safepoint_ticks = self.read_known_long(BuiltinLongMetric::SafepointTime);
        let app_ticks = self.read_known_long(BuiltinLongMetric::ApplicationTime);
        let safepoints = self.read_known_long(BuiltinLongMetric::Safepoints);

        let safepoint_time_s = self.to_seconds(safepoint_ticks);
        let app_time_s = self.to_seconds(app_ticks);

        let total_ticks = safepoint_ticks.saturating_add(app_ticks);
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

fn timer_frequency_from_index(
    memory: &PerfMemory,
    index: &HashMap<String, EntryMeta>,
    is_little_endian: bool,
) -> usize {
    index
        .get("sun.os.hrt.frequency")
        .filter(|metadata| metadata.data_type == b'J' && metadata.vector_length == 0)
        .and_then(|metadata| memory.read_i64(metadata.data_offset, is_little_endian))
        .and_then(|frequency| usize::try_from(frequency).ok())
        .unwrap_or(0)
}

fn update_builtin_offsets(
    offsets: &[AtomicUsize; BUILTIN_LONG_METRIC_COUNT],
    index: &HashMap<String, EntryMeta>,
) {
    for metric in BuiltinLongMetric::ALL.iter().copied() {
        // Keep a selected key stable for the monitor's lifetime. Switching a
        // cumulative counter to a later, higher-priority alias can otherwise
        // create a non-monotonic jump in long-running collection. Missing
        // counters are still picked up when refresh discovers them.
        if offsets[metric as usize].load(Ordering::Relaxed) != MISSING_OFFSET {
            continue;
        }
        let resolved = builtin_long_candidates(metric)
            .iter()
            .filter_map(|candidate| index.get(candidate.key))
            .find(|metadata| metadata.data_type == b'J' && metadata.vector_length == 0)
            .map_or(MISSING_OFFSET, |metadata| metadata.data_offset);
        offsets[metric as usize].store(resolved, Ordering::Relaxed);
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

/// Allocation-free numeric JIT compiler statistics.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompilerNumericStats {
    /// Total number of compilations performed.
    pub compiled: i64,
    /// Total number of failed compilations.
    pub failed: i64,
    /// Total number of invalidated compilations.
    pub invalid: i64,
    /// Total time spent in compilation (seconds).
    pub time: f64,
}

/// JIT compiler statistics, including owned diagnostic strings.
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

/// Allocation-free values for one raw HotSpot GC collector slot.
///
/// Interpret `ordinal` using [`JvmCompatibility::collector_names`]. For
/// example, slot 2 is absent on JDK 8, is a G1 concurrent-cycle pause counter
/// on modern JDKs, and is a major-pause counter with generational ZGC.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[non_exhaustive]
pub struct GcCollectorNumericStats {
    /// HotSpot collector ordinal (0, 1, or 2).
    pub ordinal: u8,
    /// Invocation counter, or `None` when the slot is not published.
    pub invocations: Option<u64>,
    /// Accumulated counter time in seconds, or `None` when unavailable.
    pub time_s: Option<f64>,
}

/// Allocation-free numeric garbage collection statistics.
#[derive(Debug, Clone, Copy, Default)]
pub struct GcNumericStats {
    /// Historical generation-0 space-1 capacity (KB).
    pub s0c: f64,
    /// Historical generation-0 space-2 capacity (KB).
    pub s1c: f64,
    /// Survivor space 0 used (KB).
    pub s0u: f64,
    /// Survivor space 1 used (KB).
    pub s1u: f64,
    /// Historical generation-0 space-0 capacity (KB).
    pub ec: f64,
    /// Historical generation-0 space-0 used (KB).
    pub eu: f64,
    /// Generation-1 space-0 capacity (KB); this is the whole heap on
    /// non-generational ZGC/Shenandoah.
    pub oc: f64,
    /// Generation-1 space-0 used (KB); this is the whole heap on
    /// non-generational ZGC/Shenandoah.
    pub ou: f64,
    /// Metaspace capacity (KB).
    pub mc: f64,
    /// Metaspace used (KB).
    pub mu: f64,
    /// Compressed class space capacity (KB).
    pub ccsc: f64,
    /// Compressed class space used (KB).
    pub ccsu: f64,
    /// Collector slot 0 invocations (the historical jstat `YGC` column).
    pub ygc: u64,
    /// Collector slot 0 accumulated time (the historical `YGCT` column).
    pub ygct: f64,
    /// Collector slot 1 invocations (the historical jstat `FGC` column).
    pub fgc: u64,
    /// Collector slot 1 accumulated time (the historical `FGCT` column).
    pub fgct: f64,
    /// Collector slot 2 invocations (the JDK 11+ jstat `CGC` column).
    pub cgc: u64,
    /// Collector slot 2 accumulated time (the JDK 11+ `CGCT` column).
    pub cgct: f64,
    /// Total garbage collection time (seconds).
    pub gct: f64,
}

/// Garbage collection statistics, including owned cause strings.
#[derive(Debug, Clone, Default)]
pub struct GcStats {
    /// Historical generation-0 space-1 capacity (KB).
    pub s0c: f64,
    /// Historical generation-0 space-2 capacity (KB).
    pub s1c: f64,
    /// Survivor space 0 used (KB).
    pub s0u: f64,
    /// Survivor space 1 used (KB).
    pub s1u: f64,
    /// Historical generation-0 space-0 capacity (KB).
    pub ec: f64,
    /// Historical generation-0 space-0 used (KB).
    pub eu: f64,
    /// Generation-1 space-0 capacity (KB); this is the whole heap on
    /// non-generational ZGC/Shenandoah.
    pub oc: f64,
    /// Generation-1 space-0 used (KB); this is the whole heap on
    /// non-generational ZGC/Shenandoah.
    pub ou: f64,
    /// Metaspace capacity (KB).
    pub mc: f64,
    /// Metaspace used (KB).
    pub mu: f64,
    /// Compressed class space capacity (KB).
    pub ccsc: f64,
    /// Compressed class space used (KB).
    pub ccsu: f64,
    /// Collector slot 0 invocations (the historical jstat `YGC` column).
    pub ygc: u64,
    /// Collector slot 0 accumulated time (the historical `YGCT` column).
    pub ygct: f64,
    /// Collector slot 1 invocations (the historical jstat `FGC` column).
    pub fgc: u64,
    /// Collector slot 1 accumulated time (the historical `FGCT` column).
    pub fgct: f64,
    /// Collector slot 2 invocations (the JDK 11+ jstat `CGC` column).
    pub cgc: u64,
    /// Collector slot 2 accumulated time (the JDK 11+ `CGCT` column).
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

    /// Code Cache memory used (KB), or 0 when the JVM has no exact PerfData key.
    pub code_cache_used: f64,
    /// Code Cache capacity (KB), or 0 when unavailable.
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
