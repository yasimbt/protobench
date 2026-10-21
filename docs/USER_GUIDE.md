# ProtoBench user guide

ProtoBench talks to embedded devices that exchange Protocol Buffers messages over serial, TCP, UDP or a built-in simulator. It plots telemetry, sends commands, records sessions and runs test sequences. This guide covers the desktop app (`protobench-gui`) and the command line (`protobench`).

Contents: [Projects](#projects) · [Connecting](#connecting) · [Main window](#main-window) · [Panels](#panels) · [Tools and dialogs](#tools-and-dialogs) · [Recording and export](#recording-and-export) · [Sequences](#sequences) · [Command line](#command-line) · [Simulator](#simulator) · [Troubleshooting](#troubleshooting)

## Projects

A project is a folder:

```
my-board/
├── project.toml        # schema, connections, framing, commands, telemetry, simulator
├── proto/*.proto       # the device's schema (compiled at runtime; no protoc needed)
├── sequences/*.toml    # test sequences
├── layout.json         # the project's default dashboard
├── layout.local.json   # your working layout (saved by the GUI; git-ignored)
├── recordings/         # session recordings (created on first recording)
└── reports/            # sequence reports (created by `protobench run`)
```

Keep it next to the firmware, in the same repository, so schema and tool configuration change together. `docs/CONFIG_REFERENCE.md` lists every key.

**File menu**

- **New project…** creates a minimal project (one `Ping`/`Pong` schema, a `sim` and a `usb` profile) in a chosen folder.
- **Open project…** opens a folder. **Recent** lists the last ten.
- **Duplicate project…** copies the project (without recordings and reports) to a new folder and renames it.
- **Reload project** re-reads `project.toml` and the schema.
- **Settings…**, **Quit**.

A project with errors still opens. The **Problems** panel at the bottom lists each error and warning as `file:line: message`, for both `project.toml` and `.proto` files. Connecting is disabled until the errors are fixed. Unknown keys in `project.toml` are warnings.

**Watching for changes.** ProtoBench checks `project.toml` and the proto files once per second. When one changes, a dialog offers **Reload** or **Ignore**. Reload keeps the selected profile and reconnects if you were connected. If a field number or field disappeared between the old and new schema, the Problems panel shows a warning, because old recordings and firmware built against the old schema will no longer decode correctly.

## Connecting

**Connection menu**

- Pick a profile; the list shows each profile's endpoint (`/dev/ttyACM0 @ 921600`, `tcp://192.168.1.50:9000`, `simulator (loopback)`).
- **Port override** replaces the profile's serial port, or `host:port` for TCP/UDP, without editing the file.
- **Connect** / **Disconnect**.
- **DTR** / **RTS** on and off, and **Pulse reset**, which toggles the profile's `reset_line` away from its idle level for `reset_pulse_ms`. Use it to reset a target or enter a bootloader without unplugging.

Opening happens on the connection's I/O thread; the window stays responsive. With `auto_reconnect` (default), a failed open or a lost link is retried with backoff from 100 ms up to 2 s, so unplugging a USB adapter or resetting the board recovers by itself. The status bar shows the attempt number and the last error.

`port = "auto"` picks the serial port whose USB vendor ID, product ID and serial number match `usb_vid`, `usb_pid` and `usb_serial` (any of them may be left out). Exactly one port must match: with two identical boards attached, connecting fails with a message that lists both ports and their serial numbers, instead of opening whichever comes first. Add `usb_serial` to the profile to choose one; `protobench list-ports` prints the serial numbers, and in the project editor the port menu's **Match this board** fills in all three. Boards without a USB serial number (many CH340 and CP2102 adapters) cannot be told apart this way: name the port instead. Because the match is redone on every reconnect, an unplugged board comes back on its new port name.

A `tcp_server` profile shows **Waiting for client** until a client connects, and again after it leaves. Commands sent meanwhile fail with "no client connected" and periodic sends pause; requests still waiting when the client leaves fail with "client disconnected". A new client starts with a clean framer, so a half frame from the previous one cannot corrupt its first message.

Writes give up after 1 s. A TCP peer that stops reading for that long counts as a lost link (and reconnects with `auto_reconnect`); a serial port that cannot take the data in time (flow control held, device stalled) fails only that command.

### MQTT

Devices that talk through a broker (Wi-Fi or cellular sensors, a gateway) use an `mqtt` profile:

```toml
[[connection.profiles]]
name = "cloud"
transport = "mqtt"
broker = "broker.example.com:8883"
tls = true
username = "bench"
password_env = "MQTT_PASSWORD"     # read from the environment, not the file
subscribe = ["sensors/1234/up"]    # what the device publishes
publish = "sensors/1234/down"      # where commands go
qos = 1

[framing]
type = "none"                      # one MQTT message = one frame
```

ProtoBench is an ordinary MQTT 3.1.1 client: every message on a `subscribe` filter is decoded as one frame, and every command is published to `publish`. With `qos = 1` it keeps a persistent session, so the broker holds messages while the link is down, and commands that were not acknowledged are sent again after a reconnect (they may arrive twice; a sequence-ID field lets the firmware notice). The keep-alive ping notices a dead broker within about one and a half keep-alive intervals.

TLS checks the broker's certificate against the operating system's root store; `tls_ca` adds a private CA, and `tls_cert`/`tls_key` give a client certificate for brokers that require one. `protobench check` reports a missing broker, invalid topic filters, a publish topic with wildcards and QoS 2.

`protobench sim --project . --mqtt localhost:1883` runs the project's simulator as an MQTT device on a broker: it subscribes to the first mqtt profile's `publish` topic and publishes to its first `subscribe` filter (with `+`/`#` replaced by `sim`), so `run --profile cloud --port localhost:1883` works end to end without hardware. `--profile` picks another mqtt profile. A local broker is a one-liner, e.g. `mosquitto -p 1883`; ProtoBench's own tests use a small built-in broker.

### SEGGER RTT

With a debug probe already on the board (J-Link, ST-Link, a CMSIS-DAP v2 probe such as a Pico probe or DAPLink), ProtoBench can talk to the firmware through RTT instead of a UART: the firmware writes into ring buffers in its RAM and the probe reads them while the core runs.

```toml
[[connection.profiles]]
name = "rtt"
transport = "rtt"
chip = "STM32F411RETx"           # probe-rs target name
rtt_up = 1                       # protobuf frames, target → host
rtt_down = 1                     # commands, host → target
rtt_text_channel = 0             # printf → text console
# probe = "0483:374e:004D00213137510D33333639"   # with several probes
# elf = "build/firmware.elf"     # find _SEGGER_RTT directly instead of scanning RAM
```

The data channel is a byte stream like a UART, so keep the project's framing (COBS, SLIP, a length prefix). `rtt_text_channel` puts a second channel, usually the default `printf` channel 0, into the text console, so logs and data no longer share a wire. `protobench list-probes` (and the ▾ menu next to **probe** in the project editor) lists attached probes. If the probe is unplugged or the target is power-cycled, the connection reconnects like a serial port. `attach = false` resets the target while attaching.

On Linux the probe needs USB access (a udev rule, as for any probe-rs tool); on Windows, J-Link and ST-Link work with their usual drivers and CMSIS-DAP v2 probes with WinUSB. Under WSL2, attach the probe with `usbipd`.

If you already run a J-Link GDB server or OpenOCD, their RTT servers work too, with no probe access needed by ProtoBench: a `tcp_client` profile to port 19021 (J-Link) or to the port OpenOCD's `rtt server start` opened gives you the channel as a TCP stream. J-Link prints a short banner when you connect; with text passthrough on it lands in the text console.

### Bluetooth LE

Wireless sensors that expose a UART-style GATT service, most often Nordic's UART Service (NUS, the default), connect with a `ble` profile:

```toml
[[connection.profiles]]
name = "wireless"
transport = "ble"
ble_name = "Sensor-7"            # or ble_address = "C0:FF:EE:12:34:56"
# service / rx_char / tx_char default to the Nordic UART Service

[framing]
type = "cobs"                    # notifications are pieces of a byte stream
```

The host writes commands to `rx_char` and receives notifications from `tx_char`; together they are a byte stream, so keep a framer. Writes are split to the negotiated ATT MTU (20 bytes per chunk on a default link, up to 244 with a larger MTU). A device that disconnects or is power-cycled is found again by the reconnect logic, which scans for it for up to `ble_scan_s`. `protobench ble-scan` lists devices in range with their signal strength and services.

Bluetooth is a cargo feature, off by default because it needs the platform stack at build time on Linux: build with `cargo build --release -p pb-cli -p pb-gui --features ble` (on Linux install `libdbus-1-dev` and `pkg-config` first; BlueZ must be running). Without it, `ble` profiles report that the build has no Bluetooth support.

### CAN

CAN-only devices connect through an slcan adapter (a CANable, USBtin or CANtact with slcan firmware, about any USB CAN stick) on any OS, or through a Linux SocketCAN interface:

```toml
[[connection.profiles]]
name = "can"
transport = "can"
can_backend = "slcan"     # or "socketcan" with interface = "can0"
port = "auto"             # the adapter's serial port
usb_vid = 0xad50          # CANable (candleLight boards enumerate as 1d50:606f)
bitrate = 500000
tx_id = 0x7E0             # the host sends with this identifier
rx_id = 0x7E8             # the device answers with this one

[framing]
type = "none"             # one ISO-TP message = one frame
```

Messages are segmented with ISO-TP (ISO 15765-2): a message of up to 7 bytes travels in one frame, a longer one as a first frame, the receiver's flow control and consecutive frames (up to 4095 bytes on classic CAN, more with the 32-bit first frame). `fd = true` uses CAN FD frames, so most protobuf messages fit in one 64-byte frame; `data_bitrate` turns on bit-rate switching. With `isotp = false` every CAN frame is one message, for devices that keep their messages under 8 (or 64) bytes. `isotp_block_size`, `isotp_st_min_us` and `isotp_padding` match what the firmware expects; the defaults (no block limit, no gap, no padding) suit most.

Wiring: the adapter's CAN_H and CAN_L to the bus, a common ground, and a 120 Ω terminator at each end of the bus (many adapters have one you can switch on). The adapter takes care of the bit timing; the bit rate must match the device's.

`protobench sim --project . --can-slcan /dev/pts/5` runs the simulator behind an emulated slcan adapter on a serial port, typically one end of a virtual serial pair (`socat pty,raw,echo=0,link=/tmp/a pty,raw,echo=0,link=/tmp/b` on Linux and macOS, com0com on Windows), with the first can profile's identifiers swapped round. Run `protobench run --profile can --port /tmp/b` against the other end.

## Multiple devices

A project can connect several different devices at once: the board under test plus a reference meter, a power supply with a protobuf interface, or a second board. Each device is a project folder of its own (its own `.proto`, framing, commands, telemetry and simulator), and the main project lists the others:

```toml
[[devices]]
name = "ref"
project = "../demo-reference"
profile = "sim"
```

The project's own device is `main`. Every device gets its own connection, I/O thread, framer, codec, link statistics and reconnect logic, and all of them share one clock, so their samples sit on one time axis.

- **Channels** of other devices are named `<device>.<channel>` (`ref.reading.vout_mv`); `main`'s keep their plain names, so existing layouts and sequences work unchanged. A derived channel of the main project can combine devices: the demo's `err_mv = telemetry.vout_raw * 0.0503 - ref.reading.vout_mv` is the sensor minus the meter, computed for every sensor sample with the meter's newest reading.
- **Sequences** address a device with `device = "ref"` on `send`, `expect` and `capture` (see the configuration reference). The demo's `sequences/compare.toml` pings both devices, sets the sensor's bias, captures both outputs and checks that `err_mv` averages to zero. A run whose steps need a device that is not connected stops before the first step and names it.
- **Scripts** send to another device with `send("ref:Ping")` or through `device("ref")`, whose `last` and `stats` take the device's own channel names.
- **Recording** puts every device into one `.pbsession` file; decoding, CSV, Parquet and MCAP use the same names as the live view, and other devices' MCAP and Foxglove topics live under `/ref/…`. A replay profile in a device's project plays that device's part of a recording.
- **Two identical boards** can be one project listed twice: `project = "."` with a second profile, told apart by `usb_serial`.

`protobench check` checks the device list (names, paths, nesting, profiles), the devices named in sequences, and channels and derived-expression variables across devices.

In the GUI:

- The status bar has one indicator per device (`● main ● ref`): green when connected, amber while connecting, grey when off. Hover for the endpoint and rates; click to connect or disconnect that device alone.
- **Connection ▸ Connect** connects `main` with the selected profile, **Connect all** connects every device, and **Connection ▸ Device ref** picks that device's profile and connects or disconnects it. A device that drops does not disturb the others.
- The command bar shows `main`'s buttons, then each other device's under its name; badges and periodic sends are per device.
- The message composer, schema browser, raw console, text console, inspector and link statistics have a device picker. The consoles can also show all devices, each line tagged with its device; raw bytes typed in the console go to the device shown. The message table tags other devices' messages (`ref:reading`).
- Plots, readouts, indicators and tables take any device's channels by their full names; drag `ref.reading.vout_mv` next to `telemetry.vout_raw` to compare them on one time axis.
- **Record ▸ Start recording** records every running device (and devices connected while it runs) into one file; exports use the full channel names.
- The sequence runner runs on every running device.

## Main window

- **Menu bar**: File, Connection, Record, View, Tools, Help.
- **Dock area**: panels in tabs. Drag a tab to move it, split the area, or drop it as a floating window. Right-click a tab to rename it. Close tabs with ×. **View ▸ Add panel** adds any panel type, as many times as you like. Your arrangement and every panel's settings are saved automatically to `layout.local.json` in the project folder, so each board opens as you left it, while the project's own `layout.json` stays untouched. **View ▸ Save layout as project default** writes the current layout to `layout.json` (to share it through version control); **View ▸ Reset layout** goes back to it.
- **Status bar**: connection state, endpoint, receive rate (messages/s, bytes/s), transmit rate, error counters (framing, CRC, decode, unknown ID; click to open Link Statistics), a red **● REC** indicator with the elapsed time while recording, and the latest notification.
- **View ▸ Dark theme** switches the theme; **View ▸ Error panel** shows or hides the Problems panel.

Keyboard: command shortcuts from `project.toml` (e.g. `F1` for Ping) work when no text field has focus. **Ctrl+M** adds a marker.

## Panels

### Commands

Buttons from `[[commands]]`, grouped under collapsible `group` headings, in the configured colour, with the shortcut on the right. Hover for the message name and the expected reply.

- A click sends the command. Commands with `prompt` fields open the **prompt dialog** first; `dangerous` commands ask for **confirmation** (after the prompt, if both apply).
- The badge after the button shows the last result: a spinner while waiting, **✔** for a reply (hover for latency), **✔ sent** for fire-and-forget, **⏱ timeout**, or **✖ error** (hover for the reason, e.g. "expected `pong`, got `ack`").
- Commands with `periodic_ms` have an **every N ms** checkbox that sends them repeatedly until unticked.

### Channels

Every telemetry channel with its latest value. Drag a channel onto a time plot or XY plot, or click 📈 to open it in a new time plot. Channels are created when data first arrives: `telemetry.adc[*]` turns into `telemetry.adc[0]` … `telemetry.adc[3]`.

### Time Plot

Plots channels against time.

- **Add channels** by ticking them in the picker on the left (toggle with **channels**) or by dragging them from the Channels panel.
- Per trace: visibility checkbox, colour, **Y1** (left axis) or **Y2** (right axis), and ✖ to remove. Y2 traces are scaled onto the plot and labelled on the right axis.
- **window**: visible time span in seconds. The view follows the newest sample.
- **⏸ Pause** freezes the view (data keeps arriving); **▶ Resume** continues.
- Drag to pan, scroll to zoom; the view then stops following until you press **⟲ Follow**.
- **autoscale Y** fits the Y axes to the visible data.
- **Cursors**: Shift+click sets cursor A, Ctrl+click sets cursor B. Below the plot each visible trace shows its value at A and B, ΔY, and the time difference Δt with 1/Δt.
- Right-click: **Export visible data as CSV…** (visible traces, visible time range) and **Clear cursors**.

Rendering uses min/max decimation per pixel column, so a million points per channel plot smoothly and spikes are never hidden.

### XY Plot

One channel against another (hysteresis, I–V curves, Lissajous figures). Pick X and Y (or drop channels on the plot) and the number of newest points. Samples are paired by arrival order, so use channels from the same message.

### Readouts

Large tiles with the latest value, display name and unit for numeric and derived channels. Tiles turn amber outside `warn_min`/`warn_max` and red outside `min`/`max` from `[[fields]]`. **⚙ channels** chooses which channels to show (none ticked = all).

### Indicators

LEDs for enum and bool channels. An enum shows every value with the active one lit and in bold (values whose name contains FAULT or ERR light red). Bools light green when true. **⚙ channels** chooses channels.

### Table

- **Latest values**: the most recent message of each received type as a tree with field numbers and values (enums show name and number, bytes show hex).
- **Message log**: every decoded message, received (rx) and sent (tx), with number, time and JSON. Type in **filter** to narrow by type. Click a row to show it in the Message Inspector.

### Message Inspector

The selected message (or **Latest rx**) as a decoded tree, with a hex dump of its codec bytes below. Click a field in the tree to highlight its bytes (tag and value) in the hex dump. In id_prefix mode the ID prefix bytes are grey.

### Raw Console

Raw bytes as read and written: time, **RX** (green) or **TX** (blue), hex and ASCII (toggle either). **Pause** stops the view from scrolling; **Clear** empties it; **search** filters by text or by hex sequence. The send box at the top writes raw bytes, typed as hex (`AA 55 01`) or text (`\n` and `\r` escapes work), with no framing added.

### Text Console

Lines the firmware printed between frames (needs `framing.text_passthrough = true`). `E:`/`[ERROR]` lines are red, `W:`/`[WARN]` amber, `I:`/`[INFO]` green, `D:`/`[DEBUG]` grey. **filter** is case-insensitive; **auto-scroll** follows new lines; **Clear** empties the console.

### Channel Statistics

For each channel: rate (Hz), min, max, mean, standard deviation, last value and sample count since the last reset. **Reset statistics** starts over without clearing the plots.

### Link Statistics

Byte and frame rates and totals; framing, CRC and decode errors; unknown message IDs; text lines; dropped events; timeouts, retries and reconnects. **Request latency** shows count, min, average, max and 95th percentile per command, plus timeouts. **Last bad frames** lists the last 20 frames that failed framing, CRC or decoding, with the reason and their bytes in hex: the first place to look when the schema and firmware disagree. **Reset counters** clears all of it.

### Histogram / FFT

For one channel and its newest N samples: a histogram with a chosen number of bins, or the FFT magnitude spectrum (Hann window, mean removed) with the sample rate estimated from timestamps and the peak frequency printed above the plot.

### Command History

Sent commands, newest first, with their payloads. ↻ resends one.

### Markers

Type a label and press **Add marker** (or Ctrl+M anywhere) to drop a marker at the current time. **Notes** are saved as a marker when you press **Save note**. Markers go into the recording.

## Tools and dialogs

### Schema Browser (Tools menu)

The descriptor pool by file and package: messages with their fields (number, name, type, label such as `repeated` or `oneof cmd`) and the comments from the `.proto` source; nested messages; enums with values and comments. **filter** searches message and field names; **show google/nanopb** includes the bundled support files.

### Message Composer (Tools menu)

Builds and sends any message the schema allows.

1. Pick a **Message**: in envelope mode each oneof member of `tx_root` (or the root itself); in id_prefix mode each mapped type.
2. Fill in the **Form**, generated from the descriptors: numbers, checkboxes for bools, text, bytes as hex, enum drop-downs, nested messages (tick to include, then expand), repeated fields with **+ add** and ✖, and a selector for each oneof.
3. Or switch to **JSON**, edit the Protobuf JSON and press **Apply JSON** (maps are edited here).
4. Optionally pick an **expect** reply, then **Send**. The sequence ID is filled in automatically. The result appears next to the button.
5. **Save as button** writes the message as a new `[[commands]]` entry (name, group, message, payload, expect) to `project.toml`, keeping the rest of the file's formatting and comments, and reloads the project.

### Sequence Runner (Tools menu)

Pick a sequence from `sequences/`, then **▶ Run**. Steps appear with their status (✔ pass, ✖ fail or error, – skipped), duration, message and assertion results (description and measured value); the current step has a spinner. **■ Abort** stops after the current step's wait. `prompt_operator` steps show a dialog with **OK** and **Fail**, plus a field for each of the step's `inputs` (OK stays disabled until every field is valid). The **Variables** section under the sequence picker lists the sequence's `[vars]`; edit a value, or add a variable with **+ Variable**, to override it for the next run (values in TOML syntax; **Reset** reloads `[vars]`). The summary line shows the overall result; **Save report…** writes the JSON and JUnit XML reports to a folder.

### Project Editor (Tools menu)

A form for connection profiles (add, remove, edit fields for the selected transport, pick a serial port from the detected list) and for framing (type, CRC and byte order, length width and byte order, sync word, maximum frame length, text passthrough). **Save to project.toml** updates `[framing]` keys in place and rewrites `[[connection.profiles]]` with only non-default values, then reloads. Everything else is edited in `project.toml` directly.

### Triggered capture (Tools ▸ Arm triggered capture)

Lists `mode = "triggered"` telemetry entries. Arming one captures its next `samples` messages. Entries with a `trigger` expression also arm themselves when the expression becomes true.

### Foxglove server (Tools menu)

**Tools ▸ Foxglove server** serves the live session to [Foxglove](https://foxglove.dev) over its WebSocket protocol. In Foxglove choose *Open connection ▸ Foxglove WebSocket* and enter the address shown in the menu and the status bar (default `ws://127.0.0.1:8765`). Every message type is a topic, named as in the MCAP export (`/rx/telemetry`, `/tx/set_bias`, `/text`, `/markers`), with the project's schema attached, so Foxglove's plots, raw-message and log panels work without further setup. Once switched on, the server follows the connection: it starts with each session and stops on disconnect. The port and bind address are in **Settings**; bind `0.0.0.0` to let other machines connect. A viewer that cannot keep up loses messages instead of slowing the session.

### Prompt and confirmation dialogs

The **prompt dialog** lists the command's `prompt` fields pre-filled from its payload. Values are read as TOML values (`300`, `true`, `"text"`, `[1, 2]`); anything else is sent as a string. **Send** (or Enter) sends, **Cancel** discards. The **confirmation dialog** appears for `dangerous` commands; only the red **Send** button sends.

### Settings (File menu)

Theme; ring-buffer size per channel (default 1,000,000 points; applies on the next connect); plot frame-rate cap (default 60 fps); recordings folder (default `<project>/recordings`); Foxglove server address and port (default `127.0.0.1:8765`; applies when the server next starts).

## Recording and export

**Record ▸ Start recording** writes `recordings/<project>_<YYYYmmdd_HHMMSS>.pbsession`. A recording holds every received and sent frame, every text line and marker with host timestamps, plus a copy of `project.toml` and the compiled schema, so it decodes on its own, years later, without the project folder. **Stop recording** closes the file. Disconnecting stops recording too.

- **Record ▸ Export CSV (joined)…**: one file with `time_s` and one column per channel. Rows are the union of all timestamps; each cell holds the channel's latest value at or before that time (empty before its first sample).
- **Record ▸ Export CSV (one file per channel)…**: `<name>_<channel>.csv` files with `time_s,value`.
- **Record ▸ Export Parquet…**: the joined CSV's rows and columns in one Parquet file (`time_s` plus a nullable `float64` column per channel, Snappy compressed), for pandas, polars or DuckDB.
- **Record ▸ Export recording to MCAP…**: pick a `.pbsession`, then an output file. Each message type gets its own channel (`/rx/telemetry`, `/tx/set_bias`, …) with `protobuf` schema and message encoding, so Foxglove Studio and PlotJuggler can open it. Text lines go to `/text` and markers to `/markers` as JSON.
- The time plot's right-click menu exports just the visible range.

**Replay.** A profile with `transport = "replay"`, `file = "recordings/….pbsession"` and `speed` (1 = real time, 0 = as fast as possible) plays the recording's received frames and text lines back through the normal pipeline, so all panels work offline. Text lines are replayed as text whatever the framing settings, and with `framing.type = "none"` each recorded frame arrives as one read. The status bar shows "Replay finished" at the end.

## Sequences

A sequence file is a list of steps: `send`, `wait`, `expect`, `capture`, `set`, `loop`, `log`, `prompt_operator` and `script`, with `${var}` substitution and assertions on replies and channel statistics. See `CONFIG_REFERENCE.md` for every key. The demo's `sequences/smoke.toml`:

1. sends `Ping` and expects `pong`;
2. sends `set_bias` with `millivolts = ${bias_mv}` and asserts `ack.ok == true`;
3. starts the stream;
4. captures two channels for 2 s and checks the mean and standard deviation of the current and the Vout sample rate;
5. waits, asks the operator to connect a load and enter its resistance (`load_ohms`, default 10000 Ω), pings three times in a loop, and logs "Done with a … Ω load".

By default a failed step skips the remaining steps (`continue_on_failure = true` keeps going). Each executed step is one JUnit test case.

**`expect` timing.** An `expect` step matches messages received since the *previous* step started, so a reply that arrives while a fire-and-forget `send` or a `wait` is still running is not missed. Each message is used once: a reply already taken by a `send … expect` step, or by an earlier `expect`, does not match again.

**Repeated fields.** An assertion on a repeated field (`adc`) or a `[*]` path checks every element; the report lists all measured values.

Run `protobench check --project <dir>` before a test campaign: it reports unknown commands, message names, field paths, channels and statistics in every sequence, with the file and line.

### Scripts

A `script` step runs a [Rhai](https://rhai.rs) script, for logic a fixed list of steps cannot express: sweeps, conditional retries, computed limits. Write it inline with `code = """ … """` or in a file next to the sequence with `file = "sweep.rhai"`. The demo's `sequences/scripted.toml` sweeps the bias and checks each acknowledgement, then judges one second of current readings:

```rhai
let sweep = get_var("sweep");
for mv in sweep {
    let r = send("Set Bias", #{ millivolts: mv });
    check(r.status == "ok" && r.response.ack.ok, `bias ${mv} mV acknowledged`);
}
set_var("last_bias", sweep[sweep.len() - 1]);
```

| Function | Does |
|---|---|
| `get_var(name)`, `set_var(name, value)` | read and write sequence variables (`${name}` in later steps) |
| `send(command)`, `send(command, #{field: value})` | send a project command, optionally overriding payload fields |
| `send_msg(message, #{...})`, `send_msg(message, #{...}, expect)` | send any message, optionally waiting for a reply |
| `last(channel)` | newest value of a channel, or `()` before the first sample |
| `stats(channel)`, `stats(channel, ms)` | `#{count, mean, min, max, std, rate_hz, last}` over all samples or the last `ms` |
| `log(text)`, `print(text)` | a line in the run log and report |
| `check(cond, message)` | an assertion in the report; a false one fails the step, the script continues |
| `fail(message)` | stop now and fail the step |
| `sleep(ms)` | wait (stops on abort or at the time limit) |

`send` and `send_msg` return `#{status, response, latency_ms, attempts, error}`, with `status` `"ok"`, `"sent"`, `"timeout"` or `"error"`; they never stop the script, so the script decides what a timeout means. `response` is the reply as Protobuf JSON, so fields read naturally (`r.response.ack.ok`). The script's last value, if any, is stored in the step's `measured.result`.

Scripts are sandboxed: no file, network or module access, at most 50 million operations, and a wall-clock limit (`timeout_ms`, default 60 s). A script that throws or calls `fail` fails the step; one that stops for another reason (a syntax or type error, a limit, abort) is reported as an error.

## Command line

```
protobench list-ports [--json]
protobench list-probes [--json]
protobench ble-scan    [--seconds <s>] [--json]   (builds with --features ble)
protobench schema   --project <dir> [--message <type>]
protobench send     --project <dir> [--device <name>] [--profile <name>] [--port <p>] --command <name> [--set field=value ...] [--wait]
protobench monitor  --project <dir> [--profile [<device>=]<name>]... [--port [<device>=]<p>]... [--only <devices>]
                    [--json] [--channels a,b] [--send [<device>:]<cmd>] [--duration <s>] [--record <file>]
                    [--foxglove <port>] [--foxglove-bind <addr>]
protobench run      --project <dir> --sequence <name> [--profile [<device>=]<name>]... [--port [<device>=]<p>]...
                    [--only <devices>] [--report-dir <d>] [--non-interactive fail|continue] [--var <name>=<value>]...
protobench check    --project <dir> [--json]
protobench decode   <file.pbsession> [--json] [--csv out.csv] [--parquet out.parquet] [--mcap out.mcap]
                    [--channels a,b] [--range <t0>:<t1>] [--per-channel]
protobench sim      --project <dir> [--tcp <port>] [--bind <addr>] | [--mqtt <broker> | --can-slcan <port>] [--profile <name>]
```

- `list-ports` prints ports with USB VID:PID, serial numbers and product names (`--json` for scripts).
- `list-probes` prints the debug probes RTT profiles can use, with the `VID:PID:SERIAL` to put in `probe`.
- `ble-scan` lists Bluetooth LE devices in range (address, signal strength, name, services); the binary needs the `ble` feature.
- `schema` prints messages, fields with numbers, types and comments, and enums.
- `send` sends one command. `--set millivolts=300` overrides payload fields (values in TOML syntax). Without `--wait` it is fire-and-forget; with it, the reply is printed as JSON and the exit code is 1 on timeout or error.
- `monitor` prints decoded messages and text lines (`--json`: one object per line), or with `--channels` the latest channel values whenever they change. `--send` sends commands after connecting; `--duration` stops after N seconds; `--record` writes a session file. `--foxglove 8765` also serves the session to Foxglove (see *Foxglove server*), on `127.0.0.1` unless `--foxglove-bind 0.0.0.0`. With a `tcp_server` profile, `monitor`, `send` and `run` wait for the client to connect.
- `run` runs a sequence headlessly and writes `<sequence>_<timestamp>.json` and `.xml` to `--report-dir` (default `<project>/reports`). Exit codes: **0** all steps passed, **1** a step failed, **2** configuration or connection error. `--non-interactive` decides what `prompt_operator` does (default: the sequence's `non_interactive`, else `fail`). `--port` overrides the profile's port, so the same sequence runs on any test station: `protobench run --project . --sequence smoke --profile usb --port COM5`. `--var load_ohms=4700` sets a variable, overriding `[vars]` (repeatable; values in TOML syntax like `send --set`, so `--var name='"abc"'` is a string and a value that is not valid TOML is taken as a string). It also answers `prompt_operator` inputs with the same `var`, so a sequence that asks for measured values runs unattended.
- `check` checks a project without connecting: `project.toml`, the schema, every sequence (commands, messages, assertion and `save` paths, channels, statistics, scripts that do not compile, `${vars}` nothing defines) and the channels in `layout.json`. Findings print as `file:line: error|warning: message`; the exit code is **1** if there is any error, **0** otherwise, so it fits a CI step.
- `decode` prints a recording (`--json`: one object per record), or exports its channels to CSV or Parquet and its messages to MCAP. `--channels` limits the export to some channels, `--range 12.5:20` to a time window in seconds (either side may be empty), and `--per-channel` writes one CSV per channel.
- `sim` runs the project's simulator as a TCP server (default port 9000). Point a `tcp_client` profile at it to test the real network path. With `--mqtt <broker>` it acts as an MQTT device instead (see *MQTT*), and with `--can-slcan <port>` as a CAN device behind an emulated slcan adapter (see *CAN*).

With `[[devices]]` (see *Multiple devices*), `monitor` and `run` start every device; `--only main` (comma-separated names) starts only some. `--profile` and `--port` take `DEVICE=VALUE` for another device and a bare value for `main`: `protobench run --project . --sequence compare --profile usb --profile ref=usb --port ref=COM7`. `send --device ref` connects only that device, and there a bare `--profile`/`--port` is the target's own. `monitor` tags other devices' output (`ref:reading`, and `"device": "ref"` in JSON) and `--send "ref:Start Stream"` sends to one of them.

`--log debug` (or `info`, `pb_core=trace`, …) prints diagnostics to stderr.

## Python

The `protobench` Python module drives projects from pytest, notebooks or an existing test rig, with the same sessions, devices and sequences as the CLI. Build and install it into the active environment with [maturin](https://www.maturin.rs/):

```sh
pip install maturin
maturin develop --release -m crates/pb-py/Cargo.toml
```

(`maturin build --release -m crates/pb-py/Cargo.toml` makes a wheel instead; one wheel serves CPython 3.9 and later.)

```python
import time
import protobench

p = protobench.Project("examples/demo-sensor")
with p.connect(profile="sim") as s:              # every device; profiles={"ref": "usb"}, only=["main"]
    print(s.send("Ping"))                        # {'status': 'ok', 'response': {...}, 'latency_ms': 5.4, 'attempts': 1}
    s.send("Set Bias", millivolts=300)           # keyword arguments override payload fields
    s.send_msg("set_bias", {"millivolts": 5}, expect="ack")
    s.send("Start Stream")
    reading = s.wait("telemetry", timeout_s=1)   # the next message of a type, or TimeoutError
    time.sleep(1)
    print(s.stats("telemetry.vout_raw")["mean"]) # count, mean, min, max, std, rate_hz, last
    t, v = s.samples("telemetry.vout_raw", since_s=None)
    for m in s.messages(timeout_s=0.5):          # received messages until 0.5 s of silence
        print(m["name"], m["msg"])
    ref = s.device("ref")                        # the same methods for another device,
    print(ref.last("reading.vout_mv"))           # with its own channel names
    report = s.run_sequence("smoke", vars={"load_ohms": 4700})
    print(report["passed"], report["summary"])

rows = protobench.decode("recordings/demo.pbsession")
protobench.export_csv("recordings/demo.pbsession", "demo.csv", channels=["telemetry.vout_raw"])
protobench.export_parquet("recordings/demo.pbsession", "demo.parquet", range=(1.0, 5.0))
```

Messages and replies are dicts in protobuf JSON form (field names in lowerCamelCase, as `decode --json` prints them). A reply that `send` returned is not offered again to `wait`. Errors raise Python exceptions: `ConnectionError` when a device cannot connect or is not connected, `KeyError` for an unknown device, command, message or channel, `TimeoutError` from `wait`, `ValueError` for a project with errors. Connecting, sending, waiting and running sequences release the GIL, so other Python threads keep running. `examples/demo-sensor/python/smoke.py` is a complete script.

## Simulator

The simulator in `[simulator]` plays the device: it decodes the host's requests with the project's schema, framing and codec, answers configured requests with generated payloads (echoing the sequence ID), toggles streams, and sends telemetry built from generators (`sine`, `noise`, `ramp`, `enum_cycle`, `time_us`, …). It can add latency, drop frames, corrupt them after the CRC, insert garbage between frames, and print firmware-style log lines. Use it through a `transport = "sim"` profile (in-process) or `protobench sim --tcp 9000`.

This lets you build dashboards and sequences before hardware exists, and test how your configuration copes with a bad link: with `corrupt_rate = 0.05` and `garbage_rate = 0.05` the CRC and framing counters climb, telemetry keeps flowing, and commands still succeed with retries.

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| Anything odd after editing the project | Run `protobench check --project <dir>` first; it points at the file and line. |
| CRC errors on every frame | CRC type, initial value or byte order differs from the firmware (`crc`, `crc_endian`). Compare check values in `FIRMWARE_INTEGRATION.md`. |
| Framing errors, no CRC errors | Framing type, length width/byte order or sync word differs; or a baud-rate mismatch (look at the Raw Console). |
| Decode errors with good CRCs | Schema or dispatch mode does not match the firmware (`tx_root`/`rx_root`, `[codec.ids]`). The Link Statistics bad-frame list shows the bytes. |
| Unknown message IDs | An ID is missing from `[codec.ids]`. |
| Commands time out but telemetry works | The firmware does not echo the sequence ID into `response_field`, or replies with another type than `expect`. |
| No text lines | `text_passthrough` is off, lines do not end with `\n`, or the framing is a length prefix without a sync word. |
| Text lines appear a few milliseconds late | Expected with COBS/SLIP: a line is held until the next frame or 5 ms of silence, because its bytes could also be the start of a frame. |
| A `tcp_server` profile stays at "Waiting for client" | Nothing has connected to the port yet; check the device's target address and the firewall. |
| Foxglove connects but shows no data | Pick a topic in a panel (Foxglove subscribes per panel), and start the stream. From another machine, bind `0.0.0.0`. |
| GUI does not start on Linux | The OpenGL, Wayland or X11 libraries are not on the library path (see README). |
