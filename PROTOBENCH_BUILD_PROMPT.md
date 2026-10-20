# ProtoBench — Build Prompt for Claude Code

> Put this file in an empty repository and tell Claude Code:
> **"Read PROTOBENCH_BUILD_PROMPT.md and build the application it describes, end to end. Follow the milestones in order and do not stop until every acceptance criterion passes."**

---

## Part A — Feature review (why each feature matters)

This section is the design rationale. Part B is the build instruction. Each feature is tagged:
- **[MVP]** must be in this build,
- **[V1]** should be in this build if the MVP is solid,
- **[LATER]** design for it, but do not implement.

### A1. Project management
| Feature | Benefit | Tag |
|---|---|---|
| Project = folder with `project.toml` + `.proto` files + layouts + sequences | Reusable across boards; easy to commit into each firmware repo | MVP |
| Recent projects list, open/create/duplicate project | Fast switching between DUTs | MVP |
| Proto hot-reload on file change (with diff warning if field numbers changed) | Firmware and host schema evolve together without restarting the tool | V1 |
| Per-project saved UI layout (docks, plots, bindings) | Every board opens exactly as you left it | MVP |
| Multiple named connection profiles per project (e.g. "bench USB", "RS-485 adaptor") | Same project, different hardware setups | V1 |
| Project schema versioning (`config_version`) with migration | Old projects keep opening after tool upgrades | MVP |

### A2. Schema handling
| Feature | Benefit | Tag |
|---|---|---|
| Load raw `.proto` files at runtime with include paths — **no external `protoc` required** | Zero install friction; works on any bench PC | MVP |
| Also accept a compiled `FileDescriptorSet` (`.pb`/`.desc`) | Supports projects that already produce one in CI | MVP |
| Bundled stub of `nanopb.proto` so `import "nanopb.proto"` and `[(nanopb).max_size = …]` options parse | Nanopb firmware protos load as-is | MVP |
| Schema browser: packages → messages → fields, with types, numbers, enum values and proto comments | Understand an unfamiliar device protocol quickly | MVP |
| Field metadata overlay in config: unit, scale, offset, display name, min/max, format | Show `mV` instead of raw ADC counts without changing firmware | MVP |
| Support proto2 and proto3, `oneof`, nested, repeated, enums, maps, `bytes`, well-known types | Real firmware schemas use all of these | MVP |

### A3. Transports
| Feature | Benefit | Tag |
|---|---|---|
| Serial / USB-CDC: port picker, baud, data bits, parity, stop bits, flow control | Core embedded link | MVP |
| DTR/RTS control buttons and "pulse reset" action | Reset or boot-select a target without unplugging | MVP |
| Port hot-plug detection and auto-reconnect with backoff | Survives board resets and cable wiggles | MVP |
| TCP client, TCP server, UDP (bind + target) | Ethernet/Wi-Fi devices and network bridges | MVP |
| Loopback transport to the built-in simulator | Develop and test the tool and the UI with no hardware | MVP |
| Replay transport (play back a recorded session at 1×/N×/max speed) | Debug offline from a captured log | V1 |
| SEGGER RTT, BLE GATT, CAN (SocketCAN/PCAN), MQTT | Other common embedded links | LATER |

### A4. Framing and codec
| Feature | Benefit | Tag |
|---|---|---|
| Pluggable framers: COBS (0x00 delimiter), SLIP, varint length-prefix, fixed u8/u16/u32 length-prefix (LE/BE), optional sync-word header | Protobuf is not self-delimiting; framing must match the firmware | MVP |
| Optional CRC: CRC-8, CRC-16/CCITT-FALSE, CRC-16/MODBUS, CRC-32; configurable position and endianness | Detect line corruption | MVP |
| Message dispatch modes: (a) a single root "envelope" message with a `oneof`, (b) a message-ID prefix byte/varint mapped to message types, (c) one fixed type per direction | Covers the three common firmware conventions | MVP |
| Mixed text + binary demultiplexing (printable ASCII lines outside frames go to the text console) | Firmware `printf` logs and protobuf frames share one UART | MVP |
| Resynchronisation after garbage, with counters for framing/CRC/decode errors | Robust on noisy links; shows the link health | MVP |

