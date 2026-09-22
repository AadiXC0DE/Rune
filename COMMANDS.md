# Command reference

Generated from the command table the parser reads, so every entry here is a command the binary accepts.

## Global flags

Accepted anywhere on the command line.

| Flag | Value | Description |
|---|---|---|
| `--model` | `id` | Override the model for this process. |
| `--provider` | `name` | Override the provider for this process. |
| `--effort` | `level` | Override the reasoning effort: auto, none, minimal, low, medium, high, xhigh, max. |
| `--fast` | - | Request fast mode where the provider supports it. |
| `--no-fast` | - | Disable fast mode for this process. |
| `--permission-mode` | `mode` | Override the permission mode: ask, auto, full-access. |
| `--limit` | `name=value` | Override one limit. Repeatable. Use off to disable a limit. |
| `--add-dir` | `path` | Add a workspace directory for this process. Repeatable. |
| `--no-additional-dirs` | - | Ignore saved additional directories for this process. |
| `--offline` | - | Refuse every outbound network request. |
| `--json` | - | Emit machine-readable output where supported. |
| `--theme` | `name` | Override the theme for this process. |
| `--provider-order` | `a,b` | Prefer these upstream providers in order. |
| `--provider-strict` | - | Restrict requests to the listed providers. |
| `-h, --help` | - | Print help. |
| `-v, --version` | - | Print the version. |

## Run

### `rune ask [flags] <prompt>`

Run one request without an interactive session

| Flag | Description |
|---|---|
| `--json` | Print one JSON object instead of Markdown. |
| `--no-save` | Do not create a session. |
| `--image <path>` | Attach an image. Repeatable. |
| `--max-steps <n>` | Limit model steps for this run. |
| `--timeout <secs>` | Fail the run after this long. |
| `--prompt-permissions` | Prompt for approval. Requires a terminal. |
### `rune acp [--log-file <path>]`

Serve the Agent Client Protocol over standard input and output

| Flag | Description |
|---|---|
| `--log-file <path>` | Write diagnostics to this file. |
### `rune review [context]`

Review the pending changes in the workspace

### `rune connect [<name>]`

Connect a model provider


## Sessions and local records

### `rune sessions [--all] [--limit <n>] [--cursor <c>]`

List sessions

| Flag | Description |
|---|---|
| `--all` | Include sessions from every workspace. |
| `--limit <n>` | Page size, 1 to 100. |
| `--cursor <c>` | Continue from a previous page. |
| `--json` | Emit JSON. |
### `rune session <last|id> [--json] | rune session migrate <id> | rune session recover <id>`

Inspect, migrate, or recover one session

| Flag | Description |
|---|---|
| `--id <id>` | Read the argument as an exact session identifier. |
| `--allow-large` | Permit migrating an oversized session. |
| `--json` | Emit JSON. |
### `rune tree [last|id] [--json]`

Show the branch structure of a session

| Flag | Description |
|---|---|
| `--id <id>` | Read the argument as an exact identifier. |
| `--json` | Emit JSON. |
### `rune usage [--period <24h|7d|30d>] [--json]`

Report token usage recorded on this machine

| Flag | Description |
|---|---|
| `--period <span>` | One of 24h, 7d, or 30d. |
| `--json` | Emit JSON. |

## Account and configuration

### `rune auth [status|logout] [--json]`

Show or manage stored credentials

| Flag | Description |
|---|---|
| `--json` | Emit JSON. |
### `rune models [--json]`

List the models of the connected provider

| Flag | Description |
|---|---|
| `--json` | Emit JSON. |
### `rune permissions [--explain <target>] [--json]`

Show the permission mode and rules

| Flag | Description |
|---|---|
| `--explain <target>` | Explain the decision for one target. |
| `--json` | Emit JSON. |
### `rune projects [status|approve|reject|reset]`

Inspect or change workspace trust

| Flag | Description |
|---|---|
| `--json` | Emit JSON. |
### `rune config [--explain] [--json]`

Show the resolved configuration and where each value came from

| Flag | Description |
|---|---|
| `--explain` | Show the source layer of every key. |
| `--json` | Emit JSON. |
### `rune limits [--json]`

List every limit with its effective value and source

| Flag | Description |
|---|---|
| `--json` | Emit JSON. |
### `rune workspace <list|add <path>|remove <path>|clear>`

Manage additional workspace directories

| Flag | Description |
|---|---|
| `--json` | Emit JSON. |
### `rune prompt [--show]`

Show the assembled system prompt

| Flag | Description |
|---|---|
| `--show` | Print the assembled prompt. |

## Diagnostics and maintenance

### `rune status [--json]`

Show the resolved configuration and runtime state

| Flag | Description |
|---|---|
| `--json` | Emit JSON. |
### `rune doctor [--json]`

Check the local setup without starting a turn

| Flag | Description |
|---|---|
| `--json` | Emit JSON. |
### `rune upgrade --from <path> --checksum <hex> [--target <path>] [--json]`

Replace the installed binary with a verified artifact

| Flag | Description |
|---|---|
| `--from <path>` | Artifact to install. |
| `--checksum <hex>` | Published checksum of that artifact. |
| `--target <path>` | Binary to replace. Defaults to the running one. |
| `--json` | Emit JSON. |
### `rune uninstall [--target <path>] [--keep-state] [--yes] [--json]`

