#!/usr/bin/env python3
"""Minimal, dependency-free pcap parser to reconstruct the exact TFTP
sequence (RRQ/OACK/DATA/ACK/ERROR) between the Fedora Server and the
physical Endpoint for Bamep Issue #53 Phase 9a. Read-only; does not
modify the pcap.
"""
import struct
import sys

def parse_pcap(path):
    with open(path, "rb") as f:
        data = f.read()
    magic = struct.unpack_from("<I", data, 0)[0]
    if magic == 0xa1b2c3d4:
        endian = "<"
        nano = False
    elif magic == 0xa1b23c4d:
        endian = "<"
        nano = True
    elif magic == 0xd4c3b2a1:
        endian = ">"
        nano = False
    else:
        raise ValueError(f"unrecognized pcap magic {magic:#x}")
    off = 24
    pkts = []
    n = len(data)
    while off + 16 <= n:
        ts_sec, ts_usec, incl_len, orig_len = struct.unpack_from(endian + "IIII", data, off)
        off += 16
        pkt = data[off:off+incl_len]
        off += incl_len
        pkts.append((ts_sec, ts_usec, pkt))
    return pkts

def parse_udp(pkt):
    # Ethernet
    if len(pkt) < 14:
        return None
    ethertype = struct.unpack_from(">H", pkt, 12)[0]
    ip_off = 14
    if ethertype == 0x8100:  # VLAN tag
        ip_off = 18
        ethertype = struct.unpack_from(">H", pkt, 16)[0]
    if ethertype != 0x0800:
        return None
    if len(pkt) < ip_off + 20:
        return None
    ver_ihl = pkt[ip_off]
    ihl = (ver_ihl & 0x0F) * 4
    proto = pkt[ip_off + 9]
    if proto != 17:  # UDP
        return None
    src_ip = ".".join(str(b) for b in pkt[ip_off+12:ip_off+16])
    dst_ip = ".".join(str(b) for b in pkt[ip_off+16:ip_off+20])
    udp_off = ip_off + ihl
    if len(pkt) < udp_off + 8:
        return None
    sport, dport, ulen, csum = struct.unpack_from(">HHHH", pkt, udp_off)
    payload = pkt[udp_off+8: udp_off+ulen] if ulen >= 8 else pkt[udp_off+8:]
    return src_ip, sport, dst_ip, dport, payload

TFTP_OP = {1: "RRQ", 2: "WRQ", 3: "DATA", 4: "ACK", 5: "ERROR", 6: "OACK"}

def parse_tftp(payload):
    if len(payload) < 2:
        return None
    op = struct.unpack_from(">H", payload, 0)[0]
    opname = TFTP_OP.get(op)
    if not opname:
        return None
    if opname in ("RRQ", "WRQ"):
        rest = payload[2:]
        fields = rest.split(b"\x00")
        fields = [x.decode(errors="replace") for x in fields if x != b""]
        return {"op": opname, "fields": fields}
    elif opname == "DATA":
        if len(payload) < 4:
            return None
        block = struct.unpack_from(">H", payload, 2)[0]
        return {"op": opname, "block": block, "len": len(payload) - 4}
    elif opname == "ACK":
        if len(payload) < 4:
            return None
        block = struct.unpack_from(">H", payload, 2)[0]
        return {"op": opname, "block": block}
    elif opname == "ERROR":
        if len(payload) < 4:
            return None
        code = struct.unpack_from(">H", payload, 2)[0]
        msg = payload[4:].split(b"\x00")[0].decode(errors="replace")
        return {"op": opname, "code": code, "msg": msg}
    elif opname == "OACK":
        rest = payload[2:]
        fields = rest.split(b"\x00")
        fields = [x.decode(errors="replace") for x in fields if x != b""]
        return {"op": opname, "fields": fields}
    return None

def main(path):
    pkts = parse_pcap(path)
    t0 = pkts[0][0] + pkts[0][1] / 1e6
    # Track TFTP "sessions" by (src_ip,src_port,dst_ip,dst_port) ephemeral pairs
    # seeded by RRQ requests to port 69.
    sessions = {}  # key: frozenset of (ip,port) pair (unordered) -> label
    events = []
    for ts_sec, ts_usec, pkt in pkts:
        ts = ts_sec + ts_usec / 1e6
        u = parse_udp(pkt)
        if not u:
            continue
        src_ip, sport, dst_ip, dport, payload = u
        if {src_ip, dst_ip} != {"192.168.99.1", "192.168.99.66"}:
            continue
        is_rrq_port = (dport == 69)
        t = parse_tftp(payload)
        if t is None:
            # could still be a DATA/ACK on an already-tracked ephemeral session
            key = frozenset([(src_ip, sport), (dst_ip, dport)])
            if key in sessions:
                # try parse anyway (should have parsed above already)
                continue
            continue
        rel = ts - t0
        if t["op"] in ("RRQ", "WRQ"):
            label = t["fields"][0] if t["fields"] else "?"
            events.append((rel, ts, "RRQ" if t["op"]=="RRQ" else "WRQ",
                           f"{src_ip}:{sport} -> {dst_ip}:{dport} file={label!r} opts={t['fields'][1:]}"))
        elif t["op"] == "OACK":
            events.append((rel, ts, "OACK", f"{src_ip}:{sport} -> {dst_ip}:{dport} {t['fields']}"))
        elif t["op"] == "ERROR":
            events.append((rel, ts, "ERROR", f"{src_ip}:{sport} -> {dst_ip}:{dport} code={t['code']} msg={t['msg']!r}"))
        elif t["op"] == "DATA":
            events.append((rel, ts, "DATA", f"{src_ip}:{sport} -> {dst_ip}:{dport} block={t['block']} len={t['len']}"))
        elif t["op"] == "ACK":
            events.append((rel, ts, "ACK", f"{src_ip}:{sport} -> {dst_ip}:{dport} block={t['block']}"))
    return events

if __name__ == "__main__":
    evs = main(sys.argv[1])
    for rel, ts, kind, desc in evs:
        print(f"t+{rel:8.3f}s  {kind:6s} {desc}")
