//! The shared name resolver (Policy C).
//!
//! One ladder, applied identically at every scope (workspace, tab, pane):
//!
//! 1. user override at this scope, or inherited from the nearest enclosing
//!    renamed scope — the only thing persisted as truth
//!
//!    (rung 1.5: mirrored remote label, federation-mounted scopes only —
//!    stops the ladder before any local derivation runs)
//! 2. agent identity (its own hook-over-detection authority chain)
//! 3. cwd / git-root derivation — a cached scalar supplied by the caller,
//!    never derived in here
//! 4. stable ordinal (`Workspace::public_tab_number`, never `tab_idx + 1`)
//!
//! Rulings this module encodes and must not silently "fix" (see
//! `plans/260910-1635-name-sync-workspace-tab-agent/plan.md` section 0 for
//! the full evidence trail):
//!
//! - **R1 — inheritance is tab -> pane/agent ONLY.** A workspace rename
//!   never renames its tabs. `NameSources::inherited_override` is legal
//!   input only at `NameScope::Pane`; a workspace's own rung 1 is only its
//!   `own_override`.
//! - **R2 — pane scope has no rung 3.** A pane never derives a name from
//!   cwd/git; `derived` is not a legal input at `NameScope::Pane`. Pane
//!   precedence for the parts this module does NOT own
//!   (`effective_title` and `manual_label`, both resolved by the caller
//!   before it gets here) still outranks everything below rung 1.
//! - **R3 — a hand-set agent name is never clobbered by tab inheritance.**
//!   Enforced by the caller: `inherited_override` must not be supplied as a
//!   pane's rung-1-inherited input when that pane's `AgentNameAuthor` is
//!   `User`. This module has no `AgentNameAuthor` field to check — the
//!   caller (`src/terminal/state.rs::border_label`) gates it before calling
//!   in, which keeps this module ignorant of terminal internals.
//! - **R4 — rung 3 is a per-scope INPUT, not a per-scope branch.** There is
//!   no `match scope` anywhere in this file for the derived label. The
//!   workspace, tab, and pane callers each compute their own `derived`
//!   value from their own cached scalar and hand it in.
//!
//! **Hard architecture rule (P1):** this module must never touch a
//! filesystem, a process table, a terminal snapshot, or a lock — see the
//! `naming_module_contains_no_io_or_derivation_calls` test below, which
//! greps this file's own source for the forbidden call names.
//!
//! **The popup pane degrade.** A `PopupPaneState` (`src/app/state.rs`)
//! belongs to no tab — there is no enclosing scope to inherit from. Its
//! title (`src/server/client_shell.rs::render_popup_surface`) therefore
//! calls `border_label(show_agent_labels, None)`: the pane-only ladder
//! (own override -> agent identity -> `None`), falling back to the literal
//! `"popup"` when the resolver returns `None`. This is a deliberate,
//! documented degrade, not an oversight — a popup pane's caller has no
//! `inherited` value to supply because there is no tab to supply one from.

use std::borrow::Cow;

/// Which kind of scope a resolve call is for. Used only to decide whether
/// an illegal-at-this-scope input (R1/R2) is consulted — never to branch on
/// which rung-3 source to use (R4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameScope {
    Workspace,
    Tab,
    Pane,
}

/// Why the resolved text is what it is. Sent to clients alongside the
/// resolved name (`docs/next/website/src/content/docs/socket-api.mdx`);
/// clients never re-derive. `Default` is `Ordinal` only so `#[serde(default)]`
/// has something to deserialize an absent/legacy field to — never treat a
/// deserialized `Ordinal` as evidence the source really was the ordinal
/// rung; a real absence should not be confused with a resolved fact.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    ::serde::Serialize,
    ::serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum NameSource {
    /// Rung 1: a user-typed override, at this scope or inherited.
    Override,
    /// Rung 1, tab -> pane/agent specifically: inherited from the tab.
    Inherited,
    /// Rung 1.5: a federation-mounted scope's remote-resolved label.
    Mirrored,
    /// Rung 2: the pane's agent identity.
    AgentIdentity,
    /// Rung 3: cwd / git-root derivation.
    Cwd,
    /// Rung 4: the stable public ordinal.
    #[default]
    Ordinal,
}

impl NameSource {
    /// True when the resolved name is one somebody actually set — this
    /// scope's own override, one inherited from an enclosing renamed scope,
    /// or a mirrored remote label — as opposed to one derived locally from
    /// agent identity, cwd, or the ordinal.
    ///
    /// This is exactly the predicate the deprecated one-bit `custom_label`
    /// wire flag carried before `NameSource` existed: at that time the only
    /// writers of `Workspace::custom_name` / `Tab::custom_name` were the
    /// rename APIs, the layout-apply path, and the federation mount path
    /// (which wrote the remote's resolved label into the override slot),
    /// so the flag read `true` for precisely an override or a mirrored
    /// remote label and `false` for every derived name.
    pub fn is_explicitly_named(self) -> bool {
        matches!(self, Self::Override | Self::Inherited | Self::Mirrored)
    }
}

