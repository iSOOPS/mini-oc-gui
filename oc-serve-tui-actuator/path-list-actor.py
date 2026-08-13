#!/usr/bin/env python3
"""
path-list-actor.py

A pure-Python utility for managing the path-list.md index.

Subcommands:
  add <path>       scan/edit .md under path, sync sections into path-list.md
  list             pretty-print all entries
  remove <path>    delete a path entry

This script never talks to opencode. It is purely local file operations.
Stdlib only (no third-party deps).
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sys
import tempfile
from pathlib import Path

# --- constants ---------------------------------------------------------------

SCRIPT_DIR = Path(__file__).resolve().parent
PATH_LIST_FILE = SCRIPT_DIR / "path-list.md"
PATH_LIST_BAK = PATH_LIST_FILE.with_suffix(PATH_LIST_FILE.suffix + ".bak")

# Reject these in path arguments (mirrors oc-serve-tui-actuator.sh:validate_local_path).
# Order matters: each check is a separate guarded branch.
PATH_REJECT_PROTOCOLS = ("://",)
PATH_REJECT_BACKSLASH = "\\"
PATH_REJECT_SHELL_META = re.compile(r"[\$\`\;\&\|\<\>\(\)\{\}\"\']")
PATH_REJECT_CONTROL = re.compile(r"[\x00-\x1f\x7f]")

# Where the actor reads/creates its markdown surface.
MARKDOWN_RELATIVE = "AGENTS.md"

# Section-id derivation from heading text. The format is "seq_<8 hex>".
# These ids are placeholders; the actuator replaces them with real
# opencode session ids once it queries GET /session?directory=<path>.
def heading_to_section_id(text: str) -> str:
    digest = hashlib.sha1(text.strip().encode("utf-8")).hexdigest()[:8]
    return f"seq_{digest}"


# --- error model -------------------------------------------------------------

def die(msg: str, code: int = 1) -> None:
    print(f"[!] {msg}", file=sys.stderr)
    sys.exit(code)


# --- validation --------------------------------------------------------------

def validate_path(p: str) -> str:
    """Return the normalised absolute path string, or die."""
    if PATH_REJECT_PROTOCOLS[0] in p:
        die(f"拒绝：检测到协议符号 {PATH_REJECT_PROTOCOLS[0]!r}")
    if PATH_REJECT_BACKSLASH in p:
        die("拒绝：路径包含反斜杠 \\\\，请使用 POSIX 风格")
    if PATH_REJECT_SHELL_META.search(p):
        die("拒绝：路径包含 shell 元字符")
    if PATH_REJECT_CONTROL.search(p):
        die("拒绝：路径包含控制字符")

    # Reject paths that are not absolute / home-relative / dot-relative.
    if not (p.startswith("/") or p.startswith("./") or p.startswith("../") or p == "." or p.startswith("~/")):
        die("拒绝：路径必须以 /、./、../ 或 ~/ 开头")

    expanded = os.path.expanduser(p)
    if expanded == "~":
        return os.environ.get("HOME", "/")
    return expanded


# --- atomic write of path-list.md -------------------------------------------

def load_index() -> list:
    if not PATH_LIST_FILE.exists():
        return []
    try:
        text = PATH_LIST_FILE.read_text(encoding="utf-8")
        data = json.loads(text)
    except (json.JSONDecodeError, OSError) as e:
        # Try .bak restore.
        if PATH_LIST_BAK.exists():
            try:
                bak_text = PATH_LIST_BAK.read_text(encoding="utf-8")
                data = json.loads(bak_text)
                print(f"[!] path-list.md 损坏，已从 .bak 还原 ({e})", file=sys.stderr)
            except (json.JSONDecodeError, OSError) as e2:
                die(f"path-list.md 和 .bak 都不可读: {e2}", code=3)
        else:
            die(f"path-list.md 不可读且无 .bak: {e}", code=3)
    if not isinstance(data, list):
        die("path-list.md 顶层不是 JSON 数组", code=3)
    return data


def save_index(entries: list) -> None:
    # Validate that we are about to write a valid array.
    if not isinstance(entries, list):
        die("save_index: 入参不是列表", code=3)

    # Backup existing file.
    if PATH_LIST_FILE.exists():
        try:
            PATH_LIST_BAK.write_text(
                PATH_LIST_FILE.read_text(encoding="utf-8"), encoding="utf-8"
            )
        except OSError as e:
            die(f"备份 .bak 失败: {e}", code=2)

    # Atomic write via tempfile in the same directory.
    try:
        fd, tmp_path = tempfile.mkstemp(
            prefix=PATH_LIST_FILE.name + ".", suffix=".tmp", dir=str(SCRIPT_DIR)
        )
    except OSError as e:
        die(f"创建临时文件失败: {e}", code=2)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump(entries, f, ensure_ascii=False, indent=2)
            f.write("\n")
        os.replace(tmp_path, PATH_LIST_FILE)
    except OSError as e:
        # Best-effort cleanup of the orphaned tmp.
        try:
            os.unlink(tmp_path)
        except OSError:
            pass
        die(f"原子写失败: {e}", code=2)

    # Drop the backup on success.
    try:
        PATH_LIST_BAK.unlink()
    except FileNotFoundError:
        pass


# --- markdown surface --------------------------------------------------------

DEFAULT_TEMPLATE = """# {title}

