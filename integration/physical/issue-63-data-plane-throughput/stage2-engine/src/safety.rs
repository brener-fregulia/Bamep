//! The Issue #63 physical source-safety predicate — PURE, fail-closed.
//!
//! Stage 3's future physical target is the ALREADY-AUTHORISED disposable MiniPC
//! SSD. `\\.\PhysicalDrive0` is NOT authority: the probe preserves the #61
//! model —
//!
//! ```text
//! fresh source-observation epoch (THIS process / THIS boot)
//!   -> opaque agent_source_id
//!   -> local resolver
//!   -> resolved local locator
//! ```
//!
//! — and THEN this predicate validates the resolved source BEFORE any bulk
//! bytes are read. It is unit-testable with no Windows / device access.
//!
//! Mandatory checks (any failure ⇒ Reject, and Reject ⇒ ZERO bulk reads):
//!   * exactly one selected source;
//!   * the selection is an opaque authority tuple, not a device path / ordinal;
//!   * the selected source came from the CURRENT same-process observation epoch;
//!   * the resolved local device model matches the expected disposable model;
//!   * the exact device length equals [`EXPECTED_DEVICE_LENGTH_BYTES`];
//!   * the requested bounded extent is `<=` the device length;
//!   * a non-zero device length;
//!   * if the read-only descriptor exposed a serial, it equals the expected one;
//!   * NO fallback to the first / ordinal disk.
//!
//! Previously observed disposable-device evidence (Issue #61):
//!   model  `NGFF 2280 256GB SSD`
//!   serial `20240905101720`
//!   length `256,060,514,304` bytes
//!   prior local locator `\\.\PhysicalDrive0` (NOT authority).

/// The model substring the read-only `STORAGE_DEVICE_DESCRIPTOR` product string
/// must contain for the disposable MiniPC SSD.
pub const EXPECTED_MODEL_SUBSTRING: &str = "NGFF 2280 256GB SSD";

/// The exact disposable-device byte length. All three read-only IOCTLs must
/// agree on this value.
pub const EXPECTED_DEVICE_LENGTH_BYTES: u64 = 256_060_514_304;

/// The disposable-device serial. Enforced ONLY when the descriptor exposes a
/// serial reliably; model + exact length remain mandatory regardless.
pub const EXPECTED_SERIAL: &str = "20240905101720";

/// What the probe learned about ONE candidate source during THIS process's
/// read-only enumeration + descriptor read + read-only length IOCTLs. Built
/// only after the resolver mapped the opaque tuple to a single source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSource {
    /// The opaque `agent_source_id` this process minted for this source.
    pub agent_source_id: String,
    /// The source-observation epoch id this source belongs to (this process).
    pub source_observation_id: String,
    /// Agent-local locator — EVIDENCE ONLY, never an authority input.
    pub local_locator: String,
    /// Product string from the read-only `STORAGE_DEVICE_DESCRIPTOR`.
    pub model: String,
    /// Serial from the descriptor, if it was exposed reliably.
    pub serial: Option<String>,
    /// The device byte length the read-only IOCTLs AGREED on, or `None` if they
    /// disagreed / did not answer.
    pub device_length_bytes: Option<u64>,
}

/// The selection + epoch context for one safety decision.
#[derive(Debug, Clone)]
pub struct SafetyRequest<'a> {
    /// The current same-process source-observation epoch id.
    pub current_observation_id: &'a str,
    /// EVERY `agent_source_id` the operator's local-evidence predicate
    /// selected. Must be exactly one.
    pub selected_agent_source_ids: &'a [String],
    /// The resolver's result for the single selection: `Some` iff the opaque
    /// tuple resolved to exactly one source; `None` = failed / ambiguous /
    /// unknown.
    pub resolved: Option<&'a ResolvedSource>,
    /// The bounded extent the matrix will read (2048 MiB).
    pub requested_extent_bytes: u64,
}

/// Why the predicate rejected. Every variant means "NO source is selected and
/// NO bulk byte may be read".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafetyRejection {
    NoSourceSelected,
    MultipleSourcesSelected { count: usize },
    /// The one selection is a device path / ordinal / empty string, not an
    /// opaque minted authority tuple.
    SelectionNotAnAuthorityTuple { presented: String },
    /// The resolver did not map the tuple to exactly one source.
    ResolutionFailed,
    /// The resolved source's `agent_source_id` is not the one that was selected.
    ResolvedSourceMismatch { selected: String, resolved: String },
    /// The resolved source belongs to a different / superseded observation epoch.
    StaleObservation { presented: String, current: String },
    WrongModel { got: String, expected_substring: String },
    DeviceLengthUnavailable,
    ZeroDeviceLength,
    ExtentExceedsDevice { requested_extent: u64, device_length: u64 },
    WrongDeviceLength { got: u64, expected: u64 },
    WrongSerial { got: String, expected: String },
}

