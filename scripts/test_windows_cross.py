import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

from scripts import windows_cross


class WindowsCrossTests(unittest.TestCase):
    def test_libc_configuration_matches_xwin_layout(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for relative in (
                "sdk/include/ucrt/stdlib.h", "crt/include/vcruntime.h",
                "sdk/lib/ucrt/x86_64/ucrt.lib", "crt/lib/x86_64/vcruntime.lib",
                "sdk/lib/um/x86_64/kernel32.lib",
            ):
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.touch()
            contents = windows_cross.libc_contents(root)
            self.assertIn(f"include_dir={root / 'sdk/include/ucrt'}\n", contents)
            self.assertIn(f"msvc_lib_dir={root / 'crt/lib/x86_64'}\n", contents)
            self.assertTrue(contents.endswith("gcc_dir=\n"))
            (root / "crt/include/vcruntime.h").unlink()
            with self.assertRaisesRegex(ValueError, "missing .*vcruntime.h"):
                windows_cross.libc_contents(root)

    def test_persistent_configuration_and_explicit_override(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            default = root / "libc.txt"
            override = root / "custom.txt"
            default.touch()
            override.touch()
            with patch.object(windows_cross, "SDK_ROOT", root), patch.dict(os.environ, {}, clear=True):
                self.assertEqual(windows_cross.libc_path(), default.resolve())
                with patch.dict(os.environ, {windows_cross.LIBC_ENV: str(override)}):
                    self.assertEqual(windows_cross.libc_path(), override.resolve())
                    override.unlink()
                    with self.assertRaisesRegex(ValueError, "just setup-windows-cross"):
                        windows_cross.libc_path()

    def test_missing_setup_does_not_run_build_or_download(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(windows_cross, "SDK_ROOT", Path(directory)), \
                    patch.dict(os.environ, {}, clear=True), \
                    patch.object(windows_cross.subprocess, "run") as run:
                with self.assertRaisesRegex(ValueError, "just setup-windows-cross"):
                    windows_cross.lint()
                run.assert_not_called()

    def test_lint_passes_sdk_to_cargo_without_changing_parent_environment(self):
        with patch.object(windows_cross, "libc_path", return_value=Path("/sdk/libc.txt")), \
                patch.object(windows_cross, "link_macos_host_libraries"), \
                patch.dict(os.environ, {"KEEP_ME": "yes"}, clear=True), \
                patch.object(windows_cross.subprocess, "run") as run:
            windows_cross.lint()
            self.assertEqual(run.call_count, 2)
            cargo = run.call_args
            self.assertEqual(cargo.args[0][:2], ["cargo", "clippy"])
            self.assertEqual(cargo.kwargs["env"][windows_cross.LIBC_ENV], str(Path("/sdk/libc.txt")))
            self.assertEqual(cargo.kwargs["env"]["KEEP_ME"], "yes")
            self.assertNotIn(windows_cross.LIBC_ENV, os.environ)

    def test_license_acceptance_is_only_forwarded_when_explicit(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(windows_cross, "SDK_ROOT", Path(directory)), \
                    patch.object(windows_cross.shutil, "which", return_value="tool"), \
                    patch.object(windows_cross, "libc_contents", return_value="configuration"), \
                    patch.object(windows_cross.subprocess, "run") as run:
                for accepted in (False, True):
                    run.reset_mock()
                    windows_cross.setup(accepted)
                    command = run.call_args_list[0].args[0]
                    self.assertEqual("--accept-license" in command, accepted)
                    self.assertEqual(command[0], "xwin")
                    self.assertIn("--copy", command)

    def _macos_sdk_and_libc(self, root):
        sdk_lib = root / "MacOSX.sdk/usr/lib"
        sdk_lib.mkdir(parents=True)
        (sdk_lib / "libSystem.tbd").touch()
        libc_file = root / "cross/libc.txt"
        libc_file.parent.mkdir()
        libc_file.write_text(f"include_dir=/x\nsys_include_dir={root / 'cross/crt/include'}\n")
        xcrun = Mock(stdout=f"{root / 'MacOSX.sdk'}\n")
        return sdk_lib, libc_file, xcrun

    def test_macos_host_links_native_libraries_where_zig_looks(self):
        # Zig resolves libSystem under <sys_include_dir>/../../usr/lib.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            sdk_lib, libc_file, xcrun = self._macos_sdk_and_libc(root)
            with patch.object(windows_cross.sys, "platform", "darwin"), \
                    patch.object(windows_cross.subprocess, "run", return_value=xcrun) as run:
                windows_cross.link_macos_host_libraries(libc_file)
                link = root / "cross/usr/lib"
                self.assertTrue(link.is_symlink())
                self.assertEqual(link.resolve(), sdk_lib.resolve())
                self.assertEqual(run.call_args.args[0], ["xcrun", "--show-sdk-path"])
                run.reset_mock()
                windows_cross.link_macos_host_libraries(libc_file)
                run.assert_not_called()

    def test_macos_host_replaces_a_dangling_link(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            sdk_lib, libc_file, xcrun = self._macos_sdk_and_libc(root)
            link = root / "cross/usr/lib"
            link.parent.mkdir(parents=True)
            link.symlink_to(root / "RemovedXcode.sdk/usr/lib", target_is_directory=True)
            with patch.object(windows_cross.sys, "platform", "darwin"), \
                    patch.object(windows_cross.subprocess, "run", return_value=xcrun):
                windows_cross.link_macos_host_libraries(libc_file)
            self.assertEqual(link.resolve(), sdk_lib.resolve())

    def test_lint_leaves_a_custom_libc_tree_alone(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            custom = root / "custom/libc.txt"
            custom.parent.mkdir()
            custom.touch()
            with patch.object(windows_cross, "SDK_ROOT", root / "sdk"), \
                    patch.dict(os.environ, {windows_cross.LIBC_ENV: str(custom)}, clear=True), \
                    patch.object(windows_cross, "link_macos_host_libraries") as link, \
                    patch.object(windows_cross.subprocess, "run"):
                windows_cross.lint()
                link.assert_not_called()

    def test_non_macos_host_creates_no_link(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _, libc_file, xcrun = self._macos_sdk_and_libc(root)
            with patch.object(windows_cross.sys, "platform", "linux"), \
                    patch.object(windows_cross.subprocess, "run", return_value=xcrun) as run:
                windows_cross.link_macos_host_libraries(libc_file)
                run.assert_not_called()
                self.assertFalse((root / "cross/usr").exists())

    def test_macos_sdk_without_libsystem_is_reported(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            sdk_lib = root / "MacOSX.sdk/usr/lib"
            sdk_lib.mkdir(parents=True)
            libc_file = root / "cross/libc.txt"
            libc_file.parent.mkdir()
            libc_file.write_text(f"sys_include_dir={root / 'cross/crt/include'}\n")
            xcrun = Mock(stdout=f"{root / 'MacOSX.sdk'}\n")
            with patch.object(windows_cross.sys, "platform", "darwin"), \
                    patch.object(windows_cross.subprocess, "run", return_value=xcrun):
                with self.assertRaisesRegex(ValueError, "no libSystem.tbd"):
                    windows_cross.link_macos_host_libraries(libc_file)
                self.assertFalse((root / "cross/usr/lib").exists())


if __name__ == "__main__":
    unittest.main()