### A5. Commands (host → device)
| Feature | Benefit | Tag |
|---|---|---|
| Config-defined command buttons: name, message type, field values (JSON/TOML mapping), optional shortcut key, colour, group | One-click repeatable actions | MVP |
| Editable parameters per button (fields marked `prompt = true` show a form before sending) | Adjustable setpoints without editing config | MVP |
| Auto-generated message composer for **any** message in the schema (form built from descriptors, with enums as dropdowns, nested/repeated support) | Send arbitrary messages during exploration | MVP |
| Request/response correlation via a configurable sequence-ID field, auto-incremented | Know which reply belongs to which request | MVP |
| Timeout, retry count, "expect response type" per command, with OK/timeout/error status shown on the button | Immediate feedback | MVP |
| Confirmation dialog for commands flagged `dangerous = true` | Avoid accidental "erase flash" | MVP |
| Periodic send (send every N ms) toggle per button | Heartbeats and polling | MVP |
| Save the composer contents as a new button in the project | Grows the project organically | V1 |
| Command history with resend | Quick repeat while debugging | MVP |

### A6. Telemetry (device → host)
| Feature | Benefit | Tag |
|---|---|---|
| **Stream mode**: subscribe to a message type and extract field paths into channels | Continuous sensor data | MVP |
| **Poll mode**: send a request on a timer, then extract channels from the matched response | Devices that only answer on request | MVP |
| **Triggered mode**: capture N samples after a button press or a condition | Single-shot measurements | V1 |
| Field-path syntax with repeated indexing and wildcards: `sample.adc[2]`, `sample.adc[*]` | Arrays of channels without listing each one | MVP |
| Timestamp source: device field (with unit µs/ms/s and wrap handling) or host receive time | Accurate timing when the firmware timestamps samples | MVP |
| Derived channels from expressions (`vout * 1000 / gain`, `sqrt(a*a+b*b)`) | Engineering units and computed values without firmware changes | V1 |
| Per-channel statistics: rate (Hz), min/max/mean/std, last value, sample count | Instant sanity check of the data | MVP |
| Large ring buffers (configurable, default 1,000,000 points per channel) | Long captures without running out of memory | MVP |

### A7. Visualisation
| Feature | Benefit | Tag |
|---|---|---|
| Dockable, rearrangeable panels saved per project | Each board gets a bespoke dashboard | MVP |
| Time plot: multiple channels, multiple Y axes, autoscale, pause/resume, scroll window length, zoom/pan, two cursors with Δt/ΔY readout | The core analysis view | MVP |
| Min/max decimation for rendering | Smooth with millions of points | MVP |
| XY plot (channel vs channel) | Hysteresis, I–V curves, Lissajous | V1 |
| Numeric readouts / gauges / big-number tiles with limit colouring | Live values at a glance on a bench | MVP |
| State / LED indicators for enums and bools, with enum names | Show modes and faults clearly | MVP |
| Table view: latest value per field, or a scrolling history of messages | Inspect structured data | MVP |
| Message inspector: decoded tree of the selected message, plus raw hex with field highlighting | Debug the encoding itself | MVP |
| Raw byte console (hex + ASCII, rx/tx colouring, timestamps) | Low-level link debugging | MVP |
| Text console for demultiplexed firmware log lines, with filter and level colouring | Firmware logs next to the data | MVP |
| Histogram / FFT panel | Noise analysis for sensor work | V1 |

### A8. Recording, export and replay
| Feature | Benefit | Tag |
|---|---|---|
| Session recording: raw rx/tx bytes with timestamps + decoded messages, to one session file | Reproducible captures; the evidence for bug reports | MVP |
| Export channels to CSV (selected channels, time range) | Post-processing in Python/Excel | MVP |
| Export to MCAP with Protobuf schema | Open captures in Foxglove or PlotJuggler | V1 |
| Export to Parquet | Large datasets for pandas/polars | LATER |
| Session notes and markers (press a key to drop a labelled marker) | Annotate events during testing | V1 |

### A9. Test automation
| Feature | Benefit | Tag |
|---|---|---|
| Sequence files (TOML): steps `send`, `wait`, `expect` (message type, timeout, field assertions with limits), `capture` (N samples → statistics → limit check), `set` variable, `loop`, `log`, `prompt_operator` | Repeatable bring-up and production tests | MVP |
| Run a sequence from the GUI with live step progress and a pass/fail summary | Bench testing without scripting | MVP |
| Headless CLI: `protobench run --project X --sequence Y --port COM5` with exit code and JSON + JUnit XML reports | CI, test stations and jigs | MVP |
| CLI subcommands `decode` (decode a capture file), `send` (one message), `list-ports`, `schema` (print messages) | Scriptable utilities | MVP |
| Python bindings / scripting engine (Rhai or Lua) | Custom logic in sequences | LATER |
| Foxglove WebSocket server output | Use Foxglove's panels live | LATER |

