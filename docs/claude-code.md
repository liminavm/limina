# Claude Code in this repository

`CLAUDE.md` is the project guide for every human and every coding agent, and it cites only
files in the repository. This file holds what is specific to
the Claude Code harness: its sandbox, its tools, and where its per-project memory lives. Other
harnesses need their own equivalent of each item below; nothing here changes what the
project guide asks for.

## The shell tool is sandboxed

The Bash tool runs with no network and cannot write outside the repository. Pass
`dangerouslyDisableSandbox: true` for anything that needs either: `git` fetch/push, cloning
or building `third_party/`, spike builds, and every run that touches `hv_vm_*` (the HVF boot
suite via `scripts/run-suite.sh`, `cargo xtask run`, any boot script). A codesigned worker
is still required; the sandbox flag lifts the tool's confinement, not the entitlement check.

## Asking the user to act or look

When the next step needs the user (interact with the VM window, eyeball the screen, plug in
hardware, run a host command, an interactive login), ask with the `AskUserQuestion` tool. It
puts an un-missable prompt in front of them and blocks until they answer; a request in prose
gets missed when they are not watching the stream, and a timed probe then returns nothing.
Give the options as the observations you expect ("nothing responds / mouse only / keyboard
only / both work"). For a brief window such as the GRUB countdown, ask before launching the
run, so they are positioned when it arrives.

Driving the limina window yourself is fine and needs no permission: osascript System Events
scripting against the `limina` process clicks real AX buttons and delivers key+modifier
combos. A synthetic lone modifier keystroke and a synthetic click on a custom content view
may not land; for those the human is the oracle.

For a command the user must run themselves in the session, suggest `! <command>` in the
prompt; the output lands in the conversation.

## Temporary files

Use the session scratchpad the harness names in its system prompt, never `/tmp`. Anything
worth keeping (a probe, an oracle, a negative result with a lesson) goes under `spikes/` or
the relevant crate and is committed, per the project guide.

## Memory

Claude Code keeps a per-project memory directory outside the repository, indexed by its
`MEMORY.md`. That is where facts that must not enter the public tree live: machine
names and access maps, the dogfood hosts' boundaries, incident histories, and the
verified moment a fix was seen working. The project guide never cites a memory; a memo
in that directory maps its sections to the memories that extend them. Names of the
user's machines and home directory are on a private pre-commit deny list
(`.git/local-pre-commit`, not tracked); `git commit --no-verify` skips it, so run it by
hand on the staged diff whenever hooks are bypassed.

## Session hygiene the suite depends on

`cargo build` and a hook-running `git commit` relink the debug binaries under a live HVF
suite. While a suite runs (`scripts/run-suite.sh` refuses to start a second), commit
docs-only work with `--no-verify` and hold code commits. Other sessions and other agents
share this checkout and this host: never kill a `limina-vmm` you cannot match to your own
disk path with a full `ps -o pid,lstart,command`, and never touch the dogfood Mac or its
guest without an explicit request.
