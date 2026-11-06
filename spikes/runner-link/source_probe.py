"""Local Git fixtures for runner source capture and credential-free object transfer.

Experimental: no runner/hub integration and no automatic network transports. Only use
disposable repositories. In particular this does not implement production path confinement,
process-tree supervision, unpacked-object quotas, or hostile concurrent config writers.
"""

from __future__ import annotations

import os
from pathlib import Path
import re
import signal
import subprocess
import tempfile
import uuid


class GitError(RuntimeError):
    pass


class Repository:
    def __init__(self, path: Path):
        self.path = path.resolve(strict=True)

    def git(self, *args: str, env: dict[str, str] | None = None,
            read_ignore_config: bool = False) -> bytes:
        # Do not inherit an agent's Git configuration or askpass/SSH environment.
        child_env = {
            k: v for k, v in os.environ.items()
            if not k.startswith("GIT_") and k not in ("SSH_ASKPASS", "SSH_AUTH_SOCK")
        }
        child_env.update({
            "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_SYSTEM": os.devnull,
            "GIT_CONFIG_GLOBAL": os.devnull, "GIT_TERMINAL_PROMPT": "0",
            "GIT_ALLOW_PROTOCOL": "file", "GIT_NO_LAZY_FETCH": "1",
            "GIT_NO_REPLACE_OBJECTS": "1", "GIT_LFS_SKIP_SMUDGE": "1",
            "GIT_AUTHOR_NAME": "twr source probe",
            "GIT_AUTHOR_EMAIL": "source-probe@example.invalid",
            "GIT_COMMITTER_NAME": "twr source probe",
            "GIT_COMMITTER_EMAIL": "source-probe@example.invalid",
            "GIT_AUTHOR_DATE": "2000-01-01T00:00:00+0000",
            "GIT_COMMITTER_DATE": "2000-01-01T00:00:00+0000",
        })
        if read_ignore_config:
            if args != ("config", "--includes", "--path", "--get", "core.excludesFile"):
                raise GitError("normal user configuration is restricted to an ignore-path read")
            # Reading config runs no helpers. Preserve the user's ignore-file
            # choice without enabling their programs in any later Git operation.
            for key in tuple(child_env):
                if key.startswith("GIT_CONFIG_"):
                    child_env.pop(key)
            child_env.update({k: v for k, v in os.environ.items() if k.startswith("GIT_CONFIG_")})
        child_env.update(env or {})
        command = [
            "git", "-C", str(self.path), "-c", "core.hooksPath=" + os.devnull,
            "-c", "core.fsmonitor=false", "-c", "commit.gpgSign=false",
            "-c", "gc.auto=0", "-c", "maintenance.auto=false", *args,
        ]
        with subprocess.Popen(command, env=child_env, stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE, start_new_session=True) as process:
            try:
                output, _ = process.communicate(timeout=30)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.communicate()
                raise GitError("git process group timed out") from None
            status = process.returncode
        if read_ignore_config and status == 1:
            return b""  # no explicit setting: Git's normal XDG ignore default still applies
        if status:
            # Git stderr may contain repository content/URLs; do not relay it in a protocol.
            raise GitError(f"git {args[0]} failed (exit {status})")
        return output.rstrip(b"\n")

    def commit_id(self, value: str) -> str:
        if not re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", value):
            raise GitError("expected a full object id")
        result = self.git("rev-parse", "--verify", value + "^{commit}").decode()
        if result != value:
            raise GitError("expected a commit object")
        return result

    def snapshot(self) -> str:
        """Pin an immutable effective source tree without changing the actual index."""
        parent = self.git("rev-parse", "HEAD").decode()
        self.commit_id(parent)
        for entry in self.git("ls-files", "-v", "-z").split(b"\0"):
            if entry and (entry[:1] == b"S" or entry[:1].islower()):
                raise GitError("snapshot refuses skip-worktree or assume-unchanged entries")
        # git add can run filters named by .gitattributes. Neutralize all configured
        # filter programs in this fixture, even if global config is already disabled.
        overrides: list[str] = []
        ignored = self.git("config", "--includes", "--path", "--get", "core.excludesFile",
                           read_ignore_config=True)
        if ignored:
            path = Path(os.fsdecode(ignored)).expanduser()
            if not path.is_absolute():
                path = self.path / path
            overrides.extend(["-c", "core.excludesFile=" + str(path)])
        for entry in self.git("config", "--null", "--list", "--includes").split(b"\0"):
            name = entry.split(b"\n", 1)[0].decode("utf-8", errors="strict")
            if name.lower().startswith("filter."):
                if any(c in name for c in "=\r\n"):
                    raise GitError("unsupported filter name")
                if name.rsplit(".", 1)[-1].lower() in ("clean", "smudge", "process"):
                    overrides.extend(["-c", name + "="])
                    overrides.extend(["-c", name.rsplit(".", 1)[0] + ".required=false"])
        with tempfile.TemporaryDirectory(prefix="twr-source-index-") as directory:
            env = {"GIT_INDEX_FILE": str(Path(directory) / "index")}
            self.git(*overrides, "read-tree", parent, env=env)
            self.git(*overrides, "add", "-A", "--", ".", env=env)
            # write-tree may refresh cached stat information and run a clean filter too.
            tree = self.git(*overrides, "write-tree", env=env).decode()
        if tree == self.git("rev-parse", parent + "^{tree}").decode():
            commit = parent
        else:
            commit = self.git("commit-tree", tree, "-p", parent, "-m", "twr source snapshot").decode()
        self.git("update-ref", "refs/twr/builds/" + commit, commit)
        return commit

    def bundle(self, want: str, destination: Path, tips: list[str], cap: int) -> None:
        """Make a named-ref bundle, ignoring negative tips absent from the source."""
        self.commit_id(want)
        destination = destination.resolve()
        if cap <= 0 or len(tips) > 64:
            raise GitError("invalid transfer limits")
        known = []
        for tip in tips:
            try:
                known.append(self.commit_id(tip))
            except GitError:
                pass
        ref = "refs/twr/sync/" + uuid.uuid4().hex
        self.git("update-ref", ref, want)
        try:
            # git bundle itself creates its named output atomically. A production
            # sender additionally needs supervised streaming/cancellation and disk quotas.
            self.git("bundle", "create", str(destination), ref, *["^" + t for t in known])
            if destination.stat().st_size > cap:
                destination.unlink()
                raise GitError("bundle exceeds transfer cap")
        finally:
            self.git("update-ref", "-d", ref)

    def receive(self, bundle: Path, source_ref: str, want: str, result_ref: str) -> None:
        bundle = bundle.resolve(strict=True)
        if not re.fullmatch(r"refs/twr/(?:sync|results)/[a-zA-Z0-9/_-]+", result_ref):
            raise GitError("invalid result namespace")
        if not re.fullmatch(r"refs/[a-zA-Z0-9/_.-]+", source_ref) or ".." in source_ref:
            raise GitError("invalid source reference")
        if not re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", want):
            raise GitError("invalid wanted commit")
        # Do not publish a ref until the advertised tip matches the requested commit.
        heads = self.git("bundle", "list-heads", str(bundle)).decode().splitlines()
        if f"{want} {source_ref}" not in heads:
            raise GitError("bundle does not advertise the wanted commit")
        self.git("bundle", "verify", str(bundle))
        self.git("-c", "fetch.fsckObjects=true", "-c", "transfer.fsckObjects=true",
                 "fetch", "--no-tags", "--no-write-fetch-head", str(bundle),
                 source_ref + ":" + result_ref)
        if self.git("rev-parse", result_ref).decode() != want:
            raise GitError("imported tip differs from wanted commit")
