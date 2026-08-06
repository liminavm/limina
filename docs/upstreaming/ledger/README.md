# Patch-audit ledger — schema and protocol

One file per patch series (`<series>.md`), one row per patch, answering six questions:
does upstream tip still need it, is there an issue, is there an MR/PR, is it security
sensitive, should it fold into an earlier patch, and (guest-side, enhanced-tier only)
why is a guest-side change required at all.

**Rows are keyed by the commit Subject line, never the ordinal.** Ordinals renumber on
every series re-export (this bit four memory files in the 2026-08-03 audit); the Subject
survives folds and rebases. The ordinal column is informational only and may be stale.

Seeded from `scripts/patch-audit/build-ledger.py` (phase 0), then filled in by later
phases. The 2026-07-04 inventory in `docs/upstreaming/00-obvious-fixes-and-security.md`
and the `limina-upstreaming-triage` memory pre-date this ledger and seed its judgment
columns; where they disagree with a fresh check, the fresh check wins.

## Columns

| column | values |
|---|---|
| `ord` | current ordinal in the series (informational; may drift) |
| `subject` | the row key — commit Subject, verbatim |
| `files` | top files touched (+N more) |
| `diag` | `DIAG` if the patch carries `/tmp/limina-` probe hooks (strip before upstreaming) |
| `need` | `needed` / `superseded-upstream@<sha>` / `applies-but-redundant` / `conflicts-on-tip` |
| `checked` | upstream SHA + date the `need` verdict was made against (a verdict without its SHA is unfalsifiable) |
| `issue` | URL / `none-yet` / `n/a` (not upstreamable) |
| `mr` | URL / `none-yet` / `blocked-on-disclosure` |
| `sec` | `no` / `yes:<class>` (guest-triggerable host memory-safety, info leak, …) |
| `fold` | `standalone` / `fold-into:<subject>` / `strip-diag-first` |
| `tier` | `host` / `guest-stock` / `guest-enhanced` (enhanced → justification section below the table) |
| `disp` | `upstream-now` / `upstream-after-cleanup` / `carry` (limina/macOS-specific) / `drop` |
| `notes` | free text, keep short; long findings go in a section below the table |

## Phases

- **Phase 0 — mechanical skeleton** (`build-ledger.py`): subjects, files, DIAG flags.
  Deterministic, no network, no judgment. Re-runnable; it refuses to clobber filled rows.
- **Phase 1 — needed-on-tip**: per series, in a scratch worktree of the `third_party/`
  checkout: fetch upstream, `git rebase --onto <tip> $UPSTREAM_BASE <limina-branch>`,
  record per-patch conflicts + `git range-diff` drift. A patch can *apply cleanly and
  still be redundant* (upstream fixed it differently), so also grep upstream's log since
  `UPSTREAM_BASE` for the same files/functions and read what hits. No builds — compile
  validation of the rebased series is a separate step scheduled when the build is free.
- **Phase 2 — external research**: issues/MRs per tracker (see browser protocol below).
  Record permalinks. Where nothing exists and `disp` is `upstream-*`, "file issue"
  becomes an action item; the audit records gaps, it does not post.
- **Phase 3 — judgment**: security classification (disclosure logic from
  `limina-upstreaming-triage`: guest-triggerable host memory-safety →
  `blocked-on-disclosure`, never straight to a public MR); the fold plan (executed as
  ONE re-export per series at the end, recording a subject→subject mapping); the
  enhanced-tier rubric.

## Enhanced-tier rubric (question 6)

For each `guest-enhanced` patch, a short section below the table with a fixed shape:
**(a)** capability provided; **(b)** stock-guest behavior without it (must still boot —
checkable); **(c)** host-side alternative considered and why it lost, with numbers where
we have them; **(d)** exit strategy — once it lands upstream, the enhanced-tier
requirement dissolves into "a new enough kernel/mesa" and the stock tier absorbs it.

## Browser / research protocol (preflighted 2026-08-03)

Facts established by the preflight:

- **freedesktop GitLab (virgl, mesa/KK, mutter) is behind Anubis** anti-bot: WebFetch
  and curl are blocked outright. Real Chrome passes it (self-resolving JS proof-of-work,
  ≤8 s, sometimes needs one reload; the clearance cookie then carries across pages).
  Chrome is therefore REQUIRED for freedesktop research.
- **GitHub also goes through the browser** (user decision 2026-08-03: one modality is
  simpler; the API/`gh` path stays as a fallback — note unauthenticated curl = 60 req/h,
  `gh` not installed). Exception: `curl` to `raw.githubusercontent.com` for plain-text
  file fetches is fine. Note the repo move: `containers/libkrun` → `libkrun/libkrun`.
- The Chrome profile was **not logged in to GitHub** but **may be logged in to GitLab**
  (edit affordances rendered) — the read-only rules below are load-bearing, not
  decorative.
- Two subagents in separate tabs of the shared MCP tab group did not interfere
  (2-way test). Keep browser-using agents to **2–3 concurrent**, each in its OWN tab.
- GitHub's React-rendered lists are invisible to `get_page_text`; fall back to the
  read-only `read_page` accessibility tree.

Rules block for every research agent prompt, verbatim:

> - READ-ONLY web use: navigate and read only. NEVER click buttons, submit forms, use
>   form_input, use the computer tool, comment, react, subscribe, star, approve, or take
>   ANY action that could post or mutate state on any website. The browser may be logged
>   in to real accounts — treat every page as look-don't-touch.
> - Drive searches via URL query parameters only, never via on-page search boxes.
> - Own tab only: tabs_context_mcp once, tabs_create_mcp for YOUR tab, pass its id
>   explicitly to every call, never touch another tab id, close your tab when done.
> - Anubis interstitial ("Making sure you're not a bot!"): wait and re-read; it solves
>   itself. Do NOT click anything.

