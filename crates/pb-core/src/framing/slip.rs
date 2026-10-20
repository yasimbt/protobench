//! SLIP (RFC 1055) byte stuffing.

/// Frame delimiter.
pub const END: u8 = 0xC0;
/// Escape byte.
pub const ESC: u8 = 0xDB;
/// Escaped END.
pub const ESC_END: u8 = 0xDC;
/// Escaped ESC.
pub const ESC_ESC: u8 = 0xDD;

/// Appends `END || escape(data) || END`.
pub fn encode(data: &[u8], out: &mut Vec<u8>) {
    out.push(END);
    for &b in data {
        match b {
            END => out.extend_from_slice(&[ESC, ESC_END]),
            ESC => out.extend_from_slice(&[ESC, ESC_ESC]),
            b => out.push(b),
        }
    }
    out.push(END);
}

/// Un-escapes one SLIP frame body (without END bytes).
pub fn decode(data: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut out = Vec::with_capacity(data.len());
    let mut it = data.iter();
    while let Some(&b) = it.next() {
        if b == ESC {
            match it.next() {
                Some(&ESC_END) => out.push(END),
                Some(&ESC_ESC) => out.push(ESC),
                _ => return Err("invalid SLIP escape"),
            }
        } else {
            out.push(b);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_specials() {
        let d = [0x01, END, ESC, 0x02];
        let mut e = Vec::new();
        encode(&d, &mut e);
        assert_eq!(e, vec![END, 0x01, ESC, ESC_END, ESC, ESC_ESC, 0x02, END]);
        assert_eq!(decode(&e[1..e.len() - 1]).unwrap(), d);
        assert!(decode(&[ESC, 0x00]).is_err());
        assert!(decode(&[ESC]).is_err());
    }
}
