//! Wall-clock and prompt-cache countdown helpers shared by the CLI and the client.

/// Current wall clock as unix epoch milliseconds; 0 if the clock is before 1970.
pub(crate) fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Skew below this is treated as none: it is under the countdown's one-second
/// resolution, and measuring it across a socket mostly measures delivery latency.
const CLOCK_SKEW_TOLERANCE_MS: u64 = 1000;

/// How far another host's wall clock runs ahead of ours (negative when behind),
/// from its `remote_now_ms` read at roughly our `local_now_ms`. 0 when the remote
/// sent no clock (an older peer) or the two agree within a second.
pub(crate) fn remote_clock_offset_ms(remote_now_ms: u64, local_now_ms: u64) -> i64 {
    if remote_now_ms == 0 || remote_now_ms.abs_diff(local_now_ms) < CLOCK_SKEW_TOLERANCE_MS {
        return 0;
    }
    let offset = i128::from(remote_now_ms) - i128::from(local_now_ms);
    i64::try_from(offset).unwrap_or(if offset < 0 { i64::MIN } else { i64::MAX })
}

/// `at_ms` on our clock to the remote clock `remote_offset_ms` describes.
pub(crate) fn local_to_remote_clock_ms(at_ms: u64, remote_offset_ms: i64) -> u64 {
    at_ms.saturating_add_signed(remote_offset_ms)
}

/// A remote timestamp `remote_at_ms` on our clock. A zero timestamp stays zero so
/// "never" is not shifted into a real instant.
pub(crate) fn remote_to_local_clock_ms(remote_at_ms: u64, remote_offset_ms: i64) -> u64 {
    if remote_at_ms == 0 {
        return 0;
    }
    remote_at_ms.saturating_add_signed(remote_offset_ms.saturating_neg())
}

/// How much of a prompt-cache window is left at one instant. `Copy` so token
/// resolution in the render path never allocates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PromptCacheCountdown {
    pub(crate) ttl_secs: u32,
    /// Whole seconds left, rounded up; 0 once expired.
    pub(crate) remaining_secs: u64,
    /// `floor(remaining_ms * 10 / ttl_ms)`, clamped to `0..=10`.
    pub(crate) tenths_left: u8,
}

/// Countdown for a request sent at `last_request_at_ms`. A timestamp in the
/// future counts as "just now" (elapsed 0).
pub(crate) fn prompt_cache_countdown(
    last_request_at_ms: u64,
    ttl_secs: u32,
    now_unix_ms: u64,
) -> PromptCacheCountdown {
    let ttl_ms = u64::from(ttl_secs) * 1000;
    let elapsed_ms = now_unix_ms.saturating_sub(last_request_at_ms);
    let remaining_ms = ttl_ms.saturating_sub(elapsed_ms);
    // remaining_ms <= ttl_ms <= u32::MAX * 1000, so the product fits in u64; a zero TTL has
    // no time left.
    let tenths_left = (remaining_ms * 10)
        .checked_div(ttl_ms)
        .map_or(0, |tenths| u8::try_from(tenths.min(10)).unwrap_or(10));
    PromptCacheCountdown {
        ttl_secs,
        remaining_secs: remaining_ms.div_ceil(1000),
        tenths_left,
    }
}

/// True until one second after expiry, so the transition to "cold" is painted.
pub(crate) fn prompt_cache_live(last_request_at_ms: u64, ttl_secs: u32, now_unix_ms: u64) -> bool {
    let ttl_ms = u64::from(ttl_secs) * 1000;
    now_unix_ms
        < last_request_at_ms
            .saturating_add(ttl_ms)
            .saturating_add(1000)
}

/// Writes `5m 4:12`, `1h 59:59`, `1h 1:00:00`, `90s 0:05`, or `cold` once expired.
pub(crate) fn write_prompt_cache_countdown_text(
    countdown: PromptCacheCountdown,
    out: &mut impl std::fmt::Write,
) -> std::fmt::Result {
    if countdown.remaining_secs == 0 {
        return out.write_str("cold");
    }
    let ttl = countdown.ttl_secs;
    if ttl.is_multiple_of(3600) {
        write!(out, "{}h ", ttl / 3600)?;
    } else if ttl.is_multiple_of(60) {
        write!(out, "{}m ", ttl / 60)?;
    } else {
        write!(out, "{ttl}s ")?;
    }
    let remaining = countdown.remaining_secs;
    if remaining >= 3600 {
        write!(
            out,
            "{}:{:02}:{:02}",
            remaining / 3600,
            remaining % 3600 / 60,
            remaining % 60
        )
    } else {
        write!(out, "{}:{:02}", remaining / 60, remaining % 60)
    }
}

