# Testing Agent Tracker

Agent Tracker shows every step your AI agent takes after you send it a message: each model call with its reasoning, each tool call, each subagent. You can view a run as a Network-panel-style timeline or as a node graph. It runs on your machine, and nothing leaves it.

Thanks for testing. It takes about five minutes to set up.

## 1. Start it

Unpack the download. Inside is one program, `agent-tracker` (`agent-tracker.exe` on Windows).

- **Windows:** double-click `agent-tracker.exe`. If SmartScreen says it protected your PC, click **More info**, then **Run anyway**. The build isn't signed yet.
- **macOS:** in Terminal, in the unpacked folder, run `xattr -d com.apple.quarantine agent-tracker`, then `./agent-tracker`. The first command clears the "can't be opened" block that macOS puts on unsigned downloads.
- **Linux:** run `./agent-tracker`.

Your browser opens http://127.0.0.1:4318. Click **Play demo runs** to see three sample runs arrive live. Leave the program running while you work. Starting it again just opens the browser on the running copy.

## 2. Connect Claude Code and Codex

In a terminal, in the folder with the program, run:

```
agent-tracker install
```

On Windows, run `.\agent-tracker.exe install`; on macOS and Linux, `./agent-tracker install`.

This adds the tracker's hooks to `~/.claude/settings.json`. It also adds them to `~/.codex/hooks.json` if you use Codex. Your other settings stay as they are, and the old file is kept beside it as `.bak-agent-tracker`.

- **Claude Code**, in the terminal or the desktop app's Code tab, picks up the hooks without a restart. Send a prompt and watch it appear.
- **Codex:** run `codex`, type `/hooks`, and approve the new hooks once. Codex skips hooks nobody approved.

Hooks fire even while the tracker is off. Claude Code then notes that a hook failed and carries on without slowing down; Codex waits about a quarter of a second per hook.

## 3. Things to try

- Give Claude Code a task that uses subagents, for example "use two subagents to review this folder". Then open the **Graph** view and click a node to trace its path.
- In the **Network** view, drag across the timeline to clip part of the run, and filter with `kind:llm` or `agent:Explore`.
- Open a model call and read its reasoning and reply in the details panel.

## Claude Code on the web

Sessions on claude.ai/code, and the desktop app's cloud sessions, run on Anthropic's machines. A tracker on your laptop can't hear them, so they need a tracker on a server you control. See **Hosting** in the repository's README. claude.ai chat itself has no hooks and can't be tracked.

## Where your data is

Traces are saved in a per-user folder, printed when the program starts:

- Windows: `%LOCALAPPDATA%\agent-tracker`
- macOS: `~/Library/Application Support/agent-tracker`
- Linux: `~/.local/share/agent-tracker`

They include your prompts, tool output and the model's reasoning, in plain text. Delete a trace from the list, or delete the folder to remove everything.

## Remove it

```
agent-tracker uninstall
```

This takes the hooks back out of Claude Code and Codex. Then delete the program and the data folder.

## Feedback

Tell us what was confusing, what broke and what you wished it showed. Open an issue in the GitHub repository and include the output of `agent-tracker --version` and your operating system. A screenshot of the view you were looking at helps most.
