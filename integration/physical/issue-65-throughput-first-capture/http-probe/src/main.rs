//! Bamep Issue #65 — WinPE plain-HTTP throughput CONTROL probe. THROWAWAY Spike.
//!
//! WinPE-native (`x86_64-pc-windows-msvc`). LAB-ONLY. NOT the Bamep Agent,
//! NOT `crates/agent`, NOT `bamepd`, NOT a production transfer path.
//!
//! Question (#65 control, capture path untouched): can stock WinPE + its NIC
//! driver + the physical 1 GbE link sustain >= 110 MB/s with the smallest
//! possible native HTTP transfer?
//!
//!   WinHttpOpen -> WinHttpConnect -> WinHttpOpenRequest("POST", plain HTTP,
//!   no WINHTTP_FLAG_SECURE) -> WinHttpSendRequest(dwTotalLength = 2 GiB)
//!   -> 64 x WinHttpWriteData(the SAME 32 MiB buffer, allocated + filled ONCE)
//!   -> WinHttpReceiveResponse -> HTTP status + the sink's JSON measurement.
//!
//! NO Bamep source resolver, NO physical disk read, NO producer/consumer queue,
//! NO SHA, NO TLS, NO auth, NO DB, NO Bamep protocol framing. One buffer, one
//! request, one session. Reports bytes / client send wall / HTTP status / NIC
//! link speed / TCP retransmit delta. No payload-correctness meaning.

// 32 MiB is deliberate for this control (do not sweep 8/16/64 here): the goal
// is only to learn whether WinPE + the physical network can saturate 1 GbE.
const CHUNK_BYTES: u32 = 1 << 25; // 32 MiB
const TOTAL_BYTES: u32 = 1 << 31; // 2 GiB exactly
const WRITES: u32 = TOTAL_BYTES / CHUNK_BYTES; // 64

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "bamep-i65-http-probe is WinPE-only. Cross-build:\n  RUSTFLAGS='-C target-feature=+crt-static' \\\n  cargo xwin build --release --target x86_64-pc-windows-msvc"
    );
    // Compile-time self-checks that do not need Windows.
    assert_eq!(WRITES, 64);
    assert_eq!((WRITES as u64) * (CHUNK_BYTES as u64), TOTAL_BYTES as u64);
    std::process::exit(3);
}

#[cfg(windows)]
fn main() {
    std::process::exit(win::run());
}

