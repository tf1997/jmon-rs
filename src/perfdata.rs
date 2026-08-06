use crate::JvmMonitorError;
use byteorder::{BigEndian, ByteOrder, LittleEndian};
use memmap2::{MmapOptions, MmapRaw};
use std::collections::HashMap;
use std::fs::File;
use std::ptr;

pub(crate) const PROLOGUE_SIZE: usize = 32;
pub(crate) const MAX_PERFDATA_SIZE: u64 = 64 * 1024 * 1024;
const ENTRY_HEADER_SIZE: usize = 20;
const MAX_STRUCTURE_CAPTURE_ATTEMPTS: usize = 5;
const MAX_ENTRY_NAME_LENGTH: usize = 64 * 1024;
const MAX_STRING_VECTOR_LENGTH: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EntryMeta {
    pub(crate) data_type: u8,
    pub(crate) data_offset: usize,
    pub(crate) vector_length: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PerfHeader {
    pub(crate) is_little_endian: bool,
    pub(crate) accessible: bool,
    pub(crate) used: usize,
    pub(crate) overflow: u32,
    pub(crate) modification_timestamp: i64,
    pub(crate) entry_offset: usize,
    pub(crate) num_entries: usize,
}

impl PerfHeader {
    pub(crate) fn validate_layout(&self, buffer_len: usize) -> Result<(), JvmMonitorError> {
        if self.used < PROLOGUE_SIZE || self.used > buffer_len {
            return Err(invalid(format!(
                "used bytes {} outside mapping length {}",
                self.used, buffer_len
            )));
        }
        if self.entry_offset < PROLOGUE_SIZE
            || self.entry_offset > self.used
            || !self.entry_offset.is_multiple_of(4)
        {
            return Err(invalid(format!(
                "invalid entry offset {} for used size {}",
                self.entry_offset, self.used
            )));
        }

        let entry_bytes = self.used - self.entry_offset;
        let maximum_entries = entry_bytes / ENTRY_HEADER_SIZE;
        if self.num_entries > maximum_entries {
            return Err(invalid(format!(
                "entry count {} exceeds structural maximum {}",
                self.num_entries, maximum_entries
            )));
        }
        Ok(())
    }

    fn same_structure(&self, other: &Self) -> bool {
        self.is_little_endian == other.is_little_endian
            && self.accessible == other.accessible
            && self.used == other.used
            && self.modification_timestamp == other.modification_timestamp
            && self.entry_offset == other.entry_offset
            && self.num_entries == other.num_entries
    }
}

/// Raw mapping wrapper that never exposes references into memory concurrently
/// modified by the target JVM.
#[derive(Debug)]
pub(crate) struct PerfMemory {
    mapping: MmapRaw,
}

impl PerfMemory {
    pub(crate) fn map(file: &File) -> Result<Self, JvmMonitorError> {
        let file_len = file.metadata()?.len();
        if !(PROLOGUE_SIZE as u64..=MAX_PERFDATA_SIZE).contains(&file_len) {
            return Err(invalid(format!(
                "perfdata file size {file_len} is outside the supported range"
            )));
        }
        let mapping = MmapOptions::new().map_raw_read_only(file)?;
        if mapping.len() < PROLOGUE_SIZE || mapping.len() as u64 > MAX_PERFDATA_SIZE {
            return Err(invalid(format!(
                "mapping size {} is outside the supported range",
                mapping.len()
            )));
        }
        Ok(Self { mapping })
    }

    pub(crate) fn len(&self) -> usize {
        self.mapping.len()
    }

    pub(crate) fn is_accessible(&self) -> Option<bool> {
        if self.len() <= 7 {
            return None;
        }
        // SAFETY: offset 7 is within the prologue checked by `map`.
        Some(unsafe { ptr::read_volatile(self.mapping.as_ptr().add(7)) } != 0)
    }

    pub(crate) fn header(&self) -> Result<PerfHeader, JvmMonitorError> {
        let bytes = self
            .copy_bytes(0, PROLOGUE_SIZE)
            .ok_or_else(|| invalid("could not read perfdata prologue"))?;
        parse_header(&bytes)
    }

    pub(crate) fn capture_index(
        &self,
    ) -> Result<(PerfHeader, HashMap<String, EntryMeta>), JvmMonitorError> {
        let mut last_parse_error = None;
        for _ in 0..MAX_STRUCTURE_CAPTURE_ATTEMPTS {
            let before = self.header()?;
            if !before.accessible {
                return Err(invalid("target perfdata is not accessible"));
            }
            before.validate_layout(self.len())?;

            let snapshot = self
                .copy_bytes(0, before.used)
                .ok_or_else(|| invalid("perfdata changed size while being captured"))?;
            let after = self.header()?;

            if before.same_structure(&after) {
                match parse_index(&snapshot, &before) {
                    Ok(index) => return Ok((before, index)),
                    Err(error) => last_parse_error = Some(error),
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        Err(last_parse_error.unwrap_or_else(|| {
            invalid("perfdata structure kept changing while the index was captured")
        }))
    }

    pub(crate) fn read_i64(&self, offset: usize, is_little_endian: bool) -> Option<i64> {
        let end = offset.checked_add(8)?;
        if end > self.len() {
            return None;
        }

        let mut bytes = [0_u8; 8];
        for (index, byte) in bytes.iter_mut().enumerate() {
            // SAFETY: bounds were checked above. Volatile byte loads avoid
            // constructing a Rust reference to memory the JVM may update.
            *byte = unsafe { ptr::read_volatile(self.mapping.as_ptr().add(offset + index)) };
        }

        Some(if is_little_endian {
            LittleEndian::read_i64(&bytes)
        } else {
            BigEndian::read_i64(&bytes)
        })
    }

    pub(crate) fn read_string(&self, offset: usize, maximum_len: usize) -> Option<String> {
        let mut bytes = self.copy_bytes(offset, maximum_len)?;
        let string_len = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len());
        bytes.truncate(string_len);
        match String::from_utf8(bytes) {
            Ok(value) => Some(value),
            Err(error) => Some(String::from_utf8_lossy(error.as_bytes()).into_owned()),
        }
    }

    fn copy_bytes(&self, offset: usize, len: usize) -> Option<Vec<u8>> {
        let end = offset.checked_add(len)?;
        if end > self.len() {
            return None;
        }

        let mut result = Vec::new();
        result.try_reserve_exact(len).ok()?;
        for index in offset..end {
            // SAFETY: the full range was checked before the loop. MmapRaw does
            // not create references, and the volatile load observes live data.
            result.push(unsafe { ptr::read_volatile(self.mapping.as_ptr().add(index)) });
        }
        Some(result)
    }
}

pub(crate) fn parse_snapshot(
    bytes: &[u8],
) -> Result<(PerfHeader, HashMap<String, EntryMeta>), JvmMonitorError> {
    let header = parse_header(bytes)?;
    if !header.accessible {
        return Err(invalid("target perfdata is not accessible"));
    }
    header.validate_layout(bytes.len())?;
    let index = parse_index(bytes, &header)?;
    Ok((header, index))
}

fn parse_header(bytes: &[u8]) -> Result<PerfHeader, JvmMonitorError> {
    if bytes.len() < PROLOGUE_SIZE {
        return Err(invalid("perfdata prologue is truncated"));
    }
    if BigEndian::read_u32(&bytes[0..4]) != 0xcafec0c0 {
        return Err(invalid("invalid magic number"));
    }

    let is_little_endian = match bytes[4] {
        0 => false,
        1 => true,
        value => return Err(invalid(format!("invalid byte-order marker {value}"))),
    };
    if bytes[5] != 2 || bytes[6] != 0 {
        return Err(invalid(format!(
            "unsupported perfdata version {}.{}",
            bytes[5], bytes[6]
        )));
    }

    Ok(PerfHeader {
        is_little_endian,
        accessible: bytes[7] != 0,
        used: read_u32(&bytes[8..12], is_little_endian) as usize,
        overflow: read_u32(&bytes[12..16], is_little_endian),
        modification_timestamp: read_i64(&bytes[16..24], is_little_endian),
        entry_offset: read_u32(&bytes[24..28], is_little_endian) as usize,
        num_entries: read_u32(&bytes[28..32], is_little_endian) as usize,
    })
}

fn parse_index(
    bytes: &[u8],
    header: &PerfHeader,
) -> Result<HashMap<String, EntryMeta>, JvmMonitorError> {
    let mut index = HashMap::new();
    index.try_reserve(header.num_entries).map_err(|_| {
        invalid(format!(
            "could not reserve index capacity for {} entries",
            header.num_entries
        ))
    })?;
    let mut cursor = header.entry_offset;

    for entry_number in 0..header.num_entries {
        if !cursor.is_multiple_of(4) {
            return Err(invalid(format!(
                "entry {entry_number} starts at unaligned offset {cursor}"
            )));
        }
        let entry_header_end = cursor
            .checked_add(ENTRY_HEADER_SIZE)
            .ok_or_else(|| invalid("entry header offset overflow"))?;
        if entry_header_end > header.used {
            return Err(invalid(format!(
                "entry {entry_number} header exceeds used data"
            )));
        }

        let entry_len = read_u32(&bytes[cursor..cursor + 4], header.is_little_endian) as usize;
        if entry_len < ENTRY_HEADER_SIZE || !entry_len.is_multiple_of(4) {
            return Err(invalid(format!(
                "entry {entry_number} has invalid length {entry_len}"
            )));
        }
        let entry_end = cursor
            .checked_add(entry_len)
            .ok_or_else(|| invalid("entry length overflow"))?;
        if entry_end > header.used {
            return Err(invalid(format!(
                "entry {entry_number} extends beyond used data"
            )));
        }

        let name_offset =
            read_u32(&bytes[cursor + 4..cursor + 8], header.is_little_endian) as usize;
        let vector_length =
            read_u32(&bytes[cursor + 8..cursor + 12], header.is_little_endian) as usize;
        let data_type = bytes[cursor + 12];
        let units = bytes[cursor + 14];
        let data_offset =
            read_u32(&bytes[cursor + 16..cursor + 20], header.is_little_endian) as usize;

        if name_offset < ENTRY_HEADER_SIZE || name_offset >= data_offset || data_offset > entry_len
        {
            return Err(invalid(format!(
                "entry {entry_number} has invalid name/data offsets {name_offset}/{data_offset}"
            )));
        }

        let name_start = cursor
            .checked_add(name_offset)
            .ok_or_else(|| invalid("name offset overflow"))?;
        let data_start = cursor
            .checked_add(data_offset)
            .ok_or_else(|| invalid("data offset overflow"))?;
        let name_region = &bytes[name_start..data_start];
        if name_region.len() > MAX_ENTRY_NAME_LENGTH {
            return Err(invalid(format!(
                "entry {entry_number} name region is too large"
            )));
        }
        let name_len = name_region
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| invalid(format!("entry {entry_number} name is not NUL-terminated")))?;
        if name_len == 0 {
            return Err(invalid(format!("entry {entry_number} has an empty name")));
        }
        let name = std::str::from_utf8(&name_region[..name_len])
            .map_err(|_| invalid(format!("entry {entry_number} name is not valid UTF-8")))?
            .to_owned();

        match data_type {
            b'J' if vector_length == 0
                && data_start.checked_add(8).is_none_or(|end| end > entry_end) =>
            {
                return Err(invalid(format!(
                    "long entry {name} does not contain eight data bytes"
                )));
            }
            b'B' if vector_length > 0 => {
                if units != 5 {
                    return Err(invalid(format!(
                        "byte-vector entry {name} does not have string units"
                    )));
                }
                if vector_length > MAX_STRING_VECTOR_LENGTH {
                    return Err(invalid(format!(
                        "byte-vector entry {name} exceeds the supported string size"
                    )));
                }
                if data_start
                    .checked_add(vector_length)
                    .is_none_or(|end| end > entry_end)
                {
                    return Err(invalid(format!(
                        "byte-vector entry {name} exceeds its entry"
                    )));
                }
            }
            _ => {}
        }

        if index
            .insert(
                name.clone(),
                EntryMeta {
                    data_type,
                    data_offset: data_start,
                    vector_length,
                },
            )
            .is_some()
        {
            return Err(invalid(format!("duplicate perfdata entry {name}")));
        }
        cursor = entry_end;
    }

    if cursor != header.used {
        return Err(invalid(format!(
            "parsed entries end at {cursor}, but used data ends at {}",
            header.used
        )));
    }

    Ok(index)
}

fn read_u32(bytes: &[u8], is_little_endian: bool) -> u32 {
    if is_little_endian {
        LittleEndian::read_u32(bytes)
    } else {
        BigEndian::read_u32(bytes)
    }
}

fn read_i64(bytes: &[u8], is_little_endian: bool) -> i64 {
    if is_little_endian {
        LittleEndian::read_i64(bytes)
    } else {
        BigEndian::read_i64(bytes)
    }
}

fn invalid(message: impl Into<String>) -> JvmMonitorError {
    JvmMonitorError::InvalidFormat(message.into())
}
