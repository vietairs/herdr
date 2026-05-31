from __future__ import annotations

import unittest

from scripts.nix_cargo_hash import (
    NixCargoHashError,
    check_cargo_hash,
    find_cargo_hash_assignment,
    parse_got_hash,
    replace_cargo_hash_assignment,
)


PACKAGE_TEXT = """
rustPlatform.buildRustPackage {
  pname = "herdr";
  cargoHash = "sha256-oldHash=";
}
"""


class NixCargoHashTests(unittest.TestCase):
    def test_find_cargo_hash_assignment_reads_single_hash(self) -> None:
        self.assertEqual(find_cargo_hash_assignment(PACKAGE_TEXT), '"sha256-oldHash="')

    def test_replace_cargo_hash_assignment_updates_single_hash(self) -> None:
        updated = replace_cargo_hash_assignment(PACKAGE_TEXT, '"sha256-newHash="')

        self.assertIn('cargoHash = "sha256-newHash=";', updated)
        self.assertNotIn("sha256-oldHash", updated)

    def test_replace_cargo_hash_assignment_accepts_fake_hash(self) -> None:
        updated = replace_cargo_hash_assignment(PACKAGE_TEXT, "lib.fakeHash")

        self.assertIn("cargoHash = lib.fakeHash;", updated)

    def test_replace_cargo_hash_assignment_rejects_missing_hash(self) -> None:
        with self.assertRaisesRegex(NixCargoHashError, "found 0"):
            replace_cargo_hash_assignment('pname = "herdr";\n', "lib.fakeHash")

    def test_replace_cargo_hash_assignment_rejects_multiple_hashes(self) -> None:
        text = PACKAGE_TEXT + PACKAGE_TEXT

        with self.assertRaisesRegex(NixCargoHashError, "found 2"):
            replace_cargo_hash_assignment(text, "lib.fakeHash")

    def test_parse_got_hash_reads_nix_hash_mismatch(self) -> None:
        output = """
        specified: sha256-oldHash=
             got: sha256-newHash123+/=
        """

        self.assertEqual(parse_got_hash(output), "sha256-newHash123+/=")

    def test_parse_got_hash_rejects_missing_hash(self) -> None:
        with self.assertRaisesRegex(NixCargoHashError, "found 0"):
            parse_got_hash("error: no hash here")

    def test_parse_got_hash_rejects_multiple_hashes(self) -> None:
        output = "got: sha256-one=\ngot: sha256-two=\n"

        with self.assertRaisesRegex(NixCargoHashError, "found 2"):
            parse_got_hash(output)

    def test_check_cargo_hash_restores_file_on_failure(self) -> None:
        import contextlib
        import io
        import tempfile
        from pathlib import Path
        from unittest import mock

        with tempfile.TemporaryDirectory() as temp_dir:
            package = Path(temp_dir) / "package.nix"
            package.write_text(PACKAGE_TEXT, encoding="utf-8")

            with mock.patch("scripts.nix_cargo_hash.shutil.which", return_value="/nix/bin/nix"):
                with mock.patch("scripts.nix_cargo_hash.run_capture") as run_capture:
                    run_capture.side_effect = [
                        mock.Mock(returncode=0, stdout="x86_64-linux"),
                        mock.Mock(returncode=1, stdout="error without got hash"),
                    ]

                    with contextlib.redirect_stdout(io.StringIO()):
                        with self.assertRaisesRegex(NixCargoHashError, "found 0"):
                            check_cargo_hash(package)

            self.assertIn("--no-link", run_capture.call_args_list[1].args[0])
            self.assertEqual(package.read_text(encoding="utf-8"), PACKAGE_TEXT)


if __name__ == "__main__":
    unittest.main()
