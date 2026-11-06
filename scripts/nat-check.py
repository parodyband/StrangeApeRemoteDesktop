#!/usr/bin/env python3
"""Check how this network's NAT maps UDP ports (for Sard's Connect Anywhere).

    curl -fsSL https://raw.githubusercontent.com/parodyband/StrangeApeRemoteDesktop/main/scripts/nat-check.py | python3

Asks three STUN servers from one socket. If they all see the same public
port, direct connections can be set up through this network. If the port
differs per server ("symmetric" NAT, common on phone networks), direct
connections need a relay.
"""
import os, socket, struct, sys
servers = [("stun.cloudflare.com", 3478), ("stun.l.google.com", 19302), ("stun1.l.google.com", 19302)]
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.bind(("0.0.0.0", 0)); s.settimeout(2)
print("local port", s.getsockname()[1])
seen = []
for host, port in servers:
    try:
        addr = socket.getaddrinfo(host, port, socket.AF_INET, socket.SOCK_DGRAM)[0][4]
        tx = os.urandom(12)
        s.sendto(struct.pack("!HHI", 1, 0, 0x2112A442) + tx, addr)
        while True:
            data, frm = s.recvfrom(2048)
            if data[8:20] == tx: break
        i = 20; mapped = None
        while i + 4 <= len(data):
            t, l = struct.unpack("!HH", data[i:i+4]); v = data[i+4:i+4+l]
            if t == 0x20:
                p = struct.unpack("!H", v[2:4])[0] ^ 0x2112
                ip = ".".join(str(b ^ k) for b, k in zip(v[4:8], b"\x21\x12\xa4\x42")); mapped = f"{ip}:{p}"
            i += 4 + (l + 3) // 4 * 4
        print(f"{host:22} sees us as {mapped}")
        if mapped: seen.append(mapped)
    except Exception as e:
        print(f"{host:22} error {e}")
if len(seen) < 2:
    print("\nNot enough answers to tell (is UDP blocked on this network?)")
elif len(set(seen)) == 1:
    print("\nSame public port for every server: direct connections can work from here.")
else:
    print("\nDifferent public port per server (symmetric NAT): direct connections from here need a relay.")
