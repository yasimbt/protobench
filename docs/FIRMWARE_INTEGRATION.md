# Firmware integration

This document describes, byte by byte, what ProtoBench puts on the wire and what it expects back, so that firmware can talk to it. The reference snippets in `examples/demo-sensor/firmware/` implement the demo project's conventions in C (nanopb) and in Rust (`no_std`, prost).

A message goes through three layers:

```
protobuf message ──codec──▶ codec bytes ──CRC──▶ payload ‖ crc ──framer──▶ wire bytes
```

1. **Codec**: chooses the message type and encodes it (dispatch mode).
2. **CRC** (optional): appended to the codec bytes.
3. **Framer**: delimits frames on the byte stream.

The receiver undoes the layers in reverse order. Every layer is configured in `project.toml`; see `CONFIG_REFERENCE.md`.

## 1. Framing

### COBS (`type = "cobs"`)

```
wire = COBS( codec_bytes ‖ crc ) ‖ 0x00
```

Consistent Overhead Byte Stuffing removes every 0x00 from the data, so 0x00 can end a frame. Each COBS block starts with a code byte `n` (1–255): `n−1` data bytes follow, then an implied 0x00, except after a 0xFF block or at the end of the frame. The encoder emits a final block even when the data length is a multiple of 254; the decoder accepts frames with or without that block.

The decoder collects bytes until 0x00, COBS-decodes them, checks and strips the CRC, and passes the rest to the codec. Bytes before the first 0x00 after a reset or after garbage are discarded together with the frame they corrupt; every following frame decodes normally.

### SLIP (`type = "slip"`)

```
wire = 0xC0 ‖ escape( codec_bytes ‖ crc ) ‖ 0xC0
```

Escapes: 0xC0 → `DB DC`, 0xDB → `DB DD`. ProtoBench sends an END (0xC0) before and after every frame; empty frames between two ENDs are ignored.

### Length prefix (`type = "varint_len"` or `"fixed_len"`)

```
wire = [sync_word] ‖ length ‖ codec_bytes ‖ crc
length = len(codec_bytes) + len(crc)
```

- `varint_len`: length as a protobuf varint (LEB128).
- `fixed_len`: length as an unsigned integer of `len_bytes` (1, 2 or 4) in `endian` byte order.
- `sync_word` (optional) precedes the length, e.g. `AA 55`.

A length of 0 or above `max_frame_len + len(crc)` is a framing error. On a framing or CRC error the decoder drops one byte and searches again (for the sync word when there is one), so it resynchronises after garbage. While it searches, it reports one error per episode instead of one per byte. Without a CRC or a sync word, garbage cannot be detected reliably; use at least one of them on noisy links.

### None (`type = "none"`)

Every transport read is one frame: one UDP datagram, for example. The CRC, if configured, is still appended and checked.

### Message links (MQTT)

On a link that carries whole messages, such as MQTT, set `type = "none"`: the MQTT payload is exactly what goes between the delimiters on a serial line, the codec bytes plus the optional CRC, with no COBS, SLIP or length prefix. The firmware publishes one encoded message per MQTT PUBLISH to the topic ProtoBench subscribes to, and decodes each PUBLISH it receives on the command topic as one message.

### SEGGER RTT

Over RTT the firmware writes the same framed bytes it would send to a UART (COBS, SLIP or a length prefix, with the CRC) into an RTT up buffer, and reads commands from a down buffer. Keep `printf` on channel 0 and give the protobuf stream a channel of its own:

```c
#include "SEGGER_RTT.h"

static uint8_t pb_up[1024], pb_down[256];

void link_init(void) {
    SEGGER_RTT_ConfigUpBuffer(1, "proto", pb_up, sizeof pb_up, SEGGER_RTT_MODE_NO_BLOCK_SKIP);
    SEGGER_RTT_ConfigDownBuffer(1, "proto", pb_down, sizeof pb_down, SEGGER_RTT_MODE_NO_BLOCK_SKIP);
}

void link_send(const uint8_t *frame, unsigned len) {   /* a complete COBS frame */
    SEGGER_RTT_Write(1, frame, len);                    /* skipped whole if it does not fit */
}

unsigned link_poll(uint8_t *buf, unsigned size) {      /* feed the result to the COBS decoder */
    return SEGGER_RTT_Read(1, buf, size);
}
```

`NO_BLOCK_SKIP` drops a frame that does not fit rather than writing half of it, which a framer would then reject; `NO_BLOCK_TRIM` would cut frames. In Rust firmware, `rtt-target`'s `rtt_init!` declares the same two channels (up 0 "Terminal" and 1 "proto", down 1 "proto"). Profile: `rtt_up = 1`, `rtt_down = 1`, `rtt_text_channel = 0`.

### Bluetooth LE (Nordic UART Service)

Over BLE the firmware sends the same framed bytes it would send on a UART as notifications on the NUS TX characteristic (Zephyr's `bt_nus_send`, nRF5 SDK's `ble_nus_data_send`), in pieces of at most MTU − 3 bytes, and receives commands as writes on the NUS RX characteristic. Frames may be split across notifications and several may share one; the framer reassembles them, so keep COBS (or another framer) on this link.

