#!/usr/bin/env bash
# Ask a question to the LLM gateway (OpenAI-compatible chat completion API).
#
# USAGE:
#   ./scripts/ask_llm.sh "你的问题"
#   echo "你好" | ./scripts/ask_llm.sh
#   PROMPT="..." ./scripts/ask_llm.sh
#
# CONFIG (override via env):
#   LLM_API_URL   gateway address            (default http://api.do.top:18080/v1/chat/completions)
#   LLM_API_KEY   api key                    (REQUIRED — no default; the script
#                                              aborts before any request if unset)
#   LLM_MODEL     model name                 (default deepseek-ai/DeepSeek-R1-0528-Qwen3-8B)
#   LLM_TEMPERATURE  sampling temperature    (default 0.7)
set -euo pipefail

LLM_API_URL="${LLM_API_URL:-http://api.do.top:18080/v1/chat/completions}"
LLM_API_KEY="${LLM_API_KEY:?LLM_API_KEY is required (export it before running; no default)}"
LLM_MODEL="${LLM_MODEL:-deepseek-ai/DeepSeek-R1-0528-Qwen3-8B}"
LLM_TEMPERATURE="${LLM_TEMPERATURE:-0.7}"

# ---- prompt resolution: CLI arg > $PROMPT > stdin --------------------------
if [[ $# -ge 1 ]]; then
  prompt="$*"
elif [[ -n "${PROMPT:-}" ]]; then
  prompt="${PROMPT}"
elif [[ ! -t 0 ]]; then
  prompt="$(cat)"
else
  echo "error: no prompt given. Pass it as an argument, set \$PROMPT, or pipe stdin." >&2
  exit 1
fi
[[ -n "$prompt" ]] || { echo "error: empty prompt." >&2; exit 1; }

body="$(jq -n --arg model "$LLM_MODEL" --arg content "$prompt" --argjson temp "$LLM_TEMPERATURE" \
  '{model: $model, messages: [{role: "user", content: $content}], temperature: $temp}')"

echo "==> POST ${LLM_API_URL}" >&2
echo "    model: ${LLM_MODEL}" >&2
echo "    prompt: ${prompt}" >&2

resp="$(curl -sS -X POST "${LLM_API_URL}" \
  -H "Authorization: Bearer ${LLM_API_KEY}" \
  -H "content-type: application/json" \
  -d "$body")"

if command -v jq >/dev/null 2>&1; then
  # Prefer jq: print the reply text plainly, keep the full JSON for inspection.
  echo "$resp" | jq -r '.choices[0].message.content // .' 2>/dev/null \
    || echo "$resp" | jq .
else
  echo "$resp"
fi