## Overview

## Goals

## Notes
"""


def section_ids_for_md(md_path: Path) -> list:
    """Extract seq_<8hex> ids from a markdown file's top-level headings."""
    if not md_path.exists():
        return []
    try:
        text = md_path.read_text(encoding="utf-8")
    except OSError:
        return []
    ids = []
    for line in text.splitlines():
        m = re.match(r"^##\s+(.*?)\s*$", line)
        if m:
            heading = m.group(1)
            if heading.strip():
                ids.append(heading_to_section_id(heading))
    return ids


def ensure_md(target: Path) -> list:
    """Create AGENTS.md if absent, return section ids derived from headings."""
    md_path = target / MARKDOWN_RELATIVE
    if not md_path.exists():
        try:
            target.mkdir(parents=True, exist_ok=True)
        except OSError as e:
            die(f"创建目录失败 {target}: {e}", code=2)
        try:
            md_path.write_text(
                DEFAULT_TEMPLATE.format(title=target.name or "project"),
                encoding="utf-8",
            )
        except OSError as e:
            die(f"写入 {md_path} 失败: {e}", code=2)
    return section_ids_for_md(md_path)


# --- mutators ----------------------------------------------------------------

def add_path(target: str) -> list:
    target = validate_path(target)
    target_path = Path(target)

    sections = ensure_md(target_path)

    entries = load_index()
    found = False
    for entry in entries:
        if entry.get("path") == target:
            # Merge sections (set union, preserve order).
            merged = list(dict.fromkeys(entry.get("sections", []) + sections))
            entry["sections"] = merged
            found = True
            break
    if not found:
        entries.append({"path": target, "sections": sections})

    save_index(entries)
    return entries


def remove_path(target: str) -> list:
    target = validate_path(target)
    entries = load_index()
    before = len(entries)
    entries = [e for e in entries if e.get("path") != target]
    if len(entries) == before:
        print(f"[!] 未在 path-list.md 中找到 {target}", file=sys.stderr)
        sys.exit(1)
    save_index(entries)
    return entries


def list_entries() -> list:
    return load_index()


# --- pretty printing ---------------------------------------------------------

def render_table(entries: list) -> str:
    if not entries:
        return "no entries"
    rows = [("path", "sections", "first 3 section ids")]
    for e in entries:
        path = e.get("path", "?")
        secs = e.get("sections", []) or []
        first3 = ", ".join(secs[:3]) if secs else "(empty)"
        rows.append((path, str(len(secs)), first3))
    widths = [max(len(r[i]) for r in rows) for i in range(3)]
    lines = []
    for i, row in enumerate(rows):
        lines.append("  ".join(c.ljust(widths[j]) for j, c in enumerate(row)))
        if i == 0:
            lines.append("  ".join("-" * w for w in widths))
    return "\n".join(lines)


# --- CLI ---------------------------------------------------------------------

def main(argv=None) -> int:
    parser = argparse.ArgumentParser(
        prog="path-list-actor.py",
        description="Manage the path-list.md index for oc-serve-tui-actuator.sh.",
    )
    sub = parser.add_subparsers(dest="cmd", required=True)

    p_add = sub.add_parser("add", help="Add or merge a path entry")
    p_add.add_argument("path", help="Absolute path (or ~/foo, ./foo, ../foo)")

    sub.add_parser("list", help="List all entries")

    p_rm = sub.add_parser("remove", help="Remove a path entry")
    p_rm.add_argument("path", help="Path to remove")

    args = parser.parse_args(argv)

    if args.cmd == "add":
        entries = add_path(args.path)
        print(render_table(entries))
        return 0
    elif args.cmd == "list":
        entries = list_entries()
        print(render_table(entries))
        return 0
    elif args.cmd == "remove":
        entries = remove_path(args.path)
        print(render_table(entries))
        return 0
    return 2  # unreachable


if __name__ == "__main__":
    sys.exit(main())