/// The predicate outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafetyVerdict {
    Accept {
        locator: String,
        device_length_bytes: u64,
    },
    Reject(SafetyRejection),
}

impl SafetyVerdict {
    pub fn is_accept(&self) -> bool {
        matches!(self, SafetyVerdict::Accept { .. })
    }
}

/// True for strings that are a device path / ordinal locator rather than an
/// opaque minted authority tuple.
fn looks_like_device_path(s: &str) -> bool {
    let t = s.trim();
    t.is_empty()
        || t.starts_with(r"\\.\")
        || t.starts_with(r"\\?\")
        || t.starts_with("/dev/")
        || t.to_ascii_lowercase().contains("physicaldrive")
}

/// The pure predicate. Ordered so every mandated test case yields a distinct,
/// self-explaining rejection reason.
pub fn evaluate_predicate(req: &SafetyRequest) -> SafetyVerdict {
    use SafetyRejection::*;

    match req.selected_agent_source_ids.len() {
        0 => return SafetyVerdict::Reject(NoSourceSelected),
        1 => {}
        n => return SafetyVerdict::Reject(MultipleSourcesSelected { count: n }),
    }
    let selected = &req.selected_agent_source_ids[0];

    if looks_like_device_path(selected) || looks_like_device_path(req.current_observation_id) {
        return SafetyVerdict::Reject(SelectionNotAnAuthorityTuple {
            presented: selected.clone(),
        });
    }

    let Some(src) = req.resolved else {
        return SafetyVerdict::Reject(ResolutionFailed);
    };

    if src.agent_source_id != *selected {
        return SafetyVerdict::Reject(ResolvedSourceMismatch {
            selected: selected.clone(),
            resolved: src.agent_source_id.clone(),
        });
    }
    if looks_like_device_path(&src.source_observation_id) || src.source_observation_id.is_empty() {
        return SafetyVerdict::Reject(SelectionNotAnAuthorityTuple {
            presented: selected.clone(),
        });
    }
    if src.source_observation_id != req.current_observation_id {
        return SafetyVerdict::Reject(StaleObservation {
            presented: src.source_observation_id.clone(),
            current: req.current_observation_id.to_string(),
        });
    }

    if !src.model.contains(EXPECTED_MODEL_SUBSTRING) {
        return SafetyVerdict::Reject(WrongModel {
            got: src.model.clone(),
            expected_substring: EXPECTED_MODEL_SUBSTRING.to_string(),
        });
    }

    let device_length = match src.device_length_bytes {
        None => return SafetyVerdict::Reject(DeviceLengthUnavailable),
        Some(0) => return SafetyVerdict::Reject(ZeroDeviceLength),
        Some(n) => n,
    };
    if req.requested_extent_bytes > device_length {
        return SafetyVerdict::Reject(ExtentExceedsDevice {
            requested_extent: req.requested_extent_bytes,
            device_length,
        });
    }
    if device_length != EXPECTED_DEVICE_LENGTH_BYTES {
        return SafetyVerdict::Reject(WrongDeviceLength {
            got: device_length,
            expected: EXPECTED_DEVICE_LENGTH_BYTES,
        });
    }

    if let Some(serial) = &src.serial {
        if serial != EXPECTED_SERIAL {
            return SafetyVerdict::Reject(WrongSerial {
                got: serial.clone(),
                expected: EXPECTED_SERIAL.to_string(),
            });
        }
    }

    SafetyVerdict::Accept {
        locator: src.local_locator.clone(),
        device_length_bytes: device_length,
    }
}

// ---------------------------------------------------------------------------
// instrumented ordering gate — proves ZERO bulk reads on a rejection
// ---------------------------------------------------------------------------

/// The narrow instrumentation the Stage-3 evidence needs to prove that the
/// predicate runs BEFORE any bulk source read.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SafetyCounters {
    pub source_resolution_attempts: u64,
    pub device_open_count: u64,
    /// MUST remain `0` after any rejection.
    pub bulk_read_count: u64,
}

/// The device operations the gate performs, in probe order. A real
/// implementation opens a `GENERIC_READ` (never `GENERIC_WRITE`) handle; this
/// trait's `read_bulk` is the ONLY bulk-read entry point and the gate only ever
/// calls it AFTER an `Accept`.
pub trait GatedDevice {
    /// A permitted `GENERIC_READ`-only open (used for length validation).
    fn open_readonly(&mut self, locator: &str) -> Result<(), String>;
    /// A BULK source read. The gate calls this only on the `Accept` path.
    fn read_bulk(&mut self, offset: u64, len: u64) -> Result<Vec<u8>, String>;
}

/// A safety-gated bounded reader. `authorize_then_read` evaluates the predicate
/// first and only touches the device on `Accept`; on any rejection it returns
/// `Err(reason)` with `counters().bulk_read_count` still `0`.
pub struct SafetyGate<'d, D: GatedDevice> {
    device: &'d mut D,
    counters: SafetyCounters,
    authorized_locator: Option<String>,
}

