"""Drive the demo sensor from Python.

Build the module once:   pip install maturin && maturin develop -m crates/pb-py/Cargo.toml
Then:                     python examples/demo-sensor/python/smoke.py [project] [profile]

With the default `sim` profile it talks to the built-in simulators; use
`usb` (or your own profile) for the real board.
"""

import sys
import time

import protobench

project = sys.argv[1] if len(sys.argv) > 1 else "examples/demo-sensor"
profile = sys.argv[2] if len(sys.argv) > 2 else "sim"

p = protobench.Project(project)
print(f"{p.name}: devices {', '.join(p.devices)}")

with p.connect(profile=profile) as s:
    pong = s.send("Ping")
    print(f"ping: {pong['status']} in {pong.get('latency_ms', 0):.1f} ms")

    s.send("Set Bias", millivolts=250)
    s.send("Start Stream")
    time.sleep(1.0)

    vout = s.stats("telemetry.vout_raw")
    print(f"Vout: {vout['count']} samples at {vout['rate_hz']:.0f} Hz, mean {vout['mean']:.2f} mV")
    if "ref" in s.devices:
        ref = s.device("ref")
        print(f"reference meter: {ref.last('reading.vout_mv'):.2f} mV")

    report = s.run_sequence("smoke", vars={"load_ohms": 4700})
    summary = report["summary"]
    print(
        f"smoke: {'PASSED' if report['passed'] else 'FAILED'} "
        f"({summary['passed']} passed, {summary['failed']} failed, {summary['errors']} errors)"
    )
    sys.exit(0 if report["passed"] else 1)
