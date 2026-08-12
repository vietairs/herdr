Task: study cmux's clipboard-over-SSH mechanism as a comparison point for
herdr's remote image-paste handling.

Outcome: recon + comparison verdict only, no code change warranted — cmux's
mechanism is identical to herdr's, and herdr is stricter on every axis
compared. Adaptation step skipped (logged in auto-decisions).

Verification: focused test suites green in the (throwaway) fix worktree —
image_path 18/18, bracketed_paste 15/15, remote_image_paste 21/21. TUI
key-paste e2e was NOT automated (no headless key injection); manual gate
stated, not claimed.

Overhead: 1 agent + main-loop, ~7 min, ~75k tokens.

Archived-at-SHA: d2ce3de280b2446a3d07575795416b6976077898
