# Configuration reference

ProtoBench reads three kinds of files from a project folder:

| File | Purpose |
|---|---|
| `project.toml` | Schema, connection profiles, framing, codec, commands, telemetry, simulator |
| `sequences/<name>.toml` | Test sequences |
| `layout.json` | The project's default dashboard: dock layout and panel settings |
| `layout.local.json` | Your working layout, saved automatically by the GUI (git-ignore it) |

Paths inside `project.toml` are relative to the project folder.

## `project.toml`

Unknown keys produce warnings that name the key and line; they never stop a project from opening. Type errors and validation errors name the key and line where possible. After loading the file, ProtoBench cross-checks it against the schema: command messages, `expect` names, telemetry paths, correlation fields, the timestamp field, and simulator messages must all exist.

### Top level

| Key | Type | Default | Meaning |
|---|---|---|---|
| `config_version` | integer | — | File format version. Current: `1`. A missing value means version 0 and the file is migrated on load (see below). A newer version than the tool supports is an error. |
| `name` | string | `"Untitled"` | Project name. Used in titles, recording file names and reports. |
| `description` | string | `""` | Free text. |

**Migration from version 0:** `[framing] kind` is renamed to `type`, and `[connection] default` to `profile`. The loader reports a warning; the file on disk is not rewritten until you save from the project editor.

### `[schema]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `proto_files` | array of strings | `[]` | `.proto` files to compile with protox (no `protoc` needed). |
| `include_paths` | array of strings | `[]` | Import search paths. When empty, each proto file's folder is used. |
| `descriptor_set` | string | none | A compiled `FileDescriptorSet` (`.pb`/`.desc`) to use instead of `proto_files`. |

One of `proto_files` or `descriptor_set` is required. `import "nanopb.proto";` always works: a stub of nanopb's options file is bundled (extension 1010 on file, message, enum and field options). A `nanopb.proto` in your include paths takes precedence. `google/protobuf/*.proto` well-known types are always available.

### `[connection]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `profile` | string | first profile | Name of the default profile. |

#### `[[connection.profiles]]`

