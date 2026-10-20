//! Small helpers: a deterministic PRNG, hex and base64.

/// Deterministic, seedable pseudo-random generator (SplitMix64). Good enough
/// for simulation and fault injection, not for cryptography.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// Creates a generator from a seed.
    pub fn new(seed: u64) -> Self {
        Self {
            state: seed ^ 0x9E37_79B9_7F4A_7C15,
        }
    }

    /// Next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform integer in [0, n). Returns 0 when `n == 0`.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }

    /// True with probability `p`.
    pub fn chance(&mut self, p: f64) -> bool {
        p > 0.0 && self.next_f64() < p
    }

    /// Standard normal sample (Box-Muller).
    pub fn normal(&mut self) -> f64 {
        let u1 = self.next_f64().max(f64::MIN_POSITIVE);
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

/// Formats bytes as space-separated upper-case hex.
pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&format!("{b:02X}"));
    }
    s
}

/// Formats bytes as compact lower-case hex.
pub fn hex_compact(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parses hex, ignoring whitespace, commas and `0x` prefixes.
pub fn parse_hex(s: &str) -> Result<Vec<u8>, String> {
    let cleaned: String = s
        .split(|c: char| c.is_whitespace() || c == ',')
        .map(|t| t.trim_start_matches("0x").trim_start_matches("0X"))
        .collect();
    if cleaned.len() % 2 != 0 {
        return Err(format!("odd number of hex digits in {s:?}"));
    }
    (0..cleaned.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&cleaned[i..i + 2], 16)
                .map_err(|_| format!("invalid hex {:?}", &cleaned[i..i + 2]))
        })
        .collect()
}

/// Standard base64 encoding (used by the Protobuf JSON mapping for bytes).
pub fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let b = vec![0x00, 0xAB, 0x10];
        assert_eq!(hex(&b), "00 AB 10");
        assert_eq!(parse_hex("00 ab 10").unwrap(), b);
        assert_eq!(parse_hex("0x00,0xAB,0x10").unwrap(), b);
        assert!(parse_hex("abc").is_err());
        assert!(parse_hex("zz").is_err());
    }

    #[test]
    fn rng_is_deterministic_and_normalish() {
        let mut a = Rng::new(1);
        let mut b = Rng::new(1);
        assert_eq!(a.next_u64(), b.next_u64());
        let n = 20_000;
        let xs: Vec<f64> = (0..n).map(|_| a.normal()).collect();
        let mean = xs.iter().sum::<f64>() / n as f64;
        let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n as f64;
        assert!(mean.abs() < 0.05, "mean {mean}");
        assert!((var - 1.0).abs() < 0.05, "var {var}");
    }
}
