# Agents in Your Editor (ACP)

Editors such as Zed and the JetBrains IDEs run coding agents over the [Agent Client Protocol](https://agentclientprotocol.com) (ACP). The editor starts the agent and talks to it over the agent's standard input and output. An agent that runs this way often runs its commands in the editor's terminal, where no shell hook sees them.

`atuin acp` runs the agent for the editor and records what it does:

- **Commands.** Each command the agent runs in the editor's terminal is saved to your history with its exit code, tagged with the agent's name, as [agent hooks](../guide/agent-hooks.md) tag the commands that agents run themselves.
- **Open sessions.** Atuin knows which sessions the agent has open, and whether it is working, idle, or waiting for you. The [threads tools](./mcp.md#atuin_threads_search-atuin_threads_list-and-atuin_threads_read) show this.

Atuin passes every message between the editor and the agent on unchanged, so the agent works as it would if the editor started it directly.

## Setting it up

Put `atuin acp --` in front of the agent's command in the editor's settings. For example, in Zed's `settings.json`:

```json
{
  "agent_servers": {
    "Claude Code": {
      "command": "atuin",
      "args": ["acp", "--", "npx", "@zed-industries/claude-code-acp"]
    }
  }
}
```

Use the full path from `which atuin` if the editor can't find `atuin`.

Atuin names the agent after its command: `claude-code` for a command that mentions Claude, `codex` for Codex, and so on, else the program's own name. Pass `--name` to choose it yourself:

```shell
atuin acp --name gemini -- gemini --experimental-acp
```

## What it records

| The agent | What Atuin records |
|---|---|
| Runs a command with `terminal/create` | A history entry for the command, in the directory it runs in |
| Gets the command's exit from `terminal/wait_for_exit` or `terminal/output` | The entry's exit code |
| Lets the terminal go before the command exits | Nothing: the entry is dropped |
| Has a session opened with `session/new`, `session/load` or `session/resume` | The session, as open and idle |
| Gets a prompt with `session/prompt` | The session, as working until the reply ends the turn |
| Asks with `session/request_permission` | The session, as waiting for you |
| Exits | Its sessions, as closed |

Commands that the agent runs itself, without the editor's terminal, are not seen by `atuin acp`. Install the agent's [hooks](../guide/agent-hooks.md) to record those.
