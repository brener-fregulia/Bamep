//! Checkpoint 5: read-only physical capture-source enumeration under the
//! Approved #59 contract
//! (`docs/specifications/m2-endpoint-capture-service-intent-and-source-reference-contract.md`).
//!
//! Cross-boundary output is ONLY:
//!   { "capture_source_observation_id": "<43-char base64url>",
//!     "capturable_sources": [ { "agent_source_id": "<opaque>" } ] }
//!
//! `capture_source_observation_id` reuses the `boot_nonce` idiom (32 CSPRNG
//! bytes -> canonical base64url, no padding, 43 chars). `agent_source_id` is
//! a fresh opaque token per source, unique within this epoch. `\\.\PhysicalDriveN`,
//! model, serial, bus type and size are retained as LOCAL LAB EVIDENCE ONLY —
//! never promoted to cross-boundary identity. Every disk handle is opened with
//! zero access rights: the probe structurally cannot write to a disk.

/// One enumerated local source. Only `agent_source_id` crosses the boundary;
/// every other field is Agent-local lab evidence.
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
    /// `agent_source_id` values that appeared more than once — a fail-closed
    /// signal per #59 RF-4 (duplicates make the projection ambiguous).
    pub duplicate_ids: Vec<String>,
}

impl SourceEpoch {
    /// The exact cross-boundary JSON fragment (#59 RF-4), as a compact string.
    pub fn capture_fragment_json(&self) -> String {
        let mut out = String::new();
        out.push_str(r#"{"capture_source_observation_id":""#);
        out.push_str(&self.observation_id);
        out.push_str(r#"","capturable_sources":["#);
        for (i, src) in self.sources.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(r#"{"agent_source_id":""#);
            out.push_str(&src.agent_source_id);
            out.push_str(r#""}"#);
        }
        out.push_str("]}");
        out
    }
}

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Canonical RFC 4648 base64url, no padding.
fn base64url_nopad(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
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

fn new_observation_id() -> String {
    // 32 raw CSPRNG bytes -> 43-char base64url, no padding (the boot_nonce idiom).
    base64url_nopad(&random_bytes::<32>())
}

fn new_agent_source_id() -> String {
    // Opaque, freshly minted, unique within the epoch. Not derived from any
    // local locator.
    base64url_nopad(&random_bytes::<18>())
}

fn find_duplicates(sources: &[LocalSource]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut dups = std::collections::BTreeSet::new();
    for src in sources {
        if !seen.insert(src.agent_source_id.clone()) {
            dups.insert(src.agent_source_id.clone());
        }
    }
    dups.into_iter().collect()
}

#[cfg(windows)]
pub fn enumerate() -> SourceEpoch {
    let mut sources = win::enumerate_physical_drives();
    // Assign opaque ids after physical discovery, so ids are never a function
    // of drive order/number.
    for src in &mut sources {
        src.agent_source_id = new_agent_source_id();
    }
    let duplicate_ids = find_duplicates(&sources);
    SourceEpoch {
        observation_id: new_observation_id(),
        sources,
        duplicate_ids,
    }
}

#[cfg(not(windows))]
pub fn enumerate() -> SourceEpoch {
    // Non-Windows build (developer host): two synthetic sources so the epoch
    // JSON / duplicate / sink path can be exercised. Never used in WinPE.
    let sources = vec![
        LocalSource {
            agent_source_id: new_agent_source_id(),
            local_locator: "STUB:0".into(),
            size_bytes: 0,
            vendor: "STUB".into(),
            product: "STUB".into(),
            serial: "STUB".into(),
            bus_type: "STUB".into(),
            removable: false,
        },
        LocalSource {
            agent_source_id: new_agent_source_id(),
            local_locator: "STUB:1".into(),
            size_bytes: 0,
            vendor: "STUB".into(),
            product: "STUB".into(),
            serial: "STUB".into(),
            bus_type: "STUB".into(),
            removable: false,
        },
    ];
    let duplicate_ids = find_duplicates(&sources);
    SourceEpoch {
        observation_id: new_observation_id(),
        sources,
        duplicate_ids,
    }
}

#[cfg(windows)]
mod win {
    use super::LocalSource;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Ioctl::{
        PropertyStandardQuery, StorageDeviceProperty, GET_LENGTH_INFORMATION,
        IOCTL_DISK_GET_LENGTH_INFO, IOCTL_STORAGE_QUERY_PROPERTY, STORAGE_DEVICE_DESCRIPTOR,
        STORAGE_PROPERTY_QUERY,
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
            5 => "SSA",
            6 => "FIBRE",
            7 => "USB",
            8 => "RAID",
            9 => "iSCSI",
            10 => "SAS",
            11 => "SATA",
            12 => "SD",
            13 => "MMC",
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

    pub fn enumerate_physical_drives() -> Vec<LocalSource> {
        let mut out = Vec::new();
        for n in 0u32..16 {
            let path = wide(&format!(r"\\.\PhysicalDrive{n}"));
            // Zero desired access: enough for query IOCTLs, cannot read or
            // write disk contents.
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
                // `buf` is a byte array (alignment 1); read the descriptor by
                // value without forming a reference to unaligned storage.
                let d: STORAGE_DEVICE_DESCRIPTOR = unsafe {
                    std::ptr::read_unaligned(buf.as_ptr() as *const STORAGE_DEVICE_DESCRIPTOR)
                };
                vendor = ascii_at(&buf, d.VendorIdOffset);
                product = ascii_at(&buf, d.ProductIdOffset);
                serial = ascii_at(&buf, d.SerialNumberOffset);
                bus = bus_type_name(d.BusType as u32).to_string();
                removable = d.RemovableMedia != 0;
            }

            unsafe {
                CloseHandle(handle);
            }

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
}
