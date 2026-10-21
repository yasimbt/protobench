"""End-to-end test of the `protobench` module against the demo simulators.

Run by `cargo test -p pb-py` (tests/python.rs), which builds the module and
puts it on PYTHONPATH:  python test_protobench.py <examples/demo-sensor> <tmp>
"""

import os
import sys
import time

import protobench


def check_errors(project):
    try:
        protobench.Project(os.path.join(project, "no-such-folder"))
    except ValueError as e:
        assert "has errors" in str(e), e
    else:
        raise AssertionError("opening a missing project must fail")


def main(project, tmp):
    check_errors(project)
    p = protobench.Project(project)
    assert p.name == "Demo Sensor", p.name
    assert p.devices == ["main", "ref"], p.devices
    assert "smoke" in p.sequences and "compare" in p.sequences, p.sequences
    assert p.profiles("ref") == ["sim", "usb"], p.profiles("ref")

    with p.connect(profile="sim", profiles={"ref": "sim"}) as s:
        assert s.devices == ["main", "ref"], s.devices
        assert s.state == "Connected", s.state

        # Commands: names from project.toml, keyword arguments override fields.
        r = s.send("Ping")
        assert r["status"] == "ok" and "pong" in r["response"], r
        assert r["latency_ms"] > 0
        r = s.send("Set Bias", millivolts=321)
        assert r["status"] == "ok" and r["response"]["ack"]["ok"] is True, r
        r = s.send_msg("set_bias", {"millivolts": 5}, expect="ack")
        assert r["status"] == "ok", r
        try:
            s.send("No Such Command")
        except KeyError:
            pass
        else:
            raise AssertionError("an unknown command must raise KeyError")

        # Telemetry.
        assert s.send("Start Stream")["status"] == "ok"
        msg = s.wait("telemetry", timeout_s=2)
        assert "telemetry" in msg, msg
        time.sleep(0.6)
        st = s.stats("telemetry.current_na")
        assert st["count"] > 200 and 200 < st["mean"] < 300, st
        t, v = s.samples("telemetry.vout_raw")
        assert len(t) == len(v) > 200, (len(t), len(v))
        later, _ = s.samples("telemetry.vout_raw", since_s=t[-1] - 0.1)
        assert 0 < len(later) < len(t)
        assert s.last("telemetry.vout_raw") is not None
        assert s.last("no.such.channel") is None
        assert "ref.reading.vout_mv" in s.channels()
        try:
            s.wait("status_unknown", timeout_s=0.1)
        except KeyError:
            pass
        n = 0
        for m in s.messages(timeout_s=0.2):
            assert m["device"] == "main" and "type" in m
            n += 1
            if n >= 50:
                break
        assert n == 50, n

        # The other device, by its own channel names.
        ref = s.device("ref")
        assert ref.state == "Connected"
        r = ref.send("Ping")
        assert r["response"]["pong"]["model"] == "SIM-REF-1", r
        assert ref.last("reading.vout_mv") is not None
        assert ref.stats("reading.vout_mv")["count"] > 10
        # Messages use protobuf JSON names, as `decode --json` prints them.
        assert ref.wait("reading", timeout_s=1)["reading"]["voutMv"] >= 0

        # Sequences run on every connected device.
        rep = s.run_sequence("smoke", vars={"load_ohms": 4700})
        assert rep["passed"], rep["summary"]
        assert "input load_ohms = 4700.0" in rep["log"], rep["log"]
        rep = s.run_sequence("compare")
        assert rep["passed"], rep["summary"]
        assert rep["summary"]["total"] == 8
    # Leaving the block closed the session.
    assert s.state == "Disconnected", s.state

    # Recordings, through the CLI's recorder: decode and export.
    rec = os.environ.get("PB_TEST_RECORDING")
    if rec:
        records = protobench.decode(rec)
        assert len(records) > 100 and records[0]["kind"], records[0]
        files = protobench.export_csv(rec, os.path.join(tmp, "out.csv"), channels=["telemetry.vout_raw"])
        with open(files[0]) as f:
            assert f.readline().strip() == "time_s,telemetry.vout_raw"
        rows = protobench.export_parquet(rec, os.path.join(tmp, "out.parquet"))
        assert rows > 100, rows
    print("python tests passed")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
