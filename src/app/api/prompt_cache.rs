//! Per-pane prompt-cache fact: the last model request that touched the provider's
//! prompt cache and how long that cache lives. Integrations report it; the
//! server stores it on the terminal and exposes it on pane and agent info.

use crate::api::schema::{PaneReportPromptCacheParams, PromptCacheInfo, ResponseResult};
use crate::app::App;

use super::super::api_helpers::normalize_metadata_source;
use super::panes::pane_not_found;
use super::responses::{encode_error, encode_success};

/// Cache lifetime assumed when a reporter has never stated one.
pub(crate) const PROMPT_CACHE_DEFAULT_TTL_SECS: u32 = 300;
/// Longest cache lifetime a reporter may state (24 hours).
pub(crate) const PROMPT_CACHE_MAX_TTL_SECS: u32 = 86_400;

/// Pure merge of one report into the stored fact.
///
/// `None` means the report is ignored because it is older than the stored
/// request. A missing ttl keeps the stored ttl, else the default. Any source may
/// replace another source's value: the most recent request wins.
pub(super) fn merge_prompt_cache_report(
    current: Option<&PromptCacheInfo>,
    source: String,
    last_request_at_ms: u64,
    ttl_secs: Option<u32>,
) -> Option<PromptCacheInfo> {
    if current.is_some_and(|stored| last_request_at_ms < stored.last_request_at_ms) {
        return None;
    }
    let ttl_secs = ttl_secs
        .or_else(|| current.map(|stored| stored.ttl_secs))
        .unwrap_or(PROMPT_CACHE_DEFAULT_TTL_SECS);
    Some(PromptCacheInfo {
        source,
        last_request_at_ms,
        ttl_secs,
    })
}

