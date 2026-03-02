#!/usr/bin/env python3
"""Test listener for rapid_driver ZMQ replay stream.

Auto-discovers rapid_driver via mDNS (_rapiddriver._tcp.local.)
and subscribes to the ZMQ PUB socket to print incoming messages.

Usage:
    python replay_listener_test.py                  # auto-discover
    python replay_listener_test.py --pose-only      # only /pose topics
    python replay_listener_test.py --zmq tcp://192.168.0.100:5560  # manual
"""

import argparse
import json
import signal
import sys
import time
from collections import defaultdict

import zmq


# ─── mDNS auto-discovery ────────────────────────────────────────────────────

def discover_rapiddriver(timeout: float = 5.0):
    """Discover rapid_driver via mDNS, return (host_ip, zmq_port)."""
    from zeroconf import ServiceBrowser, Zeroconf

    SERVICE_TYPE = "_rapiddriver._tcp.local."
    result = {}

    class Listener:
        def add_service(self, zc, type_, name):
            info = zc.get_service_info(type_, name)
            if info is None:
                return
            import socket
            addresses = info.parsed_scoped_addresses()
            if not addresses:
                return
            host = addresses[0]
            zmq_port = None
            if info.properties:
                zmq_port_bytes = info.properties.get(b"zmq_port")
                if zmq_port_bytes:
                    zmq_port = int(zmq_port_bytes.decode())
            result["host"] = host
            result["zmq_port"] = zmq_port or 5560

        def remove_service(self, zc, type_, name):
            pass

        def update_service(self, zc, type_, name):
            pass

    zc = Zeroconf()
    listener = Listener()
    browser = ServiceBrowser(zc, SERVICE_TYPE, listener)

    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if "host" in result:
            break
        time.sleep(0.1)

    zc.close()

    if "host" not in result:
        return None
    return result["host"], result["zmq_port"]


# ─── Main ────────────────────────────────────────────────────────────────────

def main():
    parser = argparse.ArgumentParser(
        description="Test listener for rapid_driver ZMQ replay stream")
    parser.add_argument(
        "--zmq", type=str, default=None,
        help="ZMQ endpoint (e.g. tcp://192.168.0.100:5560). Auto-discovered if omitted.")
    parser.add_argument(
        "--pose-only", action="store_true",
        help="Only show messages whose topic contains '/pose'")
    parser.add_argument(
        "--timeout", type=float, default=5.0,
        help="mDNS discovery timeout in seconds (default: 5)")
    args = parser.parse_args()

    # Resolve ZMQ endpoint
    if args.zmq:
        endpoint = args.zmq
        print(f"Using manual ZMQ endpoint: {endpoint}")
    else:
        print(f"Discovering rapid_driver via mDNS (timeout={args.timeout}s)...")
        result = discover_rapiddriver(timeout=args.timeout)
        if result is None:
            print("ERROR: Could not discover rapid_driver via mDNS.", file=sys.stderr)
            print("Use --zmq tcp://<ip>:5560 to connect manually.", file=sys.stderr)
            sys.exit(1)
        host, zmq_port = result
        endpoint = f"tcp://{host}:{zmq_port}"
        print(f"Discovered rapid_driver at {endpoint}")

    # Connect ZMQ SUB
    ctx = zmq.Context()
    sub = ctx.socket(zmq.SUB)
    sub.connect(endpoint)
    sub.setsockopt(zmq.SUBSCRIBE, b"")  # subscribe to all topics
    print(f"Connected to {endpoint}, waiting for messages...\n")

    # Stats
    topic_counts = defaultdict(int)
    total = 0
    t_start = time.time()

    def print_stats():
        elapsed = time.time() - t_start
        print(f"\n{'='*50}")
        print(f"  Statistics ({elapsed:.1f}s)")
        print(f"{'='*50}")
        print(f"  Total messages: {total}")
        for topic in sorted(topic_counts):
            print(f"    {topic}: {topic_counts[topic]}")
        print(f"{'='*50}")

    def signal_handler(sig, frame):
        print_stats()
        sys.exit(0)

    signal.signal(signal.SIGINT, signal_handler)

    # Main loop
    while True:
        if sub.poll(100):
            frames = sub.recv_multipart()
            if len(frames) >= 2:
                topic = frames[0].decode("utf-8", errors="replace")
                payload = frames[1]
            elif len(frames) == 1:
                # Fallback: single-frame (legacy)
                topic = "(none)"
                payload = frames[0]
            else:
                continue

            topic_counts[topic] += 1
            total += 1

            # Filter
            if args.pose_only and "/pose" not in topic:
                continue

            # Try to parse JSON for summary
            try:
                data = json.loads(payload)
                # Compact summary: first few keys
                keys = list(data.keys())[:4]
                summary = ", ".join(f"{k}={_compact(data[k])}" for k in keys)
            except Exception:
                summary = f"{len(payload)} bytes"

            print(f"[{total:>6}] {topic:30s} | {summary}")


def _compact(val):
    """Compact representation of a value for display."""
    if isinstance(val, dict):
        return "{...}"
    if isinstance(val, list):
        return f"[{len(val)} items]"
    s = str(val)
    return s[:40] if len(s) > 40 else s


if __name__ == "__main__":
    main()
