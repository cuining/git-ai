"""Exercise benchmark startup across binaries with different default state roots."""

import os
from pathlib import Path
import socket
import tempfile
import unittest
from unittest.mock import Mock, patch

import benchmark_modes_vs_main as modes
import benchmark_nasty_modes_vs_main as nasty


@unittest.skipUnless(os.name == "posix", "Benchmarks use Unix daemon sockets")
class DaemonRuntimeTests(unittest.TestCase):
    def check_runtime(self, benchmark):
        for default_root in (".baseline-state", ".dcc-ai-tracking"):
            with self.subTest(default_root=default_root), tempfile.TemporaryDirectory(
                prefix="gai-br-", dir="/tmp"
            ) as tmp:
                home = Path(tmp) / "home"
                runtime = Path(tmp) / "runtime"
                runtime.mkdir()
                spawned_envs = []
                sockets = []
                process = Mock()
                process.poll.return_value = None

                def start_daemon(_args, *, env, **_kwargs):
                    spawned_envs.append(dict(env))
                    default_dir = Path(env["HOME"]) / default_root / "internal/daemon"
                    for key, name in (
                        ("GIT_AI_DAEMON_CONTROL_SOCKET", "control.sock"),
                        ("GIT_AI_DAEMON_TRACE_SOCKET", "trace2.sock"),
                    ):
                        address = Path(env.get(key, str(default_dir / name)))
                        address.parent.mkdir(parents=True, exist_ok=True)
                        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                        sockets.append(sock)
                        sock.bind(str(address))
                    return process

                variant = benchmark.Variant("test", "test", Path("git-ai"), "daemon")
                try:
                    with (
                        patch.dict(os.environ, {}, clear=True),
                        patch.object(benchmark.tempfile, "mkdtemp", return_value=str(home)),
                        patch.object(benchmark.subprocess, "Popen", side_effect=start_daemon),
                        patch.object(benchmark.time, "sleep"),
                    ):
                        if benchmark is nasty:
                            env, _, daemon, _ = benchmark.setup_variant_runtime(
                                variant, runtime, Path("git")
                            )
                            self.assertIs(daemon, process)
                        else:
                            runner = benchmark.VariantRunner(variant, runtime, Path("git"))
                            env = runner.base_env
                            self.assertTrue(runner.daemon_started)

                    expected_dir = home / ".dcc-ai-tracking/internal/daemon"
                    for key, name in (
                        ("GIT_AI_DAEMON_CONTROL_SOCKET", "control.sock"),
                        ("GIT_AI_DAEMON_TRACE_SOCKET", "trace2.sock"),
                    ):
                        expected = str(expected_dir / name)
                        self.assertEqual(spawned_envs[0].get(key), expected)
                        self.assertEqual(env[key], expected)
                    self.assertEqual(
                        env["GIT_TRACE2_EVENT"],
                        f"af_unix:stream:{expected_dir / 'trace2.sock'}",
                    )
                finally:
                    for sock in sockets:
                        sock.close()

    def test_nasty_runtime_selects_sockets_before_starting_daemon(self):
        self.check_runtime(nasty)

    def test_modes_runtime_selects_sockets_before_starting_daemon(self):
        self.check_runtime(modes)


if __name__ == "__main__":
    unittest.main()
