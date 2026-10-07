//! Per-pane context-usage fact: how many tokens the agent's last model request
//! sent as context and, when a reporter observed it exactly, the model's context
//! window. Integrations report it; the server stores it on the terminal and
//! exposes it on pane and agent info. A window is never guessed.

use crate::api::schema::{ContextUsageInfo, PaneReportContextUsageParams, ResponseResult};
use crate::app::App;

use super::super::api_helpers::normalize_metadata_source;
use super::panes::pane_not_found;
use super::responses::{encode_error, encode_success};

/// Largest token count a reporter may state for either figure.
pub(crate) const CONTEXT_MAX_TOKENS: u64 = 100_000_000;

/// Pure merge of one report into the stored fact.
///
/// `None` means the report is ignored because it was observed before the stored
/// reading. A report without `window_tokens` keeps the stored window, so a
/// tokens-only reporter never erases an exact window another reporter sent.
pub(super) fn merge_context_usage_report(
    current: Option<&ContextUsageInfo>,
    source: String,
    used_tokens: u64,
    window_tokens: Option<u64>,
    observed_at_ms: u64,
) -> Option<ContextUsageInfo> {
    if current.is_some_and(|stored| observed_at_ms < stored.observed_at_ms) {
        return None;
    }
    let window_tokens = window_tokens.or_else(|| current.and_then(|stored| stored.window_tokens));
    Some(ContextUsageInfo {
        source,
        used_tokens,
        window_tokens,
        observed_at_ms,
    })
}

impl App {
    pub(super) fn handle_pane_report_context_usage(
        &mut self,
        id: String,
        params: PaneReportContextUsageParams,
    ) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let source = match normalize_metadata_source(params.source) {
            Ok(source) => source,
            Err(message) => return encode_error(id, "invalid_metadata_source", message),
        };
        let request_shape_valid = if params.clear {
            params.used_tokens.is_none()
                && params.window_tokens.is_none()
                && params.observed_at_ms.is_none()
        } else {
            params.used_tokens.is_some() && params.observed_at_ms.is_some_and(|at| at > 0)
        };
        if !request_shape_valid {
            return encode_error(
                id,
                "invalid_context_usage",
                "set used_tokens and observed_at_ms, or clear",
            );
        }
        if params
            .used_tokens
            .is_some_and(|used| used > CONTEXT_MAX_TOKENS)
            || params
                .window_tokens
                .is_some_and(|window| window == 0 || window > CONTEXT_MAX_TOKENS)
        {
            return encode_error(
                id,
                "invalid_context_tokens",
                format!(
                    "used_tokens must be at most {CONTEXT_MAX_TOKENS} and window_tokens between 1 and {CONTEXT_MAX_TOKENS}"
                ),
            );
        }
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.pane_state(pane_id))
            .map(|pane| pane.attached_terminal_id.clone())
        else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(terminal) = self.state.terminals.get_mut(&terminal_id) else {
            return pane_not_found(id, &params.pane_id);
        };

        let changed = match (params.used_tokens, params.observed_at_ms) {
            (Some(used_tokens), Some(observed_at_ms)) => {
                match merge_context_usage_report(
                    terminal.context_usage.as_ref(),
                    source,
                    used_tokens,
                    params.window_tokens,
                    observed_at_ms,
                ) {
                    Some(next) if terminal.context_usage.as_ref() != Some(&next) => {
                        terminal.context_usage = Some(next);
                        true
                    }
                    _ => false,
                }
            }
            _ => terminal.context_usage.take().is_some(),
        };
        if changed {
            terminal.revision = terminal.revision.saturating_add(1);
            self.emit_pane_updated(ws_idx, pane_id);
            self.render_dirty.request_generic();
        }
        encode_success(id, ResponseResult::Ok {})
    }
}

#[cfg(test)]
mod tests {
    use super::super::prompt_cache::tests::app_with_test_workspace;
    use super::super::test_support::shutdown_test_runtimes;
    use super::*;
    use crate::api::schema::{ErrorResponse, Method, Request, SuccessResponse};

