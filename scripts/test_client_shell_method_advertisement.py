"""Every API method the client shell sends must be advertised on its command lane.

The lane's allow-list (`CLIENT_SHELL_METHODS` in `src/server/client_commands.rs`) gates a
request twice: the client drops an unadvertised method before queueing it, and the server
rejects it as `unsupported_endpoint_command`. A method the client shell pushes but the lane
does not advertise is therefore dead on arrival, and silently so -- the rejection surfaces as
an endpoint notice, which a modal dialog draws over.

That is how `workspace.mount_remote` sat broken after the mount dialog was rebuilt onto this
lane: the Rust-side contract test only checks that advertised methods exist in the schema,
never the reverse direction. This test closes that direction.
"""

import re
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
CLIENT_ROOT = REPO_ROOT / "src" / "client"


def _production_source(path: Path) -> str:
    """Source with the trailing `#[cfg(test)]` block removed.

    This repository keeps unit tests at the bottom of the file behind `#[cfg(test)]`, so
    truncating there is enough to drop test-only method references without parsing Rust.
    """
    text = path.read_text(encoding="utf-8")
    marker = text.find("#[cfg(test)]")
    return text if marker == -1 else text[:marker]


def advertised_methods() -> set[str]:
    source = (REPO_ROOT / "src" / "server" / "client_commands.rs").read_text(encoding="utf-8")
    body = source.split("CLIENT_SHELL_METHODS", 1)[1].split("];", 1)[0]
    return set(re.findall(r'"([a-z_]+\.[a-z_.]+)"', body))


def variant_to_method_name() -> dict[str, str]:
    source = (REPO_ROOT / "src" / "api" / "server.rs").read_text(encoding="utf-8")
    return dict(re.findall(r"Method::(\w+)\([^)]*\) => \"([^\"]+)\"", source))


def methods_pushed_by_the_client_shell() -> dict[str, Path]:
    names = variant_to_method_name()
    found: dict[str, Path] = {}
    for path in sorted(CLIENT_ROOT.rglob("*.rs")):
        if "tests" in path.parts:
            continue
        for variant in re.findall(r"Method::(\w+)", _production_source(path)):
            if variant in names:
                found.setdefault(names[variant], path)
    return found


class ClientShellMethodAdvertisement(unittest.TestCase):
    def test_the_parsers_find_something(self) -> None:
        # Guards against a rename silently turning this whole test into a no-op.
        self.assertGreater(len(advertised_methods()), 20)
        self.assertGreater(len(variant_to_method_name()), 20)
        self.assertGreater(len(methods_pushed_by_the_client_shell()), 20)

    def test_every_method_the_client_shell_sends_is_advertised(self) -> None:
        advertised = advertised_methods()
        unadvertised = {
            method: path.relative_to(REPO_ROOT).as_posix()
            for method, path in methods_pushed_by_the_client_shell().items()
            if method not in advertised
        }
        self.assertEqual(
            unadvertised,
            {},
            "these methods are sent by the client shell but missing from CLIENT_SHELL_METHODS, "
            "so every request carrying one is dropped before it reaches the server",
        )


if __name__ == "__main__":
    unittest.main()