### CAN with ISO-TP

On CAN, set `type = "none"` as well: ISO-TP delimits each message. The firmware sends the encoded message (plus the optional CRC) as an ISO-TP payload with the identifier the host listens on (`rx_id` in the profile, 0x7E8 in the examples) and receives commands on the host's `tx_id` (0x7E0). Normal addressing, no extended addressing byte. When the host receives a first frame it answers with flow control `30 BS ST` from `isotp_block_size` and `isotp_st_min_us` (default `30 00 00`: send everything, no gap); when the host sends a long command it waits up to `isotp_timeout_ms` for the device's flow control and honours its block size and STmin (including the 100–900 µs codes `F1`–`F9`), WAIT frames, and overflow. Any ISO-TP library for the MCU works (for example isotp-c); on Linux the kernel's `CAN_ISOTP` sockets do too.

```text
host → device, Ping (`08 01 12 00` plus CRC `22 53`, see the worked examples), one frame:
  id 0x7E0  06 08 01 12 00 22 53
device → host, 47-byte telemetry, classic CAN:
  id 0x7E8  10 2F d0 d1 d2 d3 d4 d5      first frame, length 0x02F
  id 0x7E0  30 00 00                     flow control from the host
  id 0x7E8  21 d6 … d12                  consecutive frames 1, 2, …
```

### Text passthrough

With `text_passthrough = true`, firmware can print log lines on the same UART as the frames. A line is the bytes before a `\n` that start with a printable character (0x21–0x7E) and contain only printable ASCII, tab or CR. It goes to the text console instead of the framer. Rules:

