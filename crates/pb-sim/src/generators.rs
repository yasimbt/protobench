//! Field generators: `sine(amp=1500, offset=1600, freq=2)` and friends.

use std::f64::consts::PI;

use pb_core::util::Rng;

/// A parsed generator expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Gen {
    /// `sine(amp, offset, freq)`
    Sine {
        /// Amplitude.
        amp: f64,
        /// Offset.
        offset: f64,
        /// Frequency in Hz.
        freq: f64,
        /// Phase in radians.
        phase: f64,
    },
    /// `square(amp, offset, freq, duty)`
    Square {
        /// Amplitude.
        amp: f64,
        /// Offset.
        offset: f64,
        /// Frequency in Hz.
        freq: f64,
        /// Duty cycle 0..1.
        duty: f64,
        /// Phase in cycles.
        phase: f64,
    },
    /// `ramp(start, end, period_s)`: sawtooth from start to end.
    Ramp {
        /// Start value.
        start: f64,
        /// End value.
        end: f64,
        /// Period.
        period_s: f64,
    },
    /// `noise(mean, std)`: Gaussian.
    Noise {
        /// Mean.
        mean: f64,
        /// Standard deviation.
        std: f64,
    },
    /// `random_walk(start, step)`
    RandomWalk {
        /// Current value.
        value: f64,
        /// Step standard deviation.
        step: f64,
    },
    /// `constant(v)` or a plain number.
    Constant(f64),
    /// `counter()`: 0, 1, 2, …
    Counter(u64),
    /// `enum_cycle(period_s)`: steps through the enum's values.
    EnumCycle {
        /// Time per value.
        period_s: f64,
    },
    /// `time_us()`: simulated device time in microseconds.
    TimeUs,
    /// `uptime_s()`: whole seconds since the simulator started.
    UptimeS,
    /// `array(n, gen)`: `n` independent copies of `gen`.
    Array(Vec<Gen>),
}

/// Value produced by a generator.
#[derive(Debug, Clone, PartialEq)]
pub enum GenValue {
    /// A number (enum generators produce a step index).
    Num(f64),
    /// An array.
    Array(Vec<f64>),
}

impl Gen {
    /// Parses a generator expression.
    pub fn parse(s: &str) -> Result<Gen, String> {
        let mut p = Parser {
            s: s.as_bytes(),
            pos: 0,
            src: s,
        };
        let g = p.gen()?;
        p.ws();
        if p.pos != p.s.len() {
            return Err(format!("unexpected `{}` in `{s}`", &s[p.pos..]));
        }
        Ok(g)
    }

    /// True for `enum_cycle`.
    pub fn is_enum_cycle(&self) -> bool {
        matches!(self, Gen::EnumCycle { .. })
    }

    /// Evaluates at simulated time `t` (seconds since start).
    pub fn eval(&mut self, t: f64, rng: &mut Rng) -> GenValue {
        match self {
            Gen::Array(items) => GenValue::Array(
                items
                    .iter_mut()
                    .map(|g| match g.eval(t, rng) {
                        GenValue::Num(v) => v,
                        GenValue::Array(a) => a.first().copied().unwrap_or(0.0),
                    })
                    .collect(),
            ),
            g => GenValue::Num(g.eval_num(t, rng)),
        }
    }

    fn eval_num(&mut self, t: f64, rng: &mut Rng) -> f64 {
        match self {
            Gen::Sine {
                amp,
                offset,
                freq,
                phase,
            } => *offset + *amp * (2.0 * PI * *freq * t + *phase).sin(),
            Gen::Square {
                amp,
                offset,
                freq,
                duty,
                phase,
            } => {
                let cycle = (t * *freq + *phase).rem_euclid(1.0);
                if cycle < *duty {
                    *offset + *amp
                } else {
                    *offset - *amp
                }
            }
            Gen::Ramp {
                start,
                end,
                period_s,
            } => {
                let f = if *period_s > 0.0 {
                    (t / *period_s).rem_euclid(1.0)
                } else {
                    0.0
                };
                *start + (*end - *start) * f
            }
            Gen::Noise { mean, std } => *mean + *std * rng.normal(),
            Gen::RandomWalk { value, step } => {
                *value += *step * rng.normal();
                *value
            }
            Gen::Constant(v) => *v,
            Gen::Counter(n) => {
                let v = *n as f64;
                *n += 1;
                v
            }
            Gen::EnumCycle { period_s } => {
                if *period_s > 0.0 {
                    (t / *period_s).floor()
                } else {
                    0.0
                }
            }
            Gen::TimeUs => (t * 1e6).floor(),
            Gen::UptimeS => t.floor(),
            Gen::Array(_) => 0.0,
        }
    }
}

struct Parser<'a> {
    s: &'a [u8],
    pos: usize,
    src: &'a str,
}

