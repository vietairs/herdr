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

/// A sample this far below the estimate is a clock step rather than delivery delay,
/// which stays far shorter on a live connection.
const CLOCK_STEP_DOWN_MS: i64 = 30_000;

/// A low sample confirms a step only when the remote stamped it at least this long
/// after the held one, so the frames one poll stamps together count once.
const CLOCK_STEP_CONFIRM_SPACING_MS: u64 = 5_000;

/// Two low samples this close describe one steady new offset. Messages from a backlog
/// read in a burst differ by the gaps between their stamps, which the spacing above
/// keeps wider than this.
const CLOCK_STEP_AGREEMENT_MS: u64 = 2_000;

/// Running estimate of another host's clock offset over one connection.
///
/// A timestamped message can wait in a queue before it is read, and that delay only
/// lowers a sample, so the largest sample is the closest to the true offset. A clock
/// step can lower the true offset too, so a sample more than `CLOCK_STEP_DOWN_MS`
/// below the estimate is held, and replaces the estimate once a later one confirms
/// it: stamped at least `CLOCK_STEP_CONFIRM_SPACING_MS` after it by the remote, and
/// within `CLOCK_STEP_AGREEMENT_MS` of it. Delayed messages, alone or drained from a
/// backlog together, never agree that way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ClockOffsetEstimate {
    offset_ms: i64,
    sampled: bool,
    /// The held low sample and the remote stamp it was taken from.
    stepped_down: Option<(i64, u64)>,
}

impl ClockOffsetEstimate {
    /// The current offset; 0 before the first sample.
    pub(crate) fn offset_ms(&self) -> i64 {
        self.offset_ms
    }

    /// Folds in one `remote_clock_offset_ms` sample taken from the remote stamp
    /// `remote_now_ms`. The first sample on a connection stands on its own.
    pub(crate) fn observe(&mut self, sample_ms: i64, remote_now_ms: u64) {
        if !self.sampled {
            self.offset_ms = sample_ms;
            self.sampled = true;
            return;
        }
        if sample_ms >= self.offset_ms.saturating_sub(CLOCK_STEP_DOWN_MS) {
            self.stepped_down = None;
            self.offset_ms = self.offset_ms.max(sample_ms);
            return;
        }
        let Some((held_ms, held_remote_ms)) = self.stepped_down else {
            self.stepped_down = Some((sample_ms, remote_now_ms));
            return;
        };
        let confirm_after_ms = held_remote_ms.saturating_add(CLOCK_STEP_CONFIRM_SPACING_MS);
        if remote_now_ms >= held_remote_ms && remote_now_ms < confirm_after_ms {
            // Stamped with or just after the held sample, as one poll's frames are:
            // not a second opinion.
            return;
        }
        if remote_now_ms >= confirm_after_ms
            && held_ms.abs_diff(sample_ms) <= CLOCK_STEP_AGREEMENT_MS
        {
            self.offset_ms = held_ms.max(sample_ms);
            self.stepped_down = None;
            return;
        }
        // A disagreeing sample, or a remote stamp that went backwards because that
        // clock stepped since the held one: hold this one instead.
        self.stepped_down = Some((sample_ms, remote_now_ms));
    }

    /// True when the offset moved by at least the countdown's one-second resolution,
    /// so timestamps already moved with `previous_ms` read differently now.
    pub(crate) fn moved_from(&self, previous_ms: i64) -> bool {
        self.offset_ms.abs_diff(previous_ms) >= CLOCK_SKEW_TOLERANCE_MS
    }
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

    /// The estimate after `samples`, each `(sample_ms, remote_now_ms)`.
    fn estimate_after_stamped(samples: &[(i64, u64)]) -> ClockOffsetEstimate {
        let mut estimate = ClockOffsetEstimate::default();
        for &(sample_ms, remote_now_ms) in samples {
            estimate.observe(sample_ms, remote_now_ms);
        }
        estimate
    }

    /// The estimate after `samples` read one minute apart on both clocks, so each one's
    /// remote stamp is a minute after the last.
    fn estimate_after(samples: &[i64]) -> ClockOffsetEstimate {
        let stamped: Vec<(i64, u64)> = samples
            .iter()
            .zip(0_u64..)
            .map(|(&sample_ms, minute)| {
                let local_now_ms = L + minute * 60_000;
                (sample_ms, local_now_ms.saturating_add_signed(sample_ms))
            })
            .collect();
        estimate_after_stamped(&stamped)
    }