impl<'d, D: GatedDevice> SafetyGate<'d, D> {
    pub fn new(device: &'d mut D) -> Self {
        Self {
            device,
            counters: SafetyCounters::default(),
            authorized_locator: None,
        }
    }

    pub fn counters(&self) -> SafetyCounters {
        self.counters
    }

    /// Evaluate the predicate for `req`. On `Accept` this records the authorised
    /// locator and performs the permitted read-only open; on `Reject` it
    /// returns the reason and leaves `bulk_read_count == 0`.
    pub fn authorize(&mut self, req: &SafetyRequest) -> Result<String, SafetyRejection> {
        self.counters.source_resolution_attempts += 1;
        match evaluate_predicate(req) {
            SafetyVerdict::Reject(reason) => Err(reason),
            SafetyVerdict::Accept { locator, .. } => {
                self.device
                    .open_readonly(&locator)
                    .map_err(|_| SafetyRejection::ResolutionFailed)?;
                self.counters.device_open_count += 1;
                self.authorized_locator = Some(locator.clone());
                Ok(locator)
            }
        }
    }

    /// A bounded bulk read — only possible after a successful [`Self::authorize`].
    pub fn read_bulk(&mut self, offset: u64, len: u64) -> Result<Vec<u8>, String> {
        if self.authorized_locator.is_none() {
            return Err("read_bulk before authorize() PASS — refused".into());
        }
        let bytes = self.device.read_bulk(offset, len)?;
        self.counters.bulk_read_count += 1;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OBS: &str = "OBS-current-epoch-2wKp8ssDdr-blMbFHcUzUEVM";
    const ASID: &str = "asid-disposable-ssd-0001";

    fn good_source() -> ResolvedSource {
        ResolvedSource {
            agent_source_id: ASID.to_string(),
            source_observation_id: OBS.to_string(),
            local_locator: r"\\.\PhysicalDrive0".to_string(),
            model: "STUB NGFF 2280 256GB SSD".to_string(),
            serial: Some(EXPECTED_SERIAL.to_string()),
            device_length_bytes: Some(EXPECTED_DEVICE_LENGTH_BYTES),
        }
    }

    fn req<'a>(sel: &'a [String], resolved: Option<&'a ResolvedSource>) -> SafetyRequest<'a> {
        SafetyRequest {
            current_observation_id: OBS,
            selected_agent_source_ids: sel,
            resolved,
            requested_extent_bytes: crate::matrix::EXTENT_BYTES,
        }
    }

    // ---- predicate: the 9 mandated cases --------------------------------
    #[test]
    fn exact_expected_source_accepts() {
        let src = good_source();
        let sel = vec![ASID.to_string()];
        match evaluate_predicate(&req(&sel, Some(&src))) {
            SafetyVerdict::Accept {
                locator,
                device_length_bytes,
            } => {
                assert_eq!(locator, r"\\.\PhysicalDrive0");
                assert_eq!(device_length_bytes, EXPECTED_DEVICE_LENGTH_BYTES);
            }
            other => panic!("expected Accept, got {other:?}"),
        }
    }

    #[test]
    fn wrong_model_rejects() {
        let mut src = good_source();
        src.model = "STUB ST9320423AS".to_string();
        let sel = vec![ASID.to_string()];
        assert!(matches!(
            evaluate_predicate(&req(&sel, Some(&src))),
            SafetyVerdict::Reject(SafetyRejection::WrongModel { .. })
        ));
    }

    #[test]
    fn wrong_length_by_one_byte_rejects() {
        let mut src = good_source();
        src.device_length_bytes = Some(EXPECTED_DEVICE_LENGTH_BYTES - 1);
        let sel = vec![ASID.to_string()];
        assert!(matches!(
            evaluate_predicate(&req(&sel, Some(&src))),
            SafetyVerdict::Reject(SafetyRejection::WrongDeviceLength {
                got,
                expected
            }) if got == EXPECTED_DEVICE_LENGTH_BYTES - 1 && expected == EXPECTED_DEVICE_LENGTH_BYTES
        ));
    }

    #[test]
    fn extent_greater_than_device_rejects() {
        let mut src = good_source();
        src.device_length_bytes = Some(1024 * 1024 * 1024); // 1 GiB < 2 GiB extent
        let sel = vec![ASID.to_string()];
        assert!(matches!(
            evaluate_predicate(&req(&sel, Some(&src))),
            SafetyVerdict::Reject(SafetyRejection::ExtentExceedsDevice { .. })
        ));
    }

    #[test]
    fn zero_length_rejects() {
        let mut src = good_source();
        src.device_length_bytes = Some(0);
        let sel = vec![ASID.to_string()];
        assert!(matches!(
            evaluate_predicate(&req(&sel, Some(&src))),
            SafetyVerdict::Reject(SafetyRejection::ZeroDeviceLength)
        ));
    }

    #[test]
    fn two_selected_sources_reject() {
        let src = good_source();
        let sel = vec![ASID.to_string(), "asid-other".to_string()];
        assert!(matches!(
            evaluate_predicate(&req(&sel, Some(&src))),
            SafetyVerdict::Reject(SafetyRejection::MultipleSourcesSelected { count: 2 })
        ));
    }

    #[test]
    fn unknown_agent_source_id_rejects() {
        // resolver failed to map the tuple
        let sel = vec![ASID.to_string()];
        assert!(matches!(
            evaluate_predicate(&req(&sel, None)),
            SafetyVerdict::Reject(SafetyRejection::ResolutionFailed)
        ));
        // resolver returned a DIFFERENT source than selected
        let mut src = good_source();
        src.agent_source_id = "asid-some-other-source".to_string();
        assert!(matches!(
            evaluate_predicate(&req(&sel, Some(&src))),
            SafetyVerdict::Reject(SafetyRejection::ResolvedSourceMismatch { .. })
        ));
    }

    #[test]
    fn stale_observation_rejects() {
        let mut src = good_source();
        src.source_observation_id = "OBS-a-different-superseded-epoch".to_string();
        let sel = vec![ASID.to_string()];
        assert!(matches!(
            evaluate_predicate(&req(&sel, Some(&src))),
            SafetyVerdict::Reject(SafetyRejection::StaleObservation { .. })
        ));
    }

    #[test]
    fn physicaldrive0_alone_without_authority_tuple_rejects() {
        // The operator "selected" a raw device path — not an opaque minted id.
        let sel = vec![r"\\.\PhysicalDrive0".to_string()];
        assert!(matches!(
            evaluate_predicate(&req(&sel, None)),
            SafetyVerdict::Reject(SafetyRejection::SelectionNotAnAuthorityTuple { .. })
        ));
        // Even with a plausible-looking resolved source attached.
        let src = good_source();
        assert!(matches!(
            evaluate_predicate(&req(&sel, Some(&src))),
            SafetyVerdict::Reject(SafetyRejection::SelectionNotAnAuthorityTuple { .. })
        ));
    }

    #[test]
    fn missing_serial_is_allowed_when_model_and_length_hold() {
        let mut src = good_source();
        src.serial = None;
        let sel = vec![ASID.to_string()];
        assert!(evaluate_predicate(&req(&sel, Some(&src))).is_accept());
    }

    #[test]
    fn present_but_wrong_serial_rejects() {
        let mut src = good_source();
        src.serial = Some("99999999999999".to_string());
        let sel = vec![ASID.to_string()];
        assert!(matches!(
            evaluate_predicate(&req(&sel, Some(&src))),
            SafetyVerdict::Reject(SafetyRejection::WrongSerial { .. })
        ));
    }

    #[test]
    fn device_length_unavailable_rejects() {
        let mut src = good_source();
        src.device_length_bytes = None;
        let sel = vec![ASID.to_string()];
        assert!(matches!(
            evaluate_predicate(&req(&sel, Some(&src))),
            SafetyVerdict::Reject(SafetyRejection::DeviceLengthUnavailable)
        ));
    }

    // ---- ordering gate: a rejection performs ZERO bulk reads ------------
    struct PanicOnBulkRead {
        opened: bool,
    }
    impl GatedDevice for PanicOnBulkRead {
        fn open_readonly(&mut self, _locator: &str) -> Result<(), String> {
            self.opened = true;
            Ok(())
        }
        fn read_bulk(&mut self, _offset: u64, _len: u64) -> Result<Vec<u8>, String> {
            panic!("read_bulk MUST NOT be reached on a rejected / un-authorized source");
        }
    }

    fn all_rejection_requests() -> Vec<(SafetyRequest<'static>, &'static str)> {
        // Own the backing data via leaks so the requests can be 'static in the
        // test table (throwaway test code only).
        fn leak_src(s: ResolvedSource) -> &'static ResolvedSource {
            Box::leak(Box::new(s))
        }
        fn leak_sel(v: Vec<String>) -> &'static [String] {
            Box::leak(v.into_boxed_slice())
        }
        let wrong_model = {
            let mut s = good_source();
            s.model = "nope".into();
            leak_src(s)
        };
        let wrong_len = {
            let mut s = good_source();
            s.device_length_bytes = Some(EXPECTED_DEVICE_LENGTH_BYTES - 1);
            leak_src(s)
        };
        let zero_len = {
            let mut s = good_source();
            s.device_length_bytes = Some(0);
            leak_src(s)
        };
        let stale = {
            let mut s = good_source();
            s.source_observation_id = "OBS-superseded".into();
            leak_src(s)
        };
        let mk = |sel: &'static [String], resolved: Option<&'static ResolvedSource>| SafetyRequest {
            current_observation_id: OBS,
            selected_agent_source_ids: sel,
            resolved,
            requested_extent_bytes: crate::matrix::EXTENT_BYTES,
        };
        let one = leak_sel(vec![ASID.to_string()]);
        vec![
            (mk(leak_sel(vec![]), None), "NoSourceSelected"),
            (
                mk(leak_sel(vec![ASID.into(), "x".into()]), Some(leak_src(good_source()))),
                "MultipleSourcesSelected",
            ),
            (mk(one, None), "ResolutionFailed"),
            (
                mk(leak_sel(vec![r"\\.\PhysicalDrive0".into()]), None),
                "SelectionNotAnAuthorityTuple",
            ),
            (mk(one, Some(stale)), "StaleObservation"),
            (mk(one, Some(wrong_model)), "WrongModel"),
            (mk(one, Some(wrong_len)), "WrongDeviceLength"),
            (mk(one, Some(zero_len)), "ZeroDeviceLength"),
        ]
    }

    #[test]
    fn every_rejection_leaves_bulk_read_count_zero() {
        for (request, label) in all_rejection_requests() {
            let mut dev = PanicOnBulkRead { opened: false };
            let mut gate = SafetyGate::new(&mut dev);
            let r = gate.authorize(&request);
            assert!(r.is_err(), "{label}: expected authorize() to reject");
            assert_eq!(
                gate.counters().bulk_read_count,
                0,
                "{label}: bulk_read_count must be 0 after a rejection"
            );
            // read_bulk is refused outright without an authorize() PASS.
            assert!(gate.read_bulk(0, 8).is_err(), "{label}: read_bulk refused");
            assert_eq!(gate.counters().bulk_read_count, 0, "{label}: still 0");
        }
    }

    struct CountingDevice {
        opens: u64,
        reads: u64,
    }
    impl GatedDevice for CountingDevice {
        fn open_readonly(&mut self, _locator: &str) -> Result<(), String> {
            self.opens += 1;
            Ok(())
        }
        fn read_bulk(&mut self, _offset: u64, len: u64) -> Result<Vec<u8>, String> {
            self.reads += 1;
            Ok(vec![0u8; len as usize])
        }
    }

    #[test]
    fn accept_then_bulk_read_is_counted_and_ordered() {
        let src = good_source();
        let sel = vec![ASID.to_string()];
        let mut dev = CountingDevice { opens: 0, reads: 0 };
        let mut gate = SafetyGate::new(&mut dev);

        let locator = gate.authorize(&req(&sel, Some(&src))).expect("authorize PASS");
        assert_eq!(locator, r"\\.\PhysicalDrive0");
        assert_eq!(gate.counters().device_open_count, 1);
        assert_eq!(gate.counters().bulk_read_count, 0, "no bulk read from authorize()");

        let bytes = gate.read_bulk(0, 8 * 1024 * 1024).expect("bulk read");
        assert_eq!(bytes.len(), 8 * 1024 * 1024);
        assert_eq!(gate.counters().bulk_read_count, 1);
        assert_eq!(gate.counters().source_resolution_attempts, 1);
    }
}
