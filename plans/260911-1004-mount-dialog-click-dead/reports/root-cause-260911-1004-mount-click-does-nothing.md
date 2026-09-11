# Root cause: the mount button did nothing

Branch `fix/mount-dialog-click`, based on `origin/master` 95e61cb2.

## Symptom

Clicking `⏎mount` (or pressing Enter) in the "mount remote workspace" dialog produced no visible
effect. The dialog stayed open, showed no error, and no mount was attempted.

## Cause

`workspace.mount_remote` was missing from `CLIENT_SHELL_METHODS`
(`src/server/client_commands.rs`), the list of methods the server advertises on, and accepts
through, the client-shell endpoint lane.

That list gates the submit twice:

- Client side, `push_endpoint_method_with_kind` (`src/client/shell/actions.rs:392`) calls
  `supports_endpoint_method`; an unadvertised method is dropped before a request is ever queued.
- Server side, `handle_client_shell_endpoint_request`
  (`src/server/headless/endpoint_requests.rs:18`) rejects it as `unsupported_endpoint_command`.

So the request never left the client, which is why neither log recorded anything.

## Why it regressed

Commit 5010c8ac (2026-09-09) rebuilt the dialog onto the server-owned runtime boundary, moving it
off the deleted app-side path (`src/app/remote_mount.rs`, removed by the v0.9.0 merge 7bb4f039) and
onto the client-shell endpoint lane. The lane's method list was never updated to admit the method
the rebuilt dialog now sends.

Nothing caught it: `advertised_client_shell_methods_are_sorted_unique_and_in_schema` only checks
that advertised methods exist in the schema, never that a method the client shell actually sends is
advertised. An omission is invisible to it in exactly this direction.

## Why it looked completely inert

The client-side rejection reported itself through an endpoint notice (a toast). The mount dialog is
a modal drawn after the toast in `composition.rs`, so it covered the only feedback that existed.
The dialog's own `error` field was left unset, even though `handle_remote_mount_endpoint_result`
already uses that field for *server*-side rejections.

## Evidence

- Server log has 261 mount lines historically, so mount does log on both success and failure; the
  absence of any line at the click time is meaningful, not merely uninformative.
- Server log between 00:00Z and 00:10Z (the click was ~00:04Z) contains only workspace focus and
  session saves.
- `herdr-client.log` contains zero occurrences of "mount" across its whole history.
- Adding an assertion that `supports_client_shell_method(&Method::WorkspaceMountRemote(..))` holds
  failed against unmodified `origin/master`, which pins the cause mechanically.

## Three more methods were missing the same way

Auditing every method the client shell pushes against the advertised list found three more
omissions of the identical kind, all reachable from the context menu and all silently dead:

- `workspace.close_remote` and `tab.close_remote` -- the "Close on host" rows.
- `layout.balance` -- the "Balance splits" row.

Each has a live server handler (`src/app/api.rs:1291`, `:1319`, `:1363`), so the only thing
stopping them was the lane's allow-list. `workspace.list` also appears in `src/client/` but only
inside a `#[cfg(test)]` block, so it is correctly absent.

## Fix

1. `src/server/client_commands.rs` — advertise `workspace.mount_remote` on the client-shell lane.
2. `tests/fixtures/endpoint-method-shapes-v1.json` — record the new method's shape digest. Exactly
   one key added; no existing digest changed.
3. `src/client/shell/remote_mount.rs` — when the active server does not advertise the method,
   set the dialog's own inline error instead of relying on a toast the modal hides.
4. The same three additions for `layout.balance`, `tab.close_remote` and `workspace.close_remote`.

## Tests

Three regression tests, each mutation-checked (the mutation named in each comment was applied and
the test failed):

- `the_real_server_method_list_admits_a_mount_submit` builds its fixture from
  `supported_client_shell_method_names()` rather than a hand-written list, so a future omission
  from the real list fails the test instead of passing through it.
- `an_unsupported_server_rejects_the_submit_inline_instead_of_silently` covers the visibility gap.
- The lane-policy test now asserts mount is admitted alongside its existing exclusions.

`scripts/test_client_shell_method_advertisement.py` closes the class rather than the instance: it
reads every `Method::` the non-test client-shell source sends and asserts each one is advertised.
Removing any of the four fixed methods makes it fail. It follows the existing static-analysis
convention of `scripts/test_ui_hot_path_architecture.py` and is wired into `just maintenance-test`.

## Unresolved

- Not yet exercised against a live remote. The fix is verified by tests and by the mechanically
  pinned cause, not by an end-to-end mount.
- Both the local and the remote server must be rebuilt and restarted before the dialog works at
  runtime, since the advertised list is served by the server.
