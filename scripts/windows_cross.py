"""One-time Windows SDK setup and Windows target linting from Unix hosts."""

import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


SDK_ROOT = Path.home() / ".local/share/herdr/windows-cross"
LIBC_ENV = "LIBGHOSTTY_VT_WINDOWS_LIBC"
TARGET = "x86_64-pc-windows-msvc"


def libc_contents(root: Path) -> str:
    paths = {
        "include_dir": root / "sdk/include/ucrt",
        "sys_include_dir": root / "crt/include",
        "crt_dir": root / "sdk/lib/ucrt/x86_64",
        "msvc_lib_dir": root / "crt/lib/x86_64",
        "kernel32_lib_dir": root / "sdk/lib/um/x86_64",
    }
    required = {
        "include_dir": "stdlib.h",
        "sys_include_dir": "vcruntime.h",
        "crt_dir": "ucrt.lib",
        "msvc_lib_dir": "vcruntime.lib",
        "kernel32_lib_dir": "kernel32.lib",
    }
    for key, filename in required.items():
        if not (paths[key] / filename).is_file():
            raise ValueError(f"Windows SDK is incomplete: missing {paths[key] / filename}")
    return "".join(f"{key}={value}\n" for key, value in paths.items()) + "gcc_dir=\n"


def libc_path() -> Path:
    override = os.environ.get(LIBC_ENV)
    path = Path(override).expanduser() if override else SDK_ROOT / "libc.txt"
    if not path.is_file():
        raise ValueError(
            f"Windows cross-check needs SDK configuration at {path}.\n"
            "Run `just setup-windows-cross` once, or set "
            f"{LIBC_ENV} to an existing Zig libc configuration."
        )
    return path.resolve()


def link_macos_host_libraries(libc_file: Path) -> None:
    """Let Zig link native build helpers on a macOS host.

    `zig build --libc` applies the Windows libc file to every compile step,
    including the native helper programs libghostty-vt builds and runs. On
    macOS those helpers link libSystem, which Zig looks for under
    `<sys_include_dir>/../../usr/lib`. Point that at the macOS SDK.
    """
    if sys.platform != "darwin":
        return
    sys_include = next(
        (
            line.split("=", 1)[1].strip()
            for line in libc_file.read_text().splitlines()
            if line.startswith("sys_include_dir=")
        ),
        "",
    )
    if not sys_include:
        return
    link = Path(sys_include).parent.parent / "usr/lib"
    if link.exists():
        return
    if link.is_symlink():
        # The SDK it pointed at moved (Xcode removed or xcode-select switched).
        link.unlink()
    sdk = subprocess.run(
        ["xcrun", "--show-sdk-path"], check=True, capture_output=True, text=True
    ).stdout.strip()
    target = Path(sdk) / "usr/lib"
    if not (target / "libSystem.tbd").is_file():
        raise ValueError(f"macOS SDK has no libSystem.tbd under {target}")
    link.parent.mkdir(parents=True, exist_ok=True)
    link.symlink_to(target, target_is_directory=True)


def setup(accept_license: bool) -> None:
    if not shutil.which("xwin"):
        raise ValueError("Install xwin first: cargo install xwin --locked")
    if not shutil.which(os.environ.get("ZIG", "zig")):
        raise ValueError("Install Zig 0.16.0 first, or set ZIG to its executable.")
    SDK_ROOT.mkdir(parents=True, exist_ok=True)
    # SDK downloads can be large; /tmp is often RAM-backed on Linux.
    temp_parent = "/var/tmp" if sys.platform.startswith("linux") else None
    with tempfile.TemporaryDirectory(prefix="herdr-windows-sdk-", dir=temp_parent) as cache:
        command = ["xwin", "--arch", "x86_64", "--cache-dir", cache]
        if accept_license:
            command.append("--accept-license")
        subprocess.run(command + ["splat", "--copy", "--output", str(SDK_ROOT)], check=True)
    config = SDK_ROOT / "libc.txt"
    config.write_text(libc_contents(SDK_ROOT))
    subprocess.run(
        [os.environ.get("ZIG", "zig"), "libc", "-target", "x86_64-windows-msvc", str(config)],
        check=True,
    )
    link_macos_host_libraries(config)
    print(f"Windows SDK configured at {config}. Run `just windows-lint` or `just check`.")


def lint() -> None:
    libc_file = libc_path()
    # Also covers SDKs set up before setup created the link. A user's own
    # libc configuration is left untouched.
    if libc_file.is_relative_to(SDK_ROOT.resolve()):
        link_macos_host_libraries(libc_file)
    env = {**os.environ, LIBC_ENV: str(libc_file), "LIBGHOSTTY_VT_SIMD": "false"}
    subprocess.run(["rustup", "target", "add", TARGET], check=True)
    subprocess.run(
        # --tests also checks Windows-only test code, which Unix builds never compile.
        [
            "cargo",
            "clippy",
            "--bin",
            "herdr",
            "--tests",
            "--locked",
            "--target",
            TARGET,
            "--",
            "-D",
            "warnings",
        ],
        env=env,
        check=True,
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    setup_parser = commands.add_parser("setup", help="Download Microsoft's SDK using xwin")
    setup_parser.add_argument(
        "--accept-license", action="store_true",
        help="Explicitly accept Microsoft's SDK license instead of xwin's interactive prompt",
    )
    commands.add_parser("lint", help="Run Windows clippy with the configured SDK")
    args = parser.parse_args()
    try:
        if args.command == "setup":
            setup(args.accept_license)
        else:
            lint()
    except (ValueError, OSError) as error:
        print(error, file=sys.stderr)
        return 1
    except subprocess.CalledProcessError as error:
        return error.returncode
    return 0


if __name__ == "__main__":
    sys.exit(main())
