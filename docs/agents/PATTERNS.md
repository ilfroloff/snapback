# Patterns: how to build new things

Active implementation rules that repeat across the codebase. For *what the
pieces are*, see [ARCHITECTURE.md](ARCHITECTURE.md); for the *session format*,
see [DOMAIN.md](DOMAIN.md). These are the conventions to match when editing.

## 1. Fail-soft over external input

The JSONL format is external and undocumented, so treat every read as hostile:

- Parse each line as `serde_json::Value` — **never** hard-typed
  `#[derive(Deserialize)]` structs. Schema drift must never be fatal.
- Skip an unparseable line, a non-object value, or an unreadable file; keep
  going. One bad line never aborts a file; one bad file never aborts the scan.
- The same discipline governs both `claude agents --json` readings
  (`agents::parse_agents_json`, shared by the `--all` board poll and the bare
  liveness probe): a missing binary, non-zero exit, non-JSON, or a non-array top
  level all collapse to an **empty set**, never a panic. There is exactly one
  place the wire shape is interpreted per source.
- **Fail-soft has a DIRECTION, and it is chosen per consumer.** The display
  classifier fails toward *active* (drift must not hide a busy session);
  `agents::live_agents` fails toward *not live* (empty ⇒ plain resume ⇒
  claude's own check backstops it). Opposite, and both correct — a classifier
  facing an unknown bucket should assume the worst, whereas a membership test has
  no bucket to be unsure about and an authority one step downstream. State the
  direction and its reason whenever you add a fail-soft path.
- **A fail-soft answer may COLLAPSE premises — then say only what you observed.**
  `live_agents`' empty map means "finished" and "could not ask" alike, so the
  Attach refusal (`resume::ATTACH_NOT_LIVE`) is worded for the report ("claude no
  longer reports this session as a running agent"), never for a cause the probe
  cannot distinguish. Do not fabricate certainty a degraded signal cannot carry;
  name the routes that hold in every collapsed world instead.
- DEFINED-agent discovery (`defined_agents`) is the same: a missing
  `.claude/agents` dir, an unreadable file, or malformed YAML frontmatter is
  skipped (`parse_frontmatter` returns `None`), collapsing to a (possibly empty)
  list — never a panic. The frontmatter is hand-parsed (no YAML crate) to keep the
  crate dependency-free, exactly like the hand-rolled markdown pass in
  `store::preview`.
- **Reading is the default, but not the whole story.** `snapback` is
  overwhelmingly a reader of a hostile external store, and the Claude store stays
  read-only save for the one gated hard delete. It does, though, have exactly one
  file of its OWN — the hidden-session id set (`hidden::load_hidden`) — and that
  read gets the identical fail-soft discipline: a missing file or an unparseable
  line collapses to an empty set, never a panic, and its write is atomic (temp +
  rename) so a crashed write can never leave a half-file that fails the next read.
  The two write postures (owned state; gated store mutation) are the AGENTS.md
  critical rules; their store/layout mechanism is in
  [DOMAIN.md](DOMAIN.md#snapback-owned-state-srchiddenrs).

## 2. Authoritative-from-file

`cwd` and `sessionId` come from **inside** the file, never decoded from the
`<encoded-cwd>` folder name (the `/`→`-` encoding is lossy). At hand-off time
`resume::read_authoritative` re-reads them fresh (the on-disk file may have
changed since load) via the same `parse::parse_file`, so parsing lives in one
place. A file with no `cwd` is not a resumable session — refuse rather than
guess.

## 3. Pure core, thin impure drivers

Decision logic is pure and unit-tested; side effects sit in thin wrappers over
it. Follow this split when adding behavior:

- Pure, tested: `resume::plan` / `plan_from_parts` / `build_argv` /
  `build_new_argv` / `status_for_exit`; every decision in `send` — `reply_gate` /
  `interrupt_gate` (the whole routing tree, asserted with no process spawned),
  `build_send_argv` / `build_stop_argv` / `build_bg_launch_argv`, `plan_send` /
  `plan_bg_launch`, and the `status_for_output` / `status_for_failed_send` /
  `status_for_stop` / `status_for_bg_launch` mapping;
  `compose::compose_key_to_action`; `defined_agents::select_agents` /
  `parse_frontmatter`; `agents::classify` and the outputs derived from it
  (`qualifier_copy`, the shared banner/list-row phrase that `friendly_status`
  fuses onto the kind label, plus `is_active`) and both argv builders (`agents_argv` /
  `live_agents_argv`) and `agents_from_output` (the shell-out's
  non-zero-exit-means-no-signal decision, split from the spawn so it is testable
  without one); `worktrees`' whole trio, split from its spawn the same way —
  `git_worktree_argv` (the invocation is a contract with an external CLI, so it
  is asserted without running one), `set_from_output` (the same
  non-zero-exit-means-no-signal decision, plus non-UTF-8 output rejected WHOLE
  rather than lossily repaired, since a replacement character inside a path is a
  directory that exists nowhere) and `parse_porcelain` (which takes its
  canonicalizer as a PARAMETER, so the parse stays hermetic — no filesystem, no
  git — and is pinned against real captured porcelain bytes); `store::group`'s
  `repo_root_of` / `repo_of` (ONE pure marker scan, two consumers — the root path
  and its label — with inline path fixtures, including a NEGATIVE case that must
  not collapse and a pinned known limitation); `App::in_scope` (the scope
  predicate, which takes the worktree set as a parameter rather than reaching for
  it, so it never resolves git and is tested from a seeded set); `store::lineage`'s `lineage_key` / `head_of` / `fold` (the whole
  fold is one pure fn of `(sessions, filtered, expanded)`, so the `(+N)` board can
  be tested as a list transformation with no terminal and no store);
  `update::key_to_action` / `wheel_target` / `accept_paste` (line-ending
  normalization plus the char-counted cap, fused so neither can be skipped at a
  call site) / `flatten_for_query`; every `App` state transition (incl.
  `pick_default_index`, the agent-picker cycle, `child_indices`, which marks
  the indented rows by reusing `lineage::head_of` rather than re-deriving a head,
  and `delete_confirm_message`, the delete confirm's copy as a function of
  `(members, hidden)` — so the sentence that discloses off-screen lineage members
  is testable without a store, a modal or a terminal);
  `view`'s `wrapped_text_rows` / `clamp_preview_offset` / `preview_split` /
  `centered_rect` / `highlight_runs` / `fit_label` (the marker-vs-label width
  reservation, pure so it is tested as arithmetic rather than only through a
  rendered pane) / `child_msgs` and `fit_child_msgs` (a lineage child's
  turn-count segment and whether the row can afford it — ALL-OR-NOTHING: the
  segment is drawn whole or dropped entirely and never ellipsized, because a
  clipped `171 msgs` reads back as a plausible `17` and a confidently WRONG
  number is worse than none, where a clipped label merely looks clipped. The
  segment folds its own leading gap in, exactly as `lineage_marker` does, so the
  width weighed is the width drawn) / `blink_visible` (the board's ONE pulse phase, a pure fn of
  `App::tick`) / `badge_color` and its `pulse_color` partner (also derived from
  `classify`, but a rendering decision, so the palette sits in the view rather
  than dragging ratatui into the parser layer).
- Thin, impure: `resume::launch` (chdir + spawn + wait), `defined_agents::discover_agents`
  (the FS walk over `select_agents` / `parse_frontmatter`), `worktrees::resolve`
  (spawn + capture, delegating every decision to `set_from_output`), the `watch`
  threads, `tui::run` (draw loop). Keep these small and delegate to tested
  helpers.

The terminal-up **refusal gate** is an instance of this: `resume::check` (and its
sibling `resume::check_new` for starting a fresh session in the launch dir) runs
the pure predicate while the UI is still drawn, so a refusal becomes a board
status with no teardown flash; only a confirmed `Ready` escalates to
`Outcome::Resume` and the impure `launch`.

## 4. Isolate volatile dependencies

All `nucleo` and `memchr` calls live in `src/search.rs` and nowhere else — the
pins are exact and nucleo's API is evolving, so an upgrade touches one module.
The rest of the crate sees only `SearchIndex`, `SearchMode`, and `filter`.
Matching is **substring, not fuzzy**: patterns are built with
`AtomKind::Substring` in code (never from user-typed atom syntax), and the
filter enforces the same rule independently because memmem searches substrings
by nature. When you touch search, preserve the incrementality contract:
`set_query` rebuilds only the small pattern and the per-atom finders per
keystroke; `refresh` rebuilds haystacks only for sessions whose fingerprint
changed.

`results` answers **membership only** — every atom present as a byte substring,
via `memchr::memmem` — and returns candidates in the order given. nucleo is not
on that path; it backs the **highlight seam** (`match_indices`) alone. Do not
re-introduce ranking: `App::order_filtered` re-sorts every result by a **tie-free
total order**, so a rank cannot reach the screen, and computing one cost 76–81%
of each keystroke through nucleo's `Utf32Str` UTF-32 conversion.

Two invariants are easy to break and expensive to get wrong:

- **Smart case is per ATOM, not per query.** `CaseMatching::Smart` makes each
  atom case-sensitive iff *that atom* carries an uppercase char, so `foo BAR`
  folds `foo` but not `BAR`. The decision rides each `AtomFinder`. A per-query
  branch looks equivalent and silently breaks mixed queries — and answering an
  uppercase query from the lowercased haystack is an *inclusion regression*
  (measured: `NPX` finds 6 entries there, nucleo matches 0), not a nuance.
- **Both haystacks are live.** The cased one backs the case-sensitive branch, the
  lowercased one the case-insensitive branch. Neither is dead; deleting the cased
  one breaks every uppercase query.

`gate_atoms` mirrors nucleo's splitter so the filter and the highlight demand the
same atoms; keep them in lockstep. Where the two deliberately diverge (the
upstream non-ASCII tail off-by-one, unicode normalization, the per-atom case
predicate on non-ASCII atoms), the module docs enumerate it and a test pins it.

## 5. Selection and scroll survive reloads

TUI state that must persist across an autorefresh reload is keyed by **stable
`session_id`**, never by list index (`App::selected` is an id; `App::scroll` is
preserved and only clamped). On reload, restore the selection by locating the id
in the new filtered list; if it vanished, clamp the previous position to the
nearest surviving row. Path canonicalization (the scope predicate) runs only on
reload / scope-toggle (`recompute_scope`), never per keystroke.

The preview's own scroll is **bottom-anchored by default**
(`App::preview_follow_bottom` starts true, is re-armed on every selection change
and preview show, and `clamp_preview_offset` then pins to `max_offset`). So
anything that must STAY visible is a **layout row, never a line prepended into
the scrolled `Text`** — a prepended line is scrolled off for any transcript
taller than the pane, which is the normal case. The status banner is the
instance: `view::preview_split(area, has_banner)` carves the pane's inner rect
into a pinned banner row and the transcript beneath it, and returns the WHOLE
inner rect when there is no banner (so a banner-less pane's geometry is exactly
`Block::inner`, unchanged). Three rules follow:

- `preview_split` is the ONE place the banner/transcript geometry is derived.
  `render_preview` draws against its rects and `update::link_under_pointer`
  hit-tests against the same transcript rect. A click resolves through
  `App::preview_scroll` and the width-scoped hit cache, both measured from that
  rect's origin — derive it anywhere else and a click silently opens the wrong
  link. The compose split (`preview_compose_split`) is built ON `preview_split`,
  carving the docked compose zone off the bottom of that same transcript rect
  rather than re-deriving it; a docked compose zone shrinks the transcript, but
  link hit-testing is gated off while composing (`overlay_active`), so the two
  never disagree. Both rects trace back to `preview_inner`, the ONE place the
  pane's border inset is applied — which matters most for the docked compose
  zone, because it then draws a border of its OWN: measure it from the pane's
  OUTER rect and its editor is four columns narrower than whatever measured it,
  so a wrapping draft under-grows and the editor scrolls its own first row away.
  That is why the box's height is asked of the editor rather than modeled here at
  all (see `ComposeState::screen_rows`).
- **A height is ASKED OF THE WIDGET that draws it, never modeled beside it.** Both
  wrapping panes now do this and for the same reason: the compose box asks the
  editor (`ComposeState::screen_rows`), and the transcript asks ratatui
  (`view::wrapped_text_rows` → `Paragraph::line_count`, the only public door to the
  private `reflow::WordWrapper` — hence the `unstable-rendered-line-info` feature
  on the exact `ratatui` pin). A `ceil(width / inner)` character-packing count is a
  DIFFERENT function of the same text, wrong in BOTH directions: it under-counts
  where a row ends early at a word boundary (which made the tail of a long
  transcript unreachable, since `max_offset = content_h - inner_height`) and
  over-counts where the wrapper swallows the whitespace it broke on. That model
  survives in exactly one place — `wrapped_line_height`, backing the mouse
  hit-test's per-line map, which `line_count`'s single TOTAL cannot answer — and it
  is documented there as an approximation with the click drift it implies. The
  transcript's count is measured ONCE per (session, width), inside the
  `preview_cache` entry, so it can never be invalidated apart from the text it
  describes; whatever is NOT that cached transcript (the draft card, an in-flight
  reply's echo turns) is measured at the draw site.
- `has_banner` is **`view::preview_banner(app).is_some()` — never liveness**.
  Since the poller passes `--all`, an agent that reported completion still has a
  banner while claude would not call it live; keying the geometry on liveness
  would draw the banner but hit-test one row off for every `done` session. Name
  it for the banner, not for liveness. Liveness is also *unaskable* here: it now
  means a shell-out to claude (`App::is_live_now`), which a render must never do.
  Anything that REPLACES the transcript must therefore suppress the banner inside
  that one fn rather than skipping it at the draw site: the in-flight quick reply
  does (its echo turns take the banner's place inline) and so does the new-session
  draft card (there is no session to describe). Skip it at the draw site instead
  and the hit-test still reserves a row that was never painted.
- **A replacement pane must not write its own offset back.** `render_preview`
  persists the clamped offset into `App::preview_scroll` so the scroll keys stay in
  bounds, and that is right only while the transcript is what was measured. The
  draft card is four lines, so it clamps every offset to 0; writing that back
  rewound the previewed session to the top the moment a draft opened and handed it
  back there on `Esc`. `preview_scroll` describes the TRANSCRIPT, so a pane showing
  something else renders from the clamped value and leaves the field alone.
- The split is **vertical only**, so a banner and a banner-less pane share one
  inner width and therefore one `preview_cache` entry. Keep it that way: a
  banner-dependent width would thrash the cache on every agents poll.

## 6. Off-UI-thread for anything that can block

The render loop must never block. A **recurring** shell-out (`claude agents
--json --all`), a FS watch, and the input read all run on their own threads and
deliver `AppEvent`s onto the merged channel.

**What ENDS a recurring thread is a shared shutdown FLAG, not a failed send.**
`EventLoop` owns an `Arc<AtomicBool>` that its `Drop` sets first, before anything
else, and the two producers that nothing else can stop — the input reader and the
agents poller — read that flag at the top of every loop turn. Three shapes result,
and their differences are the pattern to copy:

- The input reader is flag-signalled **and joined**. It never parks
  indefinitely on stdin — it polls with an `INPUT_POLL_INTERVAL` timeout purely so
  it can come back and read the flag — and `Drop` sets the flag, then `join()`s
  the handle. The join is the load-bearing half: it proves the reader has released
  fd 0 *before* `run` returns and `claude` is spawned onto that same stdin, and it
  bounds each resume round trip's reader to that iteration so readers never
  accumulate.
- The agents poller is flag-**bounded but deliberately NOT joined**. It may be mid
  shell-out, and a hand-off must never wait on a `claude` child, so teardown
  states the request and moves on: the thread reads the flag at the top of every
  turn and exits within one `interval` of the turn that observes it — plus, in
  the worst case, the shell-out already in flight, because a flag set just after
  a check is not read again until that turn's `claude` spawn AND its sleep have
  both finished. Bounded is not immediate, and the slack is the point: it is
  exactly what teardown buys by refusing to wait on a child.
- A failed send is the **secondary** exit, and it is **insufficient on its own for
  any producer whose emission is CONDITIONAL**. The tick thread may still rely on
  it alone, because it sends every turn unconditionally, so a dropped receiver is
  guaranteed to reach it. The agents poller may not: the moment it began skipping
  the shell-out on an idle board, an idle poller sent nothing, noticed nothing,
  and leaked one thread per resume round trip. Send failure now covers only the
  window between the receiver dropping and the flag being read. (The watcher is
  bounded a third way, by OWNERSHIP: `EventLoop` holds the `SessionWatcher`, and
  dropping it stops the debouncer's thread.)

New recurring background work follows THAT pattern: own thread, `AppEvent`
variant, the shutdown flag read at the top of every loop turn, a failed send as a
secondary exit — and a `join()` if, and only if, it holds something the next board
session or a spawned child needs back (stdin is the one instance). Reason about
the flag FIRST: "it exits when the send fails" is an argument about a send the
thread may never attempt. The quick-reply send (`send::spawn_send`) is the
reference instance: a **one-shot** detached thread — spawned per `Ctrl-R` send, not a
poller — that runs the multi-second `claude -p` child to completion and delivers a
single `AppEvent::SendFinished`. It mirrors `resume::open_url` (fire-and-forget off
the render loop), never `resume::launch` (which spawns+waits after a teardown), so
the board keeps drawing while the child runs. The pure send DECISION is returned as
`Outcome::Send` and the spawn happens in the `run` driver, keeping the effect out of
the pure event handler. `send::spawn_interrupt` and `send::spawn_bg_launch` are the
same shape for `claude stop` and `claude --bg`; a new one-shot child belongs here
rather than behind a teardown whenever it needs no TTY.

The rule is about the **poll cadence**, not about the word "shell-out". A
ONE-SHOT at hand-off is a different thing and is allowed — `agents::live_agents`
is the instance, directly analogous to `resume`'s authoritative re-read of
`cwd`/`sessionId` at the same moment. Two conditions keep it honest, and both
must be argued at the call site rather than assumed:

- It must not touch the poller. The `--all` poll stays **one call per cycle**;
  the probe adds no tick, thread, or event source, so background cost is
  unchanged.
- Be **accurate about what it costs**, per branch. Where nothing renders between
  the probe and the terminal teardown (plain resume; a confirmed Attach) it is
  invisible. Where the board draws again — the Enter gate's overlay, Attach's
  two refusals, and EVERY branch of the delete confirm — it lands ~0.26s after the
  keypress: a real, deliberate hitch. Do not paper over it with a zero-render
  claim that only holds on one branch.
- **One shot means one, whatever the target count.** The hard-delete confirm
  (`confirm_delete`) judges a whole fork lineage, so it takes claude's active list
  ONCE for the entire set (`App::live_agents_now`) and evaluates every member
  against that single map. Reaching for the per-session accessor in the loop would
  turn one probe into N blocking spawns on the render loop — the poll cadence rule
  broken by a different route — and would judge one family against N different
  instants.

It runs at **EVERY hand-off, not just the first**: the Enter gate asks, the
Attach hand-off asks AGAIN rather than reusing the gate's answer or the polled
map, and the hard-delete confirm asks for itself. `route_handoff` is where that
second ask lives; `confirm_delete` is the third site, and it counts as
hand-off-shaped for the same reason — an irreversible unlink is exactly the kind
of decision that must not be made from a stale snapshot. The reason is the same one
that moved the gate here — an authoritative decision must not be made from a
stale snapshot — and it is sharper at Attach, because the overlay can sit open
indefinitely, so the gate's answer has no bounded freshness at all. **Nothing
hands off on polled data; the poll draws badges.** Fork is the deliberate
exception that proves the shape: it has no liveness question to ask (a fork works
live or finished), so it must NOT be dragged behind the probe — it is the route
the not-live refusal points at.

The **second** allowed one-shot is `worktrees::resolve` (`git -C <dir> worktree
list --porcelain`), and it is worth stating separately because its moment is not
a hand-off: it runs at **construction and on reload** — `App::new` and
`App::apply_sessions` — which puts it in the same category as the synchronous
store reload that already happens there, one bounded child per reload rather than
a cadence. The same two conditions apply and are met: it adds no tick, thread, or
event source (so background cost is unchanged), and its cost is a single local
`git` invocation folded into a reload the board is already blocked on, not a
hitch on a frame that would otherwise have rendered. What it may NOT do is move:
`recompute_scope` and `toggle_scope` run on a keystroke, so they read the CACHED
set and never resolve, and the render path never resolves at all. A test pins
that boundary directly by counting probe invocations across a `toggle_scope`.

That resolver is reached through an **injected probe**, the same seam
`App::live_probe` uses: a boxed `Fn` field on `App`, with the real shell-out as
the `#[cfg(not(test))]` default and a `#[cfg(test)]` default plus a
`set_*_probe` setter for tests. It is a SEAM, not a strategy — production swaps
it exactly never — and it exists so the suite can state an answer instead of
spawning a child. The two defaults differ on purpose, and the difference is the
pattern to copy: `default_live_probe` PANICS under test, because a liveness
answer that silently defaulted to "nothing is live" would let a test pass for the
wrong reason; `default_worktree_probe` returns the EMPTY set, because empty is
the module's documented "could not resolve" answer, under which the scope falls
back to its git-free repo-root arm — so no `App::new` call site spawns git and
every one of them still gets the answer a user with no `git` on `PATH` would.
Pick the default that makes an unconsidered case obvious, not merely convenient.

The same seam appears one level down — as a plain PARAMETER rather than an `App`
field — wherever the THREAD is the thing under test. `spawn_agents_thread` takes
its `poll` and its `idle_after`; `spawn_input_thread` takes the terminal read it
loops on. Both are named exactly once, in `EventLoop`, where production passes
`agents::reported_agents` and the real crossterm `poll`+`read` pair. Nothing else
may pass anything else: the seam exists so a test can state a poll's answer
without spawning `claude`, and state an input event without a TTY, which is what
makes the loop's own behavior — the idle gate it obeys, the board-activity stamp
it writes — assertable at all.

## 7. Restrained, terminal-safe styling

The preview and list are styled with ratatui `Style` only — **never** embedded
ANSI escape sequences. Prefer `Modifier`s (BOLD/ITALIC/DIM/UNDERLINED) plus a
small palette of **named** ANSI `Color`s (they adapt to the user's terminal
theme). Do **not** hardcode RGB (it can vanish on a light background) and do not
syntax-highlight code (code is DIM). The markdown pass in `store::preview` is
hand-rolled and self-contained — no external markdown crate.

This is why the live badge honors "Claude's palette" as the named `Yellow` /
`Green` / `Gray` rather than brand hex: named colors stay legible on a light
terminal, and the semantics survive. Three further rules hold there.

**Color unifies, pulse does not.** The dot and the kind label are separate spans
ONLY so they can share one `view::badge_color` while the pulse stays on the dot
alone (a blinking text label is noise on a board of live sessions).

**The pulse changes STYLE, never a SYMBOL.** Each row's badge glyph — `●`, or `!`
for the `NeedsInput` bucket (`view::badge_glyph` chooses one per bucket, a shape
channel over the yellow-only color) — is drawn in EVERY phase; what alternates is
its color — `view::badge_color` against `view::pulse_color`'s dim partner (`Gray`
<-> `DarkGray`). The glyph is bucket-chosen, but WITHIN a row it is fixed across
the phases. It must stay that way, and the reason is not cosmetic: we emit
**plain-text URLs (no OSC 8)**,
so the terminal auto-detects links by TEXT PATTERN. Any change to a line's text
forces it to re-scan and re-render that line's URL underline — so the dot's
original glyph->blank swap made a session label containing a URL flicker every
500ms, on a row the pulse was supposed to leave alone. A style-only change leaves
the text byte-identical and there is nothing to re-detect. Do not "optimize" it
back into a blank span. `pulse_color` is also the ONE place a bucket's dim
partner is declared, and its fallback is identity (fail-soft), so a new pulsing
bucket without an arm there would silently render steady — the bucket walk in
`every_pulsing_buckets_badge_color_has_a_distinct_dim_partner` is what makes that
loud. Use a named color, never `Modifier::DIM`: attribute support is inconsistent
across terminals, which is the same trap described next.

The search cursor is the deliberate exception: it show/hides, because a cursor's
job IS to appear and disappear and its line carries nothing auto-detected. The
asymmetry is the point, not an oversight.

**Animate from the tick, never from the terminal.** Do NOT reach for the ANSI
blink attribute (ratatui's slow-blink `Modifier`) to animate anything: most
modern terminals (iTerm2, Ghostty, WezTerm, Alacritty, macOS Terminal) IGNORE it
and render steady, so the feature silently does not ship. The badge dot and
`render_search`'s cursor were both built that way once, and neither ever blinked
for the user. They now animate off state the board already owns: `App::tick`
counts `AppEvent::Tick`s (`wrapping_add`, so a long-running board cannot
overflow), and the pure `view::blink_visible(tick)` phases it — `2 *
watch::TICK` = 500ms on / 500ms off (~1Hz). That reuses the existing redraw
cadence and adds no tick, thread, or event source.

Expiry is animated the same way. A transient status is stored as a
remaining-ticks countdown (`App::status_ttl`) and decremented by
`App::tick_status` on each `AppEvent::Tick`. It is neither a wall-clock timeout
nor an absolute deadline, and it adds no second cadence: it reuses the board's
existing tick and redraw loop, so confirmations fade within ≤250 ms of the
countdown reaching zero without ever drifting from the pulse or needing its own
event source.

`blink_visible` is THE phase source, not one of two: the dot and the cursor both
read it and therefore one `BLINK_TICKS`, so they pulse together. Anything
animated later phases off it too — a second counter or cadence would drift
visibly against the first. Two testing rules follow for any future animation:

- **Assert drawn cells, not modifiers.** A test that pins "the modifier is set"
  passes green against an animation the user never sees; that is exactly how the
  dead blink shipped. Render both phases through `TestBackend` and assert what the
  cell actually carries — for the dot, that its symbol is UNCHANGED and its style
  is not (diff the two phases' `Buffer`s; `Cell`'s `PartialEq` covers fg/bg/
  modifier, so a style-only change does surface). Scope such a diff to the row
  under test: the search cursor legitimately changes symbol, so a board-wide
  version fails for an unrelated reason. This bans a modifier as a PROXY for a
  pulse — not reading a modifier back off an already-rendered cell, which is what
  `DrawnBadge` does to pin the badge's `BOLD` (use `contains`: the List's
  `highlight_style` patches `REVERSED | BOLD` onto the selected row).
- **Break-check the phase test.** Every phase test must be watched failing (make
  the glyph conditional; make `pulse_color` the identity; make both phases use
  `badge_color`; invert the phase). A pulse test that has never failed is the
  exact shape of the ones that shipped green over a dead blink — twice. Watch for
  the vacuous pass in particular: assert the diff is NON-empty, or a `diff` that
  noticed nothing would call the bug fixed.

Ahead of the markdown pass, each message body runs through an **allowlist-driven
control-wrapper collapse** (`store::preview::collapse_control_wrappers`). Claude
Code injects a fixed set of paired pseudo-tags (`<command-name>`,
`<system-reminder>`, `<local-command-stdout>`, `<local-command-caveat>`,
`<task-notification>`, `<persisted-output>`, …); each collapses to a single dim
marker (a slash-command turn renders as `▷ /name args`, a `<local-command-caveat>`
renders as `[command caveat]`). Only names in the `CONTROL_WRAPPERS` allowlist
that have a matching close tag are touched — legitimate angle-bracket content
(open-only placeholders like `<session-id>`, generics like `<String>`,
comparisons like `x < y > z`) is left byte-for-byte literal, and a known opener
with no close fails soft to literal. The collapse is a pure `body -> Vec<Segment>`
function; the thin renderer routes each literal segment through the markdown pass
and each collapsed segment to its marker line.

## 8. Name every constant

No magic numbers. Tunables are named `const`s with a rationale comment near the
top of their module: `DEBOUNCE` / `TICK` / `AGENTS_REFRESH` / `AGENTS_IDLE_AFTER`
(`watch`), `LABEL_MAX` (`label`), `MTIME_SETTLE_WINDOW` (`store`),
`CONTENT_INDEX_CAP` (`parse`), `PREVIEW_LINES` / `TABLE_MAX_WIDTH` (`preview`),
`PREVIEW_WHEEL_STEP` / `LIST_WHEEL_STEP` (`app`), `PASTE_MAX_CHARS`
(`tui::update`), `BLINK_TICKS` (`view`). Add new tunables the same way.

A const whose rationale depends on ANOTHER const says so and names it:
`BLINK_TICKS` is meaningless without `watch::TICK` (they multiply into the pulse's
500ms phase), so its doc comment shows the arithmetic and points at `TICK`.
Likewise, `tui::app::STATUS_DWELL_TICKS` is meaningless without `watch::TICK`
(`16 * 250 ms = 4 s`), so its doc comment names `TICK` and shows the arithmetic.
That naming is what makes the coupling discoverable when the other value is retuned.

## 9. `#[allow(dead_code)]` is narrow and justified

`snapback` is a **library** crate (`src/lib.rs`) plus two thin binary shims that
call into it. `run()` is the ONLY public API — every other module is declared
`mod`, not `pub mod` — so `dead_code` is measured against the private module
tree plus the unit tests, and it fires on any item no runtime path reaches, even
one fully exercised by those tests. Where that happens, attach a
**narrowly-scoped** `#[allow(dead_code)]` to the single item with a one-line
reason. **Never** use a crate- or module-wide blanket allow — the lint must
stay sharp everywhere else.

That prohibition binds `src/lib.rs` itself: marking a module `pub` from the crate
root suppresses `dead_code` for everything inside it in one stroke, which is a
module-wide blanket allow by the back door. A `pub mod` there also turns the
module's public items into crate API that `cargo-semver-checks` polices, so an
internal rename starts scoring as a downstream break. Keep the module private and
take the narrow allow instead.

## 10. Keys, actions, outcomes

Input handling is a three-stage pipeline, all terminal-free and testable:

1. `key_to_action(key, query_empty)` → an `Action` (`j`/`k`/`q` navigate/quit
   only while the query is empty; arrows, Enter, Tab, and `Ctrl-*` always act so
   search never blocks navigation).
2. `apply_action` mutates the `App` and returns an `Outcome`
   (`Continue`/`Quit`/`Resume`/`Send`/`Interrupt`/`BgLaunch`). `Send`, `Interrupt`
   and `BgLaunch` carry a confirmed `SendRequest` / `InterruptRequest` /
   `BgLaunchRequest` the driver spawns without a teardown (the board stays up), the
   way `Resume` carries a confirmed `Ready` — the decision is data, the effect is
   the driver's. Add a new effect this way, not by spawning inside the handler.
   Which of the two shapes a new action takes is decided by the CHILD, not by what
   it is called: a background-agent launch is `--bg` (returns at once, needs no
   TTY) so it stays on the no-teardown side, while its `Ctrl-O` twin hands the
   terminal over and is therefore an ordinary `Resume`.
3. Modal state owns the keyboard: ONE `App.modal: Option<Modal>` serves every
   titled overlay — the running-session choice, the new-session agent picker, and
   the hard-delete confirm — through the generic `modal_key` → `confirm_modal`
   machine, dispatching each choice's `ModalAction` tag (`Row` layout binds the
   horizontal keys, `List` does not). A key that belongs to ONE overlay rather than
   to modals in general is narrowed twice, at both stages: the picker's `Ctrl-O` is
   bound on the `List` layout in `modal_key` and acted on only for a
   `ModalAction::New` choice in `launch_pick_interactively`, so neither a new `Row`
   modal nor a future `List` one can inherit a verb it has no meaning for. Four
   more keyboard owners sit alongside it: the `Ctrl-X` leader chord (while
   `App.pending_chord` is set, `chord_key` routes the next key — `x` hide, `d`
   delete-confirm, `h` show-hidden, `r` forced full store re-read, anything else
   cancels), the "stop the
   waiting agent?" confirmation via `App.pending_stop` (a plain Enter/Esc gate
   before compose, for the `needs input` quick-reply path), its `Ctrl-K` sibling
   `App.pending_interrupt` (the same Enter/Esc gate, but resolving to a bare
   `Outcome::Interrupt` rather than into compose), and the compose zone via
   `App.compose` + its `compose_key_to_action` machine — ONE keyboard owner for
   BOTH drafts, since which one is open is a `ComposeTarget` rather than a
   second piece of state. `handle_event` checks each in turn before the board.
   `App::overlay_active` (`modal.is_some() || compose.is_some() ||
   draft.is_some() || pending_stop.is_some() || pending_interrupt.is_some() ||
   pending_chord`) gates mouse actions (splitter drag / link open) so none fires
   while any is up. A mouse wheel is handled **before** and **independent of** that
   gate. A new keyboard owner must be added to `overlay_active` too, or the mouse
   will act underneath it.

   A **terminal paste** is routed by that same list, and `update::handle_paste`
   walks it in the identical order — the per-owner table is
   [DOMAIN.md](DOMAIN.md#terminal-paste-routing-eventpaste). Two rules follow for
   anyone editing this area. A new keyboard owner must be added to `handle_paste`
   as well, not only to `handle_event` and `overlay_active`, or pasted text lands
   on the surface underneath it. And `handle_paste` returns no `Outcome` on
   purpose: a paste is DATA, so it structurally cannot send, resume, or answer a
   confirmation. That is the shape of the fix for the bug where a pasted newline
   arrived as a bare `Enter` — `ComposeAction::Send` — and submitted a draft's
   first line before resuming on its second. `compose_key_to_action` is SHARED by
   both compose targets, so that hit BOTH boxes: a quick reply sent one line, and a
   `Ctrl-N` background draft launched an agent on one.

   `draft` is the one arm that is not a keyboard owner: it owns the **pane**. While
   the new-session draft card is drawn the transcript is not, so the cached link
   regions describe text no longer on screen and a click would open a link from a
   session the user cannot see. It outlives the compose editor by AT MOST one
   in-flight launch, which is the window nothing else covers — so a pane owner
   earns an arm here for the same reason a keyboard owner does. "At most" is the
   operative bound: a pane owner that outlives its keyboard owner also outlives the
   gate that used to end it, so it needs its own end conditions — see
   [DOMAIN.md](DOMAIN.md#background-agent-draft-pane-ctrl-n) for the two the card
   carries.

Add a keybinding by extending the `Action` enum + `key_to_action` + `apply_action`
and covering it with a `key_to_action` unit test. Then satisfy the KEEP KEY DOCS IN
SYNC rule in [AGENTS.md](../../AGENTS.md), which owns the list of surfaces that
must agree — do not re-enumerate them here.

## 11. Status-line ownership

`App::status` carries only **outcomes and refusals** — facts true at a single
point in time: a send result, a resume refusal, a paste-too-long warning, an
empty-buffer nudge. A fact that is true over an **interval** lives in typed
state and renders on the surface that owns it:

- the quick reply's in-flight echo lives in `App::sending` and renders **inline**
  in the preview pane (`view::sending_tail`), not on the help line;
- a background-agent launch lives in `App::draft.launch_id` and renders on the
  draft card (`view::draft_card`), not on the help line;
- an interrupt in flight lives in `App::interrupting` and deliberately has **no**
  visible label — `claude stop` is fast and the badge clears on the next agents
  poll — but the guard still prevents a stale completion from landing on a
  surface that has moved on.

The help line renders `App::status` and nothing else. `set_status` is sticky
(failures and refusals persist until the next actionable keypress);
`set_status_transient` expires after `STATUS_DWELL_TICKS` ticks so confirmations
and nudges do not squat on the keymap row. This keeps each fact told exactly
once and prevents interval-scoped facts from colonizing a keypress-scoped
surface.

## Testing patterns

Tests are **inline** `#[cfg(test)] mod tests` at the bottom of each source file
(no separate integration crate). Conventions to match:

- **Fixture store**: `tests/fixtures/store/` holds representative JSONL — a
  normal session, a no-summary session, a malformed-line session, a worktree
  cwd, a sidecar (no `cwd`), a nested subagent, a **background-fork pair**
  (two files sharing one tree root, `cwd`, branch and label — the duplicate-row
  shape), and a **root-less** session (no `parentUuid: null` record). Reach it
  via `env!("CARGO_MANIFEST_DIR")`. Add a fixture when you add a format edge
  case, and update the counts in `store::mod`'s discovery/session-count tests.
  A fixture pair must **differ in the field under test**, and the fork pair
  differs twice on purpose: its leading user records differ (a pair that agrees
  everywhere passes against the wrong lineage key too and cannot distinguish it),
  and its members carry different turn counts (2 vs 4, behind three copied
  `attachment` records each) so the child row's count column has something to
  tell apart — a pair that agreed there could not test the one field that exists
  to separate the stub from the member holding the work.
- **Synthetic models**: build `Session`/`ReportedAgent` values directly in tests
  (see the `session(...)` helpers) rather than round-tripping through disk.
- **Isolated temp dirs**: watcher/app tests create a unique
  `snapback-<tag>-<pid>-<nanos>` dir under `std::env::temp_dir()` and never
  touch the real `~/.claude/projects`. Clean up with `remove_dir_all`.
- **Test the pure helper, not the impure driver**: exit handling is tested via
  `status_for_exit`, teardown via the `Write`-generic `disable_mouse`, argv via
  `build_argv` — no real `claude` process is ever spawned, and no real `git`
  either (the worktree set is stated through `App::set_worktree_probe`, and the
  porcelain parser is fed captured sample text; see
  [§6](#6-off-ui-thread-for-anything-that-can-block) for the probe seam).
- **Assert structure, not styling**: preview tests flatten `Text` to plain
  strings to check markers, and separately assert `Style`/`Modifier` on specific
  spans.

Every new pure function gets a unit test in the same file.

## Watch every test fail before you trust it

A test that has never been observed red is an unverified claim. Before reporting
work green: temporarily break what the test pins, confirm it FAILS, restore. If
it still passes, it was never testing what you thought.

This is not a hypothetical discipline — the live-status work shipped three tests
that passed against broken code, each for a different reason:

| What shipped green | Why it lied |
| --- | --- |
| `assert!(dot.modifier.contains(SLOW_BLINK))` | Pinned that a **modifier was set**, not that anything rendered. Most terminals ignore the ANSI blink attribute, so the dot never pulsed — the test certified the mechanism that didn't work. |
| A banner test calling `preview_top()` in its fixture | The fixture **arranged away** the bug. The board bottom-anchors by default, so the real banner scrolled off-screen; only the test's un-real scroll position made it visible. |
| A test board with exactly one bucket | The mutant "colour the label like the dot" survived **all 257 tests** — the requirement simply had no case that could distinguish it. |

The lessons those encode, in order of how often they bite:

- **Assert what the user would see** (drawn cells / observable behavior), never a
  proxy for it (a modifier is set, a fn was called, a flag is true). A proxy can
  be true while the feature is dead.
- **A fixture that arranges away the failure is worse than no test**, because it
  reads as coverage. Ask what the fixture had to be for the bug to hide.
- **Vacuous passes are the default failure mode**, not an edge case. If breaking
  the code doesn't turn the test red, the test is decoration.
- **Report what made a check capable of failing**, not that it passed. See the
  execution checklist in [AGENTS.md](../../AGENTS.md) and the lint gate's own two
  false-clean modes in [OPERATIONS.md](OPERATIONS.md).