### A10. Diagnostics and robustness
| Feature | Benefit | Tag |
|---|---|---|
| Link statistics panel: bytes/s, frames/s, framing errors, CRC errors, decode errors, unknown message IDs, dropped samples | Shows immediately when the link or schema is wrong | MVP |
| Request latency statistics (min/avg/max/p95) per command | Characterise firmware responsiveness | V1 |
| Decode-error capture: keep the last N bad frames as hex for inspection | Debug schema mismatches | MVP |
| Status bar: connection state, port, rates, recording indicator | Always-visible health | MVP |

### A11. Built-in device simulator
| Feature | Benefit | Tag |
|---|---|---|
| Simulator configured in `project.toml`: generates telemetry messages at a set rate with field generators (`sine`, `square`, `ramp`, `noise`, `random_walk`, `constant`, `counter`, `enum_cycle`) | Develop dashboards before hardware exists | MVP |
| Responds to commands with configured responses (echoing the sequence ID); can inject delays, drops and corrupt frames | Test timeouts, retries and error handling | MVP |
| Runs over the in-process loopback **or** as a TCP server (`protobench sim --project X --tcp 9000`) | Exercise the real transport path | MVP |

---

## Part B — Build instructions for Claude Code

You are building **ProtoBench**: a cross-platform desktop tool for developing and testing embedded sensors and systems that communicate using Protocol Buffers. Build the complete application described below in this repository. Work autonomously: plan, implement, test, and fix until all acceptance criteria in section B12 pass. Implement everything tagged **[MVP]** in Part A, then as many **[V1]** items as you can while keeping the build green. Do not implement **[LATER]** items, but keep the architecture open for them (trait-based transports, framers and exporters).

### B1. Technology stack (mandatory)
- **Language:** Rust (stable, edition 2021), as a Cargo workspace.
- **GUI:** `eframe`/`egui`, `egui_plot` for plots, `egui_dock` for dockable panels.
- **Protobuf reflection:** `prost-reflect` (`DynamicMessage`, descriptors, serde JSON mapping) and `protox` to compile `.proto` source at runtime in pure Rust. **Do not require an external `protoc`.**
- **Serial:** `serialport`. **Networking:** std or `tokio`. Choose one concurrency model and use it consistently.
- **Channels:** `crossbeam-channel` (or tokio mpsc if you use tokio throughout).
- **Config:** `serde` + `toml`. **Expressions:** `evalexpr`. **CLI:** `clap` (derive). **Errors:** `thiserror` in libraries, `anyhow` in binaries. **Logging:** `tracing`.
- **CRC:** the `crc` crate. **COBS:** the `cobs` crate or your own implementation with tests.
- **MCAP (V1):** the `mcap` crate.
- Before writing code, check the current published versions and APIs of these crates using the documentation tools available to you, and pin compatible versions. Where an API differs from what this prompt assumes, adapt to the real API; do not invent functions.

### B2. Workspace layout
```
protobench/
├── Cargo.toml                  # workspace
├── crates/
│   ├── pb-core/                # no UI; all logic, fully unit-tested
│   │   ├── config/             # project.toml model, validation, migration
│   │   ├── schema/             # protox compile, descriptor pool, field paths, metadata overlay
│   │   ├── framing/            # Framer trait + COBS, SLIP, varint, fixed-length, sync-word; CRC
│   │   ├── codec/              # dispatch modes (envelope/oneof, id-prefix, fixed); encode/decode
│   │   ├── transport/          # Transport trait + serial, tcp client/server, udp, loopback, replay
│   │   ├── session/            # connection runtime, rx pipeline, tx queue, correlation, stats
│   │   ├── telemetry/          # channels, ring buffers, extraction, derived expressions, statistics
│   │   ├── commands/           # button model, payload building from TOML/JSON, periodic send
│   │   ├── recorder/           # session file write/read, CSV export, MCAP export
│   │   └── sequence/           # sequence model, runner, assertions, reports (JSON, JUnit)
│   ├── pb-sim/                 # device simulator (library + used by the CLI)
│   ├── pb-gui/                 # eframe app (binary: protobench-gui)
│   └── pb-cli/                 # clap CLI (binary: protobench)
├── assets/proto/nanopb.proto   # bundled stub so nanopb options parse
├── examples/
│   └── demo-sensor/            # complete example project (see B10)
├── docs/                       # USER_GUIDE.md, CONFIG_REFERENCE.md, FIRMWARE_INTEGRATION.md
└── README.md
```
`pb-core` must not depend on egui. The GUI and CLI are thin layers over `pb-core`.

