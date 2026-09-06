//! Issue #61 CP4/CP5 — read-only physical source enumeration + raw byte access.
//!
//! CP4 uses [`enumerate`] (zero-access handles: query IOCTLs only, structurally
//! cannot read or write disk contents) to build this boot's source-observation
//! epoch mapping.
//!
//! CP5 uses [`RawReadSource::open`] — the ONLY function in this probe that
//! requests a non-zero access right, and it requests exactly `GENERIC_READ`
//! (never `GENERIC_WRITE`). It obtains the device length through three
//! read-only IOCTLs and performs bounded raw reads. No write API, no
//! state-mutating `DeviceIoControl`, no filesystem mount / lock / dismount /
//! repair is ever issued.
//!
//! `\\.\PhysicalDriveN`, model, serial, bus type and size are Agent-local lab
//! EVIDENCE ONLY — never a cross-boundary identity and never a selection input.

/// Instrumentation the CP4/CP5 runner asserts on. Only [`RawReadSource`]
/// mutates the data-handle counters; enumeration never does.
#[derive(Debug, Default, Clone, Copy)]
pub struct Counters {
    pub resolution_attempt_count: u64,
    pub resolution_success_count: u64,
    pub data_device_open_count: u64,
    pub data_read_count: u64,
}

/// One enumerated local source. Only `agent_source_id` crosses the boundary.
#[derive(Debug, Clone)]
pub struct LocalSource {
    pub agent_source_id: String,
    pub local_locator: String,
    pub size_bytes: u64,
    pub vendor: String,
    pub product: String,
    pub serial: String,
    pub bus_type: String,
    pub removable: bool,
}

