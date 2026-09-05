#!/usr/bin/env python3
"""Extract the exact TCP payload bytes of the HTTP response body segment
(server 192.168.99.1:8080 -> client, the packet carrying Content-Length
118 bytes) from the Phase 9b pcap, and hash it. Read-only; reuses the
minimal pcap parser already written for TFTP reconstruction in this Issue.
"""
import struct
import sys
import hashlib

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
    sport, dport, seq, ack = struct.unpack_from(">HHII", pkt, tcp_off)
    data_off = ((pkt[tcp_off + 12] >> 4) & 0x0F) * 4
    flags = pkt[tcp_off + 13]
    payload = pkt[tcp_off + data_off:]
    return src_ip, sport, dst_ip, dport, seq, ack, flags, payload

def main(path):
    pkts = parse_pcap(path)
    segments = []
    for pkt in pkts:
        t = parse_tcp(pkt)
        if not t:
            continue
        src_ip, sport, dst_ip, dport, seq, ack, flags, payload = t
        if src_ip == "192.168.99.1" and sport == 8080 and len(payload) > 0:
            segments.append((seq, payload))
    segments.sort()
    print(f"Found {len(segments)} server->client payload-bearing TCP segments on port 8080:")
    for seq, payload in segments:
        print(f"  seq={seq} len={len(payload)}")
    # Reassemble full stream then split headers/body on CRLFCRLF
    full = b"".join(p for _, p in segments)
    sep = b"\r\n\r\n"
    idx = full.find(sep)
    if idx == -1:
        print("ERROR: could not find header/body separator")
        return 1
    headers = full[:idx].decode(errors="replace")
    body = full[idx+len(sep):]
    print("\n--- Reassembled response headers ---")
    print(headers)
    print(f"\n--- Reassembled body: {len(body)} bytes ---")
    print(body.decode(errors="replace"))
    print(f"\nSHA-256 of reassembled body: {hashlib.sha256(body).hexdigest()}")
    return 0

if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
