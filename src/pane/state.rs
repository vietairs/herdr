use std::collections::HashMap;

use crate::detect::{Agent, AgentState};

const CLAUDE_WORKING_HOLD: std::time::Duration = std::time::Duration::from_millis(1200);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookAuthority {
    pub source: String,
    pub agent_label: String,
    pub state: AgentState,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveStateChange {
    pub previous_agent_label: Option<String>,
    pub previous_known_agent: Option<Agent>,
    pub previous_state: AgentState,
    pub agent_label: Option<String>,
    pub known_agent: Option<Agent>,
    pub state: AgentState,
}

/// Observable state for a single pane.
/// This is the only part of a pane that workspace logic and tests need.
pub struct PaneState {
    pub detected_agent: Option<Agent>,
    pub fallback_state: AgentState,
    pub hook_authority: Option<HookAuthority>,
    hook_report_sequences: HashMap<String, u64>,
    pub state: AgentState,
    /// Whether the user has seen this pane since its last state change to Idle.
    /// False = "Done" (agent finished while user was in another workspace).
    pub seen: bool,
}

impl PaneState {
    pub fn new() -> Self {
        Self {
            detected_agent: None,
            fallback_state: AgentState::Unknown,
            hook_authority: None,
            hook_report_sequences: HashMap::new(),
            state: AgentState::Unknown,
            seen: true,
        }
    }

    pub fn set_detected_state(
        &mut self,
        agent: Option<Agent>,
        fallback_state: AgentState,
    ) -> Option<EffectiveStateChange> {
        let previous_agent_label = self.effective_agent_label().map(str::to_string);
        let previous_known_agent = self.effective_known_agent();
        let previous_state = self.state;
        let previous_detected_agent = self.detected_agent;
        self.detected_agent = agent;
        self.fallback_state = fallback_state;
        if previous_detected_agent.is_some()
            && agent != previous_detected_agent
            && self.hook_authority.as_ref().is_some_and(|authority| {
                crate::detect::parse_agent_label(&authority.agent_label) == previous_detected_agent
            })
        {
            self.hook_authority = None;
        }
        self.recompute_effective_state(previous_agent_label, previous_known_agent, previous_state)
    }

    pub fn set_hook_authority(
        &mut self,
        source: String,
        agent_label: String,
        state: AgentState,
        message: Option<String>,
        seq: Option<u64>,
    ) -> Option<EffectiveStateChange> {
        if !self.accept_hook_report(&source, seq) {
            return None;
        }

        let previous_agent_label = self.effective_agent_label().map(str::to_string);
        let previous_known_agent = self.effective_known_agent();
        let previous_state = self.state;
        self.hook_authority = Some(HookAuthority {
            source,
            agent_label,
            state,
            message,
        });
        self.recompute_effective_state(previous_agent_label, previous_known_agent, previous_state)
    }

    fn accept_hook_report(&mut self, source: &str, seq: Option<u64>) -> bool {
        let Some(seq) = seq else {
            return !self.hook_report_sequences.contains_key(source);
        };

        if self
            .hook_report_sequences
            .get(source)
            .is_some_and(|last_seq| seq <= *last_seq)
        {
            return false;
        }

        self.hook_report_sequences.insert(source.to_string(), seq);
        true
    }

    pub fn clear_hook_authority(
        &mut self,
        source: Option<&str>,
        seq: Option<u64>,
    ) -> Option<EffectiveStateChange> {
        let sequence_source = source.map(str::to_string).or_else(|| {
            self.hook_authority
                .as_ref()
                .map(|authority| authority.source.clone())
        });
        if let Some(source) = sequence_source.as_deref() {
            if !self.accept_hook_report(source, seq) {
                return None;
            }
        }

        let previous_agent_label = self.effective_agent_label().map(str::to_string);
        let previous_known_agent = self.effective_known_agent();
        let previous_state = self.state;
        let should_clear = self
            .hook_authority
            .as_ref()
            .is_some_and(|authority| source.is_none_or(|source| authority.source == source));
        if !should_clear {
            return None;
        }
        self.hook_authority = None;
        self.recompute_effective_state(previous_agent_label, previous_known_agent, previous_state)
    }

    pub fn release_agent(
        &mut self,
        source: &str,
        agent_label: &str,
        seq: Option<u64>,
    ) -> Option<EffectiveStateChange> {
        if !self.accept_hook_report(source, seq) {
            return None;
        }

        let current_agent_label = self.effective_agent_label()?;
        if current_agent_label != agent_label {
            return None;
        }

        if self.hook_authority.as_ref().is_some_and(|authority| {
            authority.agent_label != agent_label || authority.source != source
        }) {
            return None;
        }

        let previous_agent_label = self.effective_agent_label().map(str::to_string);
        let previous_known_agent = self.effective_known_agent();
        let previous_state = self.state;
        self.detected_agent = None;
        self.fallback_state = AgentState::Unknown;
        self.hook_authority = None;
        self.recompute_effective_state(previous_agent_label, previous_known_agent, previous_state)
    }

    pub fn effective_agent_label(&self) -> Option<&str> {
        self.hook_authority
            .as_ref()
            .map(|authority| authority.agent_label.as_str())
            .or_else(|| self.detected_agent.map(crate::detect::agent_label))
    }

    pub fn effective_known_agent(&self) -> Option<Agent> {
        if let Some(authority) = &self.hook_authority {
            return crate::detect::parse_agent_label(&authority.agent_label);
        }
        self.detected_agent
    }

    fn recompute_effective_state(
        &mut self,
        previous_agent_label: Option<String>,
        previous_known_agent: Option<Agent>,
        previous_state: AgentState,
    ) -> Option<EffectiveStateChange> {
        let state = self
            .hook_authority
            .as_ref()
            .map(|authority| authority.state)
            .unwrap_or(self.fallback_state);
        let agent_label = self.effective_agent_label().map(str::to_string);
        let known_agent = self.effective_known_agent();

        if previous_agent_label == agent_label && previous_state == state {
            return None;
        }

        self.state = state;
        Some(EffectiveStateChange {
            previous_agent_label,
            previous_known_agent,
            previous_state,
            agent_label,
            known_agent,
            state,
        })
    }
}

pub(crate) fn stabilize_agent_state(
    agent: Option<Agent>,
    previous: AgentState,
    raw: AgentState,
    now: std::time::Instant,
    last_claude_working_at: &mut Option<std::time::Instant>,
) -> AgentState {
    if agent != Some(Agent::Claude) {
        return raw;
    }

    match raw {
        AgentState::Working => {
            *last_claude_working_at = Some(now);
            AgentState::Working
        }
        AgentState::Blocked => AgentState::Blocked,
        AgentState::Idle if previous == AgentState::Working => {
            if last_claude_working_at
                .is_some_and(|last_working| now.duration_since(last_working) < CLAUDE_WORKING_HOLD)
            {
                AgentState::Working
            } else {
                AgentState::Idle
            }
        }
        _ => raw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_working_is_sticky_for_short_gap() {
        let now = std::time::Instant::now();
        let mut last_working = None;

        let working = stabilize_agent_state(
            Some(Agent::Claude),
            AgentState::Idle,
            AgentState::Working,
            now,
            &mut last_working,
        );
        assert_eq!(working, AgentState::Working);

        let still_working = stabilize_agent_state(
            Some(Agent::Claude),
            AgentState::Working,
            AgentState::Idle,
            now + std::time::Duration::from_millis(400),
            &mut last_working,
        );
        assert_eq!(still_working, AgentState::Working);
    }

    #[test]
    fn claude_transitions_to_idle_after_hold_expires() {
        let now = std::time::Instant::now();
        let mut last_working = Some(now);

        let state = stabilize_agent_state(
            Some(Agent::Claude),
            AgentState::Working,
            AgentState::Idle,
            now + CLAUDE_WORKING_HOLD + std::time::Duration::from_millis(1),
            &mut last_working,
        );
        assert_eq!(state, AgentState::Idle);
    }

    #[test]
    fn non_claude_states_are_unchanged() {
        let now = std::time::Instant::now();
        let mut last_working = None;

        let state = stabilize_agent_state(
            Some(Agent::Codex),
            AgentState::Working,
            AgentState::Idle,
            now,
            &mut last_working,
        );
        assert_eq!(state, AgentState::Idle);
    }

    #[test]
    fn hook_authority_overrides_fallback_for_same_agent() {
        let mut pane = PaneState::new();
        pane.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        pane.set_hook_authority(
            "herdr:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            None,
        );

        assert_eq!(pane.detected_agent, Some(Agent::Pi));
        assert_eq!(pane.fallback_state, AgentState::Idle);
        assert_eq!(pane.effective_agent_label(), Some("pi"));
        assert_eq!(pane.state, AgentState::Working);
    }

    #[test]
    fn hook_authority_can_override_with_unknown_agent_label() {
        let mut pane = PaneState::new();
        pane.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        pane.set_hook_authority(
            "herdr:custom".into(),
            "hermes".into(),
            AgentState::Working,
            None,
            None,
        );

        assert_eq!(pane.detected_agent, Some(Agent::Pi));
        assert_eq!(pane.effective_agent_label(), Some("hermes"));
        assert_eq!(pane.effective_known_agent(), None);
        assert_eq!(pane.state, AgentState::Working);
    }

    #[test]
    fn hook_authority_survives_unrelated_detected_agent_clear() {
        let mut pane = PaneState::new();
        pane.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        pane.set_hook_authority(
            "herdr:custom".into(),
            "hermes".into(),
            AgentState::Working,
            None,
            None,
        );

        pane.set_detected_state(None, AgentState::Unknown);

        assert!(pane.hook_authority.is_some());
        assert_eq!(pane.detected_agent, None);
        assert_eq!(pane.effective_agent_label(), Some("hermes"));
        assert_eq!(pane.state, AgentState::Working);
    }

    #[test]
    fn detected_agent_clear_clears_matching_hook_authority() {
        let mut pane = PaneState::new();
        pane.set_detected_state(Some(Agent::OpenCode), AgentState::Idle);
        pane.set_hook_authority(
            "herdr:opencode".into(),
            "opencode".into(),
            AgentState::Idle,
            None,
            None,
        );

        pane.set_detected_state(None, AgentState::Unknown);

        assert!(pane.hook_authority.is_none());
        assert_eq!(pane.detected_agent, None);
        assert_eq!(pane.fallback_state, AgentState::Unknown);
        assert_eq!(pane.effective_agent_label(), None);
        assert_eq!(pane.state, AgentState::Unknown);
    }

    #[test]
    fn detected_agent_change_clears_previous_matching_hook_authority() {
        let mut pane = PaneState::new();
        pane.set_detected_state(Some(Agent::Codex), AgentState::Idle);
        pane.set_hook_authority(
            "herdr:codex".into(),
            "codex".into(),
            AgentState::Idle,
            None,
            None,
        );

        pane.set_detected_state(Some(Agent::OpenCode), AgentState::Working);

        assert!(pane.hook_authority.is_none());
        assert_eq!(pane.detected_agent, Some(Agent::OpenCode));
        assert_eq!(pane.effective_agent_label(), Some("opencode"));
        assert_eq!(pane.state, AgentState::Working);
    }

    #[test]
    fn release_agent_clears_identity_immediately() {
        let mut pane = PaneState::new();
        pane.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        pane.set_hook_authority(
            "herdr:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            None,
        );

        pane.release_agent("herdr:pi", "pi", None);

        assert!(pane.hook_authority.is_none());
        assert_eq!(pane.detected_agent, None);
        assert_eq!(pane.fallback_state, AgentState::Unknown);
        assert_eq!(pane.state, AgentState::Unknown);
    }

    #[test]
    fn stale_hook_report_sequence_is_ignored_for_same_source() {
        let mut pane = PaneState::new();
        pane.set_hook_authority(
            "herdr:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            Some(20),
        );

        let change = pane.set_hook_authority(
            "herdr:pi".into(),
            "pi".into(),
            AgentState::Idle,
            None,
            Some(19),
        );

        assert!(change.is_none());
        assert_eq!(pane.state, AgentState::Working);
        assert_eq!(
            pane.hook_authority.as_ref().unwrap().state,
            AgentState::Working
        );
    }

    #[test]
    fn unsequenced_hook_report_is_ignored_after_source_uses_sequence() {
        let mut pane = PaneState::new();
        pane.set_hook_authority(
            "herdr:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            Some(20),
        );

        let change =
            pane.set_hook_authority("herdr:pi".into(), "pi".into(), AgentState::Idle, None, None);

        assert!(change.is_none());
        assert_eq!(pane.state, AgentState::Working);
    }

    #[test]
    fn stale_release_sequence_is_ignored_for_same_source() {
        let mut pane = PaneState::new();
        pane.set_hook_authority(
            "herdr:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            Some(20),
        );

        let change = pane.release_agent("herdr:pi", "pi", Some(19));

        assert!(change.is_none());
        assert_eq!(pane.state, AgentState::Working);
        assert!(pane.hook_authority.is_some());
    }

    #[test]
    fn stale_clear_all_sequence_is_checked_against_current_authority_source() {
        let mut pane = PaneState::new();
        pane.set_hook_authority(
            "herdr:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            Some(20),
        );

        let change = pane.clear_hook_authority(None, Some(19));

        assert!(change.is_none());
        assert_eq!(pane.state, AgentState::Working);
        assert!(pane.hook_authority.is_some());
    }

    #[test]
    fn same_sequence_from_different_sources_is_independent() {
        let mut pane = PaneState::new();
        pane.set_hook_authority(
            "herdr:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            Some(20),
        );

        pane.set_hook_authority(
            "custom:pi".into(),
            "pi".into(),
            AgentState::Idle,
            None,
            Some(19),
        );

        assert_eq!(pane.state, AgentState::Idle);
        assert_eq!(pane.hook_authority.as_ref().unwrap().source, "custom:pi");
    }
}
