#!/usr/bin/env python3
"""Minimal Icecast simulator for testing the rinse client offline.

Serves a generated two-tone AAC file (180 Hz + 4 kHz) on 127.0.0.1:8899
with icy-metaint metadata and rotating StreamTitle values. Generates
test.aac via ffmpeg on first run. Accepts connections in a loop; Ctrl-C
to stop.

Usage:  python3 tools/fake_icecast.py
Then:   rinse-rs --url http://127.0.0.1:8899/stream
"""
import itertools
import os
import socket
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
AAC = os.path.join(HERE, "test.aac")
METAINT = 8192
PORT = 8899

if not os.path.exists(AAC):
    print("generating test.aac …")
    subprocess.run(
        ["ffmpeg", "-loglevel", "quiet",
         "-f", "lavfi", "-i", "sine=frequency=180:duration=20",
         "-f", "lavfi", "-i", "sine=frequency=4000:duration=20",
         "-filter_complex", "amix", "-c:a", "aac", "-b:a", "64k", AAC],
        check=True)

DATA = open(AAC, "rb").read()

srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", PORT))
srv.listen(1)
print(f"fake icecast listening on http://127.0.0.1:{PORT}/stream", flush=True)

def serve(conn):
    conn.recv(4096)
    conn.sendall(b"ICY 200 OK\r\n"
                 b"icy-name:Fake Rinse\r\n"
                 b"icy-metaint:%d\r\n\r\n" % METAINT)
    titles = itertools.cycle([b"StreamTitle='Test Show - Live';",
                              b"StreamTitle='DJ Sandbox - B2B Set';"])
    pos = 0
    while True:  # loop the audio forever
        chunk = bytes(DATA[i % len(DATA)] for i in range(pos, pos + METAINT))
        conn.sendall(chunk)
        meta = next(titles)
        meta += b"\x00" * ((-len(meta)) % 16)
        conn.sendall(bytes([len(meta) // 16]) + meta)
        pos = (pos + METAINT) % len(DATA)
        time.sleep(0.25)  # ~32 kB/s, roughly realtime for 64 kbps AAC

try:
    while True:
        conn, addr = srv.accept()
        print(f"client connected: {addr}", flush=True)
        try:
            serve(conn)
        except (BrokenPipeError, ConnectionResetError):
            print("client disconnected", flush=True)
except KeyboardInterrupt:
    sys.exit(0)
