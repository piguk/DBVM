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
import statistics

from selfdb_env import Client
from selfdb_env.agent import Model, run_episode


def rollout(session, instruction, model, max_steps):
    try:
        return run_episode(session, instruction, model, max_steps)
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
            model = Model(a.model, a.model_name)
            results = list(pool.map(lambda s: rollout(s, t["instruction"], model, a.max_steps), sessions))
            rewards = [v["reward"] or 0.0 for _, v, _ in results]
            mean, std = statistics.fmean(rewards), statistics.pstdev(rewards)
            for i, (messages, verdict, steps) in enumerate(results):
                out.write(json.dumps({"task_id": t["task_id"], "family": t.get("family"), "seed": i, "steps": steps,
                                      "reward": rewards[i], "advantage": (rewards[i] - mean) / (std or 1.0),
                                      "judge": verdict.get("details"), "messages": messages}) + "\n")
            print("%s: rewards %s" % (t["task_id"], rewards))


if __name__ == "__main__":
    main()
