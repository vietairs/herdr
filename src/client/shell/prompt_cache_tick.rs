//! One-second repaint cadence for the prompt-cache countdown. The clock lives in
//! `ClientShellState` so render stays pure; the loop already wakes at least every 100 ms.

use super::*;

impl ClientShellState {
    /// Stores the clock; returns true only when the wall-clock second changed and a
    /// countdown is visible at the new instant or was at the previous one, so an idle
    /// or hidden countdown never forces a frame. The previous instant covers the last
    /// live second and any gap (a stalled loop, a sleeping laptop) that jumps straight
    /// past expiry: the countdown is still on screen and must be repainted as `cold`.
    pub(crate) fn tick_prompt_cache(&mut self, now_unix_ms: u64) -> bool {
        let previous = std::mem::replace(&mut self.prompt_cache_now_ms, now_unix_ms);
        previous / 1000 != now_unix_ms / 1000
            && (self.prompt_cache_countdown_visible(now_unix_ms)
                || self.prompt_cache_countdown_visible(previous))
    }

    /// The layout uses the token, agent rows are on screen (sidebar not presented
    /// collapsed, or the mobile layout), and some agent still has a live countdown,
    /// each read against its own endpoint's clock.
    pub(super) fn prompt_cache_countdown_visible(&self, now_unix_ms: u64) -> bool {
        if !self.config.agents.uses_prompt_cache_token() {
            return false;
        }
        if !self.mobile_layout_active() && self.sidebar_presented_collapsed() {
            return false;
        }
        let active_now_ms = super::endpoints::endpoint_server_clock_ms(
            &self.endpoints,
            &self.active_endpoint_id,
            now_unix_ms,
        );
        self.snapshot
            .as_deref()
            .map(|snapshot| (snapshot, active_now_ms))
            .into_iter()
            .chain(self.endpoints.iter().filter_map(|endpoint| {
                endpoint
                    .snapshot
                    .as_deref()
                    .map(|snapshot| (snapshot, endpoint.server_clock_ms(now_unix_ms)))
            }))
            .any(|(snapshot, endpoint_now_ms)| {
                snapshot
                    .agents
                    .iter()
                    .filter_map(|agent| agent.prompt_cache)
                    .any(|cache| {
                        crate::prompt_cache::prompt_cache_live(
                            cache.last_request_at_ms,
                            cache.ttl_secs,
                            endpoint_now_ms,
                        )
                    })
            })
    }
}
