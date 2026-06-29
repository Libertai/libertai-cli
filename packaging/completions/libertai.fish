# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_libertai_global_optspecs
	string join \n h/help V/version
end

function __fish_libertai_needs_command
	# Figure out if the current invocation already has a command.
	set -l cmd (commandline -opc)
	set -e cmd[1]
	argparse -s (__fish_libertai_global_optspecs) -- $cmd 2>/dev/null
	or return
	if set -q argv[1]
		# Also print the command, so this can be used to figure out what it is.
		echo $argv[1]
		return 1
	end
	return 0
end

function __fish_libertai_using_subcommand
	set -l cmd (__fish_libertai_needs_command)
	test -z "$cmd"
	and return 1
	contains -- $cmd[1] $argv
end

complete -c libertai -n "__fish_libertai_needs_command" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_needs_command" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "login" -d 'Log in (browser sign-in or paste an API key)'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "logout" -d 'Clear saved credentials (secrets removed; other settings kept)'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "status" -d 'Show current auth state and defaults'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "usage" -d 'Show plan tier, allowance windows (5h + weekly), and prepaid credits'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "keys" -d 'Manage API keys'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "models" -d 'List available models'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "ask" -d 'One-shot prompt, non-streaming'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "chat" -d 'Streaming chat REPL (Ctrl-D to exit)'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "search" -d 'Web search via LibertAI\'s search API (search.libertai.io)'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "fetch" -d 'Fetch a URL and return cleaned article text via search.libertai.io'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "image" -d 'Generate an image'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "run" -d 'Launch an arbitrary command with LibertAI env vars injected'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "claude" -d 'Launch Claude Code against LibertAI'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "opencode" -d 'Launch OpenCode against LibertAI'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "aider" -d 'Launch Aider against LibertAI'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "claw" -d 'Launch Claw Code (ultraworkers/claw-code) against LibertAI'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "hermes" -d 'Launch Hermes Agent against LibertAI'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "code" -d 'LibertAI\'s own coding agent, powered by pi_agent_rust'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "agents" -d 'One screen for all your coding-agent sessions'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "mcp" -d 'Run an MCP server exposing LibertAI web search and page fetch over stdio'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "config" -d 'Config file operations'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "skills" -d 'Install/list/uninstall the bundled agent skills (image gen etc)'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "sandbox" -d 'Inspect the bash-sandbox configuration'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "import" -d 'Import data from other coding agents into a LibertAI session'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "completions" -d 'Print a shell completion script to stdout.'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "man" -d 'Render the top-level man page (roff) to stdout'
complete -c libertai -n "__fish_libertai_needs_command" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c libertai -n "__fish_libertai_using_subcommand login" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand login" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand logout" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand logout" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand status" -l json -d 'Emit JSON (auth state, base URLs, defaults) instead of the human summary'
complete -c libertai -n "__fish_libertai_using_subcommand status" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand status" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand usage" -l json -d 'Emit the raw subscription JSON instead of the human summary'
complete -c libertai -n "__fish_libertai_using_subcommand usage" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand usage" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand keys; and not __fish_seen_subcommand_from list create delete help" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand keys; and not __fish_seen_subcommand_from list create delete help" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand keys; and not __fish_seen_subcommand_from list create delete help" -f -a "list" -d 'List all API keys for the current account'
complete -c libertai -n "__fish_libertai_using_subcommand keys; and not __fish_seen_subcommand_from list create delete help" -f -a "create" -d 'Create a new API key'
complete -c libertai -n "__fish_libertai_using_subcommand keys; and not __fish_seen_subcommand_from list create delete help" -f -a "delete" -d 'Delete an API key by id'
complete -c libertai -n "__fish_libertai_using_subcommand keys; and not __fish_seen_subcommand_from list create delete help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c libertai -n "__fish_libertai_using_subcommand keys; and __fish_seen_subcommand_from list" -l json -d 'Emit the key rows as JSON (mirrors the `/api-keys` response) instead of the table'
complete -c libertai -n "__fish_libertai_using_subcommand keys; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand keys; and __fish_seen_subcommand_from list" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand keys; and __fish_seen_subcommand_from create" -l limit -d 'Monthly spending limit in USD' -r
complete -c libertai -n "__fish_libertai_using_subcommand keys; and __fish_seen_subcommand_from create" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand keys; and __fish_seen_subcommand_from create" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand keys; and __fish_seen_subcommand_from delete" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand keys; and __fish_seen_subcommand_from delete" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand keys; and __fish_seen_subcommand_from help" -f -a "list" -d 'List all API keys for the current account'
complete -c libertai -n "__fish_libertai_using_subcommand keys; and __fish_seen_subcommand_from help" -f -a "create" -d 'Create a new API key'
complete -c libertai -n "__fish_libertai_using_subcommand keys; and __fish_seen_subcommand_from help" -f -a "delete" -d 'Delete an API key by id'
complete -c libertai -n "__fish_libertai_using_subcommand keys; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c libertai -n "__fish_libertai_using_subcommand models" -l refresh -d 'Re-sync the persisted model catalog: fetches `/v1/models` and merges any new models into pi\'s `models.json` so they become selectable in `libertai code` (`/model`)'
complete -c libertai -n "__fish_libertai_using_subcommand models" -l json -d 'Emit the `/v1/models` listing as JSON instead of the table'
complete -c libertai -n "__fish_libertai_using_subcommand models" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand models" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand ask" -l model -r
complete -c libertai -n "__fish_libertai_using_subcommand ask" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand ask" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand chat" -l model -r
complete -c libertai -n "__fish_libertai_using_subcommand chat" -l system -d 'Optional system prompt' -r
complete -c libertai -n "__fish_libertai_using_subcommand chat" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand chat" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand search" -l engines -d 'Engines to query (comma-separated). Defaults to google,bing,duckduckgo' -r
complete -c libertai -n "__fish_libertai_using_subcommand search" -l max-results -r
complete -c libertai -n "__fish_libertai_using_subcommand search" -l type -d 'web | news | images (defaults to web)' -r
complete -c libertai -n "__fish_libertai_using_subcommand search" -l json -d 'Dump the raw JSON response instead of a pretty list'
complete -c libertai -n "__fish_libertai_using_subcommand search" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand search" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand fetch" -l json -d 'Dump the raw JSON response instead of pretty-printed text'
complete -c libertai -n "__fish_libertai_using_subcommand fetch" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand fetch" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand image" -l model -r
complete -c libertai -n "__fish_libertai_using_subcommand image" -l size -d 'WIDTHxHEIGHT, e.g. 1024x1024' -r
complete -c libertai -n "__fish_libertai_using_subcommand image" -s n -l n -r
complete -c libertai -n "__fish_libertai_using_subcommand image" -s o -l out -d 'Output file (single image) or prefix (multi, e.g. `out` → out-0.png, out-1.png)' -r
complete -c libertai -n "__fish_libertai_using_subcommand image" -s f -l force -d 'Overwrite `--out` if it already exists'
complete -c libertai -n "__fish_libertai_using_subcommand image" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand image" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand run" -l model -r
complete -c libertai -n "__fish_libertai_using_subcommand run" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand run" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand claude" -l model -d 'Override all three model tiers at once' -r
complete -c libertai -n "__fish_libertai_using_subcommand claude" -l opus -r
complete -c libertai -n "__fish_libertai_using_subcommand claude" -l sonnet -r
complete -c libertai -n "__fish_libertai_using_subcommand claude" -l haiku -r
complete -c libertai -n "__fish_libertai_using_subcommand claude" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand claude" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand opencode" -l model -r
complete -c libertai -n "__fish_libertai_using_subcommand opencode" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand opencode" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand aider" -l model -r
complete -c libertai -n "__fish_libertai_using_subcommand aider" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand aider" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand claw" -l model -r
complete -c libertai -n "__fish_libertai_using_subcommand claw" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand claw" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand hermes" -l model -d 'Override the default model' -r
complete -c libertai -n "__fish_libertai_using_subcommand hermes" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c libertai -n "__fish_libertai_using_subcommand hermes" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand code" -l model -d 'Model override (defaults to `default_code_model` from config)' -r
complete -c libertai -n "__fish_libertai_using_subcommand code" -l provider -d 'Provider override (defaults to `default_code_provider` from config, or "libertai")' -r
complete -c libertai -n "__fish_libertai_using_subcommand code" -l mode -d 'Initial permission mode (`normal`, `accept-edits`, or `plan`)' -r
complete -c libertai -n "__fish_libertai_using_subcommand code" -l resume -d 'Resume a saved session. With a path, resume that specific JSONL file (see `--list-sessions` to find one). Bare `--resume` (no path) opens an interactive picker of recent sessions for the current cwd; in headless/non-TTY contexts it falls back to the most recent session' -r
complete -c libertai -n "__fish_libertai_using_subcommand code" -l sandbox -d 'Sandbox the bash tool. `off` (default) runs bash with the user\'s full host privileges. `strict` wraps it in `bwrap` (Linux only today) with no network, read-only system dirs, and a tmpfs `/tmp` — useful for untrusted models or reviewing third-party agent scripts. `auto` resolves per pillar; on the CLI that\'s currently the same as `off`. Also honours the `LIBERTAI_SANDBOX` env var' -r -f -a "off\t''
strict\t''
auto\t''"
complete -c libertai -n "__fish_libertai_using_subcommand code" -l name -d 'Display name for a `--bg` run (shown in `libertai agents`). Defaults to a slug derived from the prompt' -r
complete -c libertai -n "__fish_libertai_using_subcommand code" -l agent -d 'Run the session as the named sub-agent (from `.claude/agents` etc.) instead of the default agent. With `--bg`, dispatches that sub-agent in the background' -r
complete -c libertai -n "__fish_libertai_using_subcommand code" -l team -d 'Team name this session belongs to. When set with `--teammate`, registers the `team_task` tool so the session can read/update the shared task list. Usually set automatically by `--bg` team spawns; use manually to run a teammate interactively' -r
complete -c libertai -n "__fish_libertai_using_subcommand code" -l teammate -d 'Teammate name within the team. Paired with `--team`' -r
complete -c libertai -n "__fish_libertai_using_subcommand code" -l plan -d 'Start in plan mode: the agent can read/grep/find/ls but cannot run bash, write, or edit files until you toggle back to normal (Shift+Tab or /plan)'
complete -c libertai -n "__fish_libertai_using_subcommand code" -l continue -d 'Resume the most recent session for the current working directory'
complete -c libertai -n "__fish_libertai_using_subcommand code" -l list-sessions -d 'Print recent sessions (most recent first) and exit, without starting the agent. Filters to the current cwd by default; pass `--all` to list every project'
complete -c libertai -n "__fish_libertai_using_subcommand code" -l all -d 'With `--list-sessions`, show sessions across every project'
complete -c libertai -n "__fish_libertai_using_subcommand code" -l json -d 'With `--list-sessions`, emit a JSON array (path, name, message_count, …) instead of the human list'
complete -c libertai -n "__fish_libertai_using_subcommand code" -l dangerously-skip-permissions -d 'Bypass ALL tool approvals: run bash, edits, and every other mutating tool without prompting — like Codex\'s `--ask-for-approval never` / Claude Code\'s `--dangerously-skip-permissions`. DANGEROUS: the model can run arbitrary commands and rewrite files with no gate. Pair with `--sandbox=strict` (once shipped) or only use against a repo you control. Refused in `--print`/`--bg` and by background teammates unless you have first accepted the risk in an interactive session (a consent sentinel is written then). Also honours the `LIBERTAI_DANGEROUSLY_SKIP_PERMISSIONS` env var'
complete -c libertai -n "__fish_libertai_using_subcommand code" -s p -l print -d 'Print mode (like `claude -p`): run a single agent turn headlessly and exit — no TUI, no interactive prompts. The assistant\'s text streams to stdout; turn/tool noise goes to stderr. Tool calls not already covered by an allow rule are auto-denied instead of prompting, so scripts never hang. The prompt comes from the trailing args, piped stdin, or both (stdin becomes context above the args prompt). Composes with `--resume` / `--continue` to run one more headless turn against a saved session'
complete -c libertai -n "__fish_libertai_using_subcommand code" -l bg -d 'Start the session in the background: spawn a detached `libertai code` process for the given prompt, print its run id, and return to the shell. Inspect/attach with `libertai agents`. Implies a one-shot prompt (the trailing args) and is incompatible with `--print` and the interactive REPL'
complete -c libertai -n "__fish_libertai_using_subcommand code" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c libertai -n "__fish_libertai_using_subcommand code" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand agents" -l cwd -d 'Only show sessions started under this directory' -r
complete -c libertai -n "__fish_libertai_using_subcommand agents" -l model -d 'Model for sessions dispatched from the view\'s input' -r
complete -c libertai -n "__fish_libertai_using_subcommand agents" -l permission-mode -d 'Permission mode for dispatched sessions (`normal`, `accept-edits`, `plan`)' -r
complete -c libertai -n "__fish_libertai_using_subcommand agents" -l agent -d 'Sub-agent to run dispatched sessions as (defaults to the built-in catch-all agent)' -r
complete -c libertai -n "__fish_libertai_using_subcommand agents" -l json -d 'Emit a JSON array of sessions and exit (no TUI)'
complete -c libertai -n "__fish_libertai_using_subcommand agents" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c libertai -n "__fish_libertai_using_subcommand agents" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand mcp" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c libertai -n "__fish_libertai_using_subcommand mcp" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand config; and not __fish_seen_subcommand_from show path set unset help" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand config; and not __fish_seen_subcommand_from show path set unset help" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand config; and not __fish_seen_subcommand_from show path set unset help" -f -a "show" -d 'Print current config'
complete -c libertai -n "__fish_libertai_using_subcommand config; and not __fish_seen_subcommand_from show path set unset help" -f -a "path" -d 'Print config file path'
complete -c libertai -n "__fish_libertai_using_subcommand config; and not __fish_seen_subcommand_from show path set unset help" -f -a "set" -d 'Set a single dotted key, e.g. `default_chat_model gemma-3-27b`'
complete -c libertai -n "__fish_libertai_using_subcommand config; and not __fish_seen_subcommand_from show path set unset help" -f -a "unset" -d 'Reset a key to its current built-in default so future bumps propagate. Use `all` to reset every non-auth field'
complete -c libertai -n "__fish_libertai_using_subcommand config; and not __fish_seen_subcommand_from show path set unset help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c libertai -n "__fish_libertai_using_subcommand config; and __fish_seen_subcommand_from show" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand config; and __fish_seen_subcommand_from show" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand config; and __fish_seen_subcommand_from path" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand config; and __fish_seen_subcommand_from path" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand config; and __fish_seen_subcommand_from set" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand config; and __fish_seen_subcommand_from set" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand config; and __fish_seen_subcommand_from unset" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand config; and __fish_seen_subcommand_from unset" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand config; and __fish_seen_subcommand_from help" -f -a "show" -d 'Print current config'
complete -c libertai -n "__fish_libertai_using_subcommand config; and __fish_seen_subcommand_from help" -f -a "path" -d 'Print config file path'
complete -c libertai -n "__fish_libertai_using_subcommand config; and __fish_seen_subcommand_from help" -f -a "set" -d 'Set a single dotted key, e.g. `default_chat_model gemma-3-27b`'
complete -c libertai -n "__fish_libertai_using_subcommand config; and __fish_seen_subcommand_from help" -f -a "unset" -d 'Reset a key to its current built-in default so future bumps propagate. Use `all` to reset every non-auth field'
complete -c libertai -n "__fish_libertai_using_subcommand config; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c libertai -n "__fish_libertai_using_subcommand skills; and not __fish_seen_subcommand_from list install uninstall help" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand skills; and not __fish_seen_subcommand_from list install uninstall help" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand skills; and not __fish_seen_subcommand_from list install uninstall help" -f -a "list" -d 'List the bundled skills this CLI knows how to install'
complete -c libertai -n "__fish_libertai_using_subcommand skills; and not __fish_seen_subcommand_from list install uninstall help" -f -a "install" -d 'Install (or refresh) the bundled skills into Claude Code\'s skill dir. Defaults to the user-wide location (`~/.claude/skills/`); pass `--project` to install into `.claude/skills/` in the current directory'
complete -c libertai -n "__fish_libertai_using_subcommand skills; and not __fish_seen_subcommand_from list install uninstall help" -f -a "uninstall" -d 'Remove the bundled skills installed by this CLI'
complete -c libertai -n "__fish_libertai_using_subcommand skills; and not __fish_seen_subcommand_from list install uninstall help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c libertai -n "__fish_libertai_using_subcommand skills; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand skills; and __fish_seen_subcommand_from list" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand skills; and __fish_seen_subcommand_from install" -l project
complete -c libertai -n "__fish_libertai_using_subcommand skills; and __fish_seen_subcommand_from install" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand skills; and __fish_seen_subcommand_from install" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand skills; and __fish_seen_subcommand_from uninstall" -l project
complete -c libertai -n "__fish_libertai_using_subcommand skills; and __fish_seen_subcommand_from uninstall" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand skills; and __fish_seen_subcommand_from uninstall" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand skills; and __fish_seen_subcommand_from help" -f -a "list" -d 'List the bundled skills this CLI knows how to install'
complete -c libertai -n "__fish_libertai_using_subcommand skills; and __fish_seen_subcommand_from help" -f -a "install" -d 'Install (or refresh) the bundled skills into Claude Code\'s skill dir. Defaults to the user-wide location (`~/.claude/skills/`); pass `--project` to install into `.claude/skills/` in the current directory'
complete -c libertai -n "__fish_libertai_using_subcommand skills; and __fish_seen_subcommand_from help" -f -a "uninstall" -d 'Remove the bundled skills installed by this CLI'
complete -c libertai -n "__fish_libertai_using_subcommand skills; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c libertai -n "__fish_libertai_using_subcommand sandbox; and not __fish_seen_subcommand_from info help" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand sandbox; and not __fish_seen_subcommand_from info help" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand sandbox; and not __fish_seen_subcommand_from info help" -f -a "info" -d 'Print the resolved strict profile for this host: which bin / lib / config paths would be exposed, which are present vs missing, plus the bwrap location and the inside-sandbox PATH. Useful for debugging when something the model wants to run isn\'t reachable inside `--sandbox=strict`'
complete -c libertai -n "__fish_libertai_using_subcommand sandbox; and not __fish_seen_subcommand_from info help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c libertai -n "__fish_libertai_using_subcommand sandbox; and __fish_seen_subcommand_from info" -l json -d 'Emit JSON instead of the human summary. Suitable for piping into other tools or consuming from a wrapper script'
complete -c libertai -n "__fish_libertai_using_subcommand sandbox; and __fish_seen_subcommand_from info" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand sandbox; and __fish_seen_subcommand_from info" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand sandbox; and __fish_seen_subcommand_from help" -f -a "info" -d 'Print the resolved strict profile for this host: which bin / lib / config paths would be exposed, which are present vs missing, plus the bwrap location and the inside-sandbox PATH. Useful for debugging when something the model wants to run isn\'t reachable inside `--sandbox=strict`'
complete -c libertai -n "__fish_libertai_using_subcommand sandbox; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c libertai -n "__fish_libertai_using_subcommand import; and not __fish_seen_subcommand_from claude-code help" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand import; and not __fish_seen_subcommand_from claude-code help" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand import; and not __fish_seen_subcommand_from claude-code help" -f -a "claude-code" -d 'Claude Code transcripts (`~/.claude/projects/...`)'
complete -c libertai -n "__fish_libertai_using_subcommand import; and not __fish_seen_subcommand_from claude-code help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c libertai -n "__fish_libertai_using_subcommand import; and __fish_seen_subcommand_from claude-code" -s h -l help -d 'Print help'
complete -c libertai -n "__fish_libertai_using_subcommand import; and __fish_seen_subcommand_from claude-code" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand import; and __fish_seen_subcommand_from claude-code" -f -a "list" -d 'List Claude Code sessions discovered for the current project. Use `--all` to scan every project Claude Code has on disk'
complete -c libertai -n "__fish_libertai_using_subcommand import; and __fish_seen_subcommand_from claude-code" -f -a "show" -d 'Render the live branch of a Claude Code session as a plain-text transcript. Same source that the (still-WIP) summary import will feed to the model; use this to preview the input'
complete -c libertai -n "__fish_libertai_using_subcommand import; and __fish_seen_subcommand_from claude-code" -f -a "summarize" -d 'Build a `/compact`-style summary of a Claude Code session by calling the configured LibertAI chat model. Prints the summary to stdout. The next slice wires this into a real pi session as a `Compaction` checkpoint; for now this lets you eyeball quality'
complete -c libertai -n "__fish_libertai_using_subcommand import; and __fish_seen_subcommand_from claude-code" -f -a "import" -d 'Summarise the Claude Code session and write a new pi session file whose first entry is the resulting `/compact`-style checkpoint. Prints the new session path on success — open it with `libertai code --resume <path>` or pick it from the session picker'
complete -c libertai -n "__fish_libertai_using_subcommand import; and __fish_seen_subcommand_from claude-code" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c libertai -n "__fish_libertai_using_subcommand import; and __fish_seen_subcommand_from help" -f -a "claude-code" -d 'Claude Code transcripts (`~/.claude/projects/...`)'
complete -c libertai -n "__fish_libertai_using_subcommand import; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c libertai -n "__fish_libertai_using_subcommand completions" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c libertai -n "__fish_libertai_using_subcommand completions" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand man" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c libertai -n "__fish_libertai_using_subcommand man" -s V -l version -d 'Print version'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "login" -d 'Log in (browser sign-in or paste an API key)'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "logout" -d 'Clear saved credentials (secrets removed; other settings kept)'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "status" -d 'Show current auth state and defaults'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "usage" -d 'Show plan tier, allowance windows (5h + weekly), and prepaid credits'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "keys" -d 'Manage API keys'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "models" -d 'List available models'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "ask" -d 'One-shot prompt, non-streaming'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "chat" -d 'Streaming chat REPL (Ctrl-D to exit)'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "search" -d 'Web search via LibertAI\'s search API (search.libertai.io)'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "fetch" -d 'Fetch a URL and return cleaned article text via search.libertai.io'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "image" -d 'Generate an image'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "run" -d 'Launch an arbitrary command with LibertAI env vars injected'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "claude" -d 'Launch Claude Code against LibertAI'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "opencode" -d 'Launch OpenCode against LibertAI'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "aider" -d 'Launch Aider against LibertAI'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "claw" -d 'Launch Claw Code (ultraworkers/claw-code) against LibertAI'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "hermes" -d 'Launch Hermes Agent against LibertAI'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "code" -d 'LibertAI\'s own coding agent, powered by pi_agent_rust'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "agents" -d 'One screen for all your coding-agent sessions'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "mcp" -d 'Run an MCP server exposing LibertAI web search and page fetch over stdio'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "config" -d 'Config file operations'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "skills" -d 'Install/list/uninstall the bundled agent skills (image gen etc)'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "sandbox" -d 'Inspect the bash-sandbox configuration'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "import" -d 'Import data from other coding agents into a LibertAI session'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "completions" -d 'Print a shell completion script to stdout.'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "man" -d 'Render the top-level man page (roff) to stdout'
complete -c libertai -n "__fish_libertai_using_subcommand help; and not __fish_seen_subcommand_from login logout status usage keys models ask chat search fetch image run claude opencode aider claw hermes code agents mcp config skills sandbox import completions man help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c libertai -n "__fish_libertai_using_subcommand help; and __fish_seen_subcommand_from keys" -f -a "list" -d 'List all API keys for the current account'
complete -c libertai -n "__fish_libertai_using_subcommand help; and __fish_seen_subcommand_from keys" -f -a "create" -d 'Create a new API key'
complete -c libertai -n "__fish_libertai_using_subcommand help; and __fish_seen_subcommand_from keys" -f -a "delete" -d 'Delete an API key by id'
complete -c libertai -n "__fish_libertai_using_subcommand help; and __fish_seen_subcommand_from config" -f -a "show" -d 'Print current config'
complete -c libertai -n "__fish_libertai_using_subcommand help; and __fish_seen_subcommand_from config" -f -a "path" -d 'Print config file path'
complete -c libertai -n "__fish_libertai_using_subcommand help; and __fish_seen_subcommand_from config" -f -a "set" -d 'Set a single dotted key, e.g. `default_chat_model gemma-3-27b`'
complete -c libertai -n "__fish_libertai_using_subcommand help; and __fish_seen_subcommand_from config" -f -a "unset" -d 'Reset a key to its current built-in default so future bumps propagate. Use `all` to reset every non-auth field'
complete -c libertai -n "__fish_libertai_using_subcommand help; and __fish_seen_subcommand_from skills" -f -a "list" -d 'List the bundled skills this CLI knows how to install'
complete -c libertai -n "__fish_libertai_using_subcommand help; and __fish_seen_subcommand_from skills" -f -a "install" -d 'Install (or refresh) the bundled skills into Claude Code\'s skill dir. Defaults to the user-wide location (`~/.claude/skills/`); pass `--project` to install into `.claude/skills/` in the current directory'
complete -c libertai -n "__fish_libertai_using_subcommand help; and __fish_seen_subcommand_from skills" -f -a "uninstall" -d 'Remove the bundled skills installed by this CLI'
complete -c libertai -n "__fish_libertai_using_subcommand help; and __fish_seen_subcommand_from sandbox" -f -a "info" -d 'Print the resolved strict profile for this host: which bin / lib / config paths would be exposed, which are present vs missing, plus the bwrap location and the inside-sandbox PATH. Useful for debugging when something the model wants to run isn\'t reachable inside `--sandbox=strict`'
complete -c libertai -n "__fish_libertai_using_subcommand help; and __fish_seen_subcommand_from import" -f -a "claude-code" -d 'Claude Code transcripts (`~/.claude/projects/...`)'
