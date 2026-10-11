#!/bin/sh
# A real newline-framed MCP server. Arguments and env come from the ACP client.
[ "$2" = '--fixture' ] || exit 1
printf '%s\n' "$$" > "$1.pid"
while IFS= read -r line; do
    printf '%s\n' "$line" >> "$1"
    id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
    case "$line" in
        *'"method":"initialize"'*)
            [ "$ACP_MCP_GREETING" = 'silent' ] && continue
            result='{"protocolVersion":"2025-11-25","capabilities":{"tools":{}}}' ;;
        *'"method":"tools/list"'*)
            result='{"tools":[{"name":"greet","description":"Fixture greeting","inputSchema":{"type":"object","properties":{"who":{"type":"string"}},"required":[]}}]}' ;;
        *'"method":"tools/call"'*)
            case "$line" in
                *'"who":"fail"'*) result='{"isError":true,"content":[{"type":"text","text":"fixture failure"}]}' ;;
                *) result="{\"content\":[{\"type\":\"text\",\"text\":\"hello $ACP_MCP_GREETING\"}]}" ;;
            esac ;;
        *) continue ;;
    esac
    printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$result"
done
