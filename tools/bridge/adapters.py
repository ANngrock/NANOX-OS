"""Model adapters of the host bridge (M3).

An adapter turns the conversation (the user's request plus the results of
the actions taken so far) into the next decision: either a tool call from
tools.TOOLS or a final answer.  Adapters never touch the guest; the agent
loop (agent.py) validates the decision, sends the NCI request and feeds the
guest's response back as the observation.

  mock         deterministic scripted policy for protocol tests.  It is NOT a
               language model: it recognises one request (list the tasks,
               start the test workload, measure it, stop it) and chooses each
               step from the previous observations.
  unavailable  a provider that never answers (connection refused / timeout),
               for the model-unavailability scenario.
  anthropic    a real provider over HTTPS (Messages API with tool use).
               Written against the documented API, NOT verified: the bench
               has no network credentials (docs/m3-core.md, "не проверено").
"""

import json
import os
import urllib.error
import urllib.request

import toolspec as tools


class ModelUnavailable(Exception):
    """The provider could not be reached or did not produce a decision."""


class Decision:
    def __init__(self, tool=None, args=None, final=None, rationale="", raw=None):
        self.tool = tool
        self.args = args or {}
        self.final = final
        self.rationale = rationale
        self.raw = raw

    @property
    def is_final(self):
        return self.final is not None

    def to_json(self):
        d = {"rationale": self.rationale}
        if self.is_final:
            d["final"] = self.final
        else:
            d["tool"] = self.tool
            d["args"] = self.args
        if self.raw is not None:
            d["raw"] = self.raw
        return d


class Adapter:
    name = "abstract"

    def start(self, request):
        """Begins a conversation with the user's request."""
        raise NotImplementedError

    def next(self, observation=None):
        """Returns the next Decision; `observation` describes the result of
        the previous tool call (None for the first call)."""
        raise NotImplementedError


class MockModel(Adapter):
    """Scripted policy for 'list tasks, start the workload, measure, stop'."""

    name = "mock"
    INTENT_WORDS = ("задач", "нагрузк", "task", "load")

    def start(self, request):
        self.request = request
        self.done = []  # (tool, observation) of every completed step
        self.understood = any(w in request.lower() for w in self.INTENT_WORDS)

    def _last(self, tool):
        for t, obs in reversed(self.done):
            if t == tool:
                return obs
        return None

    def next(self, observation=None):
        if observation is not None:
            self.done.append((observation["tool"], observation))
            if observation["state"] != "SUCCEEDED":
                return Decision(final="Не удалось выполнить %s: %s %s." % (
                    observation["tool"], observation["state"],
                    observation["fields"].get("code", "")),
                    rationale="the last action failed; stop and report")
        if not self.understood:
            return Decision(final="Мок-модель умеет только сценарий M3: перечислить задачи, "
                            "запустить тестовую нагрузку, измерить и остановить её.",
                            rationale="request not recognised")
        tools_done = [t for t, _ in self.done]
        spawn = self._last("spawn_task")
        if "list_tasks" not in tools_done:
            return Decision("list_tasks", rationale="observe the current tasks first")
        if spawn is None:
            return Decision("spawn_task", {"program": "load"},
                            rationale="start the test workload bin/load")
        ref = spawn["fields"]["ref"]
        if "measure_task" not in tools_done:
            return Decision("measure_task", {"target": ref, "window_ms": "500"},
                            rationale="measure the CPU share of the workload for 500 ms")
        if "terminate_task" not in tools_done:
            rev = self._last("measure_task")["fields"].get("rev") or spawn["fields"]["rev"]
            return Decision("terminate_task", {"target": ref, "expect_rev": rev},
                            rationale="stop the workload; it must not have changed since spawn")
        if tools_done.count("list_tasks") < 2:
            return Decision("list_tasks", rationale="confirm the workload is gone")
        first = self._last_n("list_tasks", 0)
        m = self._last("measure_task")["fields"]
        names = ", ".join("%s#%s(%s)" % (i["name"], i["ref"].rsplit("/", 1)[1], i["state"])
                          for i in first["items"])
        return Decision(final=(
            "Задачи до запуска: %s. Запущена нагрузка %s; за %s тиков окна она получила %s тиков "
            "CPU (%s%%). Нагрузка остановлена: %s, ресурсы освобождены." % (
                names, ref, m["window_ticks"], m["cpu_ticks"], m["share_pct"],
                self._last("terminate_task")["fields"].get("state"))),
            rationale="all steps done and verified by the executor")

    def _last_n(self, tool, index):
        return [obs for t, obs in self.done if t == tool][index]


