//! Value formatting shared by the binary-format decoders (Parquet, Avro,
//! Arrow, …), so the same value reads the same in every format.

/// Table cells are cut to this many characters.
pub(crate) const MAX_CELL: usize = 64;

pub(crate) fn clip(s: String) -> String {
    if s.chars().count() <= MAX_CELL {
        return s;
    }
    let mut c: String = s.chars().take(MAX_CELL - 1).collect();
    c.push('…');
    c
}

fn civil(days: i64) -> (i64, u32, u32) {
    // Howard Hinnant's days-from-civil, inverted.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

pub(crate) fn date(days: i64) -> String {
    let (y, m, d) = civil(days);
    format!("{y:04}-{m:02}-{d:02}")
}

pub(crate) fn time_of_day(t: i64, digits: u32) -> String {
    let per_sec = 10i64.pow(digits);
    let secs = t.div_euclid(per_sec);
    let frac = t.rem_euclid(per_sec);
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if frac == 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}.{frac:0w$}", w = digits as usize)
    }
}

pub(crate) fn timestamp(t: i64, digits: u32, utc: bool) -> String {
    let per_day = 86_400 * 10i64.pow(digits);
    let days = t.div_euclid(per_day);
    let rest = t.rem_euclid(per_day);
    format!(
        "{} {}{}",
        date(days),
        time_of_day(rest, digits),
        if utc { "Z" } else { "" }
    )
}

pub(crate) fn decimal(x: i128, scale: i64) -> String {
    if scale <= 0 {
        return x.to_string();
    }
    let neg = x < 0;
    let digits = x.unsigned_abs().to_string();
    let scale = scale as usize;
    let padded = format!("{digits:0>w$}", w = scale + 1);
    let (int, frac) = padded.split_at(padded.len() - scale);
    format!("{}{int}.{frac}", if neg { "-" } else { "" })
}

pub(crate) fn be_i128(b: &[u8]) -> Option<i128> {
    if b.is_empty() || b.len() > 16 {
        return None;
    }
    let fill = if b[0] & 0x80 != 0 { 0xff } else { 0 };
    let mut buf = [fill; 16];
    buf[16 - b.len()..].copy_from_slice(b);
    Some(i128::from_be_bytes(buf))
}

pub(crate) fn uuid(b: &[u8]) -> String {
    let h = hex(b);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(20_734), "2026-10-08");
        assert_eq!(date(-1), "1969-12-31");
        assert_eq!(decimal(-5, 2), "-0.05");
        assert_eq!(decimal(12345, 2), "123.45");
    }
}