| Key | Type | Default | Used by | Meaning |
|---|---|---|---|---|
| `name` | string | `"default"` | all | Profile name (unique). |
| `transport` | string | `"serial"` | all | `serial`, `tcp_client`, `tcp_server`, `udp`, `sim`, `replay`, `mqtt` or `can`. |
| `port` | string | `"auto"` | serial | Port name (`COM5`, `/dev/ttyACM0`) or `"auto"`: the one port matching `usb_vid`, `usb_pid` and `usb_serial`. Without any of them, the only USB serial port (or the only port, when none is USB). Several matches are an error that lists them. |
| `usb_vid` | integer | none | serial | USB vendor ID for `port = "auto"`. |
| `usb_pid` | integer | none | serial | USB product ID for `port = "auto"`. |
| `usb_serial` | string | none | serial | USB serial number for `port = "auto"` (case-insensitive), to tell identical boards apart. `protobench list-ports` shows it. |
| `baud` | integer | `115200` | serial | Baud rate. |
| `data_bits` | integer | `8` | serial | 5–8. |
| `parity` | string | `"none"` | serial | `none`, `odd`, `even`. |
| `stop_bits` | integer | `1` | serial | 1 or 2. |
| `flow_control` | string | `"none"` | serial | `none`, `software`, `hardware`. |
| `dtr_on_open` | bool | `true` | serial | DTR level after opening; also the idle level for pulse reset. |
| `rts_on_open` | bool | `false` | serial | RTS level after opening; also the idle level for pulse reset. |
| `reset_line` | string | `"dtr"` | serial | Line toggled by **Pulse reset**: `dtr` or `rts`. |
| `reset_pulse_ms` | integer | `100` | serial | Pulse length (clamped to 1–5000 ms). |
| `auto_reconnect` | bool | `true` | all | Reopen after errors or unplugging, with backoff 100 ms doubling to 2 s. |
| `host` | string | `"127.0.0.1"` | tcp_client | Remote host. |
| `tcp_port` | integer | `9000` | tcp_client, tcp_server | TCP port. |
| `bind` | string | `0.0.0.0` (tcp_server), `0.0.0.0:0` (udp) | tcp_server, udp | Local address. For `tcp_server` a host; for `udp` a `host:port`. |
| `target` | string | none | udp | Destination `host:port`. Without it, replies go to the sender of the last datagram. |
| `file` | string | none | replay | Recording to play back (relative to the project). |
| `speed` | float | `1.0` | replay | 1.0 = real time, 2.0 = twice as fast, 0 = as fast as possible. |
| `broker` | string | required | mqtt | Broker `host:port`; the port defaults to 1883, or 8883 with `tls`. |
| `subscribe` | array of strings | `[]` | mqtt | Topic filters to receive from (`+` one level, `#` the rest). Every message on them is one frame. |
| `publish` | string | none | mqtt | Topic every sent frame is published to. Without it the profile only receives. |
| `qos` | integer | `0` | mqtt | 0 or 1, for publishing and subscribing. With 1 the session is persistent (the broker queues messages while the link is down) and unacknowledged messages are sent again after a reconnect. QoS 2 is not supported. |
| `client_id` | string | generated | mqtt | Client id. Empty picks `protobench-<random>` once per run. Two clients with the same id push each other off the broker. |
| `username` | string | none | mqtt | User name. |
| `password_env` | string | none | mqtt | Environment variable that holds the password, so it stays out of the project. |
| `password` | string | none | mqtt | The password itself (`password_env` wins if both are set). |
| `keep_alive_s` | integer | `30` | mqtt | Keep-alive in seconds; 0 turns it off. An unanswered ping counts as a lost link. |
| `tls` | bool | `false` | mqtt | TLS, verified against the operating system's root certificates. |
| `tls_ca` | string | none | mqtt | Extra CA certificate(s) to trust, PEM, relative to the project (a private broker). |
| `tls_cert`, `tls_key` | string | none | mqtt | Client certificate and key, PEM, for brokers that require mutual TLS (AWS IoT, for example). |
| `can_backend` | string | `"slcan"` | can | `slcan`: a Lawicel ASCII adapter (CANable, USBtin, CANtact) on the serial port in `port` (with `usb_vid`/`usb_pid`/`usb_serial` for `"auto"`). `socketcan`: a Linux SocketCAN interface. |
| `interface` | string | `"can0"` | can | SocketCAN interface. Its bit rate is set by the OS (`ip link set can0 type can bitrate 500000`). |
| `bitrate` | integer | `500000` | can | Bit rate set on an slcan adapter: 10k, 20k, 50k, 100k, 125k, 250k, 500k, 800k or 1M. |
| `data_bitrate` | integer | none | can | CAN FD data-phase bit rate on an slcan adapter (1M, 2M, 4M, 5M or 8M, CANable 2.0 codes); turns on bit-rate switching. |
| `tx_id` | integer | required | can | Identifier the host sends with (`0x7E0`). |
| `rx_id` | integer | required | can | Identifier the device answers with (`0x7E8`); frames with other identifiers are ignored. |
| `extended_id` | bool | `false` | can | 29-bit identifiers. |
| `fd` | bool | `false` | can | CAN FD frames (up to 64 bytes). |
| `isotp` | bool | `true` | can | ISO-TP (ISO 15765-2) segmentation, so messages may be longer than a frame. Without it every frame is one message (8 or 64 bytes at most). |
| `isotp_block_size` | integer | `0` | can | Block size the host asks for when receiving (0 = send everything after one flow control). |
| `isotp_st_min_us` | integer | `0` | can | Minimum gap between consecutive frames the host asks for, in µs (100 µs steps below 1 ms, then ms, up to 127000). |
| `isotp_padding` | integer | none | can | Pad frames to full length with this byte (some ECUs require 0x55 or 0xAA). FD frames above 8 bytes are always padded to a valid length. |
| `isotp_timeout_ms` | integer | `1000` | can | How long to wait for flow control or the next consecutive frame (N_Bs, N_Cr). |

The `sim` transport runs the built-in simulator in-process. A `tcp_server` serves one client at a time and goes back to listening when it disconnects. An `mqtt` or `can` profile usually goes with `framing.type = "none"`, since every MQTT message or ISO-TP message is already one frame (a CRC is still possible).

