//! Context-window usage formatting for the agent sidebar token.

/// One observed context-window reading. `Copy` so token resolution in the render path never
/// allocates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextReading {
    pub(crate) used_tokens: u64,
    /// The exact window an integration observed; `None` when it is unknown.
    pub(crate) window_tokens: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextLevel {
    Unknown,
    Low,
    Warn,
    Critical,
}

pub(crate) const CONTEXT_WARN_PERCENT: u64 = 50;
pub(crate) const CONTEXT_CRITICAL_PERCENT: u64 = 80;

/// `floor(used * 100 / window)` with saturating maths; `None` when the window is unknown or 0.
pub(crate) fn context_percent(reading: ContextReading) -> Option<u64> {
    reading
        .window_tokens
        .and_then(|window| reading.used_tokens.saturating_mul(100).checked_div(window))
}

/// Unknown without a percentage; Low below 50; Warn 50..=79; Critical from 80.
pub(crate) fn context_level(reading: ContextReading) -> ContextLevel {
    match context_percent(reading) {
        None => ContextLevel::Unknown,
        Some(percent) if percent >= CONTEXT_CRITICAL_PERCENT => ContextLevel::Critical,
        Some(percent) if percent >= CONTEXT_WARN_PERCENT => ContextLevel::Warn,
        Some(_) => ContextLevel::Low,
    }
}

/// `950`, `84k`, `1M`, `1.5M`: whole thousands below a million, tenths of millions above.
pub(crate) fn write_token_count(tokens: u64, out: &mut impl std::fmt::Write) -> std::fmt::Result {
    if tokens < 1000 {
        write!(out, "{tokens}")
    } else if tokens < 1_000_000 {
        write!(out, "{}k", tokens / 1000)
    } else {
        let tenths = tokens / 100_000;
        if tenths.is_multiple_of(10) {
            write!(out, "{}M", tenths / 10)
        } else {
            write!(out, "{}.{}M", tenths / 10, tenths % 10)
        }
    }
}

/// `ctx 84k/200k 42%` when the window is known, else `ctx 84k`.
pub(crate) fn write_context_usage_text(
    reading: ContextReading,
    out: &mut impl std::fmt::Write,
) -> std::fmt::Result {
    out.write_str("ctx ")?;
    write_token_count(reading.used_tokens, out)?;
    if let (Some(window), Some(percent)) = (reading.window_tokens, context_percent(reading)) {
        out.write_char('/')?;
        write_token_count(window, out)?;
        write!(out, " {percent}%")?;
    }
    Ok(())
}

/// Display width of the text without allocating (the text is ASCII).
pub(crate) fn context_usage_text_width(reading: ContextReading) -> usize {
    let mut counter = crate::prompt_cache::CountingWriter(0);
    // CountingWriter never fails, so the result carries no information.
    let _ = write_context_usage_text(reading, &mut counter);
    counter.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(used: u64, window: Option<u64>) -> String {
        let mut out = String::new();
        write_context_usage_text(
            ContextReading {
                used_tokens: used,
                window_tokens: window,
            },
            &mut out,
        )
        .unwrap();
        out
    }

    fn reading(used: u64, window: Option<u64>) -> ContextReading {
        ContextReading {
            used_tokens: used,
            window_tokens: window,
        }
    }

    #[test]
    fn context_percent_floors_and_handles_unknown_window() {
        assert_eq!(context_percent(reading(84_000, Some(200_000))), Some(42));
        assert_eq!(context_percent(reading(199_999, Some(200_000))), Some(99));
        assert_eq!(context_percent(reading(300_000, Some(200_000))), Some(150));
        assert_eq!(context_percent(reading(84_000, None)), None);
        assert_eq!(context_percent(reading(84_000, Some(0))), None);
        assert!(context_percent(reading(u64::MAX, Some(1))).is_some());
    }

    #[test]
    fn context_level_thresholds() {
        let at = |percent: u64| context_level(reading(percent, Some(100)));
        assert_eq!(at(49), ContextLevel::Low);
        assert_eq!(at(50), ContextLevel::Warn);
        assert_eq!(at(79), ContextLevel::Warn);
        assert_eq!(at(80), ContextLevel::Critical);
        assert_eq!(context_level(reading(90_000, None)), ContextLevel::Unknown);
    }

    const CASES: [(u64, Option<u64>, &str); 7] = [
        (84_000, Some(200_000), "ctx 84k/200k 42%"),
        (950, Some(200_000), "ctx 950/200k 0%"),
        (840_000, Some(1_000_000), "ctx 840k/1M 84%"),
        (1_500_000, Some(2_000_000), "ctx 1.5M/2M 75%"),
        (100_000, Some(258_400), "ctx 100k/258k 38%"),
        (84_321, None, "ctx 84k"),
        (999_999, None, "ctx 999k"),
    ];

    #[test]
    fn context_usage_text_formats() {
        for (used, window, expected) in CASES {
            assert_eq!(text(used, window), expected);
        }
    }

    #[test]
    fn context_usage_text_width_matches_written_text() {
        for (used, window, _) in CASES {
            assert_eq!(
                context_usage_text_width(reading(used, window)),
                text(used, window).len()
            );
        }
    }
}
