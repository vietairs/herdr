#!/usr/bin/env python3
from __future__ import annotations

import argparse
import re
import shutil
import subprocess
import sys
from pathlib import Path


class NixCargoHashError(ValueError):
    pass


CARGO_HASH_RE = re.compile(
    r"(?m)^(\s*cargoHash\s*=\s*)(\"sha256-[^\"]+\"|lib\.fakeHash)(;\s*)$"
)
GOT_HASH_RE = re.compile(r"\bgot:\s*(sha256-[A-Za-z0-9+/=]+)")


def find_cargo_hash_assignment(package_text: str) -> str:
    matches = list(CARGO_HASH_RE.finditer(package_text))
    if len(matches) != 1:
        raise NixCargoHashError(
            f"expected exactly one cargoHash assignment, found {len(matches)}"
        )
    return matches[0].group(2)


def replace_cargo_hash_assignment(package_text: str, replacement: str) -> str:
    matches = list(CARGO_HASH_RE.finditer(package_text))
    if len(matches) != 1:
        raise NixCargoHashError(
            f"expected exactly one cargoHash assignment, found {len(matches)}"
        )
    return CARGO_HASH_RE.sub(
        lambda match: f"{match.group(1)}{replacement}{match.group(3)}",
        package_text,
    )


def parse_got_hash(output: str) -> str:
    hashes = GOT_HASH_RE.findall(output)
    if len(hashes) != 1:
        raise NixCargoHashError(
            f"expected exactly one got: sha256-... hash, found {len(hashes)}"
        )
    return hashes[0]


def run_capture(command: list[str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        check=False,
    )


def run_checked(command: list[str]) -> None:
    subprocess.run(command, check=True)


def discover_cargo_hash(package_path: Path) -> tuple[str, str]:
    if shutil.which("nix") is None:
        raise NixCargoHashError("nix is required to refresh nix/package.nix cargoHash")
    if not package_path.exists():
        raise NixCargoHashError(f"{package_path} does not exist")

    original_text = package_path.read_text(encoding="utf-8")
    original_assignment = find_cargo_hash_assignment(original_text)
    discovered_hash: str | None = None

    try:
        package_path.write_text(
            replace_cargo_hash_assignment(original_text, "lib.fakeHash"),
            encoding="utf-8",
        )

        system = run_capture(
            ["nix", "eval", "--impure", "--raw", "--expr", "builtins.currentSystem"]
        )
        if system.returncode != 0:
            raise NixCargoHashError(
                system.stdout.strip() or "failed to read Nix currentSystem"
            )

        build_attr = f".#packages.{system.stdout.strip()}.herdr"
        print(f"building {build_attr} with lib.fakeHash to discover cargoHash")
        result = run_capture(["nix", "build", build_attr, "--no-link", "--print-build-logs"])
        if result.returncode == 0:
            raise NixCargoHashError(
                "nix build succeeded with lib.fakeHash; could not discover cargoHash"
            )

        discovered_hash = parse_got_hash(result.stdout)
    finally:
        package_path.write_text(original_text, encoding="utf-8")

    return discovered_hash, original_assignment


def refresh_cargo_hash(package_path: Path) -> None:
    discovered_hash, original_assignment = discover_cargo_hash(package_path)

    new_assignment = f'"{discovered_hash}"'
    if new_assignment == original_assignment:
        print(f"nix/package.nix cargoHash is unchanged: {discovered_hash}")
    else:
        print(f"updating nix/package.nix cargoHash to {discovered_hash}")
    package_path.write_text(
        replace_cargo_hash_assignment(package_path.read_text(encoding="utf-8"), new_assignment),
        encoding="utf-8",
    )

    print("verifying Nix flake checks")
    run_checked(["nix", "flake", "check", "--print-build-logs"])
    run_checked(["nix", "flake", "check", "--all-systems", "--no-build", "--print-build-logs"])
    print("nix/package.nix cargoHash is ready")


def check_cargo_hash(package_path: Path) -> None:
    discovered_hash, current_assignment = discover_cargo_hash(package_path)
    expected_assignment = f'"{discovered_hash}"'
    if current_assignment != expected_assignment:
        raise NixCargoHashError(
            "nix/package.nix cargoHash is stale: "
            f"expected {expected_assignment}, found {current_assignment}"
        )
    print(f"nix/package.nix cargoHash matches discovered hash: {discovered_hash}")


def cmd_refresh(args: argparse.Namespace) -> int:
    refresh_cargo_hash(Path(args.package))
    return 0


def cmd_check(args: argparse.Namespace) -> int:
    check_cargo_hash(Path(args.package))
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Refresh nix/package.nix cargoHash")
    subparsers = parser.add_subparsers(dest="command", required=True)

    check = subparsers.add_parser("check", help="Check cargoHash without patching")
    check.add_argument("--package", default="nix/package.nix")
    check.set_defaults(func=cmd_check)

    refresh = subparsers.add_parser("refresh", help="Refresh cargoHash using Nix")
    refresh.add_argument("--package", default="nix/package.nix")
    refresh.set_defaults(func=cmd_refresh)

    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()

    try:
        return args.func(args)
    except (NixCargoHashError, subprocess.CalledProcessError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