/// Counts the bytes written through `fmt::Write` without storing them.
pub(crate) struct CountingWriter(pub(crate) usize);

impl std::fmt::Write for CountingWriter {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0 += s.len();
        Ok(())
    }
}

/// Display width of the countdown text without allocating (the text is ASCII).
pub(crate) fn prompt_cache_countdown_text_width(countdown: PromptCacheCountdown) -> usize {
    let mut counter = CountingWriter(0);
    // CountingWriter never fails, so the result carries no information.
    let _ = write_prompt_cache_countdown_text(countdown, &mut counter);
    counter.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_now_ms_is_after_2020() {
        assert!(unix_now_ms() > 1_577_836_800_000);
    }

    const L: u64 = 1_000_000;

    fn text(countdown: PromptCacheCountdown) -> String {
        let mut out = String::new();
        write_prompt_cache_countdown_text(countdown, &mut out).unwrap();
        out
    }

    fn countdown(ttl_secs: u32, remaining_secs: u64) -> PromptCacheCountdown {
        PromptCacheCountdown {
            ttl_secs,
            remaining_secs,
            tenths_left: 5,
        }
    }

    #[test]
    fn prompt_cache_countdown_counts_down_in_whole_seconds() {
        for (now, expected) in [
            (L, 300),
            (L + 1, 300),
            (L + 1000, 299),
            (L + 299_001, 1),
            (L + 300_000, 0),
            (L - 5000, 300),
        ] {
            assert_eq!(
                prompt_cache_countdown(L, 300, now).remaining_secs,
                expected,
                "now = {now}"
            );
        }
    }

    #[test]
    fn prompt_cache_countdown_tenths_follow_time_left() {
        for (now, expected) in [
            (L, 10),
            (L + 60_000, 8),
            (L + 150_000, 5),
            (L + 295_000, 0),
            (L + 400_000, 0),
        ] {
            assert_eq!(
                prompt_cache_countdown(L, 300, now).tenths_left,
                expected,
                "now = {now}"
            );
        }
    }

    #[test]
    fn prompt_cache_countdown_text_formats() {
        assert_eq!(text(countdown(300, 252)), "5m 4:12");
        assert_eq!(text(countdown(3600, 3599)), "1h 59:59");
        assert_eq!(text(countdown(3600, 3600)), "1h 1:00:00");
        assert_eq!(text(countdown(90, 5)), "90s 0:05");
        assert_eq!(text(countdown(300, 0)), "cold");
    }

    #[test]
    fn prompt_cache_countdown_text_width_matches_written_text() {
        for case in [
            countdown(300, 252),
            countdown(3600, 3599),
            countdown(3600, 3600),
            countdown(90, 5),
            countdown(300, 0),
        ] {
            assert_eq!(prompt_cache_countdown_text_width(case), text(case).len());
        }
    }

    #[test]
    fn remote_clock_offset_ignores_sub_second_skew_and_missing_clocks() {
        assert_eq!(remote_clock_offset_ms(0, L), 0);
        assert_eq!(remote_clock_offset_ms(L + 999, L), 0);
        assert_eq!(remote_clock_offset_ms(L - 999, L), 0);
        assert_eq!(remote_clock_offset_ms(L + 180_000, L), 180_000);
        assert_eq!(remote_clock_offset_ms(L - 180_000, L), -180_000);
    }

    #[test]
    fn clock_translation_round_trips_and_keeps_zero() {
        assert_eq!(local_to_remote_clock_ms(L, -180_000), L - 180_000);
        assert_eq!(remote_to_local_clock_ms(L - 180_000, -180_000), L);
        assert_eq!(remote_to_local_clock_ms(L + 5000, 5000), L);
        assert_eq!(remote_to_local_clock_ms(0, -180_000), 0);
        assert_eq!(local_to_remote_clock_ms(5, -180_000), 0);
    }

    #[test]
    fn prompt_cache_live_includes_one_second_after_expiry() {
        assert!(prompt_cache_live(L, 300, L + 300_000));
        assert!(prompt_cache_live(L, 300, L + 300_999));
        assert!(!prompt_cache_live(L, 300, L + 301_000));
    }
}
