use super::JvmMonitorError;
use super::perfdata::{MAX_PERFDATA_SIZE, parse_snapshot};
use super::{BuiltinLongMetric, JvmMonitor, MetricKeySource, PerfValue};
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const FIXTURE_LEN: usize = 4096;
static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
enum FixtureValue<'a> {
    Long(i64),
    String(&'a str),
}

struct TempFixture {
    path: PathBuf,
}

impl TempFixture {
    fn create(bytes: &[u8]) -> Self {
        let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("jmon-rs-fixture-{}-{id}.bin", std::process::id()));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .expect("create fixture");
        file.write_all(bytes).expect("write fixture");
        file.sync_all().expect("sync fixture");
        Self { path }
    }
}

impl Drop for TempFixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[test]
fn parses_little_and_big_endian_entries() {
    for is_little_endian in [true, false] {
        let bytes = fixture(
            is_little_endian,
            true,
            1,
            &[
                ("test.long", FixtureValue::Long(42)),
                ("test.string", FixtureValue::String("hello")),
            ],
        );
        let (header, index) = parse_snapshot(&bytes).expect("valid fixture");
        assert_eq!(header.is_little_endian, is_little_endian);
        assert_eq!(index.len(), 2);

        let temp = TempFixture::create(&bytes);
        let monitor = JvmMonitor::connect_path(123, temp.path.clone()).expect("connect fixture");
        assert_eq!(monitor.read_long("test.long"), 42);
        assert!(matches!(
            monitor.read_metric("test.string"),
            Some(PerfValue::String(value)) if value == "hello"
        ));
    }
}

#[test]
fn malformed_structures_return_errors_instead_of_partial_indexes() {
    let valid = fixture(true, true, 1, &[("test.long", FixtureValue::Long(42))]);

    let mut bad_magic = valid.clone();
    bad_magic[0] = 0;
    assert!(parse_snapshot(&bad_magic).is_err());

    let mut bad_version = valid.clone();
    bad_version[5] = 3;
    assert!(parse_snapshot(&bad_version).is_err());

    let mut inaccessible = valid.clone();
    inaccessible[7] = 0;
    assert!(parse_snapshot(&inaccessible).is_err());

    let mut huge_count = valid.clone();
    put_u32(&mut huge_count, 28, u32::MAX, true);
    assert!(parse_snapshot(&huge_count).is_err());

    let mut zero_length = valid.clone();
    put_u32(&mut zero_length, 32, 0, true);
    assert!(parse_snapshot(&zero_length).is_err());

    let mut missing_name_terminator = valid.clone();
    let data_offset = get_u32(&missing_name_terminator, 48, true) as usize;
    missing_name_terminator[52..32 + data_offset].fill(b'x');
    assert!(parse_snapshot(&missing_name_terminator).is_err());
}

#[test]
fn oversized_perfdata_is_rejected_before_mapping_or_snapshot_allocation() {
    let temp = TempFixture::create(&[0_u8; 32]);
    OpenOptions::new()
        .write(true)
        .open(&temp.path)
        .expect("open oversized fixture")
        .set_len(MAX_PERFDATA_SIZE + 1)
        .expect("resize oversized fixture");
    assert!(matches!(
        JvmMonitor::connect_path(124, temp.path.clone()),
        Err(JvmMonitorError::InvalidFormat(_))
    ));
}

#[test]
fn parser_does_not_panic_on_mutated_structures() {
    let original = fixture(
        true,
        true,
        1,
        &[
            ("test.long", FixtureValue::Long(42)),
            ("test.string", FixtureValue::String("hello")),
        ],
    );
    let mut random = 0x9e37_79b9_7f4a_7c15_u64;
    for _ in 0..2_000 {
        let mut bytes = original.clone();
        for _ in 0..4 {
            random = random
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let offset = (random as usize) % 160;
            bytes[offset] ^= (random >> 32) as u8;
        }
        assert!(std::panic::catch_unwind(|| parse_snapshot(&bytes)).is_ok());
    }
}

