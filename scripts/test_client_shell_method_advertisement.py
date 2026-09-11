"""Every API method the client shell sends must be advertised on its command lane.

The lane's allow-list (`CLIENT_SHELL_METHODS` in `src/server/client_commands.rs`) gates a
request twice: the client drops an unadvertised method before queueing it, and the server
rejects it as `unsupported_endpoint_command`. A method the client shell pushes but the lane
does not advertise is therefore dead on arrival, and silently so -- the rejection surfaces as
an endpoint notice, which a modal dialog draws over.

That is how `workspace.mount_remote` sat broken after the mount dialog was rebuilt onto this
lane: the Rust-side contract test only checks that advertised methods exist in the schema,
never the reverse direction. This test closes that direction, and asserts set EQUALITY rather
than one-way containment so a scanner that silently stops matching fails loudly instead of
passing vacuously.
"""

import re
import unittest
from pathlib import Path

from scripts.test_ui_hot_path_architecture import production_code

REPO_ROOT = Path(__file__).resolve().parent.parent
CLIENT_ROOT = REPO_ROOT / "src" / "client"

# Advertised methods the client shell does not itself send. Empty today; an entry here needs a
# reason, because an advertisement nothing sends is usually a leftover.
EXPECTED_ADVERTISED_BUT_UNSENT: set[str] = set()


def _is_test_source(path: Path) -> bool:
    # Both conventions appear under src/client: a `tests/` directory, and a sibling
    # `*_tests.rs` pulled in with `#[path = ...]` (e.g. endpoint/activation_tests.rs).
    return "tests" in path.parts or path.stem.endswith("_tests")


def advertised_methods() -> set[str]:
    source = (REPO_ROOT / "src" / "server" / "client_commands.rs").read_text(encoding="utf-8")
    body = source.split("CLIENT_SHELL_METHODS", 1)[1].split("];", 1)[0]
    return set(re.findall(r'"([a-z_]+\.[a-z_.]+)"', body))


def variant_to_method_name() -> dict[str, str]:
    source = (REPO_ROOT / "src" / "api" / "server.rs").read_text(encoding="utf-8")
    return dict(re.findall(r"Method::(\w+)\([^)]*\) => \"([^\"]+)\"", source))


def methods_pushed_by_the_client_shell() -> dict[str, str]:
    """Method name -> the repo-relative file that sends it.

    `production_code` is shared with `test_ui_hot_path_architecture`: it blanks comments,
    string literals and whole `#[cfg(test)] mod ... { }` bodies wherever they sit. Truncating
    at the first `#[cfg(test)]` instead would be wrong here -- several client files carry a
    mid-file `#[cfg(test)] use ...`, and `src/client/mod.rs` hits one on line 41 of 2073.
    """
    names = variant_to_method_name()
    found: dict[str, str] = {}
    for path in sorted(CLIENT_ROOT.rglob("*.rs")):
        if _is_test_source(path):
            continue
        code = production_code(path.read_text(encoding="utf-8"))
        for variant in re.findall(r"Method::(\w+)", code):
            if variant in names:
                found.setdefault(names[variant], path.relative_to(REPO_ROOT).as_posix())
    return found


class ClientShellMethodAdvertisement(unittest.TestCase):
    def test_every_method_the_client_shell_sends_is_advertised(self) -> None:
        advertised = advertised_methods()
        unadvertised = {
            method: path
            for method, path in methods_pushed_by_the_client_shell().items()
            if method not in advertised
        }
        self.assertEqual(
            unadvertised,
            {},
            "these methods are sent by the client shell but missing from CLIENT_SHELL_METHODS, "
            "so every request carrying one is dropped before it reaches the server",
        )

    def test_the_lane_advertises_nothing_the_client_shell_never_sends(self) -> None:
        # Doubles as the scanner's canary: if the parsers above silently stop matching, the
        # sent set shrinks and this fails at once -- which a "found more than N things" sanity
        # check could never notice.
        unsent = advertised_methods() - set(methods_pushed_by_the_client_shell())
        self.assertEqual(unsent, EXPECTED_ADVERTISED_BUT_UNSENT)


if __name__ == "__main__":
    unittest.main()
