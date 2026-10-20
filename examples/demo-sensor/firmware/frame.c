/*
 * Reference framing for the demo sensor (documentation only, not built).
 *
 * Wire format, matching ProtoBench's `cobs` framer with
 * `crc = "crc16_ccitt_false"` and `crc_endian = "little"`:
 *
 *     COBS( protobuf_bytes || crc_le ) || 0x00
 *
 * CRC-16/CCITT-FALSE: poly 0x1021, init 0xFFFF, no reflection, no final XOR.
 * The CRC covers protobuf_bytes and is appended low byte first, then the
 * whole buffer is COBS-encoded and terminated with 0x00.
 *
 * Generate device.pb.c/h with:  nanopb_generator.py device.proto
 */
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include <pb_decode.h>
#include <pb_encode.h>

#include "device.pb.h"

#define MAX_FRAME_LEN 1024 /* project.toml: framing.max_frame_len */

static uint16_t crc16_ccitt_false(const uint8_t *data, size_t len)
{
    uint16_t crc = 0xFFFF;
    for (size_t i = 0; i < len; i++) {
        crc ^= (uint16_t)data[i] << 8;
        for (int b = 0; b < 8; b++) {
            crc = (crc & 0x8000) ? (uint16_t)((crc << 1) ^ 0x1021) : (uint16_t)(crc << 1);
        }
    }
    return crc;
}

/* COBS-encodes len bytes from in to out (out needs len + len/254 + 1 bytes).
 * Returns the encoded length, without the 0x00 delimiter. */
static size_t cobs_encode(const uint8_t *in, size_t len, uint8_t *out)
{
    size_t code_idx = 0, o = 1;
    uint8_t code = 1;
    for (size_t i = 0; i < len; i++) {
        if (in[i] == 0) {
            out[code_idx] = code;
            code_idx = o++;
            code = 1;
        } else {
            out[o++] = in[i];
            if (++code == 0xFF) {
                out[code_idx] = code;
                code_idx = o++;
                code = 1;
            }
        }
    }
    out[code_idx] = code;
    return o;
}

/* Decodes one COBS block (without the 0x00). Returns the decoded length,
 * or 0 on malformed input. out may alias in. */
static size_t cobs_decode(const uint8_t *in, size_t len, uint8_t *out)
{
    size_t i = 0, o = 0;
    while (i < len) {
        uint8_t code = in[i];
        if (code == 0 || i + code > len) {
            return 0;
        }
        for (uint8_t k = 1; k < code; k++) {
            if (in[i + k] == 0) {
                return 0;
            }
            out[o++] = in[i + k];
        }
        i += code;
        if (code < 0xFF && i < len) {
            out[o++] = 0;
        }
    }
    return o;
}

/* Encodes a DeviceToHost message into a complete frame (including the 0x00
 * delimiter). Returns the frame length, or 0 if it does not fit. */
size_t encode_frame(const demo_DeviceToHost *msg, uint8_t *out, size_t out_size)
{
    uint8_t buf[MAX_FRAME_LEN];
    pb_ostream_t s = pb_ostream_from_buffer(buf, sizeof(buf) - 2);
    if (!pb_encode(&s, demo_DeviceToHost_fields, msg)) {
        return 0;
    }
    size_t n = s.bytes_written;
    uint16_t crc = crc16_ccitt_false(buf, n);
    buf[n++] = (uint8_t)(crc & 0xFF); /* little-endian */
    buf[n++] = (uint8_t)(crc >> 8);
    if (out_size < n + n / 254 + 2) {
        return 0;
    }
    size_t m = cobs_encode(buf, n, out);
    out[m++] = 0x00;
    return m;
}

/* Decodes a received frame (the bytes before the 0x00 delimiter) into a
 * HostToDevice message. Returns false on COBS, CRC or protobuf errors. */
bool decode_frame(const uint8_t *frame, size_t len, demo_HostToDevice *msg)
{
    uint8_t buf[MAX_FRAME_LEN + 2];
    if (len == 0 || len > sizeof(buf)) {
        return false;
    }
    size_t n = cobs_decode(frame, len, buf);
    if (n < 2) {
        return false;
    }
    uint16_t rx_crc = (uint16_t)buf[n - 2] | ((uint16_t)buf[n - 1] << 8);
    if (crc16_ccitt_false(buf, n - 2) != rx_crc) {
        return false;
    }
    pb_istream_t s = pb_istream_from_buffer(buf, n - 2);
    return pb_decode(&s, demo_HostToDevice_fields, msg);
}

/*
 * Typical receive loop: collect bytes until 0x00, then decode.
 *
 *     static uint8_t rx[MAX_FRAME_LEN + 8];
 *     static size_t rx_len;
 *     void on_uart_byte(uint8_t b) {
 *         if (b == 0x00) {
 *             demo_HostToDevice req = demo_HostToDevice_init_zero;
 *             if (decode_frame(rx, rx_len, &req)) handle(&req);
 *             rx_len = 0;
 *         } else if (rx_len < sizeof(rx)) {
 *             rx[rx_len++] = b;
 *         } else {
 *             rx_len = 0; // overflow: drop until the next delimiter
 *         }
 *     }
 *
 * Replies copy req.seq into the DeviceToHost seq field. Log lines
 * ("I: booted\n") may be printed between frames: ProtoBench shows them in
 * its text console when framing.text_passthrough is true.
 */
