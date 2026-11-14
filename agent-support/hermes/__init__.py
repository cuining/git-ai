# Managed by git-ai

import json
import os
import subprocess
from functools import partial


def _pre_paths(files, tool, args, task):
    content = [args["path"]] if args.get("path") else []
    entries = []
    if tool == "patch" and args.get("mode") == "patch" and args.get("patch"):
        headers = files._collect_v4a_header_paths(args["patch"])
        if isinstance(headers, str):
            return []
        _, writes, entries = headers
        content.extend(writes)
    return [str(files._resolve_path_for_task(path, task)) for path in content] + [
        str(files._resolve_entry_for_task(path, task)) for path in entries
    ]


def _emit(event, tool_name=None, args=None, task_id=None, session_id=None,
          tool_call_id=None, result=None, status=None, **_):
    if tool_name not in {"write_file", "patch"} or not session_id or not tool_call_id:
        return

    from tools import file_tools
    from hermes_constants import get_hermes_home

    task = task_id or "default"
    try:
        from tools.file_tools_paths import _terminal_env_type_for_task
    except ImportError:
        from tools.file_tools import _terminal_env_type_for_task
    if _terminal_env_type_for_task(task) != "local":
        return
    # In hosted sessions even absolute paths are in the sandbox namespace.
    try:
        from tools.environments.sandbox_scope import hosted_scope
    except ImportError:
        pass
    else:
        if hosted_scope() is not None:
            return

    cwd = file_tools._resolve_base_dir(task)
    args = args or {}
    extra = {"result": result, "status": status} if event == "post_tool_call" else {
        "resolved_paths": _pre_paths(file_tools, tool_name, args, task),
    }
    try:
        subprocess.run(
            [GIT_AI, "checkpoint", "hermes", "--hook-input", "stdin"],
            input=json.dumps({
                "hook_event_name": event,
                "tool_name": tool_name,
                "tool_call_id": tool_call_id,
                "session_id": session_id,
                "cwd": str(cwd),
                "extra": extra,
            }).encode("utf-8"),
            env={**os.environ, "HERMES_HOME": str(get_hermes_home())},
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=5,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        pass


def register(ctx):
    for event in ("pre_tool_call", "post_tool_call"):
        ctx.register_hook(event, partial(_emit, event))