- **COBS and SLIP**: lines may appear between frames. End each line with `\n` before the next frame starts. A frame is always terminated by its delimiter, so a line that follows a frame starts clean. The bytes of a line are also a valid start of a frame (a COBS code byte is often printable, and protobuf's most common first byte, `0x0A`, is `\n`), so ProtoBench decides afterwards: when the next delimiter arrives, the whole buffer is first tried as one frame, and a frame that passes the CRC wins. Otherwise the lines go to the text console and the rest is decoded as the frame. If no frame follows, the lines appear after 5 ms of silence. **Use a CRC when text and frames share a link**; without one this decision is a heuristic.
- **Length prefix with a sync word**: lines may appear between frames; bytes outside frames that are not a complete printable line count as framing errors.
- **Length prefix without a sync word, and `none`**: text passthrough is not supported.

The text console colours lines by prefix: `E:` / `[ERROR]` red, `W:` / `[WARN]` amber, `I:` / `[INFO]` green, `D:` / `[DEBUG]` grey.

## 2. CRC

The CRC covers the codec bytes (including an ID prefix in `id_prefix` mode) and is appended after them, inside the framing. `crc_endian` sets the byte order of the CRC value.

| `crc` | Width | Polynomial | Init | Reflected | Final XOR | Check value of `"123456789"` |
|---|---|---|---|---|---|---|
| `crc8` | 8 | 0x07 | 0x00 | no | 0x00 | 0xF4 (CRC-8/SMBUS) |
| `crc16_ccitt_false` | 16 | 0x1021 | 0xFFFF | no | 0x0000 | 0x29B1 (CRC-16/IBM-3740) |
| `crc16_modbus` | 16 | 0x8005 | 0xFFFF | yes | 0x0000 | 0x4B37 |
| `crc32` | 32 | 0x04C11DB7 | 0xFFFFFFFF | yes | 0xFFFFFFFF | 0xCBF43926 (zlib) |

Example: CRC-16/CCITT-FALSE of `31 32 33 34 35 36 37 38 39` is 0x29B1, appended little-endian as `B1 29`.

## 3. Dispatch modes (codec)

### Envelope (`mode = "envelope"`)

One root message per direction, each with a `oneof`:

```proto
message HostToDevice { uint32 seq = 1; oneof cmd { Ping ping = 2; SetBias set_bias = 3; ... } }
message DeviceToHost { uint32 seq = 1; oneof msg { Pong pong = 2; Ack ack = 3; Telemetry telemetry = 4; ... } }
```

The host always sends `tx_root` and decodes everything it receives as `rx_root`. The set oneof member tells the message apart. This is the most robust convention: adding a message type never changes how existing ones are framed.

### ID prefix (`mode = "id_prefix"`)

```
codec_bytes = id ‖ protobuf(message)
```

`id` is 1 byte, 2 bytes little-endian, or a varint (`id_width`). `[codec.ids]` maps IDs to types for both directions. An ID that is not in the table increments the *unknown message IDs* counter and the frame is kept in the bad-frame list.

### Fixed (`mode = "fixed"`)

The host always sends `tx_root` and always receives `rx_root`, with no envelope. Use this for simple sensors that send one message type.

## 4. Correlation

When `[correlation]` names a `request_field` (e.g. `seq`), ProtoBench writes an incrementing sequence ID into every message it sends (1, 2, 3, …, wrapping at 2³¹−1 and never 0). The firmware copies the value into `response_field` of its reply. Unsolicited messages (telemetry) carry 0.

The host matches replies by sequence ID, so replies may arrive out of order. A reply with the right ID but the wrong type fails the request with an error. With `retries`, the host resends the *same* bytes (same sequence ID) after each timeout; firmware that has already acted on a request can recognise the duplicate by its ID. In `id_prefix` mode, message types without the sequence field are matched by type, oldest request first.

## 5. Timestamps

With `[timestamp] source = "device"`, the host reads `field` (e.g. `telemetry.timestamp_us`) from each message, converts `unit` to seconds and unwraps a `wrap_bits`-wide counter. A free-running 32-bit microsecond counter (`wrap_bits = 32`) wraps every 71.6 minutes; the host handles the wrap as long as at least one sample arrives per half period.

## 6. Worked examples

All examples use the demo project: COBS, CRC-16/CCITT-FALSE little-endian, envelope mode, `seq` correlation. The unit tests `framing::tests::documented_hex_examples` and `codec::tests::documented_payload_bytes` check these bytes.

### Ping (host → device)

`HostToDevice { seq: 1, ping: {} }`

| Step | Bytes |
|---|---|
| protobuf | `08 01` (field 1 varint = 1) `12 00` (field 2, length 0) |
| CRC-16/CCITT-FALSE | 0x5322 → `22 53` |
| payload ‖ crc | `08 01 12 00 22 53` |
| COBS | `04 08 01 12 03 22 53` |
| wire | `04 08 01 12 03 22 53 00` |

The first COBS code `04` says "3 data bytes, then a zero"; `03` says "2 data bytes"; the trailing `00` ends the frame.

### Set Bias (host → device)

`HostToDevice { seq: 2, set_bias: { millivolts: 200 } }`

| Step | Bytes |
|---|---|
| protobuf | `08 02 1A 03 08 C8 01` (field 3, length 3: `millivolts` = 200 as varint `C8 01`) |
| CRC | 0x1676 → `76 16` |
| wire | `0A 08 02 1A 03 08 C8 01 76 16 00` |

### Ack (device → host)

`DeviceToHost { seq: 2, ack: { ok: true } }`

| Step | Bytes |
|---|---|
| protobuf | `08 02 1A 02 08 01` |
| CRC | 0xC335 → `35 C3` |
| wire | `09 08 02 1A 02 08 01 35 C3 00` |

### The same Ping with other framings

| Framing | Wire bytes |
|---|---|
| `fixed_len`, `len_bytes = 2`, little-endian, `sync_word = [0xAA, 0x55]`, CRC-16/CCITT-FALSE | `AA 55 06 00 08 01 12 00 22 53` |
| `varint_len`, CRC-8 | `05 08 01 12 00 <crc8>` |

### ID prefix

`id_width = 1`, `0x01 = "demo.Telemetry"`, `Telemetry { vout_raw: 300 }`, COBS + CRC-16/CCITT-FALSE:

| Step | Bytes |
|---|---|
| codec bytes | `01` (ID) `10 AC 02` (field 2 varint = 300) |
| wire | `07 01 10 AC 02 46 C9 00` |

### SLIP escaping

Payload `01 C0 DB`, no CRC: `C0 01 DB DC DB DD C0`.

## 7. Adapting an existing firmware protocol

1. **Collect the schema.** Point `[schema] proto_files` at the firmware's `.proto` files and `include_paths` at their folders. nanopb options parse unchanged. If CI already produces a `FileDescriptorSet`, use `descriptor_set` instead.
2. **Match the framing.** Find how the firmware delimits messages: a 0x00 delimiter means COBS; 0xC0 means SLIP; a length before each message means `varint_len` or `fixed_len` (check width, byte order and whether the length includes the CRC: ProtoBench's length includes it). Copy the CRC algorithm and its byte order from the firmware's CRC routine and compare check values with the table above.
3. **Match the dispatch.** One wrapper message with a `oneof` → `envelope`. A type byte before each message → `id_prefix` with a `[codec.ids]` table. One message each way → `fixed`.
4. **Add correlation** if the firmware echoes a request ID. If it does not, leave `[correlation]` out: replies then match the oldest request expecting that type.
5. **Start with the monitor.** `protobench monitor --project . --profile usb` prints decoded messages; the GUI's Raw Console and Link Statistics panels show framing, CRC and decode errors with the bad bytes. A rising CRC counter with clean framing usually means a CRC byte-order or initial-value mismatch; decode errors with good CRCs mean a schema or dispatch mismatch.
6. **Use the simulator** to build dashboards and sequences before hardware is ready: describe the device's streams and responses in `[simulator]`, connect with the `sim` profile, and switch to the real profile later.

## 8. Reference snippets

- `examples/demo-sensor/firmware/frame.c`: `encode_frame()` / `decode_frame()` with nanopb, COBS and CRC-16/CCITT-FALSE.
- `examples/demo-sensor/firmware/frame.rs`: the same in `no_std` Rust with `prost`, `cobs` and `crc`.

Both are documentation; they are not built with the workspace.
