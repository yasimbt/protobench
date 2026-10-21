# ProtoBench

ProtoBench is a desktop and command-line tool for developing and testing embedded sensors and systems that talk Protocol Buffers. Point it at your firmware's `.proto` files and it decodes the device's messages live, plots telemetry, sends commands from buttons or an auto-generated form, records sessions, and runs test sequences headlessly in CI or on a test jig.

- Schemas are compiled at runtime with [protox](https://crates.io/crates/protox): no `protoc` to install. nanopb options (`import "nanopb.proto"`, `[(nanopb).max_size = 16]`) parse as-is.
- Serial/USB-CDC (with DTR/RTS, pulse reset, hot-plug reconnect, USB serial-number matching), TCP client and server, UDP, MQTT (TLS, QoS 0/1), CAN and CAN FD with ISO-TP (slcan adapters on any OS, SocketCAN on Linux), replay of recordings, and a built-in device simulator.
- COBS, SLIP, varint or fixed length prefix (optional sync word), CRC-8/16/32, envelope (`oneof`), message-ID prefix or fixed dispatch, firmware `printf` lines on the same UART.
- Dockable dashboards saved per project: time plots with two Y axes, cursors and min/max decimation for millions of points; XY plots; readouts with limit colours; enum/bool indicators; message tables and inspector with byte highlighting; raw and text consoles; channel and link statistics; histogram/FFT.
- Request/response correlation with timeouts and retries, periodic sends, derived channels, field metadata (units, scale, limits).
- Session recordings that decode on their own; CSV, Parquet and MCAP export (pandas/polars, Foxglove, PlotJuggler).
- A live Foxglove WebSocket server, so Foxglove's panels show the device while you work.
- Test sequences in TOML with JSON and JUnit reports, sandboxed Rhai scripts for sweeps and custom logic, and `protobench check` to catch broken projects in CI.
- Several different devices at once (`[[devices]]`): the board under test next to a reference meter, each with its own `.proto` and settings, on one time axis, in one recording, and driven by one sequence.

<!-- Screenshot: docs/images/dashboard.png (main window with the demo project connected to the simulator) -->

## Build

Rust stable (1.95 or newer) and a C linker:

```sh
cargo build --workspace --release
```

Parquet export is a default cargo feature of `pb-core` (`parquet`); build with `--no-default-features` on `pb-core` to leave it out and save compile time.

This produces `target/release/protobench` (CLI) and `target/release/protobench-gui` (GUI).

On Linux the GUI loads OpenGL, Wayland/X11 and xkbcommon at runtime; distributions ship these with any desktop. On NixOS, build inside a shell with a C compiler (e.g. `nix shell nixpkgs#gcc`) and run the GUI with those libraries on `LD_LIBRARY_PATH`, for example from a shell with `libGL`, `wayland`, `libxkbcommon` and `libX11`. Serial ports are enumerated without libudev; USB IDs come from sysfs.

## Quick start with the simulator

The demo project in `examples/demo-sensor/` describes an amperometric sensor board and includes a simulator, so it works without hardware. It lists a second device, the reference meter in `examples/demo-reference/` (device `ref`, with its own protocol and simulator), which connects alongside it.

GUI:

```sh
target/release/protobench-gui examples/demo-sensor --profile sim
```

Click **Start Stream**: Vout and current appear on the time plot, the readouts and state indicator update, and the simulator's log lines show up in the text console. **Set Bias…** asks for a value first; **Erase Calibration** asks for confirmation. **Tools ▸ Message Composer** builds any `HostToDevice` message from the schema.

Command line:

```sh
# Run the smoke test sequence; exit code 0 = pass, 1 = fail, 2 = config/connection error
target/release/protobench run --project examples/demo-sensor --sequence smoke --profile sim

# Watch decoded messages while recording, then decode the recording
target/release/protobench monitor --project examples/demo-sensor --profile sim \
    --send "Start Stream" --duration 2 --record demo.pbsession
target/release/protobench decode demo.pbsession --csv demo.csv

# Check the project (exit 1 on errors), run the scripted bias sweep
target/release/protobench check --project examples/demo-sensor
target/release/protobench run --project examples/demo-sensor --sequence scripted --profile sim

# Compare the sensor with the reference meter (device `ref`) in one sequence
target/release/protobench run --project examples/demo-sensor --sequence compare \
    --profile sim --profile ref=sim

# Export part of a recording to Parquet
target/release/protobench decode demo.pbsession --parquet demo.parquet \
    --channels telemetry.vout_raw,telemetry.current_na --range 0.5:1.5

# Watch the simulator live in Foxglove (Open connection > Foxglove WebSocket > ws://127.0.0.1:8765)
target/release/protobench monitor --project examples/demo-sensor --profile sim \
    --send "Start Stream" --foxglove 8765

# Serve the simulator over TCP and talk to it through the "net" profile
target/release/protobench sim --project examples/demo-sensor --tcp 9000 &
target/release/protobench send --project examples/demo-sensor --profile net \
    --port 127.0.0.1:9000 --command Ping --wait
```

## Your own device

Copy `examples/demo-sensor/` (or use **File ▸ New project…**), point `[schema]` at your firmware's `.proto` files, and set `[framing]` and `[codec]` to match the firmware. `docs/FIRMWARE_INTEGRATION.md` explains the wire formats byte by byte, with reference C (nanopb) and Rust framing code in `examples/demo-sensor/firmware/`.

## Documentation

- [User guide](docs/USER_GUIDE.md): every panel, dialog, workflow and CLI command.
- [Configuration reference](docs/CONFIG_REFERENCE.md): every key in `project.toml`, sequence files and `layout.json`, with defaults.
- [Firmware integration](docs/FIRMWARE_INTEGRATION.md): framing, CRC, dispatch modes and correlation, with worked hex examples.
- [Design decisions](docs/DECISIONS.md): choices made where the specification left room.

## Repository layout

```
crates/pb-core   all logic (config, schema, framing, codec, transports, session,
                 telemetry, commands, recorder, sequences and scripts, project
                 checks, Foxglove server); no UI dependencies
crates/pb-sim    device simulator (library; used by the CLI, GUI and tests)
crates/pb-cli    `protobench` command-line tool
crates/pb-gui    `protobench-gui` desktop app (eframe/egui)
assets/proto     bundled nanopb.proto stub
examples         demo-sensor project
docs             documentation
```

## Tests

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
# 5 kHz for 10 s through the simulator loopback (ignored by default)
cargo test --release -p pb-sim --test loopback performance -- --ignored --nocapture
# GUI: drives the command buttons and composer against the simulator while rendering frames
target/release/protobench-gui --smoke-test            # headless rendering
target/release/protobench-gui --smoke-test --window   # also in a real window
```

## License

MIT OR Apache-2.0.