impl App {
    pub(super) fn handle_pane_report_prompt_cache(
        &mut self,
        id: String,
        params: PaneReportPromptCacheParams,
    ) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let source = match normalize_metadata_source(params.source) {
            Ok(source) => source,
            Err(message) => return encode_error(id, "invalid_metadata_source", message),
        };
        let request_shape_valid = if params.clear {
            params.last_request_at_ms.is_none() && params.ttl_secs.is_none()
        } else {
            params.last_request_at_ms.is_some_and(|at| at > 0)
        };
        if !request_shape_valid {
            return encode_error(
                id,
                "invalid_prompt_cache",
                "set last_request_at_ms or clear, not both",
            );
        }
        if params
            .ttl_secs
            .is_some_and(|ttl| ttl == 0 || ttl > PROMPT_CACHE_MAX_TTL_SECS)
        {
            return encode_error(
                id,
                "invalid_prompt_cache_ttl",
                format!("ttl_secs must be between 1 and {PROMPT_CACHE_MAX_TTL_SECS}"),
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

        let changed = match params.last_request_at_ms {
            None => terminal.prompt_cache.take().is_some(),
            Some(last_request_at_ms) => {
                match merge_prompt_cache_report(
                    terminal.prompt_cache.as_ref(),
                    source,
                    last_request_at_ms,
                    params.ttl_secs,
                ) {
                    Some(next) if terminal.prompt_cache.as_ref() != Some(&next) => {
                        terminal.prompt_cache = Some(next);
                        true
                    }
                    _ => false,
                }
            }
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
pub(super) mod tests {
    use super::super::test_support::shutdown_test_runtimes;
    use super::*;
    use crate::{
        api::schema::{ErrorResponse, Method, Request, SuccessResponse},
        config::Config,
        workspace::Workspace,
    };

    pub(in crate::app::api) fn app_with_test_workspace() -> (App, String) {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("prompt-cache")];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();
        (app, public_pane_id)
    }

    fn params(pane_id: &str) -> PaneReportPromptCacheParams {
        PaneReportPromptCacheParams {
            pane_id: pane_id.to_string(),
            source: "herdr:claude".to_string(),
            last_request_at_ms: Some(1_000_000),
            ttl_secs: Some(300),
            clear: false,
        }
    }

    fn fact(app: &App, pane_id: &str) -> Option<PromptCacheInfo> {
        let (ws_idx, pane) = app.parse_pane_id(pane_id).unwrap();
        app.pane_info(ws_idx, pane).unwrap().prompt_cache
    }

    fn revision(app: &App, pane_id: &str) -> u64 {
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

    fn info(source: &str, at: u64, ttl: u32) -> PromptCacheInfo {
        PromptCacheInfo {
            source: source.to_string(),
            last_request_at_ms: at,
            ttl_secs: ttl,
        }
    }

    #[test]
    fn prompt_cache_report_sets_fact_and_bumps_revision() {
        let (mut app, pane) = app_with_test_workspace();
        let before = revision(&app, &pane);

        let response = app.handle_pane_report_prompt_cache("r".into(), params(&pane));

        assert_ok(&response);
        assert_eq!(
            fact(&app, &pane),
            Some(info("herdr:claude", 1_000_000, 300))
        );
        assert_eq!(revision(&app, &pane), before + 1);
        shutdown_test_runtimes(&mut app);
    }

    #[test]
    fn prompt_cache_report_missing_ttl_keeps_previous_then_defaults_to_300() {
        let (mut app, pane) = app_with_test_workspace();

        let mut report = params(&pane);
        report.ttl_secs = None;
        assert_ok(&app.handle_pane_report_prompt_cache("r".into(), report));
        assert_eq!(fact(&app, &pane).unwrap().ttl_secs, 300);

        let mut report = params(&pane);
        report.last_request_at_ms = Some(2_000_000);
        report.ttl_secs = Some(3600);
        assert_ok(&app.handle_pane_report_prompt_cache("r".into(), report));
        assert_eq!(fact(&app, &pane).unwrap().ttl_secs, 3600);

        let mut report = params(&pane);
        report.last_request_at_ms = Some(3_000_000);
        report.ttl_secs = None;
        assert_ok(&app.handle_pane_report_prompt_cache("r".into(), report));
        let stored = fact(&app, &pane).unwrap();
        assert_eq!(stored.ttl_secs, 3600);
        assert_eq!(stored.last_request_at_ms, 3_000_000);
        shutdown_test_runtimes(&mut app);
    }

    #[test]
    fn prompt_cache_report_ignores_older_request_without_revision_bump() {
        let (mut app, pane) = app_with_test_workspace();
        assert_ok(&app.handle_pane_report_prompt_cache("r".into(), params(&pane)));
        let before = revision(&app, &pane);

        let mut older = params(&pane);
        older.last_request_at_ms = Some(999_999);
        older.ttl_secs = Some(3600);
        assert_ok(&app.handle_pane_report_prompt_cache("r".into(), older));

        assert_eq!(
            fact(&app, &pane),
            Some(info("herdr:claude", 1_000_000, 300))
        );
        assert_eq!(revision(&app, &pane), before);
        shutdown_test_runtimes(&mut app);
    }

    #[test]
    fn prompt_cache_report_clear_removes_fact_and_is_idempotent() {
        let (mut app, pane) = app_with_test_workspace();
        assert_ok(&app.handle_pane_report_prompt_cache("r".into(), params(&pane)));
        let after_set = revision(&app, &pane);

        let clear = PaneReportPromptCacheParams {
            pane_id: pane.clone(),
            source: "herdr:claude".to_string(),
            last_request_at_ms: None,
            ttl_secs: None,
            clear: true,
        };
        assert_ok(&app.handle_pane_report_prompt_cache("r".into(), clear.clone()));
        assert_eq!(fact(&app, &pane), None);
        assert_eq!(revision(&app, &pane), after_set + 1);

        assert_ok(&app.handle_pane_report_prompt_cache("r".into(), clear));
        assert_eq!(fact(&app, &pane), None);
        assert_eq!(revision(&app, &pane), after_set + 1);
        shutdown_test_runtimes(&mut app);
    }

    #[test]
    fn prompt_cache_report_rejects_invalid_params() {
        let (mut app, pane) = app_with_test_workspace();

        let unknown_pane = params("p_missing");
        assert_eq!(
            error_code(&app.handle_pane_report_prompt_cache("r".into(), unknown_pane)),
            "pane_not_found"
        );

        let mut bad_source = params(&pane);
        bad_source.source = "bad source!".to_string();
        assert_eq!(
            error_code(&app.handle_pane_report_prompt_cache("r".into(), bad_source)),
            "invalid_metadata_source"
        );

        let mut clear_with_time = params(&pane);
        clear_with_time.clear = true;
        assert_eq!(
            error_code(&app.handle_pane_report_prompt_cache("r".into(), clear_with_time)),
            "invalid_prompt_cache"
        );

        let mut clear_with_ttl = params(&pane);
        clear_with_ttl.clear = true;
        clear_with_ttl.last_request_at_ms = None;
        assert_eq!(
            error_code(&app.handle_pane_report_prompt_cache("r".into(), clear_with_ttl)),
            "invalid_prompt_cache"
        );

        let mut missing_time = params(&pane);
        missing_time.last_request_at_ms = None;
        assert_eq!(
            error_code(&app.handle_pane_report_prompt_cache("r".into(), missing_time)),
            "invalid_prompt_cache"
        );

        let mut zero_time = params(&pane);
        zero_time.last_request_at_ms = Some(0);
        assert_eq!(
            error_code(&app.handle_pane_report_prompt_cache("r".into(), zero_time)),
            "invalid_prompt_cache"
        );

        let mut zero_ttl = params(&pane);
        zero_ttl.ttl_secs = Some(0);
        assert_eq!(
            error_code(&app.handle_pane_report_prompt_cache("r".into(), zero_ttl)),
            "invalid_prompt_cache_ttl"
        );

        let mut huge_ttl = params(&pane);
        huge_ttl.ttl_secs = Some(PROMPT_CACHE_MAX_TTL_SECS + 1);
        assert_eq!(
            error_code(&app.handle_pane_report_prompt_cache("r".into(), huge_ttl)),
            "invalid_prompt_cache_ttl"
        );

        assert_eq!(fact(&app, &pane), None);
        shutdown_test_runtimes(&mut app);
    }

    #[test]
    fn prompt_cache_report_dispatches_through_api_request() {
        let (mut app, pane) = app_with_test_workspace();

        let response = app.handle_api_request(Request {
            id: "r".into(),
            method: Method::PaneReportPromptCache(params(&pane)),
        });

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(success.id, "r");
        assert_eq!(
            fact(&app, &pane),
            Some(info("herdr:claude", 1_000_000, 300))
        );
        shutdown_test_runtimes(&mut app);
    }

    #[test]
    fn prompt_cache_merge_rule() {
        let stored = info("herdr:pi", 1_000, 3600);

        // Nothing stored: the report is taken, a missing ttl falls back to the default.
        assert_eq!(
            merge_prompt_cache_report(None, "a".into(), 500, Some(60)),
            Some(info("a", 500, 60))
        );
        assert_eq!(
            merge_prompt_cache_report(None, "a".into(), 500, None),
            Some(info("a", 500, PROMPT_CACHE_DEFAULT_TTL_SECS))
        );

        // Older than the stored request: ignored.
        assert_eq!(
            merge_prompt_cache_report(Some(&stored), "a".into(), 999, Some(60)),
            None
        );
        assert_eq!(
            merge_prompt_cache_report(Some(&stored), "a".into(), 999, None),
            None
        );

        // Equal timestamp: accepted; any source may replace another's value.
        assert_eq!(
            merge_prompt_cache_report(Some(&stored), "a".into(), 1_000, Some(60)),
            Some(info("a", 1_000, 60))
        );
        assert_eq!(
            merge_prompt_cache_report(Some(&stored), "a".into(), 1_000, None),
            Some(info("a", 1_000, 3600))
        );

        // Newer: accepted, a missing ttl keeps the stored one.
        assert_eq!(
            merge_prompt_cache_report(Some(&stored), "a".into(), 2_000, Some(60)),
            Some(info("a", 2_000, 60))
        );
        assert_eq!(
            merge_prompt_cache_report(Some(&stored), "a".into(), 2_000, None),
            Some(info("a", 2_000, 3600))
        );
    }

    #[test]
    fn prompt_cache_report_requests_render_only_when_fact_changes() {
        let (mut app, pane) = app_with_test_workspace();
        assert!(!crate::api::request_changes_ui(&Request {
            id: "r".into(),
            method: Method::PaneReportPromptCache(params(&pane)),
        }));
        app.render_dirty.take();

        assert_ok(&app.handle_pane_report_prompt_cache("r".into(), params(&pane)));
        assert!(app.render_dirty.is_pending());
        app.render_dirty.take();

        // Same report again: unchanged.
        assert_ok(&app.handle_pane_report_prompt_cache("r".into(), params(&pane)));
        assert!(!app.render_dirty.is_pending());

        // Older report: ignored.
        let mut older = params(&pane);
        older.last_request_at_ms = Some(1);
        assert_ok(&app.handle_pane_report_prompt_cache("r".into(), older));
        assert!(!app.render_dirty.is_pending());

        // Clear removes the stored fact and renders; clearing nothing does not.
        let clear = PaneReportPromptCacheParams {
            pane_id: pane.clone(),
            source: "herdr:claude".to_string(),
            last_request_at_ms: None,
            ttl_secs: None,
            clear: true,
        };
        assert_ok(&app.handle_pane_report_prompt_cache("r".into(), clear.clone()));
        assert!(app.render_dirty.is_pending());
        app.render_dirty.take();
        assert_ok(&app.handle_pane_report_prompt_cache("r".into(), clear));
        assert!(!app.render_dirty.is_pending());
        shutdown_test_runtimes(&mut app);
    }
}
