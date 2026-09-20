# Artifactory checklist — RESOLVED 2026-09-19

> **Outcome: the corporate Artifactory has a Cargo remote and all 76 crates
> resolve, `shared_library` included. It is a caching proxy, not a curated
> allowlist.** Nothing below is blocking. Kept as a record of the closure and
> as a starting point if the policy ever changes.
>
> One recommendation survives on *security* grounds, not availability:
> vendor `portable-pty` from herdr's tree to drop `shared_library` and close
> the DLL-hijack path (see the last section).

---

**All 76 are Rust crates from crates.io.** "Crate" and "cargo package" are the
same thing — there is no other package type anywhere in this project. No npm,
no PyPI, no Maven, no NuGet.

## Ask this before asking about any individual crate

> **Does the corporate Artifactory have a Cargo repository provisioned at all?**

This is the question that actually decides things, and it is a different
question from "is `vt100` available". Artifactory supports Cargo repositories,
but it is a **much less commonly deployed repository type** than Maven, npm,
PyPI or NuGet. Plenty of large enterprises proxy Java/Python/Node/.NET and have
no Rust story whatsoever — not because Rust is blocked, but because nobody has
ever asked.

Three possible answers:

| Answer | What it means | What we do |
| --- | --- | --- |
| **A Cargo remote repo exists**, proxying crates.io | Caching proxy. Individual crate availability is a non-question. | Nothing. Use it. The per-crate list below is then pure diagnostics. |
| **Cargo repo exists but is curated/allowlisted** | Each crate needs approval | Work the Tier 1 list below, or vendor |
| **No Cargo repo at all** | Most likely outcome in a locked-down shop | **Vendoring is not a fallback — it is the plan** (DESIGN.md §8.1 rule 2). Requesting a new repo type is a long procurement conversation; `cargo vendor` skips it entirely. |

### Two follow-ups for the platform team

1. **Sparse index or git index?** Modern cargo defaults to the sparse HTTP
   protocol (`sparse+https://…`). Older Artifactory Cargo repos only serve the
   git index, which needs different config and is slower. Ask which.
2. **What does the `.cargo/config.toml` look like?** They should be able to
   hand you something shaped like:

   ```toml
   [registries.corp]
   index = "sparse+https://<artifactory-host>/artifactory/api/cargo/<repo>/index/"

   [source.crates-io]
   replace-with = "corp"

   [source.corp]
   registry = "sparse+https://<artifactory-host>/artifactory/api/cargo/<repo>/index/"
   ```

   plus a credential in `~/.cargo/credentials.toml`. If they cannot produce
   this, the answer to the question above is "no Cargo repo".

---

**Only needed if you intend to build from the corporate registry.** If we
follow DESIGN.md §8.1 rule 2 (`cargo vendor --versioned-dirs`, committed), the
build needs zero network and none of this matters. Check anyway — it is cheap,
and it tells you which mode you are in.

Closure is 76 crates. Do not check 76. Obscurity predicts absence from a
curated mirror, so check from the bottom of the download distribution up.

## Tier 1 — project-threatening. Check these first.

| Crate | Downloads | Why it matters | If blocked |
| --- | --- | --- | --- |
| **`recvmsg`** | 6.4M | **Lowest in the whole tree.** Hard dep of `interprocess`. | Falls with interprocess ↓ |
| **`vt100`** | 11.5M | The VT engine. No drop-in replacement. | Vendor it — 3 deps, tiny, no build script |
| **`serial2`** | 12.2M | `portable-pty` dep | Falls with portable-pty ↓ |
| **`doctest-file`** | 12.7M | `interprocess` dep | Falls with interprocess ↓ |
| **`interprocess`** | 14.6M | Named-pipe IPC | **Good fallback:** `tokio::net::windows::named_pipe` — tokio is already a dep, so zero new crates |
| **`portable-pty`** | 15.8M | **ConPTY. The foundation.** | **Best fallback:** herdr already vendors it at `herdr/vendor/portable-pty/` — copy that tree |
| ~~`shared_library`~~ | 17.0M | `portable-pty` dep | **RESOLVED — see below. We remove this dep entirely.** |
| `filedescriptor` | 44M | `portable-pty` dep | Falls with portable-pty |
| `vte` | 76M | `vt100`'s parser core | Vendor with vt100 |

## Tier 2 — substitutable, but expensive

| Crate | Downloads | Notes |
| --- | --- | --- |
| `ratatui-widgets` | 19.3M | ratatui family |
| `line-clipping` | 19.3M | ratatui |
| `kasuari` | 20.5M | ratatui layout solver |
| `ratatui-core` | 20.6M | ratatui |
| `instability` | 46M | ratatui |
| `unicode-truncate` | 51M | ratatui |
| `ratatui` | 52M | Compositor + diff engine (§0.4) |
| `crossterm` | 193M | Terminal backend |

If the ratatui family is blocked, the fallback is `vt100::rows_diff` row
positioning (§0.4 shows it is offset-safe) plus hand-rolled chrome — but you
lose overlay support, which copy-mode selection needs.

## Tier 3 — do not bother

`serde`, `tokio`, `bincode`, `bitflags`, `libc`, `log`, `anyhow`, `memchr`,
`parking_lot`, `smallvec`, `syn`, `quote`, `proc-macro2`, `thiserror`,
`windows-sys`, `winapi`, `itoa`, `unicode-width`, `bytes`, `tracing`, … all
>100M downloads. If Artifactory carries Rust at all, it carries these.

## The one-command answer

Rather than checking names by hand, resolve the whole graph on the corporate
network:

```
cargo new probe-a && cd probe-a
cargo add portable-pty vt100 crossterm tokio ratatui interprocess bincode serde bytes tracing
cargo fetch
```

Success = caching remote; the count never mattered.
Failure = curated allowlist; go to `cargo vendor` and stop caring.


---

## Resolved: `shared_library`

**Name:** it is `shared_library` with an **underscore**. crates.io normalizes
hyphens/underscores on lookup (both spellings resolve), but Artifactory's index
may not — search the underscore form.

**Why it probably looked missing:** a caching remote shows *nothing* until
something actually fetches it. An empty search result is the expected state for
a crate nobody has pulled yet, not evidence that it is blocked.

**It does not matter either way — we are removing this dependency.**

`shared_library` 0.1.9 was last published **2018-07-10**. An 8-year-unmaintained
crate is exactly what a corporate dependency scanner flags. More importantly,
stock `portable-pty` uses it to load `conpty.dll` **through the relative DLL
search path** — a DLL-hijack vector, and the thing herdr's patch 0001 exists to
fix (DESIGN.md §2.3).

herdr's vendored tree already has that patch applied — verified, zero
references remain:

```
$ grep -rn "shared_library" herdr/vendor/portable-pty/src/ herdr/vendor/portable-pty/Cargo.toml
(no matches)
```

The patch swaps it for direct Win32 calls plus a hash check:

```
-shared_library = "0.1"                    -use shared_library::shared_library;
+sha2 = "0.10"                             +use sha2::{Digest, Sha256};
                                           +    GetModuleHandleW, GetProcAddress, LoadLibraryExW
```

**Action:** take herdr's vendored `portable-pty` rather than the crates.io
release. `shared_library` leaves the tree; `sha2` enters it (RustCrypto,
actively maintained, and already a herdr dependency). Net dependency count
unchanged, one stale crate and one DLL-hijack vector gone.

This was already the recommendation in DESIGN.md §2.3 for security reasons. The
Artifactory result just makes it unambiguous.