### B3. Project file format (`project.toml`)
Implement exactly this model, with validation errors that name the offending key and file line where possible. Unknown keys produce warnings, not errors.

```toml
config_version = 1
name = "Demo Sensor"
description = "Example amperometric sensor board"

[schema]
proto_files   = ["proto/device.proto"]       # relative to project folder
include_paths = ["proto"]                     # bundled nanopb.proto is always available
# descriptor_set = "build/device.desc"        # alternative to proto_files

[connection]                                   # default profile
profile = "usb"

[[connection.profiles]]
name = "usb"
transport = "serial"                           # serial | tcp_client | tcp_server | udp | sim | replay
port = "auto"                                  # "auto" = first port matching vid/pid, or a name like COM5 or /dev/ttyACM0
usb_vid = 0x0483                               # optional, for auto-select
usb_pid = 0x5740
baud = 921600
data_bits = 8
parity = "none"
stop_bits = 1
flow_control = "none"
dtr_on_open = true
rts_on_open = false
auto_reconnect = true

[[connection.profiles]]
name = "sim"
transport = "sim"

[[connection.profiles]]
name = "net"
transport = "tcp_client"
host = "192.168.1.50"
tcp_port = 9000

[framing]
type = "cobs"                     # cobs | slip | varint_len | fixed_len | none
# len_bytes = 2                   # for fixed_len: 1 | 2 | 4
# endian = "little"
# sync_word = [0xAA, 0x55]        # optional header before the length
crc = "crc16_ccitt_false"         # none | crc8 | crc16_ccitt_false | crc16_modbus | crc32
crc_endian = "little"
max_frame_len = 1024
text_passthrough = true           # printable lines outside frames go to the text console

[codec]
mode = "envelope"                 # envelope | id_prefix | fixed
tx_root = "demo.HostToDevice"     # envelope/fixed: message type sent to the device
rx_root = "demo.DeviceToHost"     # envelope/fixed: message type received
# id_prefix mode:
# id_width = 1                    # 1 | 2 | "varint"
# [codec.ids]
# 0x01 = "demo.Telemetry"
# 0x02 = "demo.Ack"

[correlation]
request_field  = "seq"            # path in tx_root that carries the sequence id
response_field = "seq"            # path in rx_root that echoes it
default_timeout_ms = 500
default_retries = 1

[timestamp]
source = "device"                 # device | host
field = "telemetry.timestamp_us"  # used when source = device
unit = "us"                       # us | ms | s
wrap_bits = 32                    # 0 = no wrap handling

# ---------- field metadata overlay ----------
[[fields]]
path = "telemetry.vout_raw"
display = "Vout"
unit = "mV"
scale = 0.0503
offset = 0.0
format = "{:.2}"
warn_min = 100.0
warn_max = 3200.0

# ---------- commands ----------
[[commands]]
name = "Ping"
group = "System"
shortcut = "F1"
message = "ping"                  # oneof member of tx_root (envelope mode) or a full type name
payload = {}
expect = "pong"                   # oneof member or type expected in reply; omit for fire-and-forget

[[commands]]
name = "Set Bias"
group = "AFE"
message = "set_bias"
payload = { millivolts = 200 }
prompt = ["millivolts"]           # fields the user edits before sending
expect = "ack"
timeout_ms = 1000

[[commands]]
name = "Start Stream"
group = "Stream"
message = "stream_ctrl"
payload = { enable = true, rate_hz = 1000 }
expect = "ack"

[[commands]]
name = "Erase Calibration"
group = "Danger"
message = "erase_cal"
payload = {}
dangerous = true
colour = "#c0392b"

[[commands]]
name = "Heartbeat"
group = "System"
message = "ping"
payload = {}
periodic_ms = 1000                # shows a periodic-send toggle on the button

# ---------- telemetry ----------
[[telemetry]]
name = "AFE stream"
mode = "stream"                   # stream | poll | triggered
message = "telemetry"
channels = [
  "telemetry.vout_raw",
  "telemetry.current_na",
  "telemetry.temperature_c",
  "telemetry.adc[*]",
  "telemetry.state",
]

[[telemetry]]
name = "Status poll"
mode = "poll"
request = "get_status"            # command message (oneof member) to send
request_payload = {}
response = "status"
interval_ms = 500
channels = ["status.battery_mv", "status.uptime_s", "status.fault_flags"]

[[derived]]
name = "power_uw"
expr = "telemetry.vout_raw * 0.0503 * telemetry.current_na / 1000"
unit = "uW"

# ---------- simulator ----------
[simulator]
latency_ms = 2
drop_rate = 0.0
corrupt_rate = 0.0

[[simulator.streams]]
message = "telemetry"
rate_hz = 1000
start_enabled = false             # toggled by the stream_ctrl command
fields = { "vout_raw" = "sine(amp=1500, offset=1600, freq=2)", "current_na" = "noise(mean=250, std=5)", "temperature_c" = "random_walk(start=24, step=0.01)", "adc" = "array(4, sine(amp=1000, offset=2048, freq=5))", "state" = "enum_cycle(period_s=3)", "timestamp_us" = "time_us()" }

[[simulator.responses]]
on = "ping"
reply = "pong"
payload = {}

[[simulator.responses]]
on = "set_bias"
reply = "ack"
payload = { ok = true }

[[simulator.responses]]
on = "stream_ctrl"
reply = "ack"
payload = { ok = true }
action = "toggle_stream:telemetry"

[[simulator.responses]]
on = "get_status"
reply = "status"
payload = { battery_mv = "ramp(start=4100, end=3600, period_s=120)", uptime_s = "uptime_s()", fault_flags = 0 }
```

