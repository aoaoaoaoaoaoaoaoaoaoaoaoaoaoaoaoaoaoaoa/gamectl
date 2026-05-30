#!/usr/bin/env python3
from __future__ import annotations

import argparse
import os
import subprocess
import tomllib
from dataclasses import dataclass
from pathlib import Path, PurePosixPath


ROOT = Path(__file__).resolve().parent
MANIFEST = ROOT / "Cargo.toml"
DEFAULT_MAX_SOURCE_FILE_LINES = 2500
DEFAULT_SOURCE_FILE_INCLUDE = ("*.rs", "**/*.rs")
IGNORED_SOURCE_DIRS = frozenset(
    {
        ".cargo_home",
        ".direnv",
        ".git",
        ".git-local",
        ".hg",
        ".jj",
        ".svn",
        "__pycache__",
        "node_modules",
        "target",
        "vendor",
    }
)
Command = tuple[str, ...]
CommandSequence = tuple[Command, ...]


@dataclass(frozen=True, slots=True)
class SourceFilePolicy:
    max_lines: int
    include: tuple[str, ...]
    exclude: tuple[str, ...]


def load_rust_starter_metadata() -> tuple[str, dict[str, object]]:
    manifest = tomllib.loads(MANIFEST.read_text(encoding="utf-8"))
    package_metadata = manifest.get("package", {}).get("metadata", {})
    if isinstance(package_metadata, dict) and isinstance(
        rust_starter := package_metadata.get("rust-starter"), dict
    ):
        return "package.metadata.rust-starter", rust_starter

    workspace_metadata = manifest.get("workspace", {}).get("metadata", {})
    if isinstance(workspace_metadata, dict) and isinstance(
        rust_starter := workspace_metadata.get("rust-starter"), dict
    ):
        return "workspace.metadata.rust-starter", rust_starter

    raise SystemExit("[check] missing package/workspace metadata.rust-starter in Cargo.toml")


def load_command(value: object, *, key_path: str) -> Command:
    if not isinstance(value, list) or not value:
        raise SystemExit(f"[check] invalid {key_path}: expected a non-empty string list")
    if not all(isinstance(part, str) and part for part in value):
        raise SystemExit(f"[check] invalid {key_path}: expected a non-empty string list")
    return tuple(value)


def load_command_sequence(value: object, *, key_path: str) -> CommandSequence:
    if not isinstance(value, list) or not value:
        raise SystemExit(f"[check] invalid {key_path}: expected a non-empty list of commands")
    return tuple(
        load_command(command, key_path=f"{key_path}[{index}]")
        for index, command in enumerate(value, start=1)
    )


def load_commands(
    metadata_key: str, metadata: dict[str, object]
) -> dict[str, Command | CommandSequence]:
    commands: dict[str, Command | CommandSequence] = {
        "format_command": load_command(
            metadata.get("format_command"),
            key_path=f"{metadata_key}.format_command",
        ),
        "clippy_command": load_command(
            metadata.get("clippy_command"),
            key_path=f"{metadata_key}.clippy_command",
        ),
        "test_command": load_command(
            metadata.get("test_command"),
            key_path=f"{metadata_key}.test_command",
        ),
        "canonicalize_commands": load_command_sequence(
            metadata.get("canonicalize_commands"),
            key_path=f"{metadata_key}.canonicalize_commands",
        ),
    }

    raw_doc_command = metadata.get("doc_command")
    if raw_doc_command is not None:
        commands["doc_command"] = load_command(
            raw_doc_command,
            key_path=f"{metadata_key}.doc_command",
        )
    return commands


def load_patterns(
    value: object,
    *,
    default: tuple[str, ...],
    key_path: str,
    allow_empty: bool,
) -> tuple[str, ...]:
    if value is None:
        return default
    if not isinstance(value, list) or not all(
        isinstance(pattern, str) and pattern for pattern in value
    ):
        raise SystemExit(f"[check] invalid {key_path}: expected a string list")
    if not allow_empty and not value:
        raise SystemExit(f"[check] invalid {key_path}: expected at least one pattern")
    return tuple(value)


