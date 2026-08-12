# Documentation Conflict Resolution: Upstream v0.8.0 Merge

## Summary

Resolved all 22 documentation and website file conflicts. Applied default strategy of taking upstream v0.8.0 content, with targeted preservation of two fork-specific federation feature keys in machine-readable contracts.

## Files Resolved by Category

### Upstream Wholesale (18 files)

No modifications to fork content—took upstream v0.8.0 directly:

**Changelogs & root docs:**
- CHANGELOG.md
- docs/next/CHANGELOG.md
- docs/next/README.md

**Markdown documentation (.mdx):**
- docs/next/website/src/content/docs/cli-reference.mdx
- docs/next/website/src/content/docs/configuration.mdx
- docs/next/website/src/content/docs/integrations.mdx
- docs/next/website/src/content/docs/troubleshooting.mdx
- docs/next/website/src/content/docs/ja/configuration.mdx
- docs/next/website/src/content/docs/ja/socket-api.mdx
- docs/next/website/src/content/docs/ja/troubleshooting.mdx
- docs/next/website/src/content/docs/zh-cn/configuration.mdx
- docs/next/website/src/content/docs/zh-cn/socket-api.mdx
- docs/next/website/src/content/docs/zh-cn/troubleshooting.mdx
- website/src/content/docs/troubleshooting.mdx
- website/src/content/docs/ja/troubleshooting.mdx
- website/src/content/docs/zh-cn/troubleshooting.mdx

**Release channel files (hand-edits forbidden by CLAUDE.md):**
- website/latest.json
- website/preview.json
- website/package.json
- website/index.html

### Integrated (2 files)

Took upstream as base, then re-added fork-specific federation keys identified in running code.

**docs/next/api/herdr-api.schema.json:**
- Took upstream v0.8.0 schema
- Re-added fork's `WorkspaceMountRemoteParams` type definition (used in `src/app/remote_mount.rs`)
- Re-added fork's `workspace.mount_remote` method variant to request schema
- Rationale: WorkspaceMountRemote is actively used in merged code; the fork's federation feature is now in master
- Validation: ✓ Valid JSON Schema

**docs/next/website/src/data/config-reference.json:**
- Took upstream v0.8.0 base
- Re-added fork's `ui.recent_remote_mount_targets` config key (Mount Recents feature, PR #9)
- Rationale: This config key is written by `src/app/config_io.rs` for the mount-recents TUI dialog
- Validation: ✓ Valid JSON

## Fork Federation Features Preserved

### API/Protocol Level (herdr-api.schema.json)

```
WorkspaceMountRemoteParams: {
  properties: {
    remote_keybindings: boolean (default: false),
    targets: array of strings
  },
  required: ["targets"]
}
```

Method: `workspace.mount_remote`

### Configuration Level (config-reference.json)

```
ui.recent_remote_mount_targets:
  type: list of strings
  default: []
  description: Most-recent-first remote-mount targets shown in the mount remote workspace dialog
```

## Localization Sync

All three translated troubleshooting and configuration files (ja/ and zh-cn/) mirror their English counterparts—upstream content only, no fork-specific text to translate.

## Validation Results

All JSON and Markdown files verified:
- ✓ docs/next/api/herdr-api.schema.json — valid JSON
- ✓ docs/next/website/src/data/config-reference.json — valid JSON
- ✓ website/*.json files — valid JSON
- ✓ All .mdx files — no conflict markers, well-formed Markdown frontmatter
- ✓ No remaining conflict markers in any assigned file

## Unresolved Questions

None. All conflicts resolved, JSON contracts validated, fork-specific keys identified and re-added based on running code evidence.

---

**Status:** DONE
**Summary:** Resolved 22 doc/website conflicts; took upstream v0.8.0 wholesale except re-added two federation schema keys and one config key actively used in merged code.
**Concerns:** None