    fn params(pane_id: &str) -> PaneReportContextUsageParams {
        PaneReportContextUsageParams {
            pane_id: pane_id.to_string(),
            source: "herdr:claude".to_string(),
            used_tokens: Some(84_000),
            window_tokens: Some(200_000),
            observed_at_ms: Some(1_000_000),
            clear: false,
        }
    }

    fn clear_params(pane_id: &str) -> PaneReportContextUsageParams {
        PaneReportContextUsageParams {
            pane_id: pane_id.to_string(),
            source: "herdr:claude".to_string(),
            used_tokens: None,
            window_tokens: None,
            observed_at_ms: None,
            clear: true,
        }
    }

    fn fact(app: &crate::app::App, pane_id: &str) -> Option<ContextUsageInfo> {
        let (ws_idx, pane) = app.parse_pane_id(pane_id).unwrap();
        app.pane_info(ws_idx, pane).unwrap().context_usage
    }

    fn revision(app: &crate::app::App, pane_id: &str) -> u64 {
        let (ws_idx, pane) = app.parse_pane_id(pane_id).unwrap();
        app.pane_info(ws_idx, pane).unwrap().revision
    }

    fn error_code(response: &str) -> String {
        let response: ErrorResponse = serde_json::from_str(response).unwrap();
        response.error.code
    }

    fn assert_ok(response: &str) {
        let _: SuccessResponse = serde_json::from_str(response).unwrap();
    }

    fn info(source: &str, used: u64, window: Option<u64>, at: u64) -> ContextUsageInfo {
        ContextUsageInfo {
            source: source.to_string(),
            used_tokens: used,
            window_tokens: window,
            observed_at_ms: at,
        }
    }

    #[test]
    fn context_usage_report_sets_fact_and_bumps_revision() {
        let (mut app, pane) = app_with_test_workspace();
        let before = revision(&app, &pane);

        let response = app.handle_pane_report_context_usage("r".into(), params(&pane));

        assert_ok(&response);
        assert_eq!(
            fact(&app, &pane),
            Some(info("herdr:claude", 84_000, Some(200_000), 1_000_000))
        );
        assert_eq!(revision(&app, &pane), before + 1);
        shutdown_test_runtimes(&mut app);
    }

    #[test]
    fn context_usage_report_without_window_keeps_stored_window() {
        let (mut app, pane) = app_with_test_workspace();
        assert_ok(&app.handle_pane_report_context_usage("r".into(), params(&pane)));

        let mut tokens_only = params(&pane);
        tokens_only.used_tokens = Some(90_000);
        tokens_only.window_tokens = None;
        tokens_only.observed_at_ms = Some(2_000_000);
        assert_ok(&app.handle_pane_report_context_usage("r".into(), tokens_only));

        let stored = fact(&app, &pane).unwrap();
        assert_eq!(stored.used_tokens, 90_000);
        assert_eq!(stored.window_tokens, Some(200_000));
        assert_eq!(stored.observed_at_ms, 2_000_000);
        shutdown_test_runtimes(&mut app);
    }

    #[test]
    fn context_usage_report_without_window_and_nothing_stored_has_no_window() {
        let (mut app, pane) = app_with_test_workspace();

        let mut tokens_only = params(&pane);
        tokens_only.window_tokens = None;
        assert_ok(&app.handle_pane_report_context_usage("r".into(), tokens_only));

        let stored = fact(&app, &pane).unwrap();
        assert_eq!(stored.used_tokens, 84_000);
        assert_eq!(stored.window_tokens, None);
        shutdown_test_runtimes(&mut app);
    }

