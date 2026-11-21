# Probe sessions

Each probe is a small, cheap session that exercises one feature, carrying a unique marker so `survey.py --marker` can find exactly which file, line or row it produced. Generate the markers once and keep them in the notes file:

```bash
for f in basic tool reason resume sub subchild fork compact title cwd abort inject revert; do
  echo "$f SLIMEPROBE-$f-$(openssl rand -hex 3)"
done
```

Run probes from fresh directories outside any repo, so no repo instruction file is injected and cwd is recognizable: `~/wrksp/.slimeprobe/AGENT/a` and `~/wrksp/.slimeprobe/AGENT/b`. Put a small `notes.txt` in each for tool probes to read.

## Matrix

Skip a row only when the docs or source show the harness lacks the feature; record "unsupported" with the citation.

| Probe | How | What to record |
| --- | --- | --- |
| basic | Headless single prompt containing the marker; ask for a one-word answer | Store path and naming, header/identity record, native session id vs filename, cwd field, timestamp format (ISO, epoch ms/s), user/assistant record shapes |
| tool | Ask it to run `ls` and read `notes.txt`, mentioning the marker | Tool call and tool result records: how calls and results pair, where the command and output live, error results |
| reason | Same with a reasoning/thinking model or flag enabled | Reasoning blocks: separate record or content block, encrypted or plain, summary vs full |
| resume | Resume the basic session (`--resume ID`, `--continue`) and send a second marker | Appended to the same file/rows or a new session? New id? Pointer back to the original? |
| sub | Ask it to delegate to a subagent/task tool; tell the subagent to include a second marker in its reply | Where child transcripts live (separate file, subdirectory, same file, separate rows), parent link fields, what marks child records as automated, whether the parent holds a copy of the child's result |
| fork | Fork or branch the session (`/fork`, `--fork-session`, branch command, edit-and-resend) | New id, copied or referenced history, parent/fork-of fields, whether history duplication would double-count search hits |
| compact | Fill a little context, then trigger compaction (`/compact` or equivalent) | Summary record type, whether pre-compaction records remain, boundary markers, whether the summary is injected as a user message (it must count as automated) |
| title | Rename the session, or let the harness auto-title it | Native title field and where it lives (header, separate record, companion file, index file); which record wins when renamed twice |
| cwd | Run the basic probe in `b` as well | Confirms the cwd field and any project/slug layout derived from it |
| abort | Start a long tool call and interrupt it (Ctrl-C / Esc) | Partial or dangling records, interruption markers |
| revert | Send a marked turn, then undo it (`/revert`, `/undo`, edit-and-resend) | Whether undone records stay in the store, and what marks them as off the live branch |
| headless vs interactive | Compare a `-p`-style session with an interactive one that exits normally | Records or companion files only one mode writes (some harnesses export transcripts only on interactive exit) |
| env override | Point each candidate env var at an empty scratch dir and run a read-only listing command (e.g. `CLI list`) | Which variables the harness really honors and the exact path it appends; third-party tools' variables are often their own convention |
| inject | Inspect any probe for content the user did not type: instruction files, system reminders, hook output, slash-command expansions | Which user-role records are automated and how to detect them reliably |

Also note: archive/delete behavior (where archived sessions go, if anywhere), companion files that change on rename or settings edits (these must join the source fingerprint), and SQLite stores' WAL usage.

## Driving the harness

**Headless** (preferred for basic/tool/reason/resume/sub/cwd): use the documented non-interactive mode, e.g. a `-p`, `exec` or `run` subcommand. Wrap each call in `timeout 300`. Keep the flags in a shell array (`P=(--model m --flag v); CLI "${P[@]}" -p "..."`); zsh does not word-split a plain `$P`.

**Permissions:** use the harness's mode that auto-approves only read-only tools, and phrase tool probes as read-only ("list the files and read notes.txt with your read-only tools"). Never pass a bypass-all/"dangerous" mode: this session's own permission layer can rightly refuse to launch an agent with every tool auto-approved. Headless runs in an untrusted directory may need an explicit trust-skip flag; the interactive TUI shows a trust prompt for a fresh probe directory — confirm it, since the agent created that directory.

**Interactive-only features** (compact, fork, rename, abort usually): drive the TUI in tmux.

```bash
tmux new-session -d -s slimeprobe -x 200 -y 50 -c ~/wrksp/.slimeprobe/AGENT/a 'HARNESS_CLI'
tmux send-keys -t slimeprobe 'SLIMEPROBE-compact-1a2b3c: reply with the word ACK followed by 6*7' Enter
timeout 300 bash -c 'until tmux capture-pane -p -t slimeprobe | grep -q ACK42; do sleep 2; done'
tmux send-keys -t slimeprobe '/compact' Enter
tmux capture-pane -p -t slimeprobe | tail -40
tmux kill-session -t slimeprobe
```

Wait for a computed token (`ACK42`) rather than the marker, because the typed prompt already shows the marker in the pane. Some TUIs need `Enter` sent separately after a short pause, or a confirmation keypress; check `capture-pane` after each step.

## After each probe

1. `python3 -I "$SURVEY" STORE_ROOT --marker MARKER` — locate the records. For SQLite, also keep a `$NOTES/dbq.sh` that copies the DB and WAL into a fresh 0700 temp dir and runs `sqlite3` on the copy, then select structure only: ids, parent links, roles, discriminators, `length(...)` of text, `instr(text, 'SLIMEPROBE-...') > 0` flags — never the text itself.
2. `python3 -I "$SURVEY" --record FILE:LINE --marker MARKER` — inspect one record's structure with free text redacted.
3. If records form a tree (parent ids, branch heads), trace the live branch with a recursive CTE from the head pointer and print which rows are on it. Copies, retries, reverted turns and pre-compaction history often sit off the live branch, and reading every row would duplicate or resurrect them.
4. Write the finding in the notes file under the probe's name: file/row location, record discriminators, linking fields. Structure only, never transcript text.