def load_source_file_policy(
    metadata_key: str, metadata: dict[str, object]
) -> SourceFilePolicy:
    raw_policy = metadata.get("source_files")
    if raw_policy is None:
        return SourceFilePolicy(DEFAULT_MAX_SOURCE_FILE_LINES, DEFAULT_SOURCE_FILE_INCLUDE, ())
    if not isinstance(raw_policy, dict):
        raise SystemExit(f"[check] invalid {metadata_key}.source_files: expected a table")

    max_lines = raw_policy.get("max_lines", DEFAULT_MAX_SOURCE_FILE_LINES)
    if not isinstance(max_lines, int) or max_lines <= 0:
        raise SystemExit(
            f"[check] invalid {metadata_key}.source_files.max_lines: expected a positive integer"
        )

    return SourceFilePolicy(
        max_lines=max_lines,
        include=load_patterns(
            raw_policy.get("include"),
            default=DEFAULT_SOURCE_FILE_INCLUDE,
            key_path=f"{metadata_key}.source_files.include",
            allow_empty=False,
        ),
        exclude=load_patterns(
            raw_policy.get("exclude"),
            default=(),
            key_path=f"{metadata_key}.source_files.exclude",
            allow_empty=True,
        ),
    )


def run(name: str, argv: Command) -> None:
    print(f"[check] {name}: {' '.join(argv)}", flush=True)
    proc = subprocess.run(argv, cwd=ROOT, check=False)
    if proc.returncode != 0:
        raise SystemExit(proc.returncode)


def run_command_sequence(name: str, commands: CommandSequence) -> None:
    for index, command in enumerate(commands, start=1):
        run(f"{name}.{index}", command)


def matches_pattern(path: PurePosixPath, pattern: str) -> bool:
    if path.match(pattern):
        return True
    prefix = "**/"
    return pattern.startswith(prefix) and path.match(pattern.removeprefix(prefix))


def iter_source_files(policy: SourceFilePolicy) -> list[Path]:
    paths: list[Path] = []
    for current_root, dirnames, filenames in os.walk(ROOT):
        dirnames[:] = sorted(name for name in dirnames if name not in IGNORED_SOURCE_DIRS)
        current = Path(current_root)
        for filename in filenames:
            path = current / filename
            relative_path = PurePosixPath(path.relative_to(ROOT).as_posix())
            if not any(matches_pattern(relative_path, pattern) for pattern in policy.include):
                continue
            if any(matches_pattern(relative_path, pattern) for pattern in policy.exclude):
                continue
            paths.append(path)
    return sorted(paths)


def line_count(path: Path) -> int:
    return len(path.read_text(encoding="utf-8").splitlines())


def enforce_source_file_policy(policy: SourceFilePolicy) -> None:
    print(f"[check] source-files: max {policy.max_lines} lines", flush=True)
    violations: list[tuple[str, int]] = []
    for path in iter_source_files(policy):
        lines = line_count(path)
        if lines > policy.max_lines:
            violations.append((path.relative_to(ROOT).as_posix(), lines))
    if not violations:
        return

    print(
        f"[check] source-files: {len(violations)} file(s) exceed the configured limit",
        flush=True,
    )
    for relative_path, lines in violations:
        print(f"[check] source-files: {relative_path}: {lines} lines", flush=True)
    raise SystemExit(1)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Thin rust-starter check runner")
    parser.add_argument(
        "mode",
        nargs="?",
        choices=("check", "verify", "deep", "fix", "canon"),
        default="check",
        help="check canonicalizes then verifies; verify is non-mutating; deep adds docs.",
    )
    return parser.parse_args()


def main() -> None:
    metadata_key, metadata = load_rust_starter_metadata()
    commands = load_commands(metadata_key, metadata)
    source_file_policy = load_source_file_policy(metadata_key, metadata)
    args = parse_args()

    if args.mode in {"fix", "canon"}:
        run_command_sequence("canonicalize", commands["canonicalize_commands"])
        return

    enforce_source_file_policy(source_file_policy)
    if args.mode != "verify":
        run_command_sequence("canonicalize", commands["canonicalize_commands"])

    run("fmt", commands["format_command"])
    run("clippy", commands["clippy_command"])
    run("test", commands["test_command"])

    if args.mode == "deep" and "doc_command" in commands:
        run("doc", commands["doc_command"])


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        raise SystemExit(130)
