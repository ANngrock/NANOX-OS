"""Tools the model may call, and their mapping to NCI operations (M3).

The model never talks NCI itself: it chooses a tool and arguments; the host
bridge validates them against this table and sends the NCI request.  A tool
call in the model output is only a proposal (ARCHITECTURE.md §8.1): whether
the action happened is decided by the guest's response and its
verification.
"""

TOOLS = [
    {
        "name": "describe_system",
        "op": "system.describe",
        "description": "Describe the running NANOX system: boot id, executor, clock, task count.",
        "params": {},
        "mutating": False,
    },
    {
        "name": "list_tasks",
        "op": "task.list",
        "description": "List all tasks (kernel threads and user tasks) with state and CPU ticks.",
        "params": {},
        "mutating": False,
    },
    {
        "name": "inspect_task",
        "op": "task.inspect",
        "description": "Show one task in detail.",
        "params": {"target": "task reference task/<boot>/<id> from list_tasks"},
        "mutating": False,
    },
    {
        "name": "spawn_task",
        "op": "task.spawn",
        "description": "Start a program from the initramfs (bin/<program>) as a new user task.",
        "params": {"program": "program name, e.g. load (the test workload)"},
        "mutating": True,
    },
    {
        "name": "measure_task",
        "op": "task.measure",
        "description": "Measure the CPU time a task gets during a window.",
        "params": {"target": "task reference", "window_ms": "window length, 1..5000 ms"},
        "mutating": False,
    },
    {
        "name": "terminate_task",
        "op": "task.terminate",
        "description": "Stop a user task and verify that it ended and its resources were released.",
        "params": {"target": "task reference",
                   "expect_rev": "optional: revision the task must still have (else CONFLICT)"},
        "optional": ["expect_rev"],
        "mutating": True,
    },
    {
        "name": "memory_stats",
        "op": "memory.stats",
        "description": "Physical memory counters.",
        "params": {},
        "mutating": False,
    },
]

BY_NAME = {t["name"]: t for t in TOOLS}


class ToolError(ValueError):
    pass


def to_nci(tool_name, args):
    """Validates a tool call of the model; returns (op, nci_args)."""
    tool = BY_NAME.get(tool_name)
    if tool is None:
        raise ToolError("unknown tool %r" % tool_name)
    args = dict(args or {})
    optional = set(tool.get("optional", []))
    for p in tool["params"]:
        if p not in args and p not in optional:
            raise ToolError("%s: missing argument %s" % (tool_name, p))
    extra = set(args) - set(tool["params"])
    if extra:
        raise ToolError("%s: unexpected arguments %s" % (tool_name, sorted(extra)))
    return tool["op"], {k: str(v) for k, v in args.items()}