### `[framing]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `type` | string | `"cobs"` | `cobs`, `slip`, `varint_len`, `fixed_len` or `none`. |
| `len_bytes` | integer | `2` | `fixed_len` only: 1, 2 or 4. |
| `endian` | string | `"little"` | `fixed_len` only: byte order of the length. |
| `sync_word` | array of bytes | `[]` | `fixed_len`/`varint_len` only: bytes before the length, e.g. `[0xAA, 0x55]`. |
| `crc` | string | `"none"` | `none`, `crc8`, `crc16_ccitt_false`, `crc16_modbus`, `crc32`. |
| `crc_endian` | string | `"little"` | Byte order of the appended CRC. |
| `max_frame_len` | integer | `1024` | Largest frame accepted (payload + CRC), 1–16777216. Bigger frames are discarded and counted as framing errors. |
| `text_passthrough` | bool | `false` | Send printable lines found between frames to the text console. |

`none` treats every transport read (one UDP datagram, for example) as one frame. See `FIRMWARE_INTEGRATION.md` for the byte layouts.

The framing has to suit the link. Byte streams (`serial`, `tcp_client`, `tcp_server`) need a framer that marks frame boundaries, so `none` is wrong for them. Links that deliver whole messages need none, so `none` is the usual choice there. `udp`, `sim` and `replay` work with any framing: the simulator and replay keep one frame per read when the framing is `none`. `protobench check` and the project editor warn about a mismatch.

### `[codec]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `mode` | string | `"envelope"` | `envelope`, `id_prefix` or `fixed`. |
| `tx_root` | string | none | envelope/fixed: full name of the type the host sends. |
| `rx_root` | string | none | envelope/fixed: full name of the type the device sends. |
| `id_width` | `1`, `2` or `"varint"` | `1` | id_prefix: width of the message ID. Two-byte IDs are little-endian. |
| `[codec.ids]` | table | `{}` | id_prefix: message ID → full type name. Keys are decimal or `0x` hex (`0x01 = "demo.Telemetry"`). The same table is used in both directions. |

**Message names.** Wherever a config refers to a message (`commands.message`, `expect`, `telemetry.message`, simulator `on`/`reply`):

- envelope mode: a field of the root, normally a `oneof` member (`"set_bias"`), or a full type name used by exactly one oneof member (`"demo.SetBias"`), or the root type itself;
- id_prefix mode: a full type name from `[codec.ids]`, its simple name, or its simple name in snake_case (`"Ack"`, `"ack"`);
- fixed mode: the root type (full, simple or snake_case name).

### `[correlation]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `request_field` | string | none | Field path in the sent message that carries the sequence ID. Filled in automatically on every send. |
| `response_field` | string | none | Field path in received messages that echoes it. Replies are matched by this value. |
| `default_timeout_ms` | integer | `500` | Reply timeout when a command does not set one. |
| `default_retries` | integer | `0` | Retries after a timeout when a command does not set them. |

Sequence IDs start at 1, skip 0 and wrap at 2³¹−1. A received message with sequence ID 0 is treated as unsolicited. Without `response_field`, the oldest outstanding request that expects the received type gets the reply.

### `[timestamp]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `source` | string | `"host"` | `host` (receive time) or `device` (a field in the message). |
| `field` | string | none | Field path of the device timestamp (required for `device`). |
| `unit` | string | `"us"` | `us`, `ms` or `s`. |
| `wrap_bits` | integer | `0` | Counter width for wrap-around handling (e.g. 32). 0 = none. |

Device timestamps keep the device's sample spacing but are anchored to the host clock at the first sample, so channels from device-timestamped and host-timestamped messages share one time axis. A jump backwards by more than one second (a device reset) re-anchors. Messages without the timestamp field use host time.

### `[[fields]]` — metadata overlay

| Key | Type | Default | Meaning |
|---|---|---|---|
| `path` | string | — | Field path, e.g. `telemetry.vout_raw`, `telemetry.adc[2]` or `telemetry.adc[*]`. |
| `display` | string | the path | Display name. |
| `unit` | string | none | Engineering unit. |
| `scale` | float | `1.0` | Displayed value = raw × scale + offset. |
| `offset` | float | `0.0` | See `scale`. |
| `format` | string | none | `{}`, `{:.N}`, `{:e}`, `{:.Ne}`, `{:x}`, `{:X}` or `{:b}`, with optional text around it (`"{:.2} V"`). |
| `warn_min`, `warn_max` | float | none | Outside these limits readouts turn amber. |
| `min`, `max` | float | none | Outside these limits readouts turn red. |