Rules:
- In `envelope` mode, `message = "set_bias"` refers to the `oneof` member of `tx_root`. The correlation `seq` is filled in automatically. A full type name like `demo.SetBias` is also accepted and wrapped automatically if exactly one oneof member has that type.
- Payloads use the Protobuf JSON mapping semantics (field names, enum names as strings, nested tables, arrays). Build them via `prost-reflect`'s serde support.
- Field paths are dot-separated, starting at the oneof member name (envelope mode) or the type's fields. Support `[n]` and `[*]`. `[*]` expands to channels `name[0]`, `name[1]`, … created on first sight.
- Enum channels plot as numbers and display enum names everywhere else.

### B4. Data pipeline and threading
- One I/O thread (or task) per active connection: read bytes → framer → CRC check → codec decode → `DecodedMessage { host_ts, device_ts, type_name, dynamic_message, raw_frame }`.
- The TX path is a queue: encode → add CRC → frame → write. Every TX and RX frame also goes to the recorder and the raw console buffer.
- The telemetry extractor turns decoded messages into channel samples, stored in per-channel ring buffers (`Vec<f64>` time + value, capacity from settings). Use a design that lets the GUI read without blocking the I/O thread for long, such as a short-held `parking_lot::RwLock` per channel or double-buffering.
- The correlation manager tracks outstanding requests by seq id and resolves each one as `Ok(response)`, `Timeout` or `Error`. It handles retries.
- The GUI runs at the display rate and repaints at most around 60 fps while connected. Never perform blocking I/O on the UI thread.
- Performance targets (verify with a benchmark test using the simulator):
  - sustain ≥ 5,000 decoded messages per second with 10 channels and no growing backlog,
  - plot 1,000,000 points per channel at interactive frame rates using min/max decimation per pixel column.

### B5. GUI specification
**Main window**
- **Menu:** File (New/Open/Recent/Duplicate project, Settings, Quit), Connection (profile selector, Connect/Disconnect, DTR/RTS, Pulse reset), Record (Start/Stop, Export CSV, Export MCAP), View (add panel of each type, Reset layout), Tools (Schema browser, Message composer, Sequence runner, Link statistics), Help.
- **Status bar:** connection state, port/endpoint, rx and tx rates (msg/s and B/s), error counters (click to open link statistics), recording indicator with elapsed time.
- **Command bar:** a dockable panel of buttons grouped by `group`, coloured. It shows the last-result badge on each button (✓ / timeout / error with tooltip), periodic toggles, and keyboard shortcuts.