    #[test]
    fn context_usage_report_ignores_older_observation_without_revision_bump() {
        let (mut app, pane) = app_with_test_workspace();
        assert_ok(&app.handle_pane_report_context_usage("r".into(), params(&pane)));
        let before = revision(&app, &pane);

        let mut older = params(&pane);
        older.used_tokens = Some(10_000);
        older.observed_at_ms = Some(999_999);
        assert_ok(&app.handle_pane_report_context_usage("r".into(), older));

        assert_eq!(
            fact(&app, &pane),
            Some(info("herdr:claude", 84_000, Some(200_000), 1_000_000))
        );
        assert_eq!(revision(&app, &pane), before);
        shutdown_test_runtimes(&mut app);
    }

    #[test]
    fn context_usage_report_clear_removes_fact_and_is_idempotent() {
        let (mut app, pane) = app_with_test_workspace();
        assert_ok(&app.handle_pane_report_context_usage("r".into(), params(&pane)));
        let after_set = revision(&app, &pane);

        assert_ok(&app.handle_pane_report_context_usage("r".into(), clear_params(&pane)));
        assert_eq!(fact(&app, &pane), None);
        assert_eq!(revision(&app, &pane), after_set + 1);

        assert_ok(&app.handle_pane_report_context_usage("r".into(), clear_params(&pane)));
        assert_eq!(fact(&app, &pane), None);
        assert_eq!(revision(&app, &pane), after_set + 1);
        shutdown_test_runtimes(&mut app);
    }

    #[test]
    fn context_usage_report_rejects_invalid_params() {
        let (mut app, pane) = app_with_test_workspace();
        let code = |app: &mut crate::app::App, report: PaneReportContextUsageParams| {
            error_code(&app.handle_pane_report_context_usage("r".into(), report))
        };

        assert_eq!(code(&mut app, params("p_missing")), "pane_not_found");

        let mut bad_source = params(&pane);
        bad_source.source = "bad source!".to_string();
        assert_eq!(code(&mut app, bad_source), "invalid_metadata_source");

        let mut clear_with_tokens = clear_params(&pane);
        clear_with_tokens.used_tokens = Some(1);
        assert_eq!(code(&mut app, clear_with_tokens), "invalid_context_usage");

        let mut clear_with_window = clear_params(&pane);
        clear_with_window.window_tokens = Some(1);
        assert_eq!(code(&mut app, clear_with_window), "invalid_context_usage");

        let mut clear_with_time = clear_params(&pane);
        clear_with_time.observed_at_ms = Some(1);
        assert_eq!(code(&mut app, clear_with_time), "invalid_context_usage");

        let mut missing_tokens = params(&pane);
        missing_tokens.used_tokens = None;
        assert_eq!(code(&mut app, missing_tokens), "invalid_context_usage");

        let mut missing_time = params(&pane);
        missing_time.observed_at_ms = None;
        assert_eq!(code(&mut app, missing_time), "invalid_context_usage");

        let mut zero_time = params(&pane);
        zero_time.observed_at_ms = Some(0);
        assert_eq!(code(&mut app, zero_time), "invalid_context_usage");

        let mut huge_used = params(&pane);
        huge_used.used_tokens = Some(CONTEXT_MAX_TOKENS + 1);
        assert_eq!(code(&mut app, huge_used), "invalid_context_tokens");

        let mut zero_window = params(&pane);
        zero_window.window_tokens = Some(0);
        assert_eq!(code(&mut app, zero_window), "invalid_context_tokens");

        let mut huge_window = params(&pane);
        huge_window.window_tokens = Some(CONTEXT_MAX_TOKENS + 1);
        assert_eq!(code(&mut app, huge_window), "invalid_context_tokens");

        assert_eq!(fact(&app, &pane), None);
        shutdown_test_runtimes(&mut app);
    }

    #[test]
    fn context_usage_report_accepts_used_above_window_and_zero_used() {
        let (mut app, pane) = app_with_test_workspace();

        let mut over = params(&pane);
        over.used_tokens = Some(250_000);
        assert_ok(&app.handle_pane_report_context_usage("r".into(), over));
        assert_eq!(fact(&app, &pane).unwrap().used_tokens, 250_000);

        let mut zero = params(&pane);
        zero.used_tokens = Some(0);
        zero.observed_at_ms = Some(2_000_000);
        assert_ok(&app.handle_pane_report_context_usage("r".into(), zero));
        assert_eq!(fact(&app, &pane).unwrap().used_tokens, 0);
        shutdown_test_runtimes(&mut app);
    }