enum Arg {
    Num(f64),
    Gen(Gen),
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.pos < self.s.len() && self.s[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn ident(&mut self) -> Option<String> {
        self.ws();
        let start = self.pos;
        while self.pos < self.s.len()
            && (self.s[self.pos].is_ascii_alphanumeric() || self.s[self.pos] == b'_')
        {
            self.pos += 1;
        }
        (self.pos > start && !self.s[start].is_ascii_digit())
            .then(|| self.src[start..self.pos].to_string())
    }

    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.s.get(self.pos) == Some(&c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn number(&mut self) -> Option<f64> {
        self.ws();
        let start = self.pos;
        while self.pos < self.s.len()
            && matches!(self.s[self.pos], b'0'..=b'9' | b'.' | b'-' | b'+' | b'e' | b'E' | b'x' | b'a'..=b'd' | b'f' | b'A'..=b'F')
        {
            self.pos += 1;
        }
        let t = &self.src[start..self.pos];
        let v = if let Some(h) = t.strip_prefix("0x") {
            i64::from_str_radix(h, 16).ok().map(|v| v as f64)
        } else {
            t.parse::<f64>().ok()
        };
        if v.is_none() {
            self.pos = start;
        }
        v
    }

    fn gen(&mut self) -> Result<Gen, String> {
        if let Some(v) = self.number() {
            return Ok(Gen::Constant(v));
        }
        let name = self
            .ident()
            .ok_or_else(|| format!("expected a generator in `{}`", self.src))?;
        if !self.eat(b'(') {
            return Err(format!("expected `(` after `{name}` in `{}`", self.src));
        }
        let mut positional: Vec<Arg> = Vec::new();
        let mut named: Vec<(String, f64)> = Vec::new();
        if !self.eat(b')') {
            loop {
                let save = self.pos;
                let key = self.ident();
                if key.is_some() && self.eat(b'=') {
                    let v = self.number().ok_or_else(|| {
                        format!(
                            "expected a number for `{}` in `{}`",
                            key.clone().unwrap_or_default(),
                            self.src
                        )
                    })?;
                    named.push((key.unwrap_or_default(), v));
                } else {
                    self.pos = save;
                    match self.number() {
                        Some(v) => positional.push(Arg::Num(v)),
                        None => positional.push(Arg::Gen(self.gen()?)),
                    }
                }
                if self.eat(b')') {
                    break;
                }
                if !self.eat(b',') {
                    return Err(format!("expected `,` or `)` in `{}`", self.src));
                }
            }
        }
        let src = self.src;
        let get = |key: &str, idx: usize, default: Option<f64>| -> Result<f64, String> {
            if let Some((_, v)) = named.iter().find(|(k, _)| k == key) {
                return Ok(*v);
            }
            match positional.get(idx) {
                Some(Arg::Num(v)) => Ok(*v),
                Some(Arg::Gen(_)) => Err(format!("`{key}` must be a number in `{src}`")),
                None => default.ok_or_else(|| format!("missing `{key}` in `{src}`")),
            }
        };
        let known: &[&str] = match name.as_str() {
            "sine" => &["amp", "offset", "freq", "phase"],
            "square" => &["amp", "offset", "freq", "duty", "phase"],
            "ramp" => &["start", "end", "period_s"],
            "noise" => &["mean", "std"],
            "random_walk" => &["start", "step"],
            "constant" => &["v", "value"],
            "enum_cycle" => &["period_s"],
            _ => &[],
        };
        if let Some((k, _)) = named.iter().find(|(k, _)| !known.contains(&k.as_str())) {
            return Err(format!("unknown argument `{k}` for `{name}` in `{src}`"));
        }
        Ok(match name.as_str() {
            "sine" => Gen::Sine {
                amp: get("amp", 0, Some(1.0))?,
                offset: get("offset", 1, Some(0.0))?,
                freq: get("freq", 2, Some(1.0))?,
                phase: get("phase", 3, Some(0.0))?,
            },
            "square" => Gen::Square {
                amp: get("amp", 0, Some(1.0))?,
                offset: get("offset", 1, Some(0.0))?,
                freq: get("freq", 2, Some(1.0))?,
                duty: get("duty", 3, Some(0.5))?,
                phase: get("phase", 4, Some(0.0))?,
            },
            "ramp" => Gen::Ramp {
                start: get("start", 0, Some(0.0))?,
                end: get("end", 1, Some(1.0))?,
                period_s: get("period_s", 2, Some(1.0))?,
            },
            "noise" => Gen::Noise {
                mean: get("mean", 0, Some(0.0))?,
                std: get("std", 1, Some(1.0))?,
            },
            "random_walk" => Gen::RandomWalk {
                value: get("start", 0, Some(0.0))?,
                step: get("step", 1, Some(1.0))?,
            },
            "constant" => Gen::Constant(
                named
                    .iter()
                    .find(|(k, _)| k == "v" || k == "value")
                    .map(|(_, v)| *v)
                    .map_or_else(|| get("v", 0, None), Ok)?,
            ),
            "counter" => Gen::Counter(0),
            "enum_cycle" => Gen::EnumCycle {
                period_s: get("period_s", 0, Some(1.0))?,
            },
            "time_us" => Gen::TimeUs,
            "uptime_s" => Gen::UptimeS,
            "array" => {
                let n = match positional.first() {
                    Some(Arg::Num(n)) if *n >= 0.0 && *n <= 4096.0 => *n as usize,
                    _ => return Err(format!("array needs a count (0..4096) first in `{src}`")),
                };
                let inner = match positional.get(1) {
                    Some(Arg::Gen(g)) => g.clone(),
                    Some(Arg::Num(v)) => Gen::Constant(*v),
                    None => return Err(format!("array needs a generator in `{src}`")),
                };
                Gen::Array((0..n).map(|i| with_phase(inner.clone(), i, n)).collect())
            }
            other => return Err(format!("unknown generator `{other}` in `{src}`")),
        })
    }
}

/// Spreads array elements of periodic generators over one cycle so the
/// channels are distinguishable.
fn with_phase(g: Gen, i: usize, n: usize) -> Gen {
    let frac = if n > 0 { i as f64 / n as f64 } else { 0.0 };
    match g {
        Gen::Sine {
            amp,
            offset,
            freq,
            phase,
        } => Gen::Sine {
            amp,
            offset,
            freq,
            phase: phase + 2.0 * PI * frac,
        },
        Gen::Square {
            amp,
            offset,
            freq,
            duty,
            phase,
        } => Gen::Square {
            amp,
            offset,
            freq,
            duty,
            phase: phase + frac,
        },
        g => g,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn num(g: &mut Gen, t: f64) -> f64 {
        match g.eval(t, &mut Rng::new(1)) {
            GenValue::Num(v) => v,
            GenValue::Array(_) => panic!("array"),
        }
    }

    #[test]
    fn parses_and_evaluates_all_generators() {
        let mut g = Gen::parse("sine(amp=1500, offset=1600, freq=2)").unwrap();
        assert!((num(&mut g, 0.125) - 3100.0).abs() < 1e-9);
        let mut g = Gen::parse("square(amp=1, offset=0, freq=1, duty=0.25)").unwrap();
        assert_eq!(num(&mut g, 0.1), 1.0);
        assert_eq!(num(&mut g, 0.5), -1.0);
        let mut g = Gen::parse("ramp(start=4100, end=3600, period_s=120)").unwrap();
        assert_eq!(num(&mut g, 0.0), 4100.0);
        assert!((num(&mut g, 60.0) - 3850.0).abs() < 1e-9);
        let mut g = Gen::parse("noise(mean=250, std=5)").unwrap();
        let mut rng = Rng::new(3);
        let xs: Vec<f64> = (0..5000)
            .map(|_| match g.eval(0.0, &mut rng) {
                GenValue::Num(v) => v,
                _ => 0.0,
            })
            .collect();
        let mean = xs.iter().sum::<f64>() / xs.len() as f64;
        assert!((mean - 250.0).abs() < 0.5, "{mean}");
        let mut g = Gen::parse("random_walk(start=24, step=0.01)").unwrap();
        assert!((num(&mut g, 0.0) - 24.0).abs() < 0.1);
        assert_eq!(num(&mut Gen::parse("constant(7)").unwrap(), 0.0), 7.0);
        assert_eq!(num(&mut Gen::parse("constant(v=7)").unwrap(), 0.0), 7.0);
        assert_eq!(num(&mut Gen::parse("42").unwrap(), 0.0), 42.0);
        let mut c = Gen::parse("counter()").unwrap();
        assert_eq!((num(&mut c, 0.0), num(&mut c, 0.0)), (0.0, 1.0));
        let mut e = Gen::parse("enum_cycle(period_s=3)").unwrap();
        assert!(e.is_enum_cycle());
        assert_eq!(num(&mut e, 7.0), 2.0);
        assert_eq!(num(&mut Gen::parse("time_us()").unwrap(), 1.5), 1_500_000.0);
        assert_eq!(num(&mut Gen::parse("uptime_s()").unwrap(), 9.9), 9.0);
        let mut a = Gen::parse("array(4, sine(amp=1000, offset=2048, freq=5))").unwrap();
        match a.eval(0.0, &mut Rng::new(1)) {
            GenValue::Array(v) => {
                assert_eq!(v.len(), 4);
                assert!((v[0] - 2048.0).abs() < 1e-9);
                assert!((v[1] - 3048.0).abs() < 1e-9);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn errors_are_readable() {
        assert!(Gen::parse("sine(amp=").unwrap_err().contains("number"));
        assert!(Gen::parse("bogus()")
            .unwrap_err()
            .contains("unknown generator"));
        assert!(Gen::parse("sine(ampl=1)")
            .unwrap_err()
            .contains("unknown argument"));
        assert!(Gen::parse("array(sine())").is_err());
        assert!(Gen::parse("sine() x").is_err());
        assert!(Gen::parse("hello world").is_err());
    }
}