    #[test]
    fn clock_offset_estimate_keeps_the_largest_sample_through_delivery_delay() {
        assert_eq!(ClockOffsetEstimate::default().offset_ms(), 0);
        assert_eq!(estimate_after(&[-180_000]).offset_ms(), -180_000);
        assert_eq!(
            estimate_after(&[600_000, 597_000, 600_040, 590_000, 571_000]).offset_ms(),
            600_040
        );
        assert_eq!(estimate_after(&[0, -5_000, 0, -10_000]).offset_ms(), 0);
    }

    #[test]
    fn clock_offset_estimate_follows_a_clock_step_down_after_two_samples() {
        // A single low sample is a delayed message, not a step.
        assert_eq!(estimate_after(&[600_000, 0]).offset_ms(), 600_000);
        assert_eq!(
            estimate_after(&[600_000, 0, 600_000, 0]).offset_ms(),
            600_000
        );
        // Two in a row that agree are a step: take the larger of them.
        assert_eq!(estimate_after(&[600_000, -40, 0]).offset_ms(), 0);
        assert_eq!(estimate_after(&[600_000, 0, -40, 0, -200]).offset_ms(), 0);
        // Two that disagree are delayed messages; the later one waits for a third.
        assert_eq!(estimate_after(&[600_000, 300_000, 0]).offset_ms(), 600_000);
        assert_eq!(estimate_after(&[600_000, 300_000, 0, -40]).offset_ms(), 0);
        // Upward steps were always taken at once.
        assert_eq!(estimate_after(&[0, 600_000]).offset_ms(), 600_000);
    }

    #[test]
    fn clock_offset_estimate_counts_one_polls_low_samples_once() {
        // Frames stamped by one poll share its stamp, so a late poll that changed two
        // terminals is still one delayed message.
        let late_poll_ms = L + 60_000;
        assert_eq!(
            estimate_after_stamped(&[
                (600_000, L + 600_000),
                (0, late_poll_ms),
                (-40, late_poll_ms),
            ])
            .offset_ms(),
            600_000
        );
        // The step is still taken once a later poll agrees.
        assert_eq!(
            estimate_after_stamped(&[
                (600_000, L + 600_000),
                (0, late_poll_ms),
                (-40, late_poll_ms),
                (-40, late_poll_ms + 60_000),
            ])
            .offset_ms(),
            0
        );
    }

    #[test]
    fn clock_offset_estimate_ignores_a_backlog_read_in_a_burst() {
        // Read together at one local instant `L`, stamped minutes apart while the link
        // was stalled: each sample is the true offset less its frame's age.
        let burst: Vec<(i64, u64)> = [300_000_i64, 200_000, 100_000]
            .iter()
            .map(|&age_ms| {
                (
                    600_000 - age_ms,
                    L + 600_000 - u64::try_from(age_ms).unwrap(),
                )
            })
            .collect();
        let mut samples = vec![(600_000, L)];
        samples.extend(&burst);
        assert_eq!(estimate_after_stamped(&samples).offset_ms(), 600_000);

        // Polls a second apart are inside the spacing until they no longer agree.
        let backlog: Vec<(i64, u64)> = (0..=10_u64)
            .map(|second| {
                let age_ms = 120_000 - second * 1000;
                (
                    600_000 - i64::try_from(age_ms).unwrap(),
                    L + 600_000 - age_ms,
                )
            })
            .collect();
        let mut samples = vec![(600_000, L)];
        samples.extend(&backlog);
        assert_eq!(estimate_after_stamped(&samples).offset_ms(), 600_000);
    }

    #[test]
    fn clock_offset_estimate_holds_a_new_sample_after_the_remote_clock_steps_back() {
        // The held sample's stamp is later than anything the remote sends once its
        // clock steps back ten minutes, so the samples after the step confirm it.
        assert_eq!(
            estimate_after_stamped(&[
                (600_000, L + 600_000),
                (0, L + 600_000 + 60_000),
                (-600_000, L),
                (-600_000, L + 60_000),
            ])
            .offset_ms(),
            -600_000
        );
    }

    #[test]
    fn clock_offset_estimate_reports_moves_of_a_second_or_more() {
        let estimate = estimate_after(&[-180_000]);
        assert!(!estimate.moved_from(-180_999));
        assert!(estimate.moved_from(-181_000));
        assert!(estimate.moved_from(0));
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