Remove the installed binary and, on request, local state

| Flag | Description |
|---|---|
| `--target <path>` | Binary to remove. Defaults to the running one. |
| `--keep-state` | Leave the state directory in place. |
| `--yes` | Confirm removal of the state directory. |
| `--json` | Emit JSON. |
### `rune reference [--write <path>]`

Print the generated command reference

| Flag | Description |
|---|---|
| `--write <path>` | Write the reference to a file. |
### `rune help [command]`

Print help

Aliases: `-h`, `--help`

### `rune version`

Print the version

Aliases: `-v`, `--version`


## Limits

Every limit takes a count, or `off` to remove it. A hard ceiling still applies to a limit set to `off`.

| Name | Default | Unit | Range | Description |
|---|---|---|---|---|
| `max_agent_steps` | 0 | items | 0 to 10000 | Maximum model tool-loop steps per turn; zero means unlimited. |
| `max_tool_result_bytes` | 65536 | bytes | 0 to unbounded | Bytes retained from one tool result before spilling. |
| `max_turn_result_bytes` | 8388608 | bytes | 0 to unbounded | Tool-result bytes one turn retains across its steps. |
| `skill_catalog_bytes` | 32768 | bytes | 0 to unbounded | Combined size of the skill catalog placed in the prompt. |
| `skill_description_bytes` | 1024 | bytes | 0 to unbounded | Size of one skill description in the catalog. |
| `skill_file_bytes` | 1048576 | bytes | 0 to unbounded | Largest skill file that may be loaded. |
| `mcp_description_bytes` | 1024 | bytes | 0 to unbounded | Size of one MCP tool description. |
| `mcp_search_result_bytes` | 16384 | bytes | 0 to unbounded | Size of MCP tool search results. |
| `mcp_server_instructions_bytes` | 2048 | bytes | 0 to unbounded | Instructions accepted from one MCP server. |
| `mcp_selected_schema_bytes` | 65536 | bytes | 0 to unbounded | Schema size for one explicitly selected MCP tool. |
| `project_instruction_file_bytes` | 65536 | bytes | 0 to unbounded | Size of one project instruction file. |
| `project_instructions_total_bytes` | 131072 | bytes | 0 to unbounded | Combined size of all applicable project instructions. |
| `image_adapter_output_bytes` | 20480 | bytes | 0 to unbounded | Text produced by an image analysis adapter. |
| `command_output_bytes` | 65536 | bytes | 0 to unbounded | Bytes retained from one command's output. |
| `read_file_lines` | 2000 | items | 0 to unbounded | Lines returned by one file read. |
| `read_file_line_bytes` | 2000 | bytes | 0 to unbounded | Bytes per line before truncation in a file read. |
| `list_entries` | 1000 | bytes | 0 to unbounded | Entries returned by one listing or glob. |
| `parallel_tool_calls` | 8 | items | 1 to 64 | Tool calls executed concurrently within one step. |
| `subagent_children` | 256 | items | 0 to 4096 | Subagent children one parent session may register. |
| `provider_head_timeout_ms` | 120000 | ms | 1000 to 3600000 | Time to wait for a model response head. |
| `provider_request_timeout_ms` | 600000 | ms | 1000 to 7200000 | Total time allowed for one model request attempt. |
| `provider_max_attempts` | 10 | bytes | 1 to 20 | Provider attempts before the turn fails. |
| `mcp_operation_timeout_ms` | 60000 | ms | 0 to unbounded | Time allowed for one MCP operation. |
| `mcp_startup_timeout_ms` | 30000 | ms | 0 to unbounded | Time allowed for an MCP server to start and be discovered. |
| `mcp_restart_limit` | 1 | items | 0 to 10 | Automatic restarts permitted for a local MCP server. |
| `review_context_bytes` | 8192 | bytes | 0 to unbounded | Current-turn tool output shown to the safety reviewer. |
| `review_holds_per_turn` | 64 | items | 0 to unbounded | Safety review holds permitted in one turn. |
| `review_timeout_ms` | 30000 | ms | 0 to unbounded | Time allowed for one safety review request. |
| `compaction_trigger_percent` | 80 | items | 10 to 99 | Compaction trigger as a percentage of usable input. |
| `web_fetch_redirects` | 5 | items | 0 to 20 | Redirects followed by one web fetch. |
| `web_fetch_bytes` | 1048576 | bytes | 0 to unbounded | Bytes returned by one web fetch. |
| `web_fetch_timeout_ms` | 30000 | ms | 0 to unbounded | Time allowed for one web fetch. |
| `web_search_results` | 10 | items | 0 to unbounded | Sources returned by one web search. |
| `vision_batch_images` | 8 | items | 1 to 16 | Images analysed in one vision request. |
| `additional_directories` | 16 | items | 0 to 64 | Additional workspace roots permitted. |
| `prompt_history_bytes` | 1048576 | bytes | 0 to unbounded | Prompt history size before compaction. |
| `steering_queue_depth` | 64 | items | 0 to unbounded | Messages accepted into a steering queue for one turn. |
