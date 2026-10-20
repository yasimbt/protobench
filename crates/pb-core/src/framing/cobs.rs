//! Consistent Overhead Byte Stuffing (COBS). The encoded data contains no
//! zero bytes, so 0x00 can delimit frames.

/// COBS-encodes `data` (without the trailing delimiter).
pub fn encode(data: &[u8], out: &mut Vec<u8>) {
    let mut code_pos = out.len();
    out.push(0);
    let mut code: u8 = 1;
    for &b in data {
        if b == 0 {
            out[code_pos] = code;
            code_pos = out.len();
            out.push(0);
            code = 1;
        } else {
            out.push(b);
            code += 1;
            if code == 0xFF {
                out[code_pos] = code;
                code_pos = out.len();
                out.push(0);
                code = 1;
            }
        }
    }
    out[code_pos] = code;
}

/// Maximum encoded length of `n` bytes.
pub fn max_encoded_len(n: usize) -> usize {
    n + n / 254 + 1
}

/// Decodes one COBS block (without the delimiter).
pub fn decode(data: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        let code = data[i] as usize;
        if code == 0 {
            return Err("zero byte inside COBS frame");
        }
        let end = i + code;
        if end > data.len() {
            return Err("COBS code runs past end of frame");
        }
        let block = &data[i + 1..end];
        if block.contains(&0) {
            return Err("zero byte inside COBS frame");
        }
        out.extend_from_slice(block);
        if code < 0xFF && end < data.len() {
            out.push(0);
        }
        i = end;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(d: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        encode(d, &mut v);
        v
    }

    #[test]
    fn known_vectors() {
        assert_eq!(enc(&[]), vec![0x01]);
        assert_eq!(enc(&[0x00]), vec![0x01, 0x01]);
        assert_eq!(enc(&[0x00, 0x00]), vec![0x01, 0x01, 0x01]);
        assert_eq!(
            enc(&[0x11, 0x22, 0x00, 0x33]),
            vec![0x03, 0x11, 0x22, 0x02, 0x33]
        );
        assert_eq!(
            enc(&[0x11, 0x00, 0x00, 0x00]),
            vec![0x02, 0x11, 0x01, 0x01, 0x01]
        );
        let long: Vec<u8> = (1..=254).collect();
        let e = enc(&long);
        assert_eq!(e[0], 0xFF);
        assert_eq!(e.len(), 256);
        assert_eq!(decode(&e).unwrap(), long);
        let long: Vec<u8> = (0..=255u8).collect();
        assert_eq!(decode(&enc(&long)).unwrap(), long);
    }

    #[test]
    fn bad_input() {
        assert!(decode(&[0x05, 0x01]).is_err());
        assert!(decode(&[0x02, 0x00]).is_err());
    }
}