A channel such as `telemetry.adc[2]` without its own entry uses `telemetry.adc[*]`, then `telemetry.adc`, and appends `[2]` to the display name. Channel samples are stored in engineering units (after scale and offset). Enum and bool channels are not scaled.

### `[[commands]]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `name` | string | — | Button label and the name sequences use. Unique. |
| `group` | string | `"General"` | Heading in the command bar. |
| `shortcut` | string | none | `F1`, `Ctrl+S`, `Shift+Alt+3` (Ctrl means Cmd on macOS). |
| `message` | string | — | Message name (see "Message names"). |
| `payload` | table | `{}` | Field values using Protobuf JSON mapping rules: field names (proto or JSON names), enum names as strings, nested tables, arrays. `bytes` fields take base64 strings, arrays of integers (`[0xDE, 0xAD]`) or `"hex:DEAD"`. |
| `prompt` | array of strings | `[]` | Payload fields (dot paths) shown in a dialog before sending, pre-filled from `payload`. |
| `expect` | string | none | Expected reply. Without it the command is fire-and-forget. |
| `timeout_ms` | integer | `correlation.default_timeout_ms` | Reply timeout. |
| `retries` | integer | `correlation.default_retries` | Retries after a timeout (the same bytes and sequence ID are resent). |
| `dangerous` | bool | `false` | Ask for confirmation before sending. |
| `colour` (or `color`) | string | none | Button colour, `#rrggbb`. |
| `periodic_ms` | integer | none | Shows a toggle that sends the command every N ms. |

### `[[telemetry]]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `name` | string | `""` | Name shown in the UI. |
| `mode` | string | `"stream"` | `stream`, `poll` or `triggered`. |
| `message` | string | none | stream/triggered: message to extract from. |
| `channels` | array of strings | `[]` | Field paths. `[n]` picks an element, `[*]` (or no index on a repeated field) creates `name[0]`, `name[1]`, … on first sight. Map fields take `[key]`. |
| `request` | string | none | poll: message to send. |
| `request_payload` | table | `{}` | poll: payload of the request. |
| `response` | string | none | poll: reply to extract from. |
| `interval_ms` | integer | `1000` | poll: interval. |
| `trigger` | string | none | triggered: expression that arms a capture when it becomes non-zero, e.g. `"telemetry.vout_raw > 2000"`. Without it, arm the capture from **Tools ▸ Arm triggered capture**. |
| `samples` | integer | `1000` | triggered: messages captured per trigger. |

Field paths start at the root message's fields. In envelope mode that means starting with the oneof member (`telemetry.vout_raw`). In id_prefix and fixed modes a path may also start with the type's own name, which is skipped. A oneof member that is not set, or an unset message field, yields nothing; unset proto3 scalars yield their default.

### `[[derived]]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `name` | string | — | Channel name. |
| `expr` | string | — | `evalexpr` expression. |
| `unit` | string | none | Unit (unless a `[[fields]]` entry for `name` sets one). |
| `format` | string | none | Format string. |

Variables are field paths or channel names (`telemetry.vout_raw`, `telemetry.adc[2]`, `power_uw`). For each received message, a variable is looked up first among the raw (unscaled) values extracted from that message, then by path in the message, then as the latest value of a channel. The expression is evaluated only when at least one variable comes from the current message. Math functions work without the `math::` prefix: `sqrt`, `abs`, `sin`, `cos`, `tan`, `atan2`, `exp`, `ln`, `log10`, `pow`, `hypot`, …; `min`, `max`, `floor`, `round`, `ceil` are built in. Integer literals combined with float variables give floats.

### `[simulator]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `latency_ms` | integer | `0` | Delay before each response (not telemetry). |
| `drop_rate` | float | `0.0` | Probability of not sending a frame. |
| `corrupt_rate` | float | `0.0` | Probability of flipping a bit in a frame after the CRC is computed. The trailing delimiter is never touched. |
| `garbage_rate` | float | `0.0` | Probability of inserting 1–8 random bytes before a frame. |
| `log_interval_ms` | integer | `1000` | Interval of simulated firmware log lines (`I:`/`W:`/`D:`/`E:`), only when `framing.text_passthrough` is on. 0 disables them. |
| `seed` | integer | `1` | Seed of the simulator's random generator. |