#[test]
fn refresh_waits_for_timestamp_publication_and_adds_dynamic_entry() {
    let initial = fixture(
        true,
        true,
        10,
        &[
            ("sun.os.hrt.frequency", FixtureValue::Long(1_000_000_000)),
            ("base.metric", FixtureValue::Long(7)),
        ],
    );
    let complete = fixture(
        true,
        true,
        11,
        &[
            ("sun.os.hrt.frequency", FixtureValue::Long(1_000_000_000)),
            ("base.metric", FixtureValue::Long(7)),
            ("dynamic.metric", FixtureValue::Long(42)),
        ],
    );
    let temp = TempFixture::create(&initial);
    let monitor = JvmMonitor::connect_path(456, temp.path.clone()).expect("connect fixture");

    // Simulate HotSpot's early publication of used/num_entries while the new
    // entry is still incomplete and mod_timestamp remains unchanged.
    let mut early = initial.clone();
    let complete_used = get_u32(&complete, 8, true);
    let complete_count = get_u32(&complete, 28, true);
    put_u32(&mut early, 8, complete_used, true);
    put_u32(&mut early, 28, complete_count, true);
    write_existing(&temp.path, &early);
    assert_eq!(monitor.refresh().expect("transient refresh"), 0);
    assert_eq!(monitor.read_long("dynamic.metric"), 0);

    write_existing(&temp.path, &complete);
    assert_eq!(monitor.refresh().expect("published refresh"), 1);
    assert_eq!(monitor.read_long("dynamic.metric"), 42);
    let dynamic = monitor
        .metric_handle("dynamic.metric")
        .expect("resolve dynamic metric");
    assert_eq!(monitor.read_long_handle(dynamic), Some(42));
    assert_eq!(monitor.refresh().expect("idempotent refresh"), 0);

    let mut overflowed = complete.clone();
    put_u32(&mut overflowed, 12, 123, true);
    write_existing(&temp.path, &overflowed);
    assert_eq!(
        monitor
            .sample()
            .expect("degraded sample")
            .health
            .overflow_bytes,
        123
    );
}

#[test]
fn refresh_accepts_a_stable_entry_when_modification_timestamp_collides() {
    let initial = fixture(true, true, 20, &[("base.metric", FixtureValue::Long(7))]);
    let updated = fixture(
        true,
        true,
        20,
        &[
            ("base.metric", FixtureValue::Long(7)),
            ("colliding.metric", FixtureValue::Long(99)),
        ],
    );
    let temp = TempFixture::create(&initial);
    let monitor = JvmMonitor::connect_path(457, temp.path.clone()).expect("connect fixture");
    write_existing(&temp.path, &updated);

    assert_eq!(monitor.refresh().expect("stable colliding refresh"), 1);
    assert_eq!(monitor.read_long("colliding.metric"), 99);
}

#[test]
fn canonical_zero_is_not_treated_as_missing() {
    let bytes = fixture(
        true,
        true,
        1,
        &[
            (
                "sun.gc.generation.0.space.1.capacity",
                FixtureValue::Long(0),
            ),
            (
                "sun.gc.generation.0.space.1.maxCapacity",
                FixtureValue::Long(1024),
            ),
        ],
    );
    let temp = TempFixture::create(&bytes);
    let monitor = JvmMonitor::connect_path(789, temp.path.clone()).expect("connect fixture");
    assert_eq!(monitor.get_gc_stats().s0c, 0.0);
    assert_eq!(
        monitor.read_builtin_long(BuiltinLongMetric::Survivor0Capacity),
        Some(0)
    );
    assert_eq!(
        monitor
            .metric_resolution(BuiltinLongMetric::Survivor0Capacity)
            .key,
        Some("sun.gc.generation.0.space.1.capacity")
    );
}

#[test]
fn canonical_builtin_key_wins_over_vendor_alias_even_when_its_value_is_zero() {
    let bytes = fixture(
        true,
        true,
        1,
        &[
            ("java.cls.loadedClasses", FixtureValue::Long(0)),
            ("sun.cls.loadedClasses", FixtureValue::Long(91)),
        ],
    );
    let temp = TempFixture::create(&bytes);
    let monitor = JvmMonitor::connect_path(791, temp.path.clone()).expect("connect fixture");

    assert_eq!(
        monitor.read_builtin_long(BuiltinLongMetric::LoadedClasses),
        Some(0)
    );
    assert_eq!(monitor.read_long("sun.cls.loadedClasses"), 91);
    let resolution = monitor.metric_resolution(BuiltinLongMetric::LoadedClasses);
    assert_eq!(resolution.key, Some("java.cls.loadedClasses"));
    assert_eq!(resolution.source, Some(MetricKeySource::OpenJdk));
}