**Dockable panel types** (all can be added multiple times; all configuration is saved in `layout.json` in the project folder):
1. **Time Plot**: drag channels from a channel list onto it, or tick them; per-trace colour, Y axis 1 or 2, visibility; window length; pause; autoscale; two cursors with readout; right-click to export visible data as CSV.
2. **XY Plot** (V1).
3. **Readouts**: tiles with value, unit, warn/limit colours from field metadata.
4. **Indicators**: enum and bool LEDs with names.
5. **Table**: "latest values" mode (field tree with live values) and "message log" mode (scrolling decoded messages, filter by type).
6. **Message Inspector**: select a message from the log to see its decoded tree and its hex dump, with the bytes of the selected field highlighted.
7. **Raw Console**: hex + ASCII, rx/tx coloured, timestamps, pause, clear, search. It can also send raw hex or text.
8. **Text Console**: firmware log lines from text passthrough, with level colouring (`E:`/`W:`/`I:`/`D:` or `[ERROR]` style prefixes), filter box, and auto-scroll toggle.
9. **Channel Statistics**: table of rate, min, max, mean, std, last, and count per channel, with a reset button.
10. **Link Statistics**: counters, the last 20 bad frames as hex, and request latency stats (V1).
11. **Histogram / FFT** (V1).

**Dialogs and tools**
- **Schema Browser**: tree of the descriptor pool with field numbers, types, labels, comments from the `.proto` source, and enum values.
- **Message Composer**: pick any message type and get an auto-generated form (numbers, bools, strings, bytes as hex, enums as dropdowns, nested messages collapsible, repeated fields with add/remove, oneof selector). It has Send, "Save as button" (writes to `project.toml` while preserving formatting with `toml_edit`), and a JSON view toggle.
- **Prompt dialog** for commands with `prompt` fields, pre-filled from the payload.
- **Confirmation dialog** for `dangerous` commands.
- **Project editor**: a basic form for connection profiles and framing. Everything else is edited in `project.toml`, with a "Reload project" action and a file watcher that triggers a reload prompt.
- **Sequence Runner**: pick a sequence, Run/Abort, live step list with status, log output, a final summary, and Save report.
- **Settings**: theme (dark/light), ring-buffer capacity, plot frame-rate cap, recordings folder.

**Errors**: config and schema errors appear in a non-modal error panel with file and line. The app must never panic on bad input. It opens a project with errors and shows them.

### B6. Recording and session files
- **Session file:** `recordings/<project>_<YYYYmmdd_HHMMSS>.pbsession`. It is a simple length-prefixed binary log of records `{kind: rx_frame | tx_frame | text_line | marker, host_ts_ns: u64, bytes}`, with a header containing the tool version and an embedded copy of the compiled `FileDescriptorSet` and the `project.toml` text, so recordings decode on their own.
- **Replay transport** reads `.pbsession` files (V1).
- **CSV export:** columns `time_s` plus the selected channels, aligned by an "as-of" join on the union of timestamps, or one CSV per channel (user choice). It supports a time range.
- **MCAP export (V1):** one channel per message type, schema encoding `protobuf` with the FileDescriptorSet, message encoding `protobuf`, log time = host timestamp.

### B7. Sequences (test automation)
File: `sequences/<name>.toml` inside the project.

```toml
name = "Bring-up smoke test"
description = "Ping, set bias, capture stream, check limits"

[vars]
bias_mv = 200

[[steps]]
action = "send"
command = "Ping"                       # by command name, or message + payload inline
expect = "pong"
timeout_ms = 500

[[steps]]
action = "send"
message = "set_bias"
payload = { millivolts = "${bias_mv}" }
expect = "ack"
assert = [ { path = "ack.ok", eq = true } ]

[[steps]]
action = "send"
command = "Start Stream"

[[steps]]
action = "capture"
channels = ["telemetry.vout_raw", "telemetry.current_na"]
duration_ms = 2000
assert = [
  { channel = "telemetry.current_na", stat = "mean", min = 200, max = 300 },
  { channel = "telemetry.current_na", stat = "std", max = 10 },
  { channel = "telemetry.vout_raw",  stat = "rate_hz", min = 900 },
]

[[steps]]
action = "wait"
ms = 100

[[steps]]
action = "prompt_operator"
message = "Connect the 10 kΩ load and press OK"

[[steps]]
action = "loop"
count = 3
steps = [ { action = "send", command = "Ping", expect = "pong" } ]

[[steps]]
action = "log"
text = "Done"
```

- Assertions on `eq`, `ne`, `min`, `max` and `within` (tolerance). Stats are `mean`, `min`, `max`, `std`, `rate_hz`, `count` and `last`.
- `${var}` substitution in strings and payloads.
- Each step records duration, result and measured values.
- Reports: a JSON report (full detail) and a JUnit XML report (one testcase per step).
- In headless mode, `prompt_operator` either fails or auto-continues depending on `--non-interactive=fail|continue`.