#### `[[simulator.streams]]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `message` | string | — | Message the device sends. |
| `rate_hz` | float | `10.0` | Messages per second. Device timestamps (`time_us()`) are evenly spaced. |
| `start_enabled` | bool | `true` | Whether the stream runs from the start. |
| `fields` | table | `{}` | Field → generator expression or constant. |

#### `[[simulator.responses]]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `on` | string | — | Request that triggers the response. |
| `reply` | string | — | Reply message. The sequence ID is echoed. |
| `payload` | table | `{}` | Reply fields; string values may be generator expressions. |
| `action` | string | none | `toggle_stream:<msg>`, `enable_stream:<msg>` or `disable_stream:<msg>`. `toggle_stream` uses the request's `enable` field when the request has one, and a positive `rate_hz` field in the request changes the stream rate. |

Requests without a matching response get no reply.

#### Generators

| Generator | Arguments (defaults) | Output |
|---|---|---|
| `sine(amp, offset, freq, phase)` | 1, 0, 1 Hz, 0 rad | `offset + amp·sin(2π·freq·t + phase)` |
| `square(amp, offset, freq, duty, phase)` | 1, 0, 1 Hz, 0.5, 0 | `offset ± amp` |
| `ramp(start, end, period_s)` | 0, 1, 1 | sawtooth from start to end |
| `noise(mean, std)` | 0, 1 | Gaussian |
| `random_walk(start, step)` | 0, 1 | random walk with Gaussian steps |
| `constant(v)` | — | `v` (a plain number works too) |
| `counter()` | — | 0, 1, 2, … per message |
| `enum_cycle(period_s)` | 1 | steps through the field's enum values, `period_s` per value |
| `time_us()` | — | simulated device time in µs |
| `uptime_s()` | — | whole seconds since the simulator started |
| `array(n, gen)` | — | `n` copies of `gen`; periodic copies are phase-shifted so they differ |

Arguments are named (`amp=1500`) or positional in the order above. Values are rounded and clamped to the field type. A string that is not a generator is used literally for string, enum and bytes fields; for other types it is an error.

### `[[devices]]` — other devices on the bench

Each entry adds a device that connects alongside the project's own (which is called `main`). A device is a project folder of its own, with its own schema, framing, codec, commands, telemetry and simulator, so every firmware repository keeps its own `project.toml`.

```toml
[[devices]]
name = "ref"                     # identifier: letters, digits, _
project = "../demo-reference"    # path relative to this project
profile = "sim"                  # optional; that project's default otherwise
```

| Key | Type | Default | Meaning |
|---|---|---|---|
| `name` | string | required | Device name: a letter or `_`, then letters, digits and `_`. Not `main`, and unique. |
| `project` | string | required | The device's project folder, relative to this project. It may be this project's own folder (`"."`) for two identical boards. |
| `profile` | string | that project's default | Connection profile of the device's project. `--profile ref=usb` overrides it. |