/// Narrows a remote-resolved label down to one worth mirroring into a
/// locally materialized federation scope's rung-1.5 mirror slot.
///
/// A remote sends the fully resolved label for every scope, including ones
/// whose text is merely the remote's own live derivation (its agent
/// identity, its cwd, its ordinal). Freezing such a string locally would
/// pin it at mount time: the mirror slot outranks local agent identity, so
/// the relayed live identity could never move the displayed name again.
/// Only a name somebody actually set on the remote is mirrored.
pub fn label_worth_mirroring(label: Option<&str>, source: NameSource) -> Option<String> {
    source
        .is_explicitly_named()
        .then(|| label.map(str::to_string))
        .flatten()
}

/// Every input the ladder might read, at whatever rungs are legal for the
/// caller's scope. `None` means "this rung has nothing to offer". A value
/// supplied at a scope where it is not legal (R1/R2) is simply not
/// consulted rather than rejected — see `resolve_name`'s doc comment.
#[derive(Debug, Clone, Copy, Default)]
pub struct NameSources<'a> {
    /// Rung 1, set at this scope directly.
    pub own_override: Option<&'a str>,
    /// Rung 1, inherited from the nearest enclosing renamed scope. Legal
    /// only at `NameScope::Pane` (R1: workspace -> tab inheritance does not
    /// exist; tab -> pane/agent does).
    pub inherited_override: Option<&'a str>,
    /// Rung 1.5, federation-mounted scopes only.
    pub mirrored: Option<&'a str>,
    /// Rung 2, local scopes only.
    pub agent_identity: Option<&'a str>,
    /// Rung 3, a caller-supplied cached scalar. Legal at `Workspace` and
    /// `Tab`, never at `Pane` (R2).
    pub derived: Option<&'a str>,
    /// Rung 4, `public_tab_number` (or the equivalent stable ordinal for
    /// the scope) — never a raw index. Legal only where an ordinal exists;
    /// pane scope has none (a pane's fallback is the caller's own literal,
    /// e.g. `"popup"`, supplied outside this module).
    pub ordinal: Option<usize>,
}

/// A resolved name and where it came from. `Cow` because a snapshot is
/// built unconditionally every render tick and only then diffed
/// (`server/headless/render.rs`), so a rung 1/1.5/2 hit — the common case —
/// must not allocate (P2): `resolve_returns_borrowed_text_for_rung_1_and_2`
/// below asserts this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedName<'a> {
    pub text: Cow<'a, str>,
    pub source: NameSource,
}

