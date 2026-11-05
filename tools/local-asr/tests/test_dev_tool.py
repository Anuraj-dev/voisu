"""Drop-in and switch-verification logic of tools/dev, with systemctl stubbed.

The script is sourced (it returns before its command dispatch when sourced) in a
`bash -c` harness whose XDG dirs point at a temp dir and whose `sctl` is a stub,
so nothing here touches the real systemd or the real user units.
"""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


DEV = Path(__file__).resolve().parents[3] / "tools" / "dev"


class DevToolTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.unit_dir = self.root / "config" / "systemd" / "user"

    def bash(self, script, stub="sctl() { echo \"unexpected systemctl $*\" >&2; return 99; }"):
        """Run `script` after sourcing tools/dev with sctl replaced by `stub`."""

        env = {**os.environ, "XDG_CONFIG_HOME": str(self.root / "config"), "XDG_DATA_HOME": str(self.root / "data"), "HOME": str(self.root)}
        harness = f'source "{DEV}"; set +e; {stub}; sleep() {{ :; }}; {script}'
        return subprocess.run(["bash", "-c", harness], cwd=DEV.parent, env=env, capture_output=True, text=True, timeout=60)

    def dropin(self, unit="voisu.service"):
        return self.unit_dir / f"{unit}.d" / "50-dev.conf"

    def test_write_creates_a_marked_dropin_and_rewrites_its_own(self):
        done = self.bash('write_dropin voisu.service /dev/bin/voisu-daemon --systemd; write_dropin voisu.service /dev/bin/other --systemd')
        self.assertEqual(done.returncode, 0, done.stderr)
        lines = self.dropin().read_text().splitlines()
        self.assertEqual(lines[0], "# Written by tools/dev use dev; removed by tools/dev use prod.")
        self.assertEqual(lines[1:], ["[Service]", "ExecStart=", "ExecStart=/dev/bin/other --systemd"])

    def test_write_refuses_to_clobber_a_dropin_it_did_not_write(self):
        self.dropin().parent.mkdir(parents=True)
        self.dropin().write_text("[Service]\nEnvironment=MINE=1\n")
        done = self.bash("write_dropin voisu.service /dev/bin/voisu-daemon --systemd")
        self.assertEqual(done.returncode, 1)
        self.assertIn("refusing to overwrite", done.stderr)
        self.assertEqual(self.dropin().read_text(), "[Service]\nEnvironment=MINE=1\n")
        self.assertEqual(self.bash("dropin_free voisu.service").returncode, 1)
        self.assertEqual(self.bash("dropin_free voisu-overlay.service").returncode, 0)

    def test_remove_only_removes_a_marked_dropin(self):
        self.bash("write_dropin voisu.service /x --systemd")
        self.assertTrue(self.dropin().is_file())
        self.assertEqual(self.bash("remove_dropin voisu.service").returncode, 0)
        self.assertFalse(self.dropin().exists())
        self.assertFalse(self.dropin().parent.exists())  # the emptied .d directory goes too
        self.assertEqual(self.bash("remove_dropin voisu.service").returncode, 1)  # nothing of ours left
        foreign = self.dropin("voisu-overlay.service")
        foreign.parent.mkdir(parents=True)
        foreign.write_text("[Service]\nEnvironment=MINE=1\n")
        self.assertEqual(self.bash("remove_dropin voisu-overlay.service").returncode, 1)
        self.assertTrue(foreign.is_file())

    @unittest.skipIf(os.geteuid() == 0, "root ignores directory permissions")
    def test_remove_reports_a_failed_rm_as_status_2(self):
        self.bash("write_dropin voisu.service /x --systemd")
        self.dropin().parent.chmod(0o500)
        self.addCleanup(self.dropin().parent.chmod, 0o700)
        done = self.bash("remove_dropin voisu.service")
        self.assertEqual(done.returncode, 2)
        self.assertTrue(self.dropin().is_file())

    def test_wait_active_requires_the_unit_to_stay_active(self):
        calls = self.root / "calls"
        steady = 'sctl() { [ "$1" = is-active ] || return 99; return 0; }'
        self.assertEqual(self.bash("wait_active voisu.service", steady).returncode, 0)
        # Active at first, then it drops out three checks later and never recovers: a crash loop.
        flapping = f'sctl() {{ [ "$1" = is-active ] || return 99; echo x >>"{calls}"; [ "$(wc -l <"{calls}")" -le 2 ]; }}'
        done = self.bash("wait_active voisu.service", flapping)
        self.assertEqual(done.returncode, 1)
        self.assertIn("did not stay active", done.stderr)

    def test_verify_exec_path_compares_with_the_expected_binary(self):
        def show(path):
            return f'sctl() {{ echo "ExecStart={{ path={path} ; argv[]={path} --x ; ignore_errors=no }}"; }}'

        data = self.root / "data" / "voisu-dev" / "bin"
        self.assertEqual(self.bash("verify_exec_path voisu.service dev", show(f"{data}/voisu-daemon")).returncode, 0)
        self.assertEqual(self.bash("verify_exec_path voisu-overlay.service prod", show("/usr/bin/voisu-overlay")).returncode, 0)
        wrong = self.bash("verify_exec_path voisu.service dev", show("/usr/bin/voisu-daemon"))
        self.assertEqual(wrong.returncode, 1)
        self.assertIn("expected", wrong.stderr)
        self.assertEqual(self.bash("verify_exec_path voisu.service prod", show(f"{data}/voisu-daemon")).returncode, 1)

    def test_install_bins_is_all_or_nothing(self):
        out = self.root / "out"
        out.mkdir()
        (out / "voisu").write_text("new-cli")
        dev_bin = self.root / "data" / "voisu-dev" / "bin"
        dev_bin.mkdir(parents=True)
        (dev_bin / "voisu").write_text("old-cli")
        (dev_bin / "voisu-daemon").write_text("old-daemon")
        done = self.bash(f'install_bins "{out}" voisu voisu-daemon')  # voisu-daemon is missing from out/
        self.assertEqual(done.returncode, 1)
        self.assertEqual((dev_bin / "voisu").read_text(), "old-cli")
        self.assertEqual((dev_bin / "voisu-daemon").read_text(), "old-daemon")
        self.assertEqual(list(dev_bin.glob("*.new")), [])
        (out / "voisu-daemon").write_text("new-daemon")
        self.assertEqual(self.bash(f'install_bins "{out}" voisu voisu-daemon').returncode, 0)
        self.assertEqual((dev_bin / "voisu").read_text(), "new-cli")
        self.assertEqual((dev_bin / "voisu-daemon").read_text(), "new-daemon")
        self.assertEqual(list(dev_bin.glob("*.new")), [])

    def test_a_failed_switch_reports_where_the_units_are_and_how_to_go_back(self):
        stub = 'sctl() { echo "ExecStart={ path=/usr/bin/voisu-daemon ; argv[]=x }"; }'
        done = self.bash("set -e; boom() { false; echo not reached; }; run_switch boom; echo not reached", stub)
        self.assertNotEqual(done.returncode, 0)
        self.assertNotIn("not reached", done.stdout)
        self.assertEqual(done.stderr.count("switch failed"), 1)
        self.assertIn("voisu.service: /usr/bin/voisu-daemon", done.stderr)
        self.assertIn("tools/dev use prod", done.stderr)

    def test_finish_switch_clears_the_start_limit_before_restarting(self):
        log = self.root / "log"
        data = self.root / "data" / "voisu-dev" / "bin"
        stub = (f'sctl() {{ echo "$*" >>"{log}"; case "$1" in show) echo "ExecStart={{ path={data}/voisu-daemon ; argv[]=x }}";; is-active) [ "$2" = --quiet ] || echo active;; esac; return 0; }}')
        done = self.bash("finish_switch dev voisu.service >/dev/null", stub)
        self.assertEqual(done.returncode, 0, done.stderr)
        verbs = [line.split()[0] for line in log.read_text().splitlines()]
        self.assertLess(verbs.index("reset-failed"), verbs.index("restart"))


if __name__ == "__main__":
    unittest.main()