At most 8 devices, `main` included. Devices nest one level: a device project that lists `[[devices]]` of its own is an error (unless it is this project's own folder, whose list is then ignored). Errors in a device project are reported with this project's and stop it from connecting.

Other devices' channels are named `<device>.<channel>`, such as `ref.reading.vout_mv`, everywhere: plots, layouts, sequences, derived expressions, CSV and Parquet columns. `main`'s channels keep their plain names. A derived channel of this project may use any device's channels: it is computed when a `main` message arrives, with the newest value of the other device's channel. A device project's own derived channels use its own channel names.

## Sequence files (`sequences/<name>.toml`)

Sequence files are strict: unknown keys are errors (with line numbers), so typos are caught before a test runs.

### Top level

| Key | Type | Default | Meaning |
|---|---|---|---|
| `name` | string | file stem | Sequence name (JUnit suite name). |
| `description` | string | `""` | Free text. |
| `[vars]` | table | `{}` | Initial variables. `protobench run --var name=value` and the GUI runner's Variables table override them. |
| `non_interactive` | `"fail"` or `"continue"` | `"fail"` | What `prompt_operator` does in headless runs. `--non-interactive` overrides it. |
| `continue_on_failure` | bool | `false` | Keep running after a failed step. By default the remaining steps are reported as skipped. |
| `[[steps]]` | array | `[]` | Steps, each with an `action` key. |

`${var}` is replaced in strings and payloads. A string that is exactly `"${var}"` takes the variable's value with its type (so `millivolts = "${bias_mv}"` sends a number).

### Steps

**`send`** — send a command and optionally check the reply.

| Key | Meaning |
|---|---|
| `device` | Device to send to (default `main`). Commands and messages are looked up in that device's project. |
| `command` | Command name from `project.toml` (`${var}` allowed), or |
| `message` | an inline message name, with |
| `payload` | an inline payload, or for `command`: overrides of the command's payload fields. |
| `expect` | Expected reply (overrides the command's). |
| `timeout_ms`, `retries` | Overrides. |
| `assert` | Assertions on the reply (`path`) or on channels (`channel`, latest value). |
| `save` | `{ var = "reply.path" }` stores reply fields in variables. |

The step fails on a timeout, on a reply of the wrong type, or on a failed assertion. It errors (exit code 2 in the CLI) when not connected.

**`wait`** — `ms`: pause.

**`expect`** — wait for a received message. `message` (required), `timeout_ms` (default 1000), `assert`, and `device` (default `main`) for a message from another device. It matches the oldest message of that type from that device received since the *previous* step started that no earlier step has used.

**`capture`** — collect samples, then check statistics. `channels` (required), and `duration_ms` or `samples` (new samples on the first channel; `timeout_ms`, default 10000, bounds the wait). `assert` entries use `channel` and `stat`. Every channel listed or named in an assertion is measured over the capture window only. `rate_hz` is the sample count divided by the capture duration. Channels of other devices have their full names (`ref.reading.vout_mv`); with `device = "ref"`, the names in `channels` and the assertions are that device's own (`reading.vout_mv`).

`wait`, `set`, `loop`, `log`, `prompt_operator` and `script` take no `device` (a script reaches devices through its API). Before the first step runs, every device a step names must be configured and connected; otherwise the run stops with a single `devices` error (exit code 2 in the CLI) naming the device.

**`set`** — `var` + `value`, or a `vars` table.

**`loop`** — `count` and nested `steps`.

**`log`** — `text`, written to the run log and the report.

**`prompt_operator`** — `message`, and optionally `inputs`: values the operator enters. The GUI shows a dialog with **OK** and **Fail**, and one field per input under the message.

```toml
[[steps]]
action = "prompt_operator"
message = "Enter the span gas concentration and the probe readings"
inputs = [
  { var = "gas_ppm", label = "Gas concentration", unit = "ppm", min = 0.001 },
  { var = "zero_mv", label = "Zero reading", unit = "mV" },
]
```

| Input key | Default | Meaning |
|---|---|---|
| `var` | required | Variable the value is stored in. |
| `label` | `var` | Field label. |
| `unit` | `""` | Shown after the field. |
| `type` | `"float"` | `"float"`, `"int"`, `"string"` or `"bool"`. The variable gets this JSON type, so `"${var}"` in a payload sends a number. |
| `default` | — | Pre-filled value, used when the variable has no value yet. |
| `min`, `max` | — | Inclusive bounds (float and int). |
| `choices` | — | Allowed values of a string input, shown as a dropdown. |

OK stays disabled until every field is valid; an invalid field says why. The entered values are stored in their variables, recorded in the step's `measured`, and logged as `input gas_ppm = 10.0`. A value entered in the GUI is pre-filled the next time the dialog opens in the same session; otherwise the field shows the variable's current value, else `default`.

Headless, each input takes the variable's value (from `--var`, `[vars]` or an earlier step), else `default`. If every input has a value, the values are checked against `type`, `min`, `max` and `choices` and the step passes, or fails on a bad value; nobody is asked. If any input has no value, the step fails and names the missing variables. A step without inputs follows `non_interactive`.

`protobench check` treats each input's `var` as defined, and reports a `var` used twice in one step, `min` above `max`, a `default` of the wrong type or out of range, `choices` on a non-string input, and `min`/`max` on a string or bool input.

**`script`** — run a Rhai script (API in the user guide, *Scripts*).

| Key | Default | Meaning |
|---|---|---|
| `code` | — | Inline script, usually a `"""` multi-line string, or |
| `file` | — | a script file, relative to the sequence file (read when the sequence loads). |
| `timeout_ms` | `60000` | Wall-clock limit for the whole script, including `sleep`. |

Exactly one of `code` and `file` is required. A script that throws or calls `fail()` fails the step, a false `check()` adds a failed assertion, and any other stop (syntax or type error, the 50-million-operation limit, the time limit, abort) is an error. Variables set with `set_var` are available to later steps as `${name}`.

### Assertions

| Key | Meaning |
|---|---|
| `path` | Field path in the reply or expected message. A repeated field or `[*]` path checks every element. |
| `channel` | Channel name. |
| `stat` | For capture: `mean` (default), `min`, `max`, `std`, `rate_hz`, `count`, `last`. |
| `eq` | Must equal. Numbers compare as floats; enums match by name or number. |
| `ne` | Must not equal. |
| `min`, `max` | Inclusive bounds. |
| `within` | With `eq`: allowed absolute difference. |

At least one of `eq`, `ne`, `min`, `max` is required. A measured value that is not a finite number (NaN, ±∞) fails any `eq`, `min` or `max`.

### Reports

`protobench run` (and **Save report** in the runner) writes `<sequence>_<YYYYmmdd_HHMMSS>.json` and `.xml`. The JSON holds every step with its index, status (`pass`, `fail`, `error`, `skipped`), duration, message, measured values (reply JSON, latency, attempts, capture statistics) and assertion results, plus the final variables and the log. The JUnit XML has one `<testsuite>` and one `<testcase>` per executed step; loop iterations appear as `7.1.1`, `7.2.1`, …

## `layout.json` and `layout.local.json`

Both files have the same format.

- `layout.json` is the project's default dashboard and belongs in version control. The GUI writes it only when you choose **View ▸ Save layout as project default**.
- `layout.local.json` is your working layout. The GUI saves it two seconds after a layout change and on exit, and opens it first next time. Add it to `.gitignore` (**File ▸ New project** does this for you); duplicating a project does not copy it.

On opening a project the GUI uses `layout.local.json`, else `layout.json`, else the built-in dashboard. A file with no panels, or one that fails to parse, is skipped with a notice. **View ▸ Reset layout** goes back to `layout.json`. Delete `layout.local.json` to start from the project default again.

```json
{ "version": 1, "dock": { ...egui_dock DockState... } }
```

`dock` is the serialized `egui_dock::DockState<Tab>`. Each tab is:

```json
{ "id": 3, "title": "Time Plot", "panel": { "type": "time_plot", ... } }
```

Panel types and their settings:

| `type` | Settings (default) |
|---|---|
| `time_plot` | `traces`: `[{ "channel", "color": [r,g,b], "axis": 1 or 2, "visible": true }]`; `window_s` (10); `autoscale` (true); `show_picker` (true) |
| `xy_plot` | `x`, `y` channel names; `points` (2000) |
| `readouts` | `channels` (empty = all numeric channels) |
| `indicators` | `channels` (empty = all enum and bool channels) |
| `table` | `mode`: `latest` or `log`; `filter` |
| `inspector` | — |
| `raw_console` | `hex` (true), `ascii` (true), `search`, `send_hex` (true) |
| `text_console` | `filter`, `autoscroll` (true) |
| `channel_stats` | — |
| `link_stats` | — |
| `histogram` | `channel`, `bins` (50), `fft` (false), `samples` (4096) |
| `command_bar` | — |
| `channels` | — |
| `history` | — |
| `markers` | — |

Node rectangles in the file are recomputed on the next frame; their values do not matter. `protobench-gui --write-default-layout <project>` writes the default dashboard for a project.

## GUI settings

Theme, ring-buffer capacity (default 1,000,000 points per channel), plot frame-rate cap (default 60 fps), recordings folder (default `<project>/recordings`), Foxglove server bind address and port (default `127.0.0.1`, `8765`) and the recent-projects list are stored in eframe's per-user storage, not in the project.

## Checking a project

`protobench check --project <dir>` validates everything above without connecting. Errors: invalid `project.toml` keys or values, schema problems, names and paths that do not resolve, unknown channels or statistics in sequences, assertions without a condition, scripts that do not compile. Warnings: unknown keys in `project.toml`, `${vars}` nothing defines, channels in `layout.json` that no telemetry entry produces. Exit code 1 if there is an error.
