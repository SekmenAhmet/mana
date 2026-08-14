# mana

Orchestrateur d'agents IA de coding en CLI/TUI (Rust). Lance un agent CLI (Claude Code en v1) comme Project Manager, qui decoupe le travail en taches et delegue a des sous-agents executor/reviewer.

## Build

    cargo build --release

## Usage

    mana install                 # enregistre les CLIs disponibles
    mana doctor                  # verifie la config
    mana launch claude           # lance Claude Code en PM dans le TUI

## Manual QA checklist (v1, requires a real `claude` install)

1. `cargo run -- install` — select `claude`, confirm it's written to `~/.mana/config.yaml` with a real version/path.
2. `cargo run -- doctor` — confirm no issues reported.
3. From a scratch project directory, run `cargo run -- launch claude`. Confirm:
   - `~/.mana/projects/<dirname>/{tasks,logs,reviews}` and `subagent-lock.yaml` get created.
   - The PM's chat output appears in the terminal.
   - Typing a message and pressing Enter forwards it to Claude Code.
4. Ask the PM (via chat) to create a trivial task and run `mana launch --subagent claude --role executor --assign <uuid>` itself. Confirm:
   - `subagent-lock.yaml` gains an entry.
   - `logs/<agent-uuid>.jsonl` is created and ends with `{"status":"done",...}` once the executor finishes.
5. Repeat with `--role reviewer` on the same task-uuid. Confirm `reviews/<task-uuid>.md` is written with the minimal validated format (or the rejected format with a numbered list).
6. Confirm the PM's PTY receives the `[mana] Review disponible pour ...` notification line after step 5.
7. Press `Ctrl+G` in the TUI — confirm the graph pane appears with a node per sub-agent launched, correct role label (EXE/REV) and status symbol (running vs done).