#[cfg(windows)]
mod win {
    use super::{CHUNK_BYTES, TOTAL_BYTES, WRITES};
    use std::ffi::c_void;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{GetLastError, ERROR_SUCCESS};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        FreeMibTable, GetIfTable2, GetTcpStatisticsEx, MIB_IF_ROW2, MIB_IF_TABLE2, MIB_TCPSTATS_LH,
    };
    use windows_sys::Win32::Networking::WinHttp::{
        WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
        WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
        WinHttpWriteData, WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_QUERY_FLAG_NUMBER,
        WINHTTP_QUERY_STATUS_CODE,
    };

    mod exit {
        pub const PASS: i32 = 0;
        pub const BAD_ARGS: i32 = 2;
        pub const CONNECT: i32 = 62;
        pub const STREAM_FATAL: i32 = 70;
        pub const INCOMPLETE: i32 = 72;
    }

    struct Args {
        host: String,
        port: u16,
        path: String,
        label: String,
        connect_wait_secs: u64,
    }

    fn parse_args() -> Result<Args, String> {
        let mut a = Args {
            host: "192.168.99.1".into(),
            port: 9265,
            path: "/i65-http-control".into(),
            label: "unlabeled".into(),
            connect_wait_secs: 60,
        };
        let mut it = std::env::args().skip(1);
        while let Some(x) = it.next() {
            match x.as_str() {
                "--host" => a.host = it.next().ok_or("--host needs a value")?,
                "--port" => {
                    a.port = it
                        .next()
                        .ok_or("--port needs a value")?
                        .parse()
                        .map_err(|_| "bad --port")?
                }
                "--path" => a.path = it.next().ok_or("--path needs a value")?,
                "--label" => a.label = it.next().ok_or("--label needs a value")?,
                "--connect-wait-secs" => {
                    a.connect_wait_secs = it
                        .next()
                        .ok_or("--connect-wait-secs needs a value")?
                        .parse()
                        .map_err(|_| "bad --connect-wait-secs")?
                }
                other => return Err(format!("unknown argument {other:?}")),
            }
        }
        Ok(a)
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn run() -> i32 {
        let args = match parse_args() {
            Ok(a) => a,
            Err(e) => {
                eprintln!("bamep-i65-http-probe: FATAL: {e}");
                return exit::BAD_ARGS;
            }
        };
        eprintln!(
            "i65-http: probe start host={} port={} path={} label={} chunk_bytes={CHUNK_BYTES} writes={WRITES} total_bytes={TOTAL_BYTES}",
            args.host, args.port, args.path, args.label
        );

        // ONE 32 MiB buffer: one allocation, one fill. The write loop only
        // borrows it. No per-write allocation or fill.
        let buf = vec![0xA5u8; CHUNK_BYTES as usize];
        eprintln!("i65-http: 1 buffer of {} bytes allocated+filled once", buf.len());

        let link = nic_link_speed();
        let retrans_before = tcp_retrans_segs();

        let ua = wide("bamep-i65-http-probe/0.1");
        let host_w = wide(&args.host);
        let verb_w = wide("POST");
        let path_w = wide(&args.path);

        unsafe {
            let session = WinHttpOpen(
                ua.as_ptr(),
                WINHTTP_ACCESS_TYPE_NO_PROXY,
                std::ptr::null(),
                std::ptr::null(),
                0,
            );
            if session.is_null() {
                eprintln!("i65-http: WinHttpOpen failed GLE={}", GetLastError());
                return exit::CONNECT;
            }
            // resolve / connect / send / receive timeouts (ms). Send + receive
            // are generous: a 2 GiB body on 1 GbE is ~20 s minimum.
            WinHttpSetTimeouts(session, 30_000, 30_000, 600_000, 600_000);

            let connect = WinHttpConnect(session, host_w.as_ptr(), args.port, 0);
            if connect.is_null() {
                eprintln!("i65-http: WinHttpConnect failed GLE={}", GetLastError());
                WinHttpCloseHandle(session);
                return exit::CONNECT;
            }

            // Bounded connect-wait: retry the header phase (which performs the
            // real TCP connect) until the sink is reachable or the deadline.
            let deadline = Instant::now() + Duration::from_secs(args.connect_wait_secs);
            let mut attempt = 0u32;
            let request = loop {
                attempt += 1;
                let h = WinHttpOpenRequest(
                    connect,
                    verb_w.as_ptr(),
                    path_w.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    0, // plain HTTP: no WINHTTP_FLAG_SECURE
                );
                if h.is_null() {
                    eprintln!("i65-http: WinHttpOpenRequest failed GLE={}", GetLastError());
                    WinHttpCloseHandle(connect);
                    WinHttpCloseHandle(session);
                    return exit::CONNECT;
                }
                let ok = WinHttpSendRequest(
                    h,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    TOTAL_BYTES, // dwTotalLength -> "Content-Length: 2147483648"
                    0,
                );
                if ok != 0 {
                    break h;
                }
                let gle = GetLastError();
                WinHttpCloseHandle(h);
                if Instant::now() >= deadline {
                    eprintln!(
                        "i65-http: WinHttpSendRequest failed after {attempt} attempts GLE={gle}"
                    );
                    WinHttpCloseHandle(connect);
                    WinHttpCloseHandle(session);
                    return exit::CONNECT;
                }
                std::thread::sleep(Duration::from_millis(1000));
            };
            if attempt > 1 {
                eprintln!("i65-http: sink reachable after {attempt} attempts");
            }

            // ---- body: WRITES writes of the SAME buffer ----
            let t0 = Instant::now();
            let mut sent: u64 = 0;
            for _ in 0..WRITES {
                let mut off: u32 = 0;
                while off < CHUNK_BYTES {
                    let mut wrote: u32 = 0;
                    let ok = WinHttpWriteData(
                        request,
                        buf.as_ptr() as *const c_void,
                        CHUNK_BYTES - off,
                        &mut wrote,
                    );
                    if ok == 0 {
                        eprintln!(
                            "i65-http: WinHttpWriteData failed at {sent} GLE={}",
                            GetLastError()
                        );
                        WinHttpCloseHandle(request);
                        WinHttpCloseHandle(connect);
                        WinHttpCloseHandle(session);
                        return exit::STREAM_FATAL;
                    }
                    off += wrote;
                    sent += wrote as u64;
                }
            }
            let send_wall = t0.elapsed();

            let got_resp = WinHttpReceiveResponse(request, std::ptr::null_mut());
            let status = if got_resp != 0 { query_status(request) } else { 0 };
            let sink_line = if got_resp != 0 {
                read_response_body(request)
            } else {
                eprintln!(
                    "i65-http: WinHttpReceiveResponse failed GLE={}",
                    GetLastError()
                );
                String::new()
            };

            WinHttpCloseHandle(request);
            WinHttpCloseHandle(connect);
            WinHttpCloseHandle(session);

            let retrans_after = tcp_retrans_segs();
            let secs = send_wall.as_secs_f64().max(1e-9);
            let mb_s = sent as f64 / 1_000_000.0 / secs;
            let mib_s = sent as f64 / 1_048_576.0 / secs;
            let (nic, ltx, lrx) = link
                .clone()
                .unwrap_or_else(|| ("unknown".into(), 0, 0));
            let retrans_delta = match (retrans_before, retrans_after) {
                (Some(b), Some(a)) => a.wrapping_sub(b).to_string(),
                _ => "null".into(),
            };

            println!(
                "I65_HTTP_CLIENT_RESULT {{\"label\":\"{}\",\"bytes\":{sent},\"writes\":{WRITES},\"chunk_bytes\":{CHUNK_BYTES},\"client_send_ms\":{:.1},\"client_mb_s\":{:.2},\"client_mib_s\":{:.2},\"http_status\":{status},\"nic\":\"{}\",\"link_tx_bps\":{ltx},\"link_rx_bps\":{lrx},\"tcp_retrans_segs_delta\":{}}}",
                args.label,
                send_wall.as_millis(),
                mb_s,
                mib_s,
                nic,
                retrans_delta
            );
            if !sink_line.is_empty() {
                println!("I65_HTTP_SINK_LINE {}", sink_line.trim());
            }

            if status != 200 || sent != TOTAL_BYTES as u64 {
                eprintln!(
                    "i65-http: FAIL http_status={status} sent={sent} want={TOTAL_BYTES}"
                );
                return exit::INCOMPLETE;
            }
            eprintln!("i65-http: OK status=200 sent={sent} in {} ms", send_wall.as_millis());
            exit::PASS
        }
    }

    unsafe fn query_status(request: *mut c_void) -> u32 {
        let mut code: u32 = 0;
        let mut len: u32 = 4;
        let ok = WinHttpQueryHeaders(
            request,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            std::ptr::null(),
            &mut code as *mut u32 as *mut c_void,
            &mut len,
            std::ptr::null_mut(),
        );
        if ok == 0 {
            0
        } else {
            code
        }
    }

    unsafe fn read_response_body(request: *mut c_void) -> String {
        let mut out: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let mut got: u32 = 0;
            if WinHttpReadData(
                request,
                chunk.as_mut_ptr() as *mut c_void,
                chunk.len() as u32,
                &mut got,
            ) == 0
            {
                break;
            }
            if got == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..got as usize]);
            if out.len() > 65_536 {
                break;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// The operational interface carrying the transfer, best-effort: prefer an
    /// up Ethernet interface (IF_TYPE_ETHERNET_CSMACD = 6) with a real link
    /// speed; otherwise the first up interface with a real link speed.
    /// Returns (alias, transmit_bps, receive_bps).
    fn nic_link_speed() -> Option<(String, u64, u64)> {
        const IF_TYPE_ETHERNET_CSMACD: u32 = 6;
        unsafe {
            let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
            if GetIfTable2(&mut table) != ERROR_SUCCESS || table.is_null() {
                return None;
            }
            let n = (*table).NumEntries as usize;
            let rows = std::ptr::addr_of!((*table).Table) as *const MIB_IF_ROW2;
            let mut eth: Option<(String, u64, u64)> = None;
            let mut any: Option<(String, u64, u64)> = None;
            for i in 0..n {
                let r = &*rows.add(i);
                if r.OperStatus != 1
                    || r.TransmitLinkSpeed == 0
                    || r.TransmitLinkSpeed == u64::MAX
                {
                    continue; // not up, or no real link speed
                }
                let alias = String::from_utf16_lossy(&r.Alias);
                let cand = (
                    alias.trim_end_matches('\0').trim().to_string(),
                    r.TransmitLinkSpeed,
                    r.ReceiveLinkSpeed,
                );
                if r.Type == IF_TYPE_ETHERNET_CSMACD {
                    if eth.is_none() {
                        eth = Some(cand);
                    }
                } else if any.is_none() {
                    any = Some(cand);
                }
            }
            FreeMibTable(table as *const c_void);
            eth.or(any)
        }
    }

    /// IPv4 TCP `dwRetransSegs` counter (cheap, cumulative). Delta across the
    /// POST is a coarse retransmit signal. Best-effort.
    fn tcp_retrans_segs() -> Option<u32> {
        unsafe {
            let mut stats: MIB_TCPSTATS_LH = std::mem::zeroed();
            if GetTcpStatisticsEx(&mut stats, 2 /* AF_INET */) == 0 {
                Some(stats.dwRetransSegs)
            } else {
                None
            }
        }
    }
}