#[test]
fn survivor_max_capacity_is_not_mistaken_for_current_capacity() {
    let bytes = fixture(
        true,
        true,
        1,
        &[(
            "sun.gc.generation.0.space.1.maxCapacity",
            FixtureValue::Long(1024),
        )],
    );
    let temp = TempFixture::create(&bytes);
    let monitor = JvmMonitor::connect_path(792, temp.path.clone()).expect("connect fixture");

    assert_eq!(
        monitor.read_builtin_long(BuiltinLongMetric::Survivor0Capacity),
        None
    );
    assert_eq!(
        monitor.metric_resolution(BuiltinLongMetric::Survivor0Capacity),
        super::MetricResolution {
            metric: BuiltinLongMetric::Survivor0Capacity,
            key: None,
            source: None,
        }
    );
    // The legacy aggregate API remains source-compatible and maps absence to zero.
    assert_eq!(monitor.get_gc_numeric_stats().s0c, 0.0);
}

#[test]
fn jdk7_permgen_does_not_alias_metaspace_or_code_cache() {
    let bytes = fixture(
        true,
        true,
        1,
        &[
            (
                "sun.gc.generation.2.space.0.capacity",
                FixtureValue::Long(2048),
            ),
            ("sun.gc.generation.2.space.0.used", FixtureValue::Long(1024)),
        ],
    );
    let temp = TempFixture::create(&bytes);
    let monitor = JvmMonitor::connect_path(793, temp.path.clone()).expect("connect fixture");

    assert_eq!(
        monitor.read_builtin_long(BuiltinLongMetric::PermGenCapacity),
        Some(2048)
    );
    assert_eq!(
        monitor.read_builtin_long(BuiltinLongMetric::PermGenUsed),
        Some(1024)
    );
    assert_eq!(
        monitor
            .metric_resolution(BuiltinLongMetric::PermGenCapacity)
            .source,
        Some(MetricKeySource::LegacyHotSpot)
    );
    for metric in [
        BuiltinLongMetric::MetaspaceCapacity,
        BuiltinLongMetric::MetaspaceUsed,
        BuiltinLongMetric::CodeCacheCapacity,
        BuiltinLongMetric::CodeCacheUsed,
    ] {
        assert_eq!(monitor.read_builtin_long(metric), None, "{metric:?}");
        assert!(
            !monitor.metric_resolution(metric).is_available(),
            "{metric:?}"
        );
    }
}

#[test]
fn gc_total_time_supports_jdk8_without_and_newer_jdks_with_collector_two() {
    let jdk8 = fixture(
        true,
        true,
        1,
        &[
            ("sun.os.hrt.frequency", FixtureValue::Long(1000)),
            ("sun.gc.collector.0.invocations", FixtureValue::Long(2)),
            ("sun.gc.collector.0.time", FixtureValue::Long(100)),
            ("sun.gc.collector.1.invocations", FixtureValue::Long(1)),
            ("sun.gc.collector.1.time", FixtureValue::Long(200)),
        ],
    );
    let jdk8_temp = TempFixture::create(&jdk8);
    let jdk8_monitor =
        JvmMonitor::connect_path(794, jdk8_temp.path.clone()).expect("connect JDK 8 fixture");
    let jdk8_gc = jdk8_monitor.get_gc_numeric_stats();
    assert_eq!(
        jdk8_monitor.read_builtin_long(BuiltinLongMetric::Collector2Time),
        None
    );
    assert_eq!(jdk8_gc.cgc, 0);
    assert_eq!(jdk8_gc.cgct, 0.0);
    assert!((jdk8_gc.gct - 0.3).abs() < f64::EPSILON);

    let newer = fixture(
        true,
        true,
        1,
        &[
            ("sun.os.hrt.frequency", FixtureValue::Long(1000)),
            ("sun.gc.collector.0.invocations", FixtureValue::Long(2)),
            ("sun.gc.collector.0.time", FixtureValue::Long(100)),
            ("sun.gc.collector.1.invocations", FixtureValue::Long(1)),
            ("sun.gc.collector.1.time", FixtureValue::Long(200)),
            ("sun.gc.collector.2.invocations", FixtureValue::Long(3)),
            ("sun.gc.collector.2.time", FixtureValue::Long(300)),
        ],
    );
    let newer_temp = TempFixture::create(&newer);
    let newer_monitor =
        JvmMonitor::connect_path(795, newer_temp.path.clone()).expect("connect newer JDK fixture");
    let newer_gc = newer_monitor.get_gc_numeric_stats();
    assert_eq!(
        newer_monitor.read_builtin_long(BuiltinLongMetric::Collector2Time),
        Some(300)
    );
    assert_eq!(newer_gc.cgc, 3);
    assert_eq!(newer_gc.cgct, 0.3);
    assert!((newer_gc.gct - 0.6).abs() < f64::EPSILON);
}

