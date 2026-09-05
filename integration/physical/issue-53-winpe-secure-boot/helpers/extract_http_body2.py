#!/usr/bin/env python3
import struct, sys, hashlib

def parse_pcap(path):
    with open(path, "rb") as f:
        data = f.read()
    off = 24
    pkts = []
    n = len(data)
    while off + 16 <= n:
        ts_sec, ts_usec, incl_len, orig_len = struct.unpack_from("<IIII", data, off)
        off += 16
        pkt = data[off:off+incl_len]
        off += incl_len
        pkts.append(pkt)
    return pkts

def parse_tcp(pkt):
    if len(pkt) < 14:
        return None
    ethertype = struct.unpack_from(">H", pkt, 12)[0]
    ip_off = 14
    if ethertype != 0x0800:
        return None
    ihl = (pkt[ip_off] & 0x0F) * 4
    proto = pkt[ip_off + 9]
    if proto != 6:
        return None
    src_ip = ".".join(str(b) for b in pkt[ip_off+12:ip_off+16])
    dst_ip = ".".join(str(b) for b in pkt[ip_off+16:ip_off+20])
    tcp_off = ip_off + ihl
    sport, dport = struct.unpack_from(">HH", pkt, tcp_off)
    seq = struct.unpack_from(">I", pkt, tcp_off+4)[0]
    data_off = ((pkt[tcp_off + 12] >> 4) & 0x0F) * 4
    payload = pkt[tcp_off + data_off:]
    return src_ip, sport, dst_ip, dport, seq, payload

def main(path, client_port):
    client_port = int(client_port)
    pkts = parse_pcap(path)
    segments = []
    for pkt in pkts:
        t = parse_tcp(pkt)
        if not t:
            continue
        src_ip, sport, dst_ip, dport, seq, payload = t
        if src_ip == "192.168.99.1" and sport == 8080 and dport == client_port and len(payload) > 0:
            segments.append((seq, payload))
    segments.sort()
    total = sum(len(p) for _, p in segments)
    print(f"segments={len(segments)} total_bytes={total}")
    full = b"".join(p for _, p in segments)
    sep = b"\r\n\r\n"
    idx = full.find(sep)
    headers = full[:idx].decode(errors="replace")
    body = full[idx+len(sep):]
    print("--- headers ---")
    print(headers)
    print(f"--- body: {len(body)} bytes, sha256={hashlib.sha256(body).hexdigest()} ---")

if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
