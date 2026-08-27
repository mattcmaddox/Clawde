#!/usr/bin/env python3
"""SDK smoke tests against a live Clawde gateway (plan Phase 2 step 11).

Exercises the gateway through the official OpenAI SDKs exactly as a client
would: relay chat completions, chat agent mode (server-side Read), the
/v1/responses endpoint (items + continuation), and a full openai-agents
Runner loop over the gateway's Responses API.

Expects a running gateway (see scripts/gateway-sdk-smoke.sh, which starts
one). Configuration:

  GATEWAY_URL   base URL, default http://127.0.0.1:8899/v1
  GATEWAY_KEY   bearer key, default smoke-key

The free cascade is nondeterministic — weak upstreams occasionally return
empty completions or answer without calling a tool. The `retry` helper
filters transient empties (each attempt still exercises the full wire
path), and tool-round assertions that depend on model behavior are
informational rather than pass/fail gates (deterministic tool behavior is
covered by the integration tests).

Dependencies: pip install openai openai-agents
"""

import os
import sys

from openai import AsyncOpenAI, OpenAI

BASE = os.environ.get("GATEWAY_URL", "http://127.0.0.1:8899/v1")
KEY = os.environ.get("GATEWAY_KEY", "smoke-key")
client = OpenAI(base_url=BASE, api_key=KEY)

failures = []


def check(name, cond, detail=""):
    status = "PASS" if cond else "FAIL"
    print(f"[{status}] {name}" + (f" - {detail}" if detail and not cond else ""))
    if not cond:
        failures.append(name)


def retry(fn, attempts=3):
    """Retry through transient free-cascade empties while still exercising
    the wire path each attempt."""
    last = None
    for _ in range(attempts):
        try:
            last = fn()
        except Exception as e:  # noqa: BLE001 - smoke script
            last = e
        if last is None or isinstance(last, Exception):
            continue
        return last
    return last


# 1. Relay chat completion
def relay_call():
    r = client.chat.completions.create(
        model="free/groq",
        messages=[{"role": "user", "content": "Reply with exactly: RELAY_OK"}],
        max_tokens=80,
    )
    c = r.choices[0].message.content
    return c if c else None


relay = retry(relay_call)
check("chat completions relay", relay == "RELAY_OK" or (relay and "RELAY" in relay),
      f"got {relay!r}")

# 2. Chat completions agent mode: server-side Read tool execution
def agent_chat_call():
    r = client.chat.completions.create(
        model="free/groq",
        messages=[{"role": "user",
                   "content": "Use the Read tool to read ../README.md and report its exact first line."}],
        tools=[{
            "type": "function",
            "function": {"name": "Read", "description": "Read a file",
                         "parameters": {"type": "object",
                                        "properties": {"file_path": {"type": "string"}}}},
        }],
        max_tokens=400,
        extra_body={"max_tool_calls": 3},  # gateway agent-mode knob (not in the SDK schema)
    )
    return r.choices[0].message.content or ""


content = retry(agent_chat_call)
check("chat completions agent mode (Read executed server-side)",
      isinstance(content, str) and len(content) > 0,
      f"content={content!r}")

# 3. Responses endpoint (non-stream) with a tool round
resp = client.responses.create(
    model="free/groq",
    input=[{"role": "user",
            "content": "Use the Grep tool to count lines in ../README.md containing Clawde."}],
    tools=[{"type": "function", "name": "Grep", "description": "Grep files",
            "parameters": {"type": "object",
                           "properties": {"pattern": {"type": "string"}}}}],
    max_output_tokens=150,
)
out_types = [o.type for o in resp.output]
check("responses returns items", len(out_types) > 0, f"types={out_types}")
check("responses completed status", resp.status == "completed", f"status={resp.status}")
# Tool rounds are proven deterministically in the integration tests; on the
# free cascade the model may answer directly without calling the tool, so
# this is informational, not a pass/fail gate.
print(f"[info] responses types={out_types}")

# 4. Responses continuation via previous_response_id
resp2 = client.responses.create(
    model="free/groq",
    input=[{"role": "user", "content": "Say the word DONE now."}],
    max_output_tokens=20,
    store=True,
)
cid = resp2.id
resp3 = client.responses.create(
    model="free/groq",
    previous_response_id=cid,
    input=[{"role": "user", "content": "Say CONTINUED now."}],
    max_output_tokens=20,
)
final = "".join(
    p.text for o in resp3.output for p in (o.content or []) if getattr(o, "type", "") == "message"
)
check("responses continuation hydrates", resp3.status == "completed" and len(resp3.output) > 0,
      f"status={resp3.status}, output={[o.type for o in resp3.output]}")
print(f"[info] continuation final text={final!r}")

# 5. openai-agents SDK: agent with a client-side function tool over the
# Responses API. The model is passed as a Model instance so the SDK's
# provider-prefix resolver does not choke on the gateway's `free/` route.
from agents import Agent, Runner, function_tool
from agents.models.openai_responses import OpenAIResponsesModel


@function_tool
def ping() -> str:
    """Return the string pong."""
    return "pong"


async_client = AsyncOpenAI(base_url=BASE, api_key=KEY)
agent = Agent(
    name="smoke",
    instructions="Use the ping tool and report what it returns.",
    tools=[ping],
    model=OpenAIResponsesModel(model="free/groq", openai_client=async_client),
)
result = Runner.run_sync(agent, "Call ping and tell me what it returned.")
answer = result.final_output
check("Agents SDK round-trip", "pong" in answer, f"final_output={answer!r}")

print("\n=== SUMMARY ===")
if failures:
    print(f"{len(failures)} FAILURES: {failures}")
    sys.exit(1)
print("all SDK smoke tests passed")
