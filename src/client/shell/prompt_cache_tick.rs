//! One-second repaint cadence for the prompt-cache countdown. The clock lives in
//! `ClientShellState` so render stays pure; the loop already wakes at least every 100 ms.

use super::*;

impl ClientShellState {
    /// Stores the clock; returns true only when the wall-clock second changed and a
    /// countdown is visible, so an idle or hidden countdown never forces a frame.
    pub(crate) fn tick_prompt_cache(&mut self, now_unix_ms: u64) -> bool {
        let previous = std::mem::replace(&mut self.prompt_cache_now_ms, now_unix_ms);
        previous / 1000 != now_unix_ms / 1000 && self.prompt_cache_countdown_visible(now_unix_ms)
    }

    /// The layout uses the token, agent rows are on screen (sidebar not presented
    /// collapsed, or the mobile layout), and some agent still has a live countdown.
    pub(super) fn prompt_cache_countdown_visible(&self, now_unix_ms: u64) -> bool {
        if !self.config.agents.uses_prompt_cache_token() {
            return false;
        }
        if !self.mobile_layout_active() && self.sidebar_presented_collapsed() {
            return false;
        }
        self.snapshot
            .as_deref()
            .into_iter()
            .chain(
                self.endpoints
                    .iter()
                    .filter_map(|endpoint| endpoint.snapshot.as_deref()),
            )
            .flat_map(|snapshot| snapshot.agents.iter())
            .filter_map(|agent| agent.prompt_cache)
            .any(|cache| {
                crate::prompt_cache::prompt_cache_live(
                    cache.last_request_at_ms,
                    cache.ttl_secs,
                    now_unix_ms,
                )
            })
    }
}