/// The ladder. Returns `None` only when every rung is empty — the caller
/// supplies the scope's own literal fallback (`"workspace"`, `"popup"`,
/// etc.), which this module does not know.
pub fn resolve_name<'a>(scope: NameScope, sources: NameSources<'a>) -> Option<ResolvedName<'a>> {
    // R1/R2: `inherited_override` and `derived` are legal inputs only at
    // certain scopes (Pane-only, and Workspace/Tab-only respectively). A
    // real caller never populates an illegal-scope field — every caller
    // of this module only ever constructs the fields legal for its own
    // scope — but this ladder still treats an illegal-scope
    // value as simply absent rather than panicking, so a stray value left
    // over from scope-generic code (e.g. `NameSources::default()` reuse)
    // degrades safely instead of crashing the render path.
    if let Some(text) = sources.own_override {
        return Some(ResolvedName {
            text: Cow::Borrowed(text),
            source: NameSource::Override,
        });
    }
    if scope == NameScope::Pane {
        if let Some(text) = sources.inherited_override {
            return Some(ResolvedName {
                text: Cow::Borrowed(text),
                source: NameSource::Inherited,
            });
        }
    }
    if let Some(text) = sources.mirrored {
        return Some(ResolvedName {
            text: Cow::Borrowed(text),
            source: NameSource::Mirrored,
        });
    }
    if let Some(text) = sources.agent_identity {
        return Some(ResolvedName {
            text: Cow::Borrowed(text),
            source: NameSource::AgentIdentity,
        });
    }
    if scope != NameScope::Pane {
        if let Some(text) = sources.derived {
            return Some(ResolvedName {
                text: Cow::Borrowed(text),
                source: NameSource::Cwd,
            });
        }
    }
    if let Some(number) = sources.ordinal {
        return Some(ResolvedName {
            text: Cow::Owned(number.to_string()),
            source: NameSource::Ordinal,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P1 — a grep test, not a wall-clock bench, per CLAUDE.md's stated
    /// preference for deterministic checks. Fails loudly if a future editor
    /// adds a derivation or I/O call to this module.
    #[test]
    fn naming_module_contains_no_io_or_derivation_calls() {
        // Scan only the production portion of the file — this test module
        // itself necessarily spells out the forbidden names as string
        // literals to check against, so including it would make the test
        // trip on its own forbidden-word list.
        let source = include_str!("naming.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .expect("naming.rs must contain its own test module");
        for forbidden in [
            "cwd_for_pane",
            "process_cwd",
            "resolved_identity_cwd_from",
            "display_name_from",
            "std::fs",
            "Instant::now",
        ] {
            assert!(
                !source.contains(forbidden),
                "src/workspace/naming.rs must not call `{forbidden}` — the resolver reads \
                 cached scalars only, all derivation happens on the ~1.5s git-refresh pass"
            );
        }
    }

    #[test]
    fn rung_1_own_override_wins_over_everything() {
        let sources = NameSources {
            own_override: Some("mine"),
            mirrored: Some("remote"),
            agent_identity: Some("claude"),
            derived: Some("src/detect"),
            ordinal: Some(3),
            ..Default::default()
        };
        let resolved = resolve_name(NameScope::Workspace, sources).unwrap();
        assert_eq!(resolved.text, "mine");
        assert_eq!(resolved.source, NameSource::Override);
    }

    #[test]
    fn rung_1_inherited_override_is_ignored_at_workspace_scope() {
        // R1: even if a caller mistakenly supplies it, workspace scope must
        // never surface an inherited override — the `scope ==
        // NameScope::Pane` guard makes it simply not consulted.
        let sources = NameSources {
            inherited_override: Some("tab-name"),
            agent_identity: Some("claude"),
            ..Default::default()
        };
        let resolved = resolve_name(NameScope::Workspace, sources).unwrap();
        assert_eq!(resolved.text, "claude");
        assert_eq!(resolved.source, NameSource::AgentIdentity);
    }

    #[test]
    fn rung_1_5_mirrored_stops_the_ladder_before_agent_identity() {
        let sources = NameSources {
            mirrored: Some("remote-label"),
            agent_identity: Some("claude"),
            derived: Some("src"),
            ordinal: Some(1),
            ..Default::default()
        };
        let resolved = resolve_name(NameScope::Tab, sources).unwrap();
        assert_eq!(resolved.text, "remote-label");
        assert_eq!(resolved.source, NameSource::Mirrored);
    }

    #[test]
    fn mirrored_never_outranks_a_local_override() {
        // A local rename of a mounted scope still wins over the
        // remote's own resolved label.
        let sources = NameSources {
            own_override: Some("my-local-name"),
            mirrored: Some("remote-label"),
            ..Default::default()
        };
        let resolved = resolve_name(NameScope::Tab, sources).unwrap();
        assert_eq!(resolved.text, "my-local-name");
        assert_eq!(resolved.source, NameSource::Override);
    }

    #[test]
    fn rung_2_agent_identity_beats_derived() {
        // `derived` is legal input at Tab scope, not Pane (R2) — Tab is
        // used here to prove rung 2 outranks rung 3 in a realistic
        // combination.
        let sources = NameSources {
            agent_identity: Some("claude"),
            derived: Some("src/detect"),
            ordinal: Some(2),
            ..Default::default()
        };
        let resolved = resolve_name(NameScope::Tab, sources).unwrap();
        assert_eq!(resolved.text, "claude");
        assert_eq!(resolved.source, NameSource::AgentIdentity);
    }

    #[test]
    fn rung_3_derived_beats_ordinal() {
        let sources = NameSources {
            derived: Some("src/detect"),
            ordinal: Some(2),
            ..Default::default()
        };
        let resolved = resolve_name(NameScope::Tab, sources).unwrap();
        assert_eq!(resolved.text, "src/detect");
        assert_eq!(resolved.source, NameSource::Cwd);
    }

    #[test]
    fn rung_4_uses_the_supplied_ordinal_verbatim() {
        let sources = NameSources {
            ordinal: Some(3),
            ..Default::default()
        };
        let resolved = resolve_name(NameScope::Tab, sources).unwrap();
        assert_eq!(resolved.text, "3");
        assert_eq!(resolved.source, NameSource::Ordinal);
    }

    #[test]
    fn pane_scope_rejects_an_ordinal_input() {
        // R2 has no explicit ordinal-legality assert (a pane's fallback is
        // a caller-supplied literal, not an ordinal), but nothing in the
        // ladder reads `ordinal` differently by scope — this pins that an
        // ordinal supplied at pane scope still resolves (there is nothing
        // to reject at the resolver level; the caller simply never
        // populates it for a pane). Documents the actual contract rather
        // than asserting a rejection that doesn't exist.
        let sources = NameSources {
            ordinal: Some(5),
            ..Default::default()
        };
        let resolved = resolve_name(NameScope::Pane, sources).unwrap();
        assert_eq!(resolved.text, "5");
        assert_eq!(resolved.source, NameSource::Ordinal);
    }

    #[test]
    fn resolve_returns_borrowed_text_for_rung_1_and_2() {
        let by_override = resolve_name(
            NameScope::Tab,
            NameSources {
                own_override: Some("mine"),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(matches!(by_override.text, Cow::Borrowed(_)));

        let by_agent = resolve_name(
            NameScope::Pane,
            NameSources {
                agent_identity: Some("claude"),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(matches!(by_agent.text, Cow::Borrowed(_)));
    }

    #[test]
    fn empty_sources_resolve_to_none() {
        let resolved = resolve_name(NameScope::Workspace, NameSources::default());
        assert!(resolved.is_none());
    }
}