#[test]
fn compatibility_preserves_zgc_generational_zgc_and_shenandoah_profiles() {
    type NameProfile = (
        &'static str,
        [Option<&'static str>; 3],
        [Option<&'static str>; 3],
        [[Option<&'static str>; 3]; 3],
    );
    let profiles: [NameProfile; 3] = [
        (
            "ZGC",
            [None, None, Some("Z concurrent cycle pauses")],
            [None, Some("old"), None],
            [
                [None, None, None],
                [Some("space"), None, None],
                [None, None, None],
            ],
        ),
        (
            "Generational ZGC",
            [
                Some("ZGC minor collection pauses"),
                None,
                Some("ZGC major collection pauses"),
            ],
            [Some("young"), Some("old"), None],
            [
                [Some("space"), None, None],
                [Some("space"), None, None],
                [None, None, None],
            ],
        ),
        (
            "Shenandoah",
            [Some("Shenandoah partial"), Some("Shenandoah full"), None],
            [Some("Young"), Some("Heap"), None],
            [
                [None, None, None],
                [Some("Heap"), None, None],
                [None, None, None],
            ],
        ),
    ];

    for (profile_index, (policy, collectors, generations, spaces)) in
        profiles.into_iter().enumerate()
    {
        const COLLECTOR_KEYS: [&str; 3] = [
            "sun.gc.collector.0.name",
            "sun.gc.collector.1.name",
            "sun.gc.collector.2.name",
        ];
        const GENERATION_KEYS: [&str; 3] = [
            "sun.gc.generation.0.name",
            "sun.gc.generation.1.name",
            "sun.gc.generation.2.name",
        ];
        const SPACE_KEYS: [[&str; 3]; 3] = [
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

        let mut entries = vec![
            ("java.property.java.version", FixtureValue::String("21")),
            ("sun.gc.policy.name", FixtureValue::String(policy)),
        ];
        for ordinal in 0..3 {
            if let Some(name) = collectors[ordinal] {
                entries.push((COLLECTOR_KEYS[ordinal], FixtureValue::String(name)));
            }
            if let Some(name) = generations[ordinal] {
                entries.push((GENERATION_KEYS[ordinal], FixtureValue::String(name)));
            }
            for space in 0..3 {
                if let Some(name) = spaces[ordinal][space] {
                    entries.push((SPACE_KEYS[ordinal][space], FixtureValue::String(name)));
                }
            }
        }

        let bytes = fixture(true, true, 1, &entries);
        let temp = TempFixture::create(&bytes);
        let monitor = JvmMonitor::connect_path(800 + profile_index as u32, temp.path.clone())
            .expect("connect GC profile fixture");
        let compatibility = monitor.compatibility();

        assert_eq!(compatibility.gc_policy.as_deref(), Some(policy));
        assert_eq!(
            compatibility.collector_names,
            collectors.map(|name| name.map(str::to_owned))
        );
        assert_eq!(
            compatibility.generation_names,
            generations.map(|name| name.map(str::to_owned))
        );
        assert_eq!(
            compatibility.space_names,
            spaces.map(|generation| generation.map(|name| name.map(str::to_owned)))
        );
    }
}

#[test]
fn refresh_keeps_the_initial_alias_binding_when_a_canonical_key_appears() {
    let initial = fixture(
        true,
        true,
        30,
        &[("sun.ci.totalCompilations", FixtureValue::Long(17))],
    );
    let updated = fixture(
        true,
        true,
        31,
        &[
            ("sun.ci.totalCompilations", FixtureValue::Long(17)),
            ("sun.ci.totalCompiles", FixtureValue::Long(0)),
        ],
    );
    let temp = TempFixture::create(&initial);
    let monitor = JvmMonitor::connect_path(796, temp.path.clone()).expect("connect fixture");

    assert_eq!(
        monitor.read_builtin_long(BuiltinLongMetric::Compilations),
        Some(17)
    );
    assert_eq!(
        monitor
            .metric_resolution(BuiltinLongMetric::Compilations)
            .source,
        Some(MetricKeySource::VendorAlias)
    );

    write_existing(&temp.path, &updated);
    assert_eq!(monitor.refresh().expect("refresh canonical counter"), 1);
    assert_eq!(
        monitor.read_builtin_long(BuiltinLongMetric::Compilations),
        Some(17)
    );
    assert_eq!(monitor.read_long("sun.ci.totalCompiles"), 0);
    let resolution = monitor.metric_resolution(BuiltinLongMetric::Compilations);
    assert_eq!(resolution.key, Some("sun.ci.totalCompilations"));
    assert_eq!(resolution.source, Some(MetricKeySource::VendorAlias));
}

#[test]
fn refresh_binds_a_builtin_metric_that_was_initially_missing() {
    let initial = fixture(true, true, 40, &[("base.metric", FixtureValue::Long(1))]);
    let updated = fixture(
        true,
        true,
        41,
        &[
            ("base.metric", FixtureValue::Long(1)),
            ("sun.rt.safepointSyncTime", FixtureValue::Long(23)),
        ],
    );
    let temp = TempFixture::create(&initial);
    let monitor = JvmMonitor::connect_path(797, temp.path.clone()).expect("connect fixture");

    assert_eq!(
        monitor.read_builtin_long(BuiltinLongMetric::SafepointSyncTime),
        None
    );
    assert!(
        !monitor
            .metric_resolution(BuiltinLongMetric::SafepointSyncTime)
            .is_available()
    );

    write_existing(&temp.path, &updated);
    assert_eq!(monitor.refresh().expect("refresh missing built-in"), 1);
    assert_eq!(
        monitor.read_builtin_long(BuiltinLongMetric::SafepointSyncTime),
        Some(23)
    );
    let resolution = monitor.metric_resolution(BuiltinLongMetric::SafepointSyncTime);
    assert_eq!(resolution.key, Some("sun.rt.safepointSyncTime"));
    assert_eq!(resolution.source, Some(MetricKeySource::OpenJdk));
}

#[test]
fn refresh_picks_up_a_late_timer_frequency_for_seconds_conversion() {
    let initial = fixture(
        true,
        true,
        50,
        &[
            ("java.ci.totalTime", FixtureValue::Long(250)),
            ("sun.gc.collector.0.time", FixtureValue::Long(250)),
        ],
    );
    let updated = fixture(
        true,
        true,
        51,
        &[
            ("java.ci.totalTime", FixtureValue::Long(250)),
            ("sun.gc.collector.0.time", FixtureValue::Long(250)),
            ("sun.os.hrt.frequency", FixtureValue::Long(1000)),
        ],
    );
    let temp = TempFixture::create(&initial);
    let monitor = JvmMonitor::connect_path(798, temp.path.clone()).expect("connect fixture");

    assert_eq!(monitor.compatibility().timer_frequency_hz, None);
    assert_eq!(monitor.get_compiler_numeric_stats().time, 0.0);
    assert_eq!(monitor.get_gc_collector_numeric_stats()[0].time_s, None);

    write_existing(&temp.path, &updated);
    assert_eq!(monitor.refresh().expect("refresh timer frequency"), 1);
    assert_eq!(monitor.compatibility().timer_frequency_hz, Some(1000));
    assert_eq!(monitor.get_compiler_numeric_stats().time, 0.25);
    assert_eq!(
        monitor.get_gc_collector_numeric_stats()[0].time_s,
        Some(0.25)
    );
}

#[test]
fn class_and_compiler_stats_support_current_hotspot_counter_names() {
    let bytes = fixture(
        true,
        true,
        1,
        &[
            ("java.cls.loadedClasses", FixtureValue::Long(4)),
            ("java.cls.sharedLoadedClasses", FixtureValue::Long(401)),
            ("sun.cls.loadedBytes", FixtureValue::Long(6_216)),
            ("sun.cls.sharedLoadedBytes", FixtureValue::Long(1_035_696)),
            ("sun.ci.totalCompiles", FixtureValue::Long(5)),
            ("sun.ci.totalInvalidates", FixtureValue::Long(2)),
            ("java.ci.totalTime", FixtureValue::Long(10)),
            ("sun.ci.lastFailedType", FixtureValue::Long(0)),
        ],
    );
    let temp = TempFixture::create(&bytes);
    let monitor = JvmMonitor::connect_path(790, temp.path.clone()).expect("connect fixture");

    let classes = monitor.get_class_stats();
    assert_eq!(classes.loaded, 405);
    assert_eq!(classes.bytes, 1_041_912.0 / 1024.0);
    let compiler = monitor.get_compiler_stats();
    assert_eq!(compiler.compiled, 5);
    assert_eq!(compiler.invalid, 2);
    assert_eq!(compiler.failed_type, "0");
}

#[cfg(unix)]
#[test]
fn checked_sample_detects_an_unlinked_target() {
    let bytes = fixture(true, true, 1, &[("test.long", FixtureValue::Long(42))]);
    let temp = TempFixture::create(&bytes);
    let monitor = JvmMonitor::connect_path(999, temp.path.clone()).expect("connect fixture");
    fs::remove_file(&temp.path).expect("unlink fixture");

    assert!(matches!(
        monitor.sample(),
        Err(JvmMonitorError::ProcessNotFound(999))
    ));
    // Compatibility reads are deliberately syscall-free and can still see
    // the old mapping. Long-running callers must use sample/refresh.
    assert_eq!(monitor.read_long("test.long"), 42);
}

#[test]
fn monitor_remains_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<JvmMonitor>();
}

#[test]
fn metric_handles_cannot_be_used_with_another_monitor() {
    let bytes = fixture(true, true, 1, &[("test.long", FixtureValue::Long(42))]);
    let first_temp = TempFixture::create(&bytes);
    let second_temp = TempFixture::create(&bytes);
    let first = JvmMonitor::connect_path(1_001, first_temp.path.clone()).expect("first monitor");
    let second = JvmMonitor::connect_path(1_002, second_temp.path.clone()).expect("second monitor");
    let handle = first.metric_handle("test.long").expect("metric handle");

    assert_eq!(first.read_long_handle(handle), Some(42));
    assert_eq!(second.read_long_handle(handle), None);
}

#[test]
fn concurrent_refreshes_are_serialized_without_false_stale_errors() {
    let bytes = fixture(true, true, 1, &[("test.long", FixtureValue::Long(42))]);
    let temp = TempFixture::create(&bytes);
    let monitor = Arc::new(
        JvmMonitor::connect_path(1_000, temp.path.clone()).expect("connect concurrent fixture"),
    );
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let monitor = Arc::clone(&monitor);
            std::thread::spawn(move || {
                for _ in 0..100 {
                    assert_eq!(monitor.refresh().expect("concurrent refresh"), 0);
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("refresh worker");
    }
}

#[test]
fn parses_linux_start_time_after_a_tricky_command_name() {
    let stat =
        "123 (name with ) a parenthesis) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 424242 0";
    assert_eq!(JvmMonitor::parse_process_start_time(stat), Some(424242));
}

fn write_existing(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open fixture writer");
    file.seek(SeekFrom::Start(0)).expect("seek fixture");
    file.write_all(bytes).expect("update fixture");
    file.sync_all().expect("sync fixture update");
}

fn fixture(
    is_little_endian: bool,
    accessible: bool,
    modification_timestamp: i64,
    entries: &[(&str, FixtureValue<'_>)],
) -> Vec<u8> {
    let mut bytes = vec![0_u8; 32];

    for (name, value) in entries {
        let entry_start = bytes.len();
        let name_offset = 20;
        let name_end = name_offset + name.len() + 1;
        let data_offset = match value {
            FixtureValue::Long(_) => align_absolute(entry_start, name_end, 8),
            FixtureValue::String(_) => align(name_end, 4),
        };
        let (data_type, units, vector_length, data_len) = match value {
            FixtureValue::Long(_) => (b'J', 1_u8, 0_usize, 8_usize),
            FixtureValue::String(value) => (b'B', 5_u8, value.len() + 1, value.len() + 1),
        };
        let entry_len = align(data_offset + data_len, 4);
        bytes.resize(entry_start + entry_len, 0);

        put_u32(&mut bytes, entry_start, entry_len as u32, is_little_endian);
        put_u32(
            &mut bytes,
            entry_start + 4,
            name_offset as u32,
            is_little_endian,
        );
        put_u32(
            &mut bytes,
            entry_start + 8,
            vector_length as u32,
            is_little_endian,
        );
        bytes[entry_start + 12] = data_type;
        bytes[entry_start + 13] = 1;
        bytes[entry_start + 14] = units;
        bytes[entry_start + 15] = 3;
        put_u32(
            &mut bytes,
            entry_start + 16,
            data_offset as u32,
            is_little_endian,
        );
        bytes[entry_start + name_offset..entry_start + name_offset + name.len()]
            .copy_from_slice(name.as_bytes());

        let data_start = entry_start + data_offset;
        match value {
            FixtureValue::Long(value) => {
                let encoded = if is_little_endian {
                    value.to_le_bytes()
                } else {
                    value.to_be_bytes()
                };
                bytes[data_start..data_start + 8].copy_from_slice(&encoded);
            }
            FixtureValue::String(value) => {
                bytes[data_start..data_start + value.len()].copy_from_slice(value.as_bytes());
            }
        }
    }

    let used = bytes.len();
    assert!(used <= FIXTURE_LEN);
    bytes.resize(FIXTURE_LEN, 0);
    bytes[0..4].copy_from_slice(&0xcafec0c0_u32.to_be_bytes());
    bytes[4] = u8::from(is_little_endian);
    bytes[5] = 2;
    bytes[6] = 0;
    bytes[7] = u8::from(accessible);
    put_u32(&mut bytes, 8, used as u32, is_little_endian);
    put_u32(&mut bytes, 12, 0, is_little_endian);
    put_i64(&mut bytes, 16, modification_timestamp, is_little_endian);
    put_u32(&mut bytes, 24, 32, is_little_endian);
    put_u32(&mut bytes, 28, entries.len() as u32, is_little_endian);
    bytes
}

fn align(value: usize, alignment: usize) -> usize {
    (value + alignment - 1) & !(alignment - 1)
}

fn align_absolute(entry_start: usize, relative: usize, alignment: usize) -> usize {
    let absolute = align(entry_start + relative, alignment);
    absolute - entry_start
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32, is_little_endian: bool) {
    let encoded = if is_little_endian {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    };
    bytes[offset..offset + 4].copy_from_slice(&encoded);
}

fn get_u32(bytes: &[u8], offset: usize, is_little_endian: bool) -> u32 {
    let encoded: [u8; 4] = bytes[offset..offset + 4].try_into().expect("u32 bytes");
    if is_little_endian {
        u32::from_le_bytes(encoded)
    } else {
        u32::from_be_bytes(encoded)
    }
}

fn put_i64(bytes: &mut [u8], offset: usize, value: i64, is_little_endian: bool) {
    let encoded = if is_little_endian {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    };
    bytes[offset..offset + 8].copy_from_slice(&encoded);
}
