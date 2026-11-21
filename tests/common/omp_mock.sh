#!/usr/bin/env bash
# Parameterized mock `omp` used by the integration tests.
# Behavior is selected by env vars set on the yoke invocation:
#   MOCK_JUDGE=pass|fail|fail-first|fail-always
#   MOCK_WORKER=done|cost|patch|await-steer|pause|summary|slow-summary
#   MOCK_STEER=accept|reject
#   MOCK_SUMMARY=cheap|slow
#   MOCK_WITNESS=1 copies verdict.md to .loop/.witness-N on each agent run.
set -euo pipefail

{
    echo "CALL"
    for arg in "$@"; do echo "$arg"; done
} >> .loop/.omp-argv

bump() {
    local f=$1 n=0
    if [[ -f "$f" ]]; then n=$(cat "$f"); fi
    n=$((n + 1))
    echo "$n" > "$f"
    printf '%s' "$n"
}

pass_judge() { printf 'VERDICT: PASS\n\nmock judge okay\n' > .loop/verdict.md; }
fail_judge() { printf 'VERDICT: FAIL\n\nmock judge feedback\n' > .loop/verdict.md; }

judge() {
    case "${MOCK_JUDGE:-pass}" in
        fail) fail_judge ;;
        fail-first) if [[ $(bump .loop/.judge-count) -eq 1 ]]; then fail_judge; else pass_judge; fi ;;
        fail-always) bump .loop/.judge-count >/dev/null; fail_judge ;;
        *) pass_judge ;;
    esac
}

agent() {
    if [[ -n "${MOCK_WITNESS:-}" ]]; then
        cp .loop/verdict.md ".loop/.witness-$(bump .loop/.agent-count)"
    fi
    printf 'STATUS: DONE\n\n## Stage 1\nDone.\n' > .loop/notes.md
}

stream() {
    echo '{"type":"session","version":3,"id":"sess-mock","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/project"}'
    echo '{"type":"turn_start"}'
    case "${MOCK_WORKER:-done}" in
        cost)
            echo '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"done"}}'
            echo '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"done"}],"model":"gpt-test","usage":{"input":10,"output":2,"cacheRead":0,"cacheWrite":0,"totalTokens":12,"cost":{"input":0.1,"output":0.15,"cacheRead":0,"cacheWrite":0,"total":0.25}},"duration":1500}}'
            ;;
        summary)
            echo '{"type":"message_update","assistantMessageEvent":{"type":"text_start","contentIndex":0}}'
            echo '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"checking files"}}'
            echo '{"type":"message_update","assistantMessageEvent":{"type":"text_end","contentIndex":0,"content":"first block"}}'
            echo '{"type":"tool_execution_start","toolName":"bash","toolCallId":"t1","args":{"command":"cargo test"}}'
            echo '{"type":"tool_execution_end","toolName":"bash","toolCallId":"t1","isError":false}'
            echo '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"partial block"}}'
            ;;
        slow-summary)
            for n in 1 2 3 4 5 6; do
                printf '{"type":"message_update","assistantMessageEvent":{"type":"text_end","contentIndex":0,"content":"block %s"}}\n' "$n"
                sleep 0.15
            done
            ;;
        *)
            echo '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"done"}}'
            ;;
    esac
}

complete() {
    printf '{"type":"agent_end","messages":[],"isTerminal":true,"yielded":true}\n'
    printf '{"type":"prompt_result","id":"%s","agentInvoked":true,"status":"completed","sessionSettled":true}\n' "$1"
    echo '{"type":"session_settled"}'
}

for arg in "$@"; do
    if [[ "$arg" == *judge.md* ]]; then judge; exit 0; fi
done

if [[ " $* " == *" --mode text "* ]]; then
    if [[ "$*" != *"--model fake-cheap"* ]]; then
        echo "summarizer used wrong model" >&2
        exit 9
    fi
    for required in --system-prompt --no-session --no-tools --no-extensions --no-skills --no-rules --no-context-files --no-lsp --no-title; do
        if [[ "$*" != *"$required"* ]]; then
            echo "summarizer missing $required" >&2
            exit 8
        fi
    done
    if [[ "${MOCK_SUMMARY:-cheap}" == inspect ]]; then
        for arg in "$@"; do
            if [[ "$arg" == @* ]]; then
                printf '\n---PROMPT---\n' >> .loop/.summary-prompts
                cat "${arg#@}" >> .loop/.summary-prompts
            fi
        done
        echo "cheap summary of the batch"
    elif [[ "${MOCK_SUMMARY:-cheap}" == slow ]]; then
        sleep 0.5
        echo "slow summary"
    else
        echo "cheap summary of the batch"
    fi
    exit 0
fi

RPC=0
for arg in "$@"; do
    if [[ "$arg" == "rpc" ]]; then RPC=1; fi
done

if [[ "$RPC" == 0 ]]; then
    agent
    exit 0
fi

prompt_id=""
echo '{"type":"ready"}'
while IFS= read -r line; do
    echo "$line" >> .loop/.omp-stdin
    id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
    if [[ "$line" == *'"type":"prompt"'* ]]; then
        prompt_id="$id"
        case "${MOCK_WORKER:-done}" in
            pause|await-steer) ;;
            *) agent ;;
        esac
        if [[ "${MOCK_WORKER:-}" == patch ]]; then printf 'artifact smoke\n' > feature.txt; fi
        printf '{"id":"%s","type":"response","command":"prompt","success":true}\n' "$id"
        case "${MOCK_WORKER:-done}" in
            pause)
                echo '{"type":"session","version":3,"id":"sess-pause","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/project"}'
                echo '{"type":"turn_start"}'
                echo '{"type":"message_update","assistantMessageEvent":{"type":"text_start","contentIndex":0}}'
                echo "$$" > .loop/worker.pid
                touch .loop/block-one
                sleep 0.4
                echo '{"type":"message_update","assistantMessageEvent":{"type":"text_end","contentIndex":0,"content":"block one"}}'
                sleep 0.4
                echo '{"type":"message_update","assistantMessageEvent":{"type":"text_end","contentIndex":0,"content":"block two"}}'
                ;;
            await-steer)
                echo '{"type":"session","version":3,"id":"sess-wait","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/project"}'
                echo '{"type":"turn_start"}'
                echo '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"waiting"}}'
                ;;
            *)
                stream
                complete "$id"
                exit 0
                ;;
        esac
    elif [[ "$line" == *'"type":"steer"'* ]]; then
        echo "$line" >> .loop/.steer-requests
        if [[ "${MOCK_STEER:-accept}" == reject ]]; then
            printf '{"id":"%s","type":"response","command":"steer","success":false,"error":"steering rejected by fake RPC"}\n' "$id"
        else
            printf '{"id":"%s","type":"response","command":"steer","success":true}\n' "$id"
            printf 'STATUS: DONE\n\nSteering handled.\n' > .loop/notes.md
            echo '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"steered"}}'
            complete "$prompt_id"
        fi
        exit 0
    fi
done
