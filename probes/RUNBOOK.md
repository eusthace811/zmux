# Phase 0 runbook

Nothing gets built until these answer. Two can kill or reshape the project.

Everything runs on the Windows side. Stage: `C:\Users\eusth\src\zmux-probes`

---

## Order matters

### 1. PROBE C — daemon survival  ← **THE go/no-go**
**Zero dependencies.** Runs even if Artifactory is blocked. Already compiled:
`probe-c\target\release\probe-c.exe`

In **Windows Terminal**:
```
probe-c.exe spawn detached
```
…then **close the tab**, reopen, and:
```
probe-c.exe report
```
Repeat with `spawn breakaway`. Then repeat both, closing the **whole window**
instead of a tab. Then repeat the whole thing from a **Git Bash (mintty)** window.

Reading it:
- `ALIVE` lines timestamped *after* you closed → that mode survives. **Persistence works.**
- Heartbeats stop exactly at the close → the job killed it.
- `spawn breakaway` returning ACCESS_DENIED → the job forbids breakaway.

**If neither mode survives from either host:** under the no-WMI policy
(DESIGN.md §0.2) we do not have a persistent multiplexer. That is a real
decision point — either take WMI to corporate security as an approved
exception, or drop to a single-process multiplexer with no detach. Do not
start Phase 1 until this is answered.

### 2. PROBE A/D/E — network, host, Windows build
In a **Git Bash** window:
```
sh /c/Users/eusth/src/zmux-probes/probe-ade.sh
```
- **D** decides whether mintty works as the host (DESIGN.md §0.1 — this is the
  one that restores your original preference).
- **A** decides caching-remote vs allowlist. Allowlist → `cargo vendor`.
- **E** records the ConPTY floor you are on.

### 3. PROBE B — ConPTY + Git Bash spike
Needs Probe A to pass first (it pulls `portable-pty`). Written once A is green;
it must exercise **clean teardown**, not just spawn — see DESIGN.md §2.5 on the
`PSEUDOCONSOLE_INHERIT_CURSOR` shutdown deadlock.

---

## Report back

| Probe | Question | Answer |
| --- | --- | --- |
| C / WT tab | survives? | |
| C / WT window | survives? | |
| C / mintty window | survives? | |
| C breakaway | refused? | |
| D | mintty = real console? | |
| A | caching or allowlist? | |
| E | Windows build | |
