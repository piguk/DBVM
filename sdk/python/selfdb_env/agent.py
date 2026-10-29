"""An agent loop over the environment's tools, with a model behind any OpenAI-compatible chat endpoint (llama.cpp's
server, vLLM, ollama, a hosted API): the model calls tools until it calls `submit` or runs out of steps, then the
judge scores the session. Used by examples/local_agent.py (rollouts for RL) and packkit's red team.
"""

import json
import os
import urllib.request

SYSTEM = ("You work in a Linux sandbox through tools. Do the task, check your work, then call submit. "
          "Each tool returns its output.")

TOOLS = [
    {"name": "bash", "description": "Run a shell command.", "parameters": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}},
    {"name": "read_file", "description": "Read a file.", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}},
    {"name": "write_file", "description": "Write a file.", "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}}, "required": ["path", "content"]}},
    {"name": "edit_file", "description": "Replace the one occurrence of old with new in a file.", "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "old": {"type": "string"}, "new": {"type": "string"}}, "required": ["path", "old", "new"]}},
    {"name": "list_dir", "description": "List a directory.", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}},
    {"name": "submit", "description": "Finish: the work is done.", "parameters": {"type": "object", "properties": {}}},
]

# what one tool result may put in the context
MAX_RESULT = 8000


class Model:
    """A chat endpoint: `url` ends in /v1; the key, if one is needed, comes from OPENAI_API_KEY."""

    def __init__(self, url, name="local", timeout=600):
        self.url, self.name, self.timeout = url.rstrip("/"), name, timeout

    def chat(self, messages):
        body = {"model": self.name, "messages": messages, "tools": [{"type": "function", "function": t} for t in TOOLS]}
        req = urllib.request.Request(self.url + "/chat/completions", data=json.dumps(body).encode(), method="POST")
        req.add_header("Content-Type", "application/json")
        if os.environ.get("OPENAI_API_KEY"):
            req.add_header("Authorization", "Bearer " + os.environ["OPENAI_API_KEY"])
        with urllib.request.urlopen(req, timeout=self.timeout) as r:
            return json.load(r)["choices"][0]["message"]


def tool_result(r):
    if r.get("entries") is not None:
        text = "\n".join("%s %s" % (e["kind"], e["name"]) for e in r["entries"])
    else:
        text = r["output"] if r["exit_code"] == 0 else "exit %d\n%s%s" % (r["exit_code"], r["output"], r["stderr"])
    return text[-MAX_RESULT:]


def run_episode(session, instruction, model, max_steps=30, system=SYSTEM):
    """Drive `session` with `model` until it submits or `max_steps` tool calls; returns (messages, verdict, steps).
    The session is left open (the caller closes it)."""
    messages = [{"role": "system", "content": system}, {"role": "user", "content": instruction}]
    steps = 0
    while steps < max_steps:
        msg = model.chat(messages)
        messages.append({k: v for k, v in msg.items() if k in ("role", "content", "tool_calls")})
        calls = msg.get("tool_calls") or []
        if not calls:
            messages.append({"role": "user", "content": "Use the tools; call submit when you are done."})
            steps += 1
            continue
        for call in calls:
            name = call["function"]["name"]
            try:
                args = json.loads(call["function"].get("arguments") or "{}")
            except ValueError:
                args = None
            if name == "submit":
                return messages, session.judge(), steps
            if name not in {t["name"] for t in TOOLS} or not isinstance(args, dict):
                messages.append({"role": "tool", "tool_call_id": call.get("id", ""), "content": "no such tool, or arguments that are not a JSON object"})
            else:
                r = session.act(dict(args, tool=name))
                messages.append({"role": "tool", "tool_call_id": call.get("id", ""), "content": tool_result(r)})
            steps += 1
    return messages, session.judge(), steps
