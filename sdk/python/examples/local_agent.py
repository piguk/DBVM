"""An agent on this machine's tasks, for RL with a small local model: for each task, a group of rollouts; in
each, the model (any OpenAI-compatible chat endpoint: llama.cpp's server, vLLM, ollama) calls the environment's
tools until it submits or runs out of steps; the judge's reward closes the rollout. The rollouts are written as
JSON lines with their messages, reward and group-relative advantage, ready for a GRPO trainer.

    python3 local_agent.py tasks.json --model http://127.0.0.1:8080/v1 --model-name qwen2.5-7b \\
        --group 8 --max-steps 30 --out rollouts.jsonl

tasks.json is packkit's output (or any list of {"task_id", "instruction"}). The environment is the one
`self env-local` runs (Client.local: SELFDB_ENV_URL / SELFDB_ENV_API_KEY, else the local key file); the model's
key, if it needs one, comes from OPENAI_API_KEY.
"""

import argparse
import concurrent.futures
import json
import os
import statistics
import urllib.request

from selfdb_env import Client

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


def chat(model, model_name, messages):
    body = {"model": model_name, "messages": messages, "tools": [{"type": "function", "function": t} for t in TOOLS]}
    req = urllib.request.Request(model.rstrip("/") + "/chat/completions", data=json.dumps(body).encode(), method="POST")
    req.add_header("Content-Type", "application/json")
    if os.environ.get("OPENAI_API_KEY"):
        req.add_header("Authorization", "Bearer " + os.environ["OPENAI_API_KEY"])
    with urllib.request.urlopen(req, timeout=600) as r:
        return json.load(r)["choices"][0]["message"]


def tool_result(r):
    text = r["output"] if r["exit_code"] == 0 else "exit %d\n%s%s" % (r["exit_code"], r["output"], r["stderr"])
    if r.get("entries") is not None:
        text = "\n".join("%s %s" % (e["kind"], e["name"]) for e in r["entries"])
    return text[-MAX_RESULT:]


def rollout(session, instruction, model, model_name, max_steps):
    messages = [{"role": "system", "content": SYSTEM}, {"role": "user", "content": instruction}]
    steps = 0
    try:
        while steps < max_steps:
            msg = chat(model, model_name, messages)
            messages.append({k: v for k, v in msg.items() if k in ("role", "content", "tool_calls")})
            calls = msg.get("tool_calls") or []
            if not calls:
                messages.append({"role": "user", "content": "Use the tools; call submit when you are done."})
                steps += 1
                continue
            for call in calls:
                name, args = call["function"]["name"], json.loads(call["function"].get("arguments") or "{}")
                if name == "submit":
                    verdict = session.judge()
                    return messages, verdict, steps
                r = session.act(dict(args, tool=name))
                messages.append({"role": "tool", "tool_call_id": call.get("id", ""), "content": tool_result(r)})
                steps += 1
        return messages, session.judge(), steps
    finally:
        session.close()


def main():
    p = argparse.ArgumentParser()
    p.add_argument("tasks")
    p.add_argument("--model", default="http://127.0.0.1:8080/v1")
    p.add_argument("--model-name", default="local")
    p.add_argument("--group", type=int, default=4)
    p.add_argument("--max-steps", type=int, default=30)
    p.add_argument("--out", default="rollouts.jsonl")
    a = p.parse_args()
    env = Client.local()
    tasks = json.load(open(a.tasks))
    with open(a.out, "w") as out, concurrent.futures.ThreadPoolExecutor(a.group) as pool:
        for t in tasks:
            sessions = env.rollouts(t["task_id"], [{"label": "g%d" % i, "seed": i} for i in range(a.group)])
            results = list(pool.map(lambda s: rollout(s, t["instruction"], a.model, a.model_name, a.max_steps), sessions))
            rewards = [v["reward"] or 0.0 for _, v, _ in results]
            mean, std = statistics.fmean(rewards), statistics.pstdev(rewards)
            for i, (messages, verdict, steps) in enumerate(results):
                out.write(json.dumps({"task_id": t["task_id"], "family": t.get("family"), "seed": i, "steps": steps,
                                      "reward": rewards[i], "advantage": (rewards[i] - mean) / (std or 1.0),
                                      "judge": verdict.get("details"), "messages": messages}) + "\n")
            print("%s: rewards %s" % (t["task_id"], rewards))


if __name__ == "__main__":
    main()
