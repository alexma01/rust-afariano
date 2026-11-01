//! GitLab durations (chronic_duration-like): `30m`, `1h 30m`, `90 minutes`,
//! `2 hours`, `1 hour and 30 minutes`, `1.5h`, `01:30:00`, `3600` (seconds).

pub fn parse(input: &str) -> Option<u64> {
    let s = input.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    if s.contains(':') {
        return parse_clock(&s);
    }
    let s = s.replace(',', " ");
    let mut total = 0f64;
    let mut seen = false;
    let mut chars = s.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&c) = chars.peek() else { break };
        if c.is_ascii_alphabetic() {
            // Only the filler word "and" may stand between amounts.
            let word: String = std::iter::from_fn(|| chars.next_if(|c| c.is_ascii_alphabetic())).collect();
            if word != "and" {
                return None;
            }
            continue;
        }
        let number: String = std::iter::from_fn(|| chars.next_if(|c| c.is_ascii_digit() || *c == '.')).collect();
        let amount: f64 = number.parse().ok()?;
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let unit: String = std::iter::from_fn(|| chars.next_if(|c| c.is_ascii_alphabetic())).collect();
        let factor = if unit.is_empty() { 1.0 } else { unit_seconds(&unit)? };
        total += amount * factor;
        seen = true;
    }
    seen.then(|| total.round() as u64)
}

fn unit_seconds(unit: &str) -> Option<f64> {
    Some(match unit {
        "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
        "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600.0,
        "d" | "day" | "days" => 86_400.0,
        "w" | "wk" | "wks" | "week" | "weeks" => 604_800.0,
        _ => return None,
    })
}

/// `h:mm:ss` or `m:ss`.
fn parse_clock(s: &str) -> Option<u64> {
    let parts: Vec<u64> = s.split(':').map(|p| p.trim().parse().ok()).collect::<Option<_>>()?;
    match parts.as_slice() {
        [m, sec] => Some(m * 60 + sec),
        [h, m, sec] => Some(h * 3600 + m * 60 + sec),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn durations() {
        assert_eq!(parse("30m"), Some(1800));
        assert_eq!(parse("45m"), Some(2700));
        assert_eq!(parse("1h"), Some(3600));
        assert_eq!(parse("2h"), Some(7200));
        assert_eq!(parse("1h 30m"), Some(5400));
        assert_eq!(parse("1h30m"), Some(5400));
        assert_eq!(parse("90 minutes"), Some(5400));
        assert_eq!(parse("2 hours"), Some(7200));
        assert_eq!(parse("1 hour and 30 minutes"), Some(5400));
        assert_eq!(parse("1 hr, 5 mins"), Some(3900));
        assert_eq!(parse("3600"), Some(3600));
        assert_eq!(parse("10s"), Some(10));
        assert_eq!(parse("1.5h"), Some(5400));
        assert_eq!(parse("1 day"), Some(86_400));
        assert_eq!(parse("1w"), Some(604_800));
        assert_eq!(parse("01:30:00"), Some(5400));
        assert_eq!(parse("2:30"), Some(150));
        assert_eq!(parse(" 3 Hours "), Some(10_800));
    }

    #[test]
    fn invalid_durations() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("soon"), None);
        assert_eq!(parse("5 parsecs"), None);
        assert_eq!(parse("1h and"), Some(3600));
        assert_eq!(parse("h"), None);
        assert_eq!(parse("1:2:3:4"), None);
    }
}