    #[test]
    fn context_usage_report_dispatches_through_api_request() {
        let (mut app, pane) = app_with_test_workspace();

        let response = app.handle_api_request(Request {
            id: "r".into(),
            method: Method::PaneReportContextUsage(params(&pane)),
        });

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(success.id, "r");
        assert_eq!(
            fact(&app, &pane),
            Some(info("herdr:claude", 84_000, Some(200_000), 1_000_000))
        );
        shutdown_test_runtimes(&mut app);
    }

    #[test]
    fn context_usage_merge_rule() {
        let stored = info("herdr:pi", 50_000, Some(1_000_000), 1_000);

        // Nothing stored: taken as reported, window stays absent when absent.
        assert_eq!(
            merge_context_usage_report(None, "a".into(), 10, Some(100), 500),
            Some(info("a", 10, Some(100), 500))
        );
        assert_eq!(
            merge_context_usage_report(None, "a".into(), 10, None, 500),
            Some(info("a", 10, None, 500))
        );

        // Older than the stored reading: ignored.
        assert_eq!(
            merge_context_usage_report(Some(&stored), "a".into(), 10, Some(100), 999),
            None
        );
        assert_eq!(
            merge_context_usage_report(Some(&stored), "a".into(), 10, None, 999),
            None
        );

        // Equal timestamp: accepted; a missing window keeps the stored one.
        assert_eq!(
            merge_context_usage_report(Some(&stored), "a".into(), 10, Some(100), 1_000),
            Some(info("a", 10, Some(100), 1_000))
        );
        assert_eq!(
            merge_context_usage_report(Some(&stored), "a".into(), 10, None, 1_000),
            Some(info("a", 10, Some(1_000_000), 1_000))
        );

        // Newer: same rules.
        assert_eq!(
            merge_context_usage_report(Some(&stored), "a".into(), 10, Some(100), 2_000),
            Some(info("a", 10, Some(100), 2_000))
        );
        assert_eq!(
            merge_context_usage_report(Some(&stored), "a".into(), 10, None, 2_000),
            Some(info("a", 10, Some(1_000_000), 2_000))
        );

        // A stored reading with no window stays without one.
        let windowless = info("herdr:hermes", 5, None, 1_000);
        assert_eq!(
            merge_context_usage_report(Some(&windowless), "a".into(), 6, None, 2_000),
            Some(info("a", 6, None, 2_000))
        );
    }

    #[test]
    fn context_usage_report_requests_render_only_when_fact_changes() {
        let (mut app, pane) = app_with_test_workspace();
        assert!(!crate::api::request_changes_ui(&Request {
            id: "r".into(),
            method: Method::PaneReportContextUsage(params(&pane)),
        }));
        app.render_dirty.take();

        assert_ok(&app.handle_pane_report_context_usage("r".into(), params(&pane)));
        assert!(app.render_dirty.is_pending());
        app.render_dirty.take();

        // Same report again: unchanged.
        assert_ok(&app.handle_pane_report_context_usage("r".into(), params(&pane)));
        assert!(!app.render_dirty.is_pending());

        // Older report: ignored.
        let mut older = params(&pane);
        older.observed_at_ms = Some(1);
        assert_ok(&app.handle_pane_report_context_usage("r".into(), older));
        assert!(!app.render_dirty.is_pending());

        // Clear removes the stored fact and renders; clearing nothing does not.
        assert_ok(&app.handle_pane_report_context_usage("r".into(), clear_params(&pane)));
        assert!(app.render_dirty.is_pending());
        app.render_dirty.take();
        assert_ok(&app.handle_pane_report_context_usage("r".into(), clear_params(&pane)));
        assert!(!app.render_dirty.is_pending());
        shutdown_test_runtimes(&mut app);
    }
}