## Method lessons (linux pilot, 2026-08-03)

- **Judge supersession against the BUG, not the diff's file paths.** 0005's upstream fix
  landed in `mm/page_reporting.c` — a file our patch never touches; the balloon file's
  history alone said "still needed" and would have been wrong. Same-file log grepping is
  a lead-generator, never a verdict.
- **A `need` verdict resting on semantic equivalence must record its premise chain, not
  just a SHA.** The series README's 0001 "superseded" verdict cited a line range but not
  the premise ("imported ⇒ covers compositor FBs"); one hop of verification
  (`drm_gem_is_imported` = cross-device only; self-import short-circuits) overturned it.
- **Search for the REJECTION, not just the absence.** Twice the decisive find was a
  declined submission shaped like ours (Finkelstein's hardcoded alignment; the 2021
  DEFERRED_OUT_FENCE uapi RFC) — those tell you the objection profile a submission must
  avoid, which "no hits" never can.
- **Check whether upstream solved it with a mechanism we should ADOPT** (0004 →
  F_BLOB_ALIGNMENT: the audit's output is a limina work item, not an MR).
- Lore craft: the Atom-feed view (`&x=A`) reads full message bodies read-only without
  clicking; `get_page_text` errors on the `&x=t` nested view (use plain search +
  `read_page` for hrefs); short-SHA github `/commit/` URLs 404 (use lore subject+date
  search); a file-history page yields the file-touch SHA, not repo HEAD — label which
  one `checked` records.
- Fleet mechanics: the shared Chrome tab group is noisy but safe IF every call passes an
  explicit own-tab id — a bare `navigate` would hijack a sibling's page; keep that rule
  hard. Prompts should state patch filenames may drift from prompt text (`ls` first) and
  that `curl` to raw.githubusercontent needs the sandbox disabled.

Additions from the linux fork migration (2026-08-03) — all three found by *doing* the
migration, which is why a rebase is an audit tool and not just chores:

- **A tolerant apply is a silent-failure machine.** `patches/linux/0001` had stopped
  applying at the 7.1.x bump and the build script printed `SKIP …` into logs nobody read,
  so a patch we believed shipped had been absent from every kernel for months. Under the
  fork model a change is on the branch or it is not — no third state. Where a tolerant
  apply must survive (test kernels built at other tags), treat every skip as a finding to
  chase, not a normal outcome.
- **"Superseded upstream" is not a licence to drop.** The page-reporting fix was merged,
  Acked, and Cc: stable — and still absent from our base tag, so dropping our patch would
  have shipped the UAF back. Check the *base you are actually building*, then backport the
  upstream commit rather than keeping your own shape (upstream's covered cases ours did not).
- **A citation is a claim; run `git log` before repeating it.** This ledger attributed the
  XRGB-only plane list to a 2017 series through several revisions. It is 2018's
  `42fd9e6c29b3`, and the narrowing is a side note inside a big-endian fix — which changes
  how the submission should be pitched.

Additions from the kosmickrisp series (2026-08-03):

- **GitLab's `/-/raw/` blobs and `/api/v4/` endpoints are NOT Anubis-gated** — plain curl
  works for file contents, per-path commit history (`/api/v4/projects/176/repository/
  commits?path=…` for mesa/mesa), tree listings, and MR/issue search. Only HTML pages
  need Chrome. Curl-first; the 0001-monolith unit ran entirely without a browser.
- There is **no GitHub mirror of mesa** (mesa3d/mesa raw = 404). Don't budget for it.
- **Mine `Fixes:` trailers, not search**: the regressing commit's own GitLab page lists
  "mentioned in" commits — that's how 0016's supersession was found. Tracker search is
  weak (work_items search missed an item its own commit trailer named); search both the
  `kk:` prefix and the full driver name.
- GitLab blob search (`scope=blobs`, `path:`/`filename:` filters) is fresh and cheap for
  single-line tip checks; raw blobs of large files defeat `get_page_text`.
- GitLab is an SPA: the first `get_page_text` after `navigate` may return the PREVIOUS
  page — verify the URL in the result before trusting content. `.patch` URLs in Chrome
  trigger downloads (stray tabs) — use raw blobs or the commits API instead.
- **Other trackers (imago, mutter series):** imago upstream is GitLab `hreitz/imago`
  (crates.io repository field), not GitHub. `gitlab.gnome.org` `/api/v4/` + `/-/raw/` work
  unauthenticated with curl (no Anubis) BUT issue notes/discussions return 401 on REST —
  the unauthenticated **GraphQL** endpoint (`/api/graphql`, `workItems` query) serves the
  full discussion, so it's how you read a rejection's rationale. crates.io API rejects the
  default curl UA — pass `-A <anything>`.
- **Actively-developed upstreams make "unchanged since base" verdicts perishable** —
  LunarG reinvented four of our KK patches within five weeks and one overlap (depth_clip)
  appeared as a Draft MR the day of the audit. Re-verify every verdict at MR/rebase time;
  for hot files the ledger records the touch date as a staleness alarm.

## Pilot order

`linux` (6 — exercises the enhanced-tier rubric and the hardest research modality,
lore/dri-devel, at minimum scale) → `kosmickrisp` (19, single tracker, several
patches already claiming MR-ready) → `mesa`/`imago` → the two big series
(`virglrenderer` 58, `libkrun` 126), one series per session, ledger committed as it
fills so every session resumes from durable state.

edk2 was a script (`apply-virtio-keyboard.py`) rather than a format-patch series at
audit time; since 2026-08-06 it is fork-model (`liminavm/edk2`, 6 commits, task #22)
and any upstreaming rows would key on those commit subjects.