pub struct SourceEpoch {
    pub observation_id: String,
    pub sources: Vec<LocalSource>,
}

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Canonical RFC 4648 base64url, no padding.
pub fn base64url_nopad(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
        out.push(B64URL[((n >> 18) & 63) as usize] as char);
        out.push(B64URL[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64URL[((n >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(B64URL[(n & 63) as usize] as char);
        }
    }
    out
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    getrandom::getrandom(&mut buf).expect("OS CSPRNG must be available");
    buf
}

/// 32 raw CSPRNG bytes -> 43-char base64url (the `boot_nonce` idiom, RF-3).
fn new_observation_id() -> String {
    base64url_nopad(&random_bytes::<32>())
}

/// Opaque, freshly minted, unique within the epoch; never derived from a
/// local locator / order / model / serial.
fn new_agent_source_id() -> String {
    base64url_nopad(&random_bytes::<18>())
}

#[cfg(windows)]
pub fn enumerate() -> SourceEpoch {
    let mut sources = win::enumerate_physical_drives();
    for src in &mut sources {
        src.agent_source_id = new_agent_source_id();
    }
    SourceEpoch {
        observation_id: new_observation_id(),
        sources,
    }
}

#[cfg(not(windows))]
pub fn enumerate() -> SourceEpoch {
    // Developer-host build: two synthetic sources so the resolver / evidence
    // path can be exercised. Never used in WinPE.
    SourceEpoch {
        observation_id: new_observation_id(),
        sources: vec![
            LocalSource {
                agent_source_id: new_agent_source_id(),
                local_locator: "STUB:0".into(),
                size_bytes: 0,
                vendor: "STUB".into(),
                product: "STUB NGFF 2280 256GB SSD".into(),
                serial: "STUB-SSD".into(),
                bus_type: "STUB".into(),
                removable: false,
            },
            LocalSource {
                agent_source_id: new_agent_source_id(),
                local_locator: "STUB:1".into(),
                size_bytes: 0,
                vendor: "STUB".into(),
                product: "STUB ST9320423AS".into(),
                serial: "STUB-HDD".into(),
                bus_type: "STUB".into(),
                removable: false,
            },
        ],
    }
}

/// One raw read result (CP5 evidence).
#[derive(Debug, Clone)]
pub struct RangeRead {
    pub label: String,
    pub offset: u64,
    pub requested_len: u64,
    pub actual_len: u64,
    pub sha256_hex: String,
    pub ok: bool,
    pub elapsed_ms: u128,
}

/// The device length as reported by up to three independent read-only APIs.
#[derive(Debug, Clone, Copy, Default)]
pub struct DeviceLength {
    pub get_length_info: Option<u64>,
    pub drive_geometry_ex: Option<u64>,
    pub storage_read_capacity: Option<u64>,
    pub bytes_per_sector: Option<u32>,
}

impl DeviceLength {
    /// The values that were actually obtained.
    pub fn obtained(&self) -> Vec<u64> {
        [
            self.get_length_info,
            self.drive_geometry_ex,
            self.storage_read_capacity,
        ]
        .into_iter()
        .flatten()
        .collect()
    }
    /// True iff at least one API answered and every API that answered agrees.
    pub fn agree(&self) -> bool {
        let v = self.obtained();
        !v.is_empty() && v.iter().all(|x| *x == v[0])
    }
    /// The agreed authoritative length, if the APIs agree.
    pub fn authoritative(&self) -> Option<u64> {
        self.agree().then(|| self.obtained()[0])
    }
}

#[cfg(windows)]
pub use win::RawReadSource;

#[cfg(not(windows))]
pub struct RawReadSource;

#[cfg(not(windows))]
impl RawReadSource {
    pub fn open(_locator: &str, _c: &mut Counters) -> Result<Self, String> {
        Err("RawReadSource is Windows-only".into())
    }
    pub fn device_length(&self) -> DeviceLength {
        DeviceLength::default()
    }
    pub fn read_at(
        &self,
        _label: &str,
        _offset: u64,
        _len: u64,
        _c: &mut Counters,
    ) -> RangeRead {
        unreachable!("Windows-only")
    }
    pub fn generic_write_requested(&self) -> bool {
        false
    }
    pub fn locator(&self) -> &str {
        ""
    }
}

#[cfg(windows)]
mod win {
    use super::{Counters, DeviceLength, LocalSource, RangeRead};
    use std::os::windows::ffi::OsStrExt;
    use std::time::Instant;

    use sha2::{Digest, Sha256};
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, SetFilePointerEx, FILE_BEGIN, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Ioctl::{
        PropertyStandardQuery, StorageDeviceProperty, DISK_GEOMETRY_EX, GET_LENGTH_INFORMATION,
        IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_DISK_GET_LENGTH_INFO, IOCTL_STORAGE_QUERY_PROPERTY,
        IOCTL_STORAGE_READ_CAPACITY, STORAGE_DEVICE_DESCRIPTOR, STORAGE_PROPERTY_QUERY,
        STORAGE_READ_CAPACITY,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn bus_type_name(v: u32) -> &'static str {
        match v {
            1 => "SCSI",
            2 => "ATAPI",
            3 => "ATA",
            4 => "1394",
            7 => "USB",
            8 => "RAID",
            10 => "SAS",
            11 => "SATA",
            17 => "NVMe",
            _ => "OTHER",
        }
    }

    fn ascii_at(buf: &[u8], offset: u32) -> String {
        if offset == 0 || offset as usize >= buf.len() {
            return String::new();
        }
        let start = offset as usize;
        let end = buf[start..]
            .iter()
            .position(|&b| b == 0)
            .map(|p| start + p)
            .unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[start..end]).trim().to_string()
    }

    /// Zero desired access: query IOCTLs only. Cannot read or write contents.
    pub fn enumerate_physical_drives() -> Vec<LocalSource> {
        let mut out = Vec::new();
        for n in 0u32..16 {
            let path = wide(&format!(r"\\.\PhysicalDrive{n}"));
            let handle = unsafe {
                CreateFileW(
                    path.as_ptr(),
                    0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                let _ = unsafe { GetLastError() };
                continue;
            }

            let mut size_bytes = 0u64;
            let mut len_info: GET_LENGTH_INFORMATION = unsafe { std::mem::zeroed() };
            let mut returned = 0u32;
            let ok = unsafe {
                DeviceIoControl(
                    handle,
                    IOCTL_DISK_GET_LENGTH_INFO,
                    std::ptr::null(),
                    0,
                    &mut len_info as *mut _ as *mut _,
                    std::mem::size_of::<GET_LENGTH_INFORMATION>() as u32,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            if ok != 0 && len_info.Length >= 0 {
                size_bytes = len_info.Length as u64;
            }

            let (mut vendor, mut product, mut serial, mut bus, mut removable) = (
                String::new(),
                String::new(),
                String::new(),
                String::from("UNKNOWN"),
                false,
            );
            let mut query: STORAGE_PROPERTY_QUERY = unsafe { std::mem::zeroed() };
            query.PropertyId = StorageDeviceProperty;
            query.QueryType = PropertyStandardQuery;
            let mut buf = [0u8; 2048];
            let ok = unsafe {
                DeviceIoControl(
                    handle,
                    IOCTL_STORAGE_QUERY_PROPERTY,
                    &query as *const _ as *const _,
                    std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
                    buf.as_mut_ptr() as *mut _,
                    buf.len() as u32,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            if ok != 0 && (returned as usize) >= std::mem::size_of::<STORAGE_DEVICE_DESCRIPTOR>() {
                let d: STORAGE_DEVICE_DESCRIPTOR = unsafe {
                    std::ptr::read_unaligned(buf.as_ptr() as *const STORAGE_DEVICE_DESCRIPTOR)
                };
                vendor = ascii_at(&buf, d.VendorIdOffset);
                product = ascii_at(&buf, d.ProductIdOffset);
                serial = ascii_at(&buf, d.SerialNumberOffset);
                bus = bus_type_name(d.BusType as u32).to_string();
                removable = d.RemovableMedia != 0;
            }

            unsafe { CloseHandle(handle) };

            out.push(LocalSource {
                agent_source_id: String::new(),
                local_locator: format!(r"\\.\PhysicalDrive{n}"),
                size_bytes,
                vendor,
                product,
                serial,
                bus_type: bus,
                removable,
            });
        }
        out
    }

    /// A `GENERIC_READ`-only raw handle to one resolved physical source.
    pub struct RawReadSource {
        handle: HANDLE,
        locator: String,
    }

    impl RawReadSource {
        /// The exact and only desired-access value this probe ever requests
        /// for a data handle.
        const DESIRED_ACCESS: u32 = GENERIC_READ;

        pub fn open(locator: &str, c: &mut Counters) -> Result<Self, String> {
            debug_assert_eq!(Self::DESIRED_ACCESS, 0x8000_0000, "GENERIC_READ only");
            let path = wide(locator);
            let handle = unsafe {
                CreateFileW(
                    path.as_ptr(),
                    Self::DESIRED_ACCESS, // GENERIC_READ, never GENERIC_WRITE
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            // Count the open attempt whether or not it succeeded: a data
            // handle open was ATTEMPTED here and nowhere else.
            c.data_device_open_count += 1;
            if handle == INVALID_HANDLE_VALUE {
                return Err(format!(
                    "CreateFileW({locator}, GENERIC_READ) failed, GetLastError={}",
                    unsafe { GetLastError() }
                ));
            }
            Ok(Self {
                handle,
                locator: locator.to_string(),
            })
        }

        pub fn generic_write_requested(&self) -> bool {
            false
        }

        pub fn locator(&self) -> &str {
            &self.locator
        }

        pub fn device_length(&self) -> DeviceLength {
            let mut out = DeviceLength::default();
            let mut returned = 0u32;

            // 1. IOCTL_DISK_GET_LENGTH_INFO
            let mut li: GET_LENGTH_INFORMATION = unsafe { std::mem::zeroed() };
            if unsafe {
                DeviceIoControl(
                    self.handle,
                    IOCTL_DISK_GET_LENGTH_INFO,
                    std::ptr::null(),
                    0,
                    &mut li as *mut _ as *mut _,
                    std::mem::size_of::<GET_LENGTH_INFORMATION>() as u32,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            } != 0
                && li.Length >= 0
            {
                out.get_length_info = Some(li.Length as u64);
            }

            // 2. IOCTL_DISK_GET_DRIVE_GEOMETRY_EX (byte buffer: the struct has a
            //    trailing flexible `Data` member and partition info follows it)
            let mut gxbuf = [0u8; 512];
            if unsafe {
                DeviceIoControl(
                    self.handle,
                    IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
                    std::ptr::null(),
                    0,
                    gxbuf.as_mut_ptr() as *mut _,
                    gxbuf.len() as u32,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            } != 0
                && (returned as usize) >= std::mem::size_of::<DISK_GEOMETRY_EX>()
            {
                let gx: DISK_GEOMETRY_EX =
                    unsafe { std::ptr::read_unaligned(gxbuf.as_ptr() as *const DISK_GEOMETRY_EX) };
                if gx.DiskSize >= 0 {
                    out.drive_geometry_ex = Some(gx.DiskSize as u64);
                }
                if gx.Geometry.BytesPerSector > 0 {
                    out.bytes_per_sector = Some(gx.Geometry.BytesPerSector);
                }
            }

            // 3. IOCTL_STORAGE_READ_CAPACITY
            let mut rc: STORAGE_READ_CAPACITY = unsafe { std::mem::zeroed() };
            rc.Version = std::mem::size_of::<STORAGE_READ_CAPACITY>() as u32;
            rc.Size = std::mem::size_of::<STORAGE_READ_CAPACITY>() as u32;
            if unsafe {
                DeviceIoControl(
                    self.handle,
                    IOCTL_STORAGE_READ_CAPACITY,
                    std::ptr::null(),
                    0,
                    &mut rc as *mut _ as *mut _,
                    std::mem::size_of::<STORAGE_READ_CAPACITY>() as u32,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            } != 0
                && rc.DiskLength >= 0
            {
                out.storage_read_capacity = Some(rc.DiskLength as u64);
                if out.bytes_per_sector.is_none() && rc.BlockLength > 0 {
                    out.bytes_per_sector = Some(rc.BlockLength);
                }
            }

            out
        }

        /// One bounded raw read: `SetFilePointerEx(FILE_BEGIN)` then `ReadFile`
        /// in a loop until `len` bytes are read or the device signals EOF /
        /// error. Never issues a write. `offset`/`len` must be sector-aligned
        /// (the caller aligns).
        pub fn read_at(&self, label: &str, offset: u64, len: u64, c: &mut Counters) -> RangeRead {
            c.data_read_count += 1;
            let started = Instant::now();
            let mut result = RangeRead {
                label: label.to_string(),
                offset,
                requested_len: len,
                actual_len: 0,
                sha256_hex: String::new(),
                ok: false,
                elapsed_ms: 0,
            };

            let mut new_pos: i64 = 0;
            let sought = unsafe {
                SetFilePointerEx(self.handle, offset as i64, &mut new_pos, FILE_BEGIN)
            };
            if sought == 0 || new_pos as u64 != offset {
                result.elapsed_ms = started.elapsed().as_millis();
                return result;
            }

            let mut hasher = Sha256::new();
            // 1 MiB scratch, 4096-aligned start (raw physical-device reads want
            // sector-aligned offsets/lengths; a page-aligned buffer is the
            // safe default on WinPE too).
            const CHUNK: usize = 1 << 20;
            let mut backing = vec![0u8; CHUNK + 4096];
            let pad = backing.as_ptr().align_offset(4096);
            let mut remaining = len;
            while remaining > 0 {
                let want = remaining.min(CHUNK as u64) as u32;
                let mut got = 0u32;
                let ok = unsafe {
                    ReadFile(
                        self.handle,
                        backing[pad..].as_mut_ptr(),
                        want,
                        &mut got,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 {
                    // I/O error mid-read: report what we have, not ok.
                    result.elapsed_ms = started.elapsed().as_millis();
                    result.sha256_hex = hex(&hasher.finalize());
                    return result;
                }
                if got == 0 {
                    break; // EOF
                }
                hasher.update(&backing[pad..pad + got as usize]);
                result.actual_len += got as u64;
                remaining -= got as u64;
            }

            result.sha256_hex = hex(&hasher.finalize());
            result.ok = result.actual_len == len;
            result.elapsed_ms = started.elapsed().as_millis();
            result
        }
    }

    impl Drop for RawReadSource {
        fn drop(&mut self) {
            if self.handle != INVALID_HANDLE_VALUE {
                unsafe { CloseHandle(self.handle) };
            }
        }
    }

    fn hex(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}