class UnavailableModel(Adapter):
    name = "unavailable"

    def start(self, request):
        self.request = request

    def next(self, observation=None):
        raise ModelUnavailable("provider endpoint refused the connection (simulated)")


class AnthropicModel(Adapter):
    """Messages API with tool use over urllib (stdlib only, like the rest of
    the bench).  Configuration: ANTHROPIC_API_KEY, optional NANOX_MODEL and
    ANTHROPIC_BASE_URL.  Not verified on this bench (no credentials)."""

    name = "anthropic"
    DEFAULT_MODEL = "claude-opus-5"
    SYSTEM = ("You operate NANOX-OS through the tools given. Every tool call is executed "
              "inside the guest OS and verified there; only a SUCCEEDED result with "
              "verify=ok means the action happened. Call one tool at a time. When the "
              "user's request is fulfilled or cannot be, answer briefly in the user's "
              "language without calling a tool.")

    def __init__(self, timeout_s=60.0):
        self.key = os.environ.get("ANTHROPIC_API_KEY")
        self.model = os.environ.get("NANOX_MODEL", self.DEFAULT_MODEL)
        self.base = os.environ.get("ANTHROPIC_BASE_URL", "https://api.anthropic.com").rstrip("/")
        self.timeout_s = timeout_s
        self.pending_tool_use = None

    def _tool_defs(self):
        out = []
        for t in tools.TOOLS:
            props = {p: {"type": "string", "description": d} for p, d in t["params"].items()}
            required = [p for p in t["params"] if p not in t.get("optional", [])]
            out.append({"name": t["name"], "description": t["description"],
                        "input_schema": {"type": "object", "properties": props,
                                         "required": required, "additionalProperties": False}})
        return out

    def start(self, request):
        self.messages = [{"role": "user", "content": request}]

    def next(self, observation=None):
        if not self.key:
            raise ModelUnavailable("ANTHROPIC_API_KEY is not set")
        if observation is not None and self.pending_tool_use:
            self.messages.append({"role": "user", "content": [{
                "type": "tool_result", "tool_use_id": self.pending_tool_use,
                "content": "\n".join(observation["lines"]),
                "is_error": observation["state"] != "SUCCEEDED"}]})
        body = {"model": self.model, "max_tokens": 16000, "system": self.SYSTEM,
                "thinking": {"type": "adaptive"}, "tools": self._tool_defs(),
                "tool_choice": {"type": "auto", "disable_parallel_tool_use": True},
                "fallbacks": "default", "messages": self.messages}
        req = urllib.request.Request(
            self.base + "/v1/messages", data=json.dumps(body).encode(), method="POST",
            headers={"content-type": "application/json", "x-api-key": self.key,
                     "anthropic-version": "2023-06-01",
                     "anthropic-beta": "server-side-fallback-2026-07-01"})
        try:
            with urllib.request.urlopen(req, timeout=self.timeout_s) as r:
                resp = json.loads(r.read().decode())
        except (urllib.error.URLError, OSError, ValueError) as e:
            raise ModelUnavailable("provider request failed: %s" % e)
        # The whole content goes back into the history (thinking blocks included).
        self.messages.append({"role": "assistant", "content": resp.get("content", [])})
        stop = resp.get("stop_reason")
        if stop == "refusal":
            raise ModelUnavailable("provider declined the request (refusal)")
        uses = [b for b in resp.get("content", []) if b.get("type") == "tool_use"]
        text = " ".join(b.get("text", "") for b in resp.get("content", [])
                        if b.get("type") == "text").strip()
        if stop == "tool_use" and uses:
            self.pending_tool_use = uses[0]["id"]
            return Decision(uses[0]["name"], uses[0].get("input") or {}, rationale=text,
                            raw={"stop_reason": stop, "model": resp.get("model")})
        self.pending_tool_use = None
        return Decision(final=text or "(empty answer)", rationale="",
                        raw={"stop_reason": stop, "model": resp.get("model")})


ADAPTERS = {"mock": MockModel, "unavailable": UnavailableModel, "anthropic": AnthropicModel}


def make(name):
    if name not in ADAPTERS:
        raise ValueError("unknown adapter %r (known: %s)" % (name, ", ".join(sorted(ADAPTERS))))
    return ADAPTERS[name]()