### B8. CLI (`protobench`)
```
protobench list-ports
protobench schema   --project <dir> [--message <type>]
protobench send     --project <dir> [--profile <name>] --command <name> [--set field=value ...] [--wait]
protobench monitor  --project <dir> [--profile <name>] [--json] [--channels a,b]   # prints decoded messages/channels
protobench run      --project <dir> --sequence <name> [--profile <name>] [--port <p>] [--report-dir <d>] [--non-interactive fail|continue]
protobench decode   <file.pbsession> [--json] [--csv out.csv]
protobench sim      --project <dir> [--tcp <port>]                              # simulator as a TCP server
```
`run` exits with code 0 if every step passes, 1 on any failure, and 2 on a configuration or connection error.

### B9. Simulator (`pb-sim`)
- Implements the device side of the same framing, codec and correlation, using the project's schema.
- Generators: `sine(amp, offset, freq)`, `square(amp, offset, freq, duty)`, `ramp(start, end, period_s)`, `noise(mean, std)`, `random_walk(start, step)`, `constant(v)`, `counter()`, `enum_cycle(period_s)`, `time_us()`, `uptime_s()`, `array(n, gen)`.
- Responses echo the correlation id. Actions supported: `toggle_stream:<msg>`, `enable_stream:<msg>`, `disable_stream:<msg>`.
- Fault injection: `latency_ms`, `drop_rate`, `corrupt_rate` (flips a random byte after the CRC is computed), and `garbage_rate` (inserts random bytes between frames). It also emits occasional text log lines so text passthrough can be tested.
- Usable through the in-process `sim` transport and as a TCP server (`protobench sim`).

### B10. Example project (`examples/demo-sensor/`)
Create a complete working example:
- `proto/device.proto` (proto3, package `demo`). It has `HostToDevice { uint32 seq = 1; oneof cmd { Ping ping; SetBias set_bias; StreamCtrl stream_ctrl; GetStatus get_status; EraseCal erase_cal; } }` and `DeviceToHost { uint32 seq = 1; oneof msg { Pong pong; Ack ack; Telemetry telemetry; Status status; } }`, with sensible fields matching the `project.toml` above. It includes an enum `SensorState { IDLE, WARMUP, MEASURING, FAULT }`, a repeated field `adc`, `uint64 timestamp_us`, an `import "nanopb.proto";` line, and at least one `[(nanopb).max_count = 4]` option to prove nanopb compatibility. Comment the fields.
- `project.toml` exactly as in B3, with all three profiles.
- `layout.json`: a default dashboard with a time plot of Vout and current, readouts, a state indicator, the command bar, a text console and channel statistics.
- `sequences/smoke.toml`: the sequence in B7, which passes against the simulator. Default `prompt_operator` handling for this example is `continue`.
- `firmware/`: a small reference C snippet using nanopb (`encode_frame()`/`decode_frame()` with COBS + CRC-16/CCITT-FALSE, little-endian CRC appended before COBS encoding) and a matching Rust `no_std` snippet using `prost` + `cobs` + `crc`. These are documentation only and do not need to compile in the workspace. They must exactly match the framing implemented in `pb-core`. Note the frame order in the docs: `COBS( protobuf_bytes || crc_le ) || 0x00`.

### B11. Documentation
- `README.md`: what the tool is, screenshots placeholder, build/run commands, and a quick start with the simulator.
- `docs/USER_GUIDE.md`: every panel, dialog and workflow.
- `docs/CONFIG_REFERENCE.md`: every key in `project.toml`, sequence files and `layout.json`, with defaults.
- `docs/FIRMWARE_INTEGRATION.md`: the framing, CRC, dispatch-mode and correlation conventions in byte-level detail with worked hex examples, plus how to adapt an existing firmware protocol.

### B12. Testing and acceptance criteria
Write tests as you go. Required:

**Unit tests (`pb-core`, `pb-sim`)**
- Each framer round-trips random payloads (property tests with `proptest`), handles split reads at every byte boundary, and resynchronises after injected garbage.
- Each CRC variant matches its published check value for `"123456789"`.
- Schema: the example `.proto` compiles with protox, including the nanopb import. A malformed proto gives an error containing the file and line.
- Codec: all three dispatch modes encode and decode; an unknown ID or type increments the counter and does not panic.
- Payload building from TOML (nested, enums by name, repeated, bytes) produces the expected `DynamicMessage`.
- Field-path extraction including `[n]` and `[*]`; scale/offset metadata; enum handling.
- Correlation: response matched by seq; timeout; retry; out-of-order responses.
- Derived-expression evaluation.
- Ring buffer wrap-around and the min/max decimator.
- Sequence assertions and report generation, both JSON and JUnit XML, the latter validated against expected structure.
- Session file write → read round trip; CSV export output.

**Integration tests**
- Run the `smoke` sequence headlessly against the in-process simulator. It must pass.
- The same test over TCP: start `pb-sim` as a TCP server on an ephemeral port and connect with the `tcp_client` transport.
- A fault-injection run (`corrupt_rate = 0.05`, `garbage_rate = 0.05`) shows non-zero CRC and framing error counters while telemetry keeps flowing and `ping` still succeeds with retries.
- Performance test (`#[ignore]` by default, run in milestone verification): the simulator at 5 kHz for 10 s with no backlog growth and no dropped samples in the loopback.

**Acceptance criteria — all must hold before you finish**
1. `cargo build --workspace --release` succeeds with no errors, and `cargo clippy --workspace -- -D warnings` is clean.
2. `cargo test --workspace` passes.
3. `protobench run --project examples/demo-sensor --sequence smoke --profile sim` exits 0 and writes JSON and JUnit reports.
4. `protobench-gui` opens the demo project with the `sim` profile and its default layout. Clicking Connect then **Start Stream** shows live plots, readouts, state indicator and text console lines. **Set Bias** opens the prompt dialog and shows ✓ on the button. **Erase Calibration** asks for confirmation.
5. The Message Composer can build and send any `HostToDevice` command from the schema.
6. Recording a session, then running `protobench decode` on it, reproduces the decoded messages, and CSV export opens correctly.
7. A project with a broken `.proto` or `project.toml` opens and shows readable errors without crashing.
8. All documentation files exist and match the implemented behaviour.

Since you cannot click the GUI, verify criteria 4–5 by (a) unit-testing the GUI's state and actions layer separately from rendering, and (b) adding a `--smoke-test` flag to `protobench-gui` that loads the demo project, connects to the simulator, sends the scripted commands through the same code paths the buttons use, runs the UI loop headlessly or for a few frames if a display is available, asserts that channels receive data, and exits 0. Run it if the environment allows; otherwise rely on (a) and state this in the final summary.

### B13. Milestones (follow in order; commit after each with a descriptive message)
1. Workspace skeleton, config model + validation, schema loading via protox (with bundled `nanopb.proto`), example project files. Tests green.
2. Framing + CRC + codec (all modes) with property tests.
3. Transports (serial, TCP client/server, UDP, loopback) + session runtime + correlation + statistics.
4. Simulator (`pb-sim`) + `protobench sim` + loopback integration test.
5. Telemetry extraction, ring buffers, decimation, derived channels, field metadata.
6. Recorder (session file, CSV) + `decode` CLI.
7. Sequences + `run` CLI + reports. **The `smoke` sequence passes against the simulator.**
8. GUI: app shell, project open/recent, connection controls, status bar, command bar, prompt/confirm dialogs.
9. GUI: dockable panels (time plot, readouts, indicators, table, inspector, raw console, text console, channel stats, link stats), layout persistence.
10. GUI: schema browser, message composer (with "save as button"), sequence runner, settings, error panel, file watching/reload.
11. V1 features in this order: replay transport, MCAP export, XY plot, markers/notes, latency stats, triggered capture, histogram/FFT, multiple profiles UI, proto hot-reload diff warning.
12. Documentation, clippy clean-up, the full acceptance pass, and a final summary listing what was implemented, what was deferred, and any deviations from this spec and why.

### B14. Engineering rules
- No `unwrap()`/`expect()` on data that comes from files, the wire or the user. Return errors and show them.
- All bytes from the wire are untrusted: bound every allocation by `max_frame_len`; never index without checking bounds.
- Keep `pb-core` deterministic and testable. Inject clocks (a `Clock` trait) so timeouts and generators are testable without sleeping.
- Prefer small modules with doc comments on public items.
- When this spec is ambiguous, choose the option that is most consistent with the rest of the spec, record the decision in `docs/DECISIONS.md`, and keep going. Do not stop to ask questions.
