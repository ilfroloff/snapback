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
- `claude`'s own settings files (`claude_settings`; which files, in which
  precedence, is
  [CLAUDE_CLI.md](CLAUDE_CLI.md#which-model-a-launch-runs-on-without---model)'s)
  are the same again: each is a `serde_json::Value`, and a missing file, an
  unreadable one, garbage JSON, a non-object top level or a wrong-typed `model` /
  `env` field is "no value from this file" — a lower file may still answer —
  never a panic. The direction is toward what `claude` does with NOTHING SET: the
  new-session answer collapses to `None`, which a `Ctrl-N` draft labels as plain
  `model: default`, and the restore-override answer to `false`, which leaves a
  reply naming its session's own model — each true, if less specific.
- The compose pick list's two sources are the same again. claude's `initialize`
  handshake reply (`claude_catalog::parse_initialize_response`) and a transcript's
  `agent_listing_delta` records (`store::skills`, a reply's `@` agents only) are
  read as `Value`, a malformed entry or record contributes nothing, and a failed
  fetch — no `claude`, a timeout, an `error` subtype, another `request_id` — is
  `None`. The direction is toward the list a box shows before claude's lands (no
  `/` list; a reply's `@` agents from its transcript), never toward a verdict: a
  `None` catalog is not a fact about the folder and is never cached (the retry is
  [DOMAIN.md](DOMAIN.md#compose-pick-list)'s), and an unreadable transcript is an
  empty list. Descriptions from both pass `store::skills::normalize_description`,
  so no control character or raw escape reaches a cell.
- claude's workspace-trust record (`claude_trust`, which file and which rule is
  [CLAUDE_CLI.md](CLAUDE_CLI.md#workspace-trust-what-an-untrusted-folder-can-run-and-the-two-argv-forms)'s)
  is read as a `Value` too, and its direction is UNTRUSTED: every input it cannot
  settle (CLAUDE_CLI.md lists them) answers untrusted, which only costs the
  folder's list the repository's own items. The other direction would load a
  repository's settings, and with them its helper commands and `env` block, and
  leave claude's own git prefetch running its git configuration, just because a
  compose box opened.
- **Reading is the default, but not the whole story.** `snapback` is
  overwhelmingly a reader of a hostile external store, and the Claude store stays
  read-only save for the one gated hard delete. It does, though, have exactly one
  file of its OWN — the hidden-session id set (`hidden::load_hidden`) — and that
  read gets the identical fail-soft discipline: a blank line is skipped and a
  missing or unreadable file collapses to an empty set, never a panic, and its
  write is atomic (temp + rename) so a crashed write can never leave a half-file
  that fails the next read.
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
  `build_new_argv` / `status_for_exit`, plus the two a compose's model pick added —
  `push_model_flag` (the ONE place the flag is formatted, over the same
  `flag_value` trim/blank guard `--agent` uses, and shared verbatim by `send`; it
  writes the pick's `--effort` too, directly after `--model` and INSIDE that
  guard, so no argv can carry an effort without its model) and
  `nonzero_hint_for` (which picks the hint from that SAME predicate, so a blank
  pick that emits nothing cannot blame a model that was never sent). A compose's
  pick is passed IN as a parameter by the reply, the draft's `--bg` launch and its
  `Ctrl-O` run, and the builders the A MODEL IS PICKED PER COMPOSE, NEVER PER
  BOARD rule in [AGENTS.md](../../AGENTS.md#critical-rules) keeps it from —
  `build_argv` (Resume, Fork) and `build_attach_argv` — simply take no pick, so
  that guarantee is a signature rather than a branch a test has to catch; every
  decision in `send` — `reply_gate` /
  `interrupt_gate` (the whole routing tree, asserted with no process spawned),
  `reply_in_flight_refusal` (`Ctrl-R`'s per-session in-flight rule, which takes
  whether the SELECTED session has a reply of its own in flight as a parameter
  rather than reading `App`),
  `build_send_argv` / `build_stop_argv` / `build_bg_launch_argv`, `plan_send` /
  `plan_bg_launch`, the `status_for_send` success map with its `answering_models` /
  `model_readout` halves (the `modelUsage` readout, fail-soft over an absent,
  empty, non-object or mistyped map), the `status_for_output` /
  `status_for_failed_send` / `status_for_stop` / `status_for_bg_launch` /
  `status_for_signal` mapping, and the signal route's two checks, `signal_plan`
  (re-verify a captured pid against a fresh record) and `signal_target` (narrow it to a strictly positive `pid_t`,
  or refuse, by `positive_pid_t` — the range half of `signallable_pid`, the one
  rule `interrupt_gate` applies (and `claude_catalog::kill_group` reuses to narrow
  its group id), whose other half refuses the board's own pid and
  takes it as a parameter, `App::own_pid`, so no test reads a real process id)
  — each split out so the guard in front of `kill(2)` is asserted without ever
  calling it; `tui::update::show_signal_result`, which takes that syscall's result
  as a parameter, so what the status line does after it is tested with no signal;
  `agents::elapsed_phrase` and `watch::epoch_ms`, which take their instants as
  parameters so no test reads a clock;
  `compose::compose_key_to_action`; `defined_agents::select_agents` /
  `parse_frontmatter`; `agents::classify` and the outputs derived from it
  (`qualifier_copy`, the LIST ROW's worded qualifier — which `friendly_status`
  also fuses onto the kind label, for the pinned row's fallback for a transcript
  with no marker at all and nothing else, and for the status a LIVE agent's pinned
  turn marker carries — plus `is_active`) and both argv
  builders (`agents_argv` /
  `live_agents_argv`) and `agents_from_output` (the shell-out's
  non-zero-exit-means-no-signal decision, split from the spawn so it is testable
  without one); `model_aliases::parse_model_aliases`, split from its read for the
  same reason and to sharper effect — it is the SINGLE interpreter of the `--model`
  alias array's byte format, so it is pinned against byte windows captured from
  two real `claude` binaries — the oldest and the current capture, a cap its test
  module argues — rather than against a 290 MB file the suite has no
  business shipping; `claude_settings`' `resolve_new_session_model` (the whole
  settings-default decision over already-read file contents and an env value, so
  precedence, `ANTHROPIC_MODEL`, blank/`default` masking and garbage input are
  asserted with no filesystem), `resolve_restore_overridden` (the `-r` restore
  skip over the same layers and an environment passed in as a closure, so the five
  override names, the settings-`env`-over-process order and the empty-value mask
  are asserted without touching the real environment), `settings_layer_paths` (the precedence stated as a
  list of PATHS, the one thing most likely to be got backwards) and
  `drop_in_order` (the managed drop-in filter and sort, split from the directory
  listing because a real `read_dir` often comes back sorted already and would let a
  missing sort pass); `worktrees`' whole trio, split from its spawn the same way —
  `git_worktree_argv` (the invocation is a contract with an external CLI, so it
  is asserted without running one), `set_from_output` (the same
  non-zero-exit-means-no-signal decision, plus non-UTF-8 output rejected WHOLE
  rather than lossily repaired, since a replacement character inside a path is a
  directory that exists nowhere) and `parse_porcelain` (which takes its
  canonicalizer as a PARAMETER, so the parse stays hermetic — no filesystem, no
  git — and is pinned against real captured porcelain bytes); `store::group`'s
  `repo_root_of` / `repo_of` (ONE pure marker scan, two consumers — the root path
  and its label — with inline path fixtures, including a NEGATIVE case that must
  not collapse and a pinned known limitation); `tui::app::in_scope` (the scope
  predicate, which takes the worktree set as a parameter rather than reaching for
  it, so it never resolves git and is tested from a seeded set) and
  `tui::app::offered_model_aliases` (the model picker's whole vocabulary decision
  — the probe's answer verbatim, or the seed when it is empty — pure so the one
  decision the runtime alias read turns on is asserted directly rather than only
  through a rendered modal) and `tui::app::resolve_compose_default` (what a
  compose runs on with no pick, over its target, the session's restorable model,
  the restore-override flag and the settings model — every `ComposeDefault` case
  asserted with no board) with its two wordings, `ComposeDefault::picker_label` /
  `picker_description` (the picker's first row) and
  `tui::app::cycle_effort` (the picker's whole `←`/`→` cycle — unset, then
  `resume::EFFORT_LEVELS` in order, wrapping both ways — asserted without a
  modal); `store::preview::restorable_model` (claude's restore rule for ONE record
  — a non-`isMeta` `assistant` turn with a real model — so the reply's default is
  pinned per record as well as per fixture); `store::lineage`'s `lineage_key` / `head_of` / `fold` (the whole
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
  `view`'s `wrapped_text_rows` / `wrapped_row_prefix` / `row_window` (which
  logical lines a viewport at a given wrapped-row offset reaches, and the rows
  left over inside the first of them — pure arithmetic over a prefix map, so the
  windowed draw is tested without a terminal) / `clamp_preview_offset` /
  `preview_split` /
  `centered_rect` / `modal_list_window` (a `List` modal's scroll window — the same
  keep-the-offset-until-the-selection-leaves-it rule ratatui's `ListState` gives the
  board list, so the two pickers scroll rather than losing their tail off a short
  terminal, and total over a viewport of 0 or 1) / `highlight_runs` and its STYLED sibling
  `highlight_matched_spans` (the same char-safe run split, but over a line that
  arrives ALREADY styled — it splits the line's own spans at the matched
  positions and ADDS a `Modifier` to those runs, so a marked word inside DIM code
  stays DIM. It promises three things the pane depends on: byte-identical text
  (hence unchanged display width, which the link hit-test measures in), one line
  in and one line out (hence an unchanged wrapped-row count), and char-boundary
  splits only. Its match map is derived in `App::preview_matches` by re-searching
  the RENDERED lines — never by projecting a `content_index` offset, see
  [DOMAIN.md](DOMAIN.md#a-content-index-position-never-projects-into-preview-coordinates))
  / `fit_label` (the marker-vs-label width
  reservation, pure so it is tested as arithmetic rather than only through a
  rendered pane) / `child_msgs` and `fit_child_msgs` (a lineage child's
  turn-count segment and whether the row can afford it — ALL-OR-NOTHING: the
  segment is drawn whole or dropped entirely and never ellipsized, because a
  clipped `171 msgs` reads back as a plausible `17` and a confidently WRONG
  number is worse than none, where a clipped label merely looks clipped. The
  segment folds its own leading gap in, exactly as `lineage_marker` does, so the
  width weighed is the width drawn) / `compose_model_label` (a compose box's
  bottom-border `model:` label, wording AND styling as spans: the value — a pick,
  `session (<label>)`, `default (<value>)` or `default` — in `MODEL_LABEL_STYLE`,
  the prefix and the `(new sessions only)` scope unstyled) / `model_pick_label` (a
  compose's pick as `<model>` or `<model> · <effort>`)
  / `modal_effort_span` (what each model-picker row draws after its label: a set
  effort always, the dim `default effort` stop only on the highlighted row,
  nothing on any other kind of row) / `blink_visible` (the board's ONE pulse phase, a pure fn of
  `App::tick`) / `badge_color` and its `pulse_color` partner (also derived from
  `classify`, but a rendering decision, so the palette sits in the view rather
  than dragging ratatui into the parser layer).
- Thin, impure: `resume::launch` (chdir + spawn + wait), `defined_agents::discover_agents`
  (the FS walk over `select_agents` / `parse_frontmatter`), `worktrees::resolve`
  (spawn + capture, delegating every decision to `set_from_output`),
  `model_aliases::installed_model_aliases` (locate on `$PATH`, canonicalize,
  chunk-read — it decides only WHERE to look and delegates what the bytes mean to
  `parse_model_aliases`), `claude_settings::model_defaults` (reads
  `ANTHROPIC_MODEL`, the `ANTHROPIC_DEFAULT_*_MODEL` overrides, `$CLAUDE_CONFIG_DIR`
  and the settings files — through `defaults_from_disk`, `managed_drop_ins` and
  `read_layers`, reading the files ONCE for both answers — and delegates every
  decision to `resolve_new_session_model` and `resolve_restore_overridden`), the `watch` threads, `tui::run` (draw loop), and
  `send::signal_term` (`Ctrl-K`'s one syscall, `kill(2)` with SIGTERM, whose only
  caller is that loop and which NO test may call — see "Watch every test fail"
  below), `claude_trust::folder_trust` (the real environment, home and record,
  delegating to the pure `global_config_path`, `trust_keys` and `is_trusted`
  through `folder_trust_in`, which tests drive with a temp record) and
  `claude_catalog::kill_group` (the crate's other syscall, `killpg(2)` with
  SIGKILL on a timed-out fetch's own child group, reached in tests only through
  stand-in children, `sh` and one `perl` leader: see
  [Testing patterns](#testing-patterns)). Keep these small and delegate to tested
  helpers.

The terminal-up **refusal gate** is an instance of this: `resume::check` (and its
sibling `resume::check_new` for starting a fresh session in the launch dir) runs
the pure predicate while the UI is still drawn, so a refusal becomes a board
status with no teardown flash; only a confirmed `Ready` escalates to
`Outcome::Resume` and the impure `launch`.

## 4. Isolate volatile dependencies

All `memchr` calls live in `src/search.rs` and nowhere else — the pins are exact,
so a matcher upgrade touches one module. `nucleo` is a **dev-dependency**: it is
reachable from `mod tests` in that same file and from no runtime path at all, and
it exists solely as the parity ORACLE (below). The rest of the crate sees only
`SearchIndex`, `SearchMode`, and `filter`. Matching is **substring, not fuzzy**,
and that follows from the mechanism rather than from a setting: memmem searches
substrings by nature, and no user-typed atom syntax can widen it. When you touch
search, preserve the incrementality contract: `set_query` rebuilds only the
per-atom finders per keystroke; `refresh` rebuilds haystacks only for sessions
whose fingerprint changed.

`results` answers **membership only** — never a rank — via `memchr::memmem`, and
returns candidates in the order given. Do not re-introduce ranking:
`App::order_filtered` re-sorts every result by a **tie-free total order**, so a
rank cannot reach the screen, and computing one cost 76–81% of each keystroke
through nucleo's `Utf32Str` UTF-32 conversion. The compose pick list's tiered
order does not bend this: it is a pure decision in `tui::complete`
(`Placement`), read off answers `search.rs` already gives, never a rank computed
inside the matcher.

Membership is "every atom present as a byte substring" in name-only mode and for
a single atom. A **multi-atom name+content** query is narrower: the AND is
**bounded to a proximity window**, so the atoms must sit in the label or within
one window of each other in the combined haystack — see
[DOMAIN.md](DOMAIN.md#content-index-storeparse) for the window, the anchor and
the rarest-atom cap. Widening that back to plain co-occurrence is the defect, not a
simplification.

ONE matcher serves every surface, under TWO rules, and which rule a surface takes
follows from what the surface IS. A row label is ONE string and the row is on the
board because that string matched, so `match_indices` applies the **whole-string**
rule — every atom in that string — and then marks through the per-atom machinery,
so every occurrence of each atom is marked rather than one chosen run. A
transcript is MANY strings admitted by atoms occurring anywhere in it, so
`atom_match_positions` runs the same memmem finders over one rendered line and
returns the CHAR positions **any atom** covers; demanding every atom per line
would mark nothing the moment the words sit on different lines, and an unmarked
pane cannot explain why the row is there. Consequences to keep: several runs on
one line are normal, and marks are a **union** (overlapping or abutting runs merge
into one span, never two).

The compose pick list is a further CONSUMER of those seams, not a third rule.
`tui::complete` compiles its token with `SearchIndex::new` + `set_query` and asks
`admits` — the filter's own membership predicate, the very gate `match_indices`
runs — of a candidate's name and then its description. Its tier comes from the
per-atom seam, because "does the name START with it" is a positional question: a
name-start hit is an `atom_match_positions` run at char 0. The token is
whitespace-free, so it is ONE atom, and on one atom the two rules coincide.

Sharing the finders is only half of asking the same question. The other half is
the **fold**: every lowercased string the module searches — the filter's
haystacks (the pair `admits` builds from a caller's string included) and both
marking seams' — goes through the one per-char,
byte-length-preserving fold. Reach for `str::to_lowercase` on any one of them and
the surfaces diverge on the chars that fold differently, which shows up as a row
the filter admitted drawing with nothing marked.

Where those positions become SPANS — `match_runs`, the one splitter behind both
the row label and the preview, in `src/tui/view.rs` — a run that would end inside
a grapheme cluster snaps **out** to the cluster's edges and merges with whatever
it now touches. It marks at most one extra codepoint; the alternative is worse
than cosmetic, because `Line::width` sums a **contextual** `unicode-width` PER
SPAN. Cut an emoji off its VS16 (-1 column) or its skin-tone modifier (+2), and
the summed width of text that did not change moves — desyncing the one cache
measured on the unsplit lines (the wrapped-row prefix map behind
`preview_wrapped_rows`, `preview_window` and `preview_hit_context` alike) from the
line actually painted, and moving where ratatui's wrapper (which segments per span
too) breaks the row. That map is now the whole pane's shared answer, which makes a
split costlier than it was: it is built over the UNMARKED transcript and then used
to WINDOW a marked one AND to hit-test a click into it, so a split that moved a row
would start the pane on the wrong line and resolve a click to the wrong one, not
merely mis-measure a height.

Two invariants are easy to break and expensive to get wrong:

- **Smart case is per ATOM, not per query.** `CaseMatching::Smart` makes each
  atom case-sensitive iff *that atom* carries an uppercase char, so `foo BAR`
  folds `foo` but not `BAR`. The decision rides each `AtomFinder`. A per-query
  branch looks equivalent and silently breaks mixed queries — and answering an
  uppercase query from the lowercased haystack is an *inclusion regression*
  (measured: `NPX` finds 6 entries there, the smart-case rule matches 0), not a
  nuance.
- **Both haystacks are live.** The cased one backs the case-sensitive branch, the
  lowercased one the case-insensitive branch. Neither is dead; deleting the cased
  one breaks every uppercase query.

`gate_atoms` is the ONE splitter: the filter, `admits` (and through it the
compose pick list), the row-label highlight and the preview marks all take the
same atoms from it — never re-split a query somewhere else.

`last_atom_start` is the ONE exception, and it proves the rule rather than
bending it. The word-delete keys need a BOUNDARY (the byte index one press
truncates to), not a list, so it scans instead of splitting and returns an index
— it never produces atoms for anyone to consume. It still walks the same space
and backslash rule, including the escape rule AS WRITTEN: a space is escaped when
the character immediately before it is a backslash, NOT on backslash parity. So
it lives in `search.rs` beside `gate_atoms` and the two MUST change together;
`truncating_at_the_boundary_drops_exactly_one_atom` pins them to each other by
asserting `gate_atoms(before).len() - 1 == gate_atoms(truncated).len()`, which is
what turns "keep these in sync" from a comment into a failing test. A delete that
split on a boundary the filter does not recognize would leave the query saying
something the user cannot see.

`nucleo` earns its dev-dependency as the **oracle**, and nothing else:
`membership_matches_nucleo_across_query_shapes_and_modes` compares this module's
match set against nucleo's own, so the per-atom smart-case rule is proved rather
than asserted from belief — hand-written expectations would encode whatever the
author believed smart case to be, which is the thing under test. The oracle's
corpus is built to avoid the places this module deliberately differs (unicode
normalization, per-string vs per-char lowercasing, the upstream non-ASCII tail
off-by-one, the case predicate on non-ASCII atoms); the module docs enumerate
them. Keep the import inside `mod tests`: a runtime `use nucleo` puts the matcher
back in the shipped binary. `admits` is chained to that proof rather than given
its own: `admits_is_the_filters_own_question_asked_of_any_string` asserts it
agrees with the name-only `filter` on every label, so the pick list inherits
whatever the oracle proves of the filter.

## 5. Selection and scroll survive reloads

TUI state that must persist across an autorefresh reload is keyed by **stable
`session_id`**, never by list index (`App::selected` is an id; `App::scroll` is
preserved and only clamped). On reload, restore the selection by locating the id
in the new filtered list; if it vanished, clamp the previous position to the
nearest surviving row. Path canonicalization (the scope predicate) runs only on
reload / scope-toggle (`recompute_scope`), never per keystroke.

**The offset lives on the model; only the RENDER knows the viewport, so the render
resolves it and writes it back.** One rule, three instances — a fourth copies it
rather than inventing a second idiom. `render_list` seeds ratatui's `ListState`
from `App::scroll` and stores `state.offset()` back; `render_preview` clamps
`App::preview_scroll` against the measured content and stores the clamped value;
`render_modal` resolves `Modal::scroll` through the pure `modal_list_window`
against the box `centered_rect` actually granted. The modal's window follows the
SELECTION for the same reason the list's does — a `List` modal grows with data (one
row per defined agent, one per model alias) while `centered_rect` clamps its
height, so without a window the tail was simply not drawn: later rows were
UNREACHABLE rather than scrolled. Its two spacer rows carry the `↑ N more` /
`↓ N more` affordance, so disclosing the off-window rows costs the box no height.

A modal row is ONE line, and its description clips at the border, unless the
choice sets `ModalChoice::wrap_description`. Only the model picker's first
(default) row sets it, because its `ComposeDefault::picker_description` says what
sending no `--model` means for THAT compose and is only honest read whole. That
row's description wraps onto DIM continuation lines at a
hanging indent under its first word, through the same `wrap_message` the modal
message uses. **The row's height is the line count of that wrap, never a
constant.** `view::modal_list_row_lines` builds the lines once, and the box
height (`modal_list_lines_asked`), the window (`modal_list_window`) and the
drawn slice (`modal_list_shown`) all read that one count. So the height follows
the text and the label beside it: two lines for a draft whose settings name no
model, three for one naming `opus[1m]`, four for a reply whose session last
answered on `Sonnet 5`, and eight for a draft whose settings name a full dated
model id. Any hard-coded row height would
clip one of those or over-ask for another. `MODAL_LIST_MAX_ROWS` counts CHOICES,
not lines. The box asks for the tallest run of that many choices, so a wrapped
row adds its extra lines to the box instead of pushing a choice out of the
window.

The mouse's TEXT selection over the preview is anchored in CONTENT coordinates
(`app::ContentPos`: a wrapped row counted from the top of the whole transcript,
the row domain `App::preview_scroll` and the cached prefix map share, plus a
column from the transcript rect's left edge), converted from the screen through
the scroll the pane last RESOLVED — never a raw request past the end. So it
survives the pane moving under it: a drag held past the transcript's top or bottom
edge autoscrolls (§7) and the selection grows into rows that were
never on screen, and the frame highlights only the part that is visible
(`view::visible_selection_runs`). What it cannot survive is its rows naming other
text, so it is dropped by the same rule the preview cache is evicted by:
`App::apply_reload` clears it only when the reload re-read the previewed
transcript (`Reload::changed`) or moved the row selection, never because some
OTHER transcript was written. The watcher reloads on every write anywhere in the
store, so clearing on every reload cancelled a drag mid-gesture whenever any agent
was working. A RESIZE clears it too (`update::dispatch`'s `Event::Resize` arm): a
new width re-wraps the transcript, and the same row number then names other text.
A fold toggle clears it too, for the same reason: `App::toggle_peer_fold` evicts
and re-renders the previewed transcript, so the rows below the node are
re-numbered and the selection's anchors would name other text — and the clear
sits beside that eviction, inside the one mutator, so no route into a toggle can
skip it.

A released selection is copied from an OFF-SCREEN REDRAW, not from the frame: the
selected rows are drawn again, `view::SELECTION_COPY_CHUNK_ROWS` at a time, with the
pane's own widget, wrap and `row_window` windowing over the cached lines
(`view::selection_copy_text`), and walked by the SAME blank rule, per-row cut and
edge-row trim the highlight uses — the trim applied once to the whole selection,
never per chunk. A per-frame record of drawn text is the rejected shape: rows the
pane never drew would be missing from it. The selection is clamped to the cached
TRANSCRIPT rows for both highlight and copy, because an in-flight reply's tail is
drawn below them but is not in that cache — the one place the frame and the
redraw would disagree. The parity test pins that an on-screen selection copies
exactly what the drawn frame shows.

The preview's own scroll is **bottom-anchored by default**
(`App::preview_follow_bottom` starts true, is re-armed on every selection change
and preview show, and `clamp_preview_offset` then pins to `max_offset`). So
anything that must STAY visible is a **layout row, never a line prepended into
the scrolled `Text`** — a prepended line is scrolled off for any transcript
taller than the pane, which is the normal case. The preview's STICKY HEADER — the
pinned row that names the turn at the top of the viewport, with a live agent's
status and age after it, unless a failed background task outranks it (the
`has_banner` rule below) — is the instance:
`view::preview_split(area, has_banner)` carves the pane's inner rect
into a pinned banner row and the transcript beneath it, and returns the WHOLE
inner rect when there is no banner (so a banner-less pane's geometry is exactly
`Block::inner`, unchanged). The rules that follow from it:

- `preview_split` is the ONE place the banner/transcript geometry is derived,
  and `view::preview_areas` the one place the docked compose zone is carved off
  it (`preview_compose_split`, sized by `docks_compose` + the editor's own
  height). `render_preview` draws against `preview_areas`' rects, and
  `view::preview_transcript_rect` hands the SAME transcript rect — re-derived on
  demand from `App::preview_rect`, never stored — to EVERY pointer action over
  the preview: BOTH click hit-tests (`update::fold_under_pointer` and
  `update::resolve_link_click`) resolve a click against it, it supplies the width
  the fold toggle re-renders at (so the hit-test and the re-render cannot resolve
  through different widths), and the selection's press gate
  (`update::press_starts_selection`, a drag's and a double-click's word alike),
  its drag clamp, its autoscroll edges, its highlight overlay and its release
  copy's width all read it. There is no second copy of it in `update`. A click
  resolves through `App::preview_scroll` and the width-scoped hit cache, both
  measured from that rect's origin — derive it anywhere else and a click
  silently opens the wrong link or folds the wrong node, or a drag starts on the
  pinned row. All three mouse actions are also gated off while a surface that hides the
  transcript or takes the keyboard for a decision is up (`preview_pointer_blocked`:
  a modal, a confirmation, a pending chord, or the draft card), so none fires over
  a draft card. An open QUICK REPLY is not in that list: its docked box is outside
  the transcript rect by construction, so a press in the box selects nothing while
  one over the transcript acts as on the bare board. Both rects trace back
  to `preview_inner`, the ONE place the pane's border inset is applied — which
  matters most for the docked compose
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
  over-counts where the wrapper swallows the whitespace it broke on. No production
  path MEASURES a wrap any more — `wrapped_line_height` survives in `mod tests` alone,
  as the FOIL a fixture proves itself against, so a case cannot pass by accidentally
  agreeing with the wrapper.

  The one production path that PERFORMS a wrap is the exception that shows where
  the line is: `store::preview::wrap_span_cells` word-wraps a GFM table cell down
  its column, so a hand-rolled wrapper does sit beside ratatui's. It is safe for one
  structural reason, not by care — a grid line is clamped to the pane width, so it
  NEVER reaches ratatui's wrapper, and the two models therefore cannot disagree
  about a table. That same fact is what lets a grid cell record a clickable
  `LinkRegion` at all: its columns are the columns painted, with no wrap in between
  to move them, so a grid region always resolves at `sub_row` 0. Record mode is the
  other side of it and records nothing — its lines ARE handed to ratatui's wrapper.
  The one height it does take — `table_data_lines` sizing a row by
  its tallest cell — is not a PREDICTION either: it counts lines it has ALREADY
  PRODUCED itself, never guessing what ratatui's wrapper would do to them. Those
  lines stay ordinary `Line`s that `wrapped_row_prefix` then measures through the
  same widget seam as every other line. A second wrapper whose output could
  soft-wrap again would be the defect this bullet exists to prevent. The
  transcript is measured ONCE per (session, width), inside the `preview_cache`
  entry, so it can never be invalidated apart from the text it describes; whatever
  is NOT that cached transcript (the draft card, an in-flight reply's echo turns)
  is measured at the draw site.
- **The measurement is a MAP, not a total, and the draw is WINDOWED.**
  `view::wrapped_row_prefix` asks that same accessor once per LOGICAL LINE and
  keeps the running sum, so entry `n` is the screen row line `n` starts on and the
  LAST entry is the whole transcript's height — the number a single whole-text
  `line_count` used to answer, replaced rather than joined. What the map buys is
  O(log n) answers to "which line holds row `r`, and how far into it?"
  (`view::row_window`), which is what lets `render_preview` hand the `Paragraph`
  only the lines the viewport can reach and scroll it by a small RESIDUAL inside
  the first of them. A `Paragraph` re-wraps everything it is given on every frame,
  so a whole transcript cost the draw its whole length per frame; the window costs
  it the viewport's height. Two things ride on that identity and must stay
  ABSOLUTE, never window-relative: `App::preview_scroll` (and therefore the
  scrollbar, the clamp and `link_at`'s hit-test) counts rows from the top of the
  TRANSCRIPT, and the match map is keyed by transcript line, so a window adds its
  own start index back before looking one up.
- **The CLICK reads that same map, and `view::line_at_row` is the one place a row
  becomes a line.** `row_window` starts the window with it, and BOTH click consumers
  resolve their ROW with it — `link_at` (a url to open) and `fold_at` (the fold key
  of a node to toggle — a peer message or injected context) — so the paint and the
  two hit-tests cannot disagree about
  which line sits where. That is the failure a second derivation guarantees, and the
  one that nearly landed: the first cut of `content_hit` kept only the ROW half of
  its pane guard and clamped the column, which aliased every click LEFT of the pane's
  inner rect onto content column 0 and made a peer header (whose region spans the
  whole line) eat the pane border. Which LINE a click lands on is therefore EXACT at
  any length or scroll position. Answering the line from a
  per-line model instead — a packing walk down the whole transcript — grew the error
  with every wrapping line above the click, bounded by nothing once the preview's
  600-line tail cap was deleted.
- **The two hit-tests part on the COLUMN, and only the FOLD still approximates.**
  `fold_at` takes the character-packed answer from the private `content_hit`
  (`sub_row * inner_width`, packed because the wrapper will not say where inside a
  line it broke), so its error is bounded by ONE logical line's wrapped extent and
  can never reach another line whichever way that column slips — and it slips BOTH
  ways. `content_hit`'s doc comment owns the two directions and what each one costs
  a click; do not restate them. The bound is harmless there because a `FoldRegion`
  claims its header's whole display width, so a slipped column lands on the same
  node either way. A link label occupies only part of its row and could not afford
  it, which is what the probe below buys.
- **Which CELL of that line was clicked is asked of the RENDERER too, by a PROBE.**
  `view::region_paints_cell` re-styles one candidate `LinkRegion`'s graphemes with a
  marker `Modifier`, pushes that single line through the same
  `Paragraph::wrap(Wrap { trim: false })` into an in-memory `Buffer`, and reads the
  clicked cell back — so a wrapped link is clickable on every cell it was painted
  into, continuation rows included. This replaced the last packed step
  (`sub_row * inner_width`), which put a sub-row's computed start on either side of
  its true one and left a 92-column url's last 73 columns dead. THE PROBE IS NOT A
  SECOND WRAP MODEL, and that is the whole reason it is allowed: re-styling leaves
  the text byte-identical and splits only at grapheme-cluster edges, so the probe's
  wrap IS the paint's wrap. The equality is checked, not assumed — a marked line that
  does not measure to the rows the prefix map gave it resolves to NO link, because a
  hit-test that guesses hands an unintended url to a browser while one that abstains
  costs a click. THE SAME DIRECTION DECIDES THE UNWRITTEN CELL, which is the other
  place that promise is easy to lose. `render_line` writes once per grapheme and
  advances by its display width, so the right half of a double-width glyph is never
  written — but `Paragraph` only `set_style`s its area and never blanks a row's
  remainder, so the columns past a row's last glyph are unwritten too. The two are
  identical by symbol and opposite in meaning, so an unwritten cell counts as a hit
  ONLY when its left neighbour is marked AND measures two columns wide. Drop that
  width test and the blank column beside a link that ENDS a painted row opens that
  link — a click on empty space, which is exactly the wrong-hit this bullet forbids.
  Cost is one small render per candidate region per click, over only
  the rows up to the clicked one; a click is human-paced, so no frame pays it.
- **The probe is BOUNDED, and the bound is a PRODUCT.** Each candidate clones and
  re-wraps the WHOLE logical line, and a MISS pays for every region on it, so one
  click costs `candidates * line_bytes` of reading — unbounded on a single logical
  line holding a minified blob with several links in it, which is tens of
  megabytes of cloning on the UI thread per click. `view::LINK_PROBE_BYTE_BUDGET`
  caps that PRODUCT, because either factor capped alone admits the case the other
  exists to stop: a line just under a size cap still carries thousands of links,
  and one link still sits inside a megabyte-long blob. BYTES rather than the
  wrapped ROWS that arrive free off the prefix map, because rows count what the
  wrapper PAINTS while the probe pays for what it READS — a line of megabytes of
  zero-width marks wraps to ONE row, so a row budget prices that click at 1 and
  then admits thousands of candidates that each re-wrap all of it. Bytes SUBSUME
  rows, and `view::line_probe_bytes` sums them in O(spans) off each span's `len`.
  A candidate costs O(spans) as well as O(bytes) — `view::highlight_matched_spans`
  keeps even the EMPTY spans — so what would defeat a byte price is not an empty
  span but an UNBOUNDED NUMBER of them, a line of thousands measuring ZERO bytes.
  The REPEATABLE empty forms, whose count grows with the input, are refused at the
  parse in `store::preview::parse_inline_collect`: `flush_plain` skips an empty
  run, `match_delim` rejects `****`, `match_link` rejects `[]()`, and the
  inline-code arm rejects an empty backtick pair. Adding a delimiter form here
  means owing it the same empty-case rejection, or that count stops being bounded.
  `regions_from_inline` then drops ZERO-WIDTH regions so an unhittable candidate
  cannot multiply what is left. The empty spans that DO survive are bounded per
  line rather than absent, and what bounds them is a property rather than a roster:
  a block construct emits O(1) empties per line, so they cannot grow with the
  line's length. Several exist today — the `" ".repeat(indent)` prefix (empty on
  any unindented list item, on a line that can carry a region), the blank
  placeholder (emitted only when an inline run produced no spans, so it never
  coincides with a region), an empty ATX header, a `Line::from("")` spacer — and
  that list is ILLUSTRATIVE, not a census. A grid row adds no empty of its own: its
  `COLUMN_RULE`s and its hold-open pads all carry bytes, since a grid column's width
  never falls below 1. So spans are `O(bytes) + O(1)` per line and
  the product stays `O(candidates * bytes)`. This is deliberately a BOUND and not a
  list of which spans are non-empty: the bound still holds when a parser arm is
  added, whereas such a list rots the moment one is. The obligation a new arm
  inherits is therefore not "emit no empty span" — it is: do not emit them in a
  count that grows with the input, and if you must, make it cost bytes.
  PAST the budget the hit-test ABSTAINS — the same direction the
  row-count check above takes, for the same reason — and SAYS SO, answering
  `view::LinkProbe::Unresolvable` for `update::note_link_click` to map to a STICKY
  message, because an underlined affordance that does nothing on purpose must
  admit it. The failure mode stays "a click was missed, and the reader was told",
  never "a url the reader never aimed at reached a browser".
- **That map's soundness is one property, and it is PINNED.** Summing per-line
  counts equals the whole-text count only because `Wrap { trim: false }` wraps
  each logical line independently and never joins two onto one row — the same
  additivity the in-flight reply tail is added by. It is a claim about a private
  ratatui module held by an exact pin, so
  `per_line_wrapped_row_counts_sum_to_the_whole_text_count` asserts it over
  wrapping ASCII and double-width CJK/emoji. A bump that breaks it must go red
  there first, because every offset the pane computes derives from that sum.
- **A row OFFSET is one index into that map.** The pane scrolls itself onto a
  search match, and the row it scrolls to is `row_prefix[k]` for matched line `k`
  — the first screen row that line occupies, read in O(1) rather than re-wrapping
  the transcript's whole prefix on each keypress.
  `view::MATCH_JUMP_LEAD_DIVISOR` then backs the offset off by a third of the
  viewport so the match lands with context above it, and the ordinary
  `clamp_preview_offset` bounds the result. The approximate model is no more
  substitutable here than it is for a height: it would park the match rows away
  from where the wrapper paints it.
- **The match jump is armed by a USER ACT, not by a session being written.** It is a
  pending FLAG (`App::preview_match_jump`), and its whole correctness is in where
  it is armed: `request_preview_match_jump` is called from the selection setter and
  from the one query-change funnel (`apply_query_change`), and nowhere else — which
  is why the `Tab` mode toggle goes through that funnel rather than straight to the
  re-filter: the query text does not move, but the same text now matches more. The
  setter arms only when the id actually MOVES, and a reload restores the selection by
  id, so the setter is a no-op there — which is what stops a live session appending
  turns at the watcher's cadence from yanking the reader's viewport every few hundred
  milliseconds. The exception is the reload whose selected row VANISHED from disk:
  restoring then clamps to a neighbour, a different id, and the flag arms. That is
  benign — the previewed session changed under the reader regardless, and the same
  branch re-anchors the pane. The flag is CONSUMED in
  `render_preview`, the only place the pane's width and height are known, and is
  dropped rather than deferred by EVERY frame that cannot act on it — something
  else owns the pane (a draft card), nothing is selected, or the 1:0
  `PaneLayout::ListOnly` layout hid the pane so that function never ran at all
  (that branch of `render_body` takes it there). Enumerate all of them or the flag
  survives onto an unrelated later frame: the board draws before it reads the next
  key, so the frame that cannot act on a request is the last one that will ever
  see it. Arm any future
  auto-scroll the same way: inside the branch that fires only when the state actually
  MOVED, never on the recompute a reload shares.
- **A layout step keeps the reader's place through a second one-shot of the same
  shape.** `preview_scroll` counts wrapped ROWS, which mean a different line at a
  different width, so `App::set_pane_layout` notes the LINE at the top of the pane
  (`view::line_at_row` over the map the last frame was drawn from) into
  `App::preview_anchor_line`, and `render_preview` puts that line back at the top
  as `row_prefix[k]` at the new width — no lead, since it restores a position
  rather than presenting a match. It is consumed on the same paths as the jump,
  loses to a pending jump, and is noted only for a pane the reader positioned: a
  pane following the newest turn keeps following it at any width. Two limits are
  by design: it is line-granular (a top row inside a wrapped line comes back as
  that line's first row), and a GFM table above the anchor that re-flows between
  grid and records at the new width shifts it by the lines it gained or lost. A
  terminal resize does NOT go through it.
  The mode gate lives at the arming site too — the AUTOMATIC jump is
  `SearchMode::NameAndContent`'s alone, while the explicit `Shift`-arrow step is a
  keypress and needs no such assumption.
- **The bottom anchor is ONE flag, and nothing overrides it for a frame.**
  `App::preview_follow_bottom` answers "is this pane still anchored, or did the
  reader position it?", so a surface that wants the tail follows it THROUGH that
  flag — arming it at the user's act — and never ORs its own condition into the
  render's decision. The in-flight quick reply is the instance and the trap: its
  tail is `Some` for the WHOLE duration of a send while `render_preview` runs
  several times a second (the spinner redraws every tick), so `follow ||
  sending` re-asserted the anchor on every one of those frames and undid — one
  frame later — whatever the reader or the match jump had just done. A one-shot
  cannot win against a per-frame recompute; give the decision state that outlasts
  the frame instead. Every transition is a USER ACT: ANY scroll releases the
  anchor (in either direction — a scroll states a position, not a subscription —
  and a drag held past the pane's edge included, on a step whose autoscroll
  actually moves the pane), and only `End` (or its `Ctrl-E` twin; an open quick reply answers every scroll key through the same methods), another row, or a layout change that
  brings the pane back from 1:0 (`App::set_pane_layout`, which `Shift-←` and an
  opening compose both go through) re-arms it. A step between two layouts that
  both show the pane neither arms nor releases it. The render writes the flag for
  exactly one thing, the match jump it alone can resolve, and
  never infers a re-arm from its own CLAMP: an offset the content cannot satisfy
  is equally a reader scrolling past the end, a pane widened by a resize, and a
  transcript that shrank, so re-arming on it took a deliberately positioned pane
  away with no key pressed.
- `has_banner` is **`view::preview_banner(app).is_some()` — never liveness, and
  never the reported-agent set**. It is true for EVERY selected session, because
  the row names the turn you are reading and every transcript has one. What the
  row SHOWS is a separate question with ONE precedence, resolved in
  `render_preview`'s banner remap and nowhere else: a FAILED background task the
  session still carries (`view::failed_task_banner_line` — claude's summary alone,
  in `FAILED_TASK_COLOR`) > the turn marker at the top of the viewport, followed —
  for a LIVE agent only — by `HEADER_SEPARATOR` and `<status> · <age>` in the
  status's Cyan + BOLD (`view::marker_with_live_status`) > the reported status,
  with its age when known > a blank row. LIVE is the polled record's `pid`
  (`view::reports_live_process` — a DISPLAY reading of the `--all` map, never the
  probe's `App::is_live_now`, never `agents::is_active`, never a `classify`
  bucket), and the suffix also needs `agents::elapsed_phrase` to answer against
  the poll's stamp; any other session's marker row is EXACTLY the marker, a
  finished record with a known age included. The marker leads, and the row is
  never wrapped, so a narrow pane cuts the age, then the status, before any of
  the marker. So the failure takes the sticky header's place — and
  the live status's — on that session until the user writes into it, and the
  reported status alone is only the fallback for a transcript with no marker and
  no standing failure. None of that
  touches the reservation: the remap is `Option::map`, so a failure can neither
  add a row nor revive one a replacement pane suppressed. Keying it on the
  polled `--all` map hid the pinned turn for every session claude does not list —
  `claude agents` lists only ACTIVE sessions, so an interactive one nothing holds
  open is absent — and moved the transcript a row whenever a poll flipped a
  session in or out. Keying it on liveness is worse: liveness is *unaskable* here,
  since it means a shell-out to claude (`App::is_live_now`), which a render must
  never do. Nor may it key on "has markers": those live in the width-scoped cache,
  which `preview_banner`'s `&App` read cannot build, so the hit-test could ask
  before the frame that fills it and disagree with the draw. Name it for the
  banner. Anything that REPLACES the transcript must therefore suppress the banner
  inside that one fn rather than skipping it at the draw site: the in-flight quick
  reply does (its echo turns take the banner's place inline) and so does the
  new-session draft card (there is no session to describe). Skip it at the draw
  site instead and the hit-test still reserves a row that was never painted.
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
rather than behind a teardown whenever it needs no TTY. A single NON-blocking
syscall is not this shape and gets no thread: `Ctrl-K`'s SIGTERM
(`Outcome::Signal`) runs inline in the driver, because this rule governs blocking
work and `kill(2)` returns as soon as the signal is queued. That satisfies the
rule rather than waiving it.

The clipboard copy is that same THREADED shape, and it is NOT a third
synchronous one-shot beside the two exceptions below. `handle_chord_key` returns
`Outcome::Copy(CopyPayload::SessionId(id))` for `Ctrl-X y`, a finished preview
drag (or a double-click's word) returns `Outcome::Copy(CopyPayload::Selection(text))` from the mouse arm,
and the driver (`run_inner` → `start_copy`) reads the
environment at that edge, picks the route (`tui::clipboard::clipboard_route`),
and, when the route has a tool, starts `clipboard::spawn_tool_copy`: a detached
worker thread per copy, like `Send`/`Interrupt`/`BgLaunch`. The worker pipes the payload's text into each candidate tool's
STDIN, with stdout and stderr `Stdio::null()` as `resume::open_url` nulls its
opener's, so a tool can neither paint over the board nor hold open a pipe
anything waits on. Exit 0 means copied and ends the walk; a missing tool, a
failed stdin write or a non-zero exit moves on to the next candidate. It then
delivers exactly ONE `AppEvent::CopyFinished { payload, copied }`, and a tool
that hangs blocks only its own worker. The worker NEVER writes to the terminal.
The OSC 52 fallback is written by `update::finish_copy` on the UI thread,
between draws: at once from `start_copy` when the route has no tool, or when the
`CopyFinished` that `handle_event` hands back as `Outcome::FinishCopy` says
`copied: false`.

`watch::spawn_model_alias_thread` is the fifth of that shape and the one that
spawns no child at all: it READS the installed `claude` binary for its `--model`
alias set and delivers a single `AppEvent::ModelAliases`, started once per board
session rather than per keypress. Its shutdown-flag argument is structural rather
than negotiated — a thread that sends once and returns has no loop to bound, so
it cannot accumulate one per resume round trip, which is the failure the flag
exists to prevent. Note what it is NOT: it is the rule's ORDINARY case (own
thread, `AppEvent`, render loop never blocked), **not** a third entry on the
exception list below. That list is for work that runs ON the UI thread, and
nothing that delivers an event belongs on it.

`watch::spawn_settings_model_thread` is the sixth, and the same shape again: it
resolves, from the user's `claude` settings files and environment, the model a NEW
session would get and whether an environment override stops a `-r` restore
(`claude_settings::model_defaults`), and delivers both in a single
`AppEvent::SettingsModel(ModelDefaults)`, started once per board session beside
the alias probe. The one difference is deliberate: it is NOT memoized. A board
session begins at launch and again on every return from a `claude` child, and the
settings are the user's live files, so re-reading at exactly that moment is what
lets a `/model` pick saved as the default inside the child reach the next draft's
label. It spawns no child and reads a handful of small files, so it is an ordinary
event-delivering one-shot, not an exception. Its consumers never read a file
either: a compose's `model:` label and the picker's first row are answered by
`App::compose_default` from what this event stored and from the CACHED preview,
which is what lets the label be asked in render and the row on a keystroke.

`claude_catalog::spawn_fetch` is the seventh, and the one that runs a `claude`
child from an event rather than from a confirmed key: claude's `initialize`
handshake for a compose's folder, delivered as a single `AppEvent::CatalogFetched`.
It keeps the shape by splitting the DECISION from the spawn exactly as `Send`
does: the pure `compose::take_catalog_fetch` derives "fetch this folder now" from
state (when that is true is [DOMAIN.md](DOMAIN.md#compose-pick-list)'s), the run
loop asks it after every wake-up, and only the driver spawns — so no key handler,
and no route that opens a compose, ever starts a child. The worker's FIRST step
is a blocking file read too: claude's workspace-trust verdict for the folder
(`claude_trust::folder_trust`), which picks the form, argv and child environment.
It stays inside the
worker, where `spawn_fetch_in` hands it in as `trust_of`; hoisting it onto the
spawning thread is the regression
`children::the_trust_read_runs_on_the_fetch_worker` pins. The child
is the first one-shot here whose run is BOUNDED by snapback rather than by the
child. It is spawned as the leader of a process group of its own, and
`CATALOG_FETCH_TIMEOUT` covers the reply, the close of stdout and the exit. Past
it the worker SIGKILLs the whole group (`claude_catalog::kill_group`, `killpg(2)`)
AND the child itself (`Child::kill`, in `reap`), both before the child is reaped,
so neither id can have been recycled. The group kill takes everything still in
the group; the direct kill takes a LEADER that moved itself into another group
(its own `setpgid`), which the group kill misses and the reap's `wait` would
otherwise block on for as long as it runs. The worker then reaps the child and
joins its stdout reader once every writer of the pipe is gone, within
`CATALOG_READER_GRACE`. A hung `claude` therefore costs the worker at most
`CATALOG_FETCH_TIMEOUT` plus `CATALOG_READER_GRACE`. The one residual is a
DESCENDANT that LEFT the group (its own `setpgid`/`setsid`) while holding stdout:
neither kill reaches it, so only its reader thread outlives the fetch, ending with
that writer, and the worker still delivers its event. Nothing waits on
the event, and it carries no shutdown flag for the alias probe's reason: it sends
once and returns.

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
  two refusals, EVERY branch of the delete confirm, and EVERY branch of the
  `Ctrl-K` signal confirm's `Enter` — it lands ~0.26s after the keypress: a real,
  deliberate hitch. Do not paper over it with a zero-render claim that only holds
  on one branch.
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
of decision that must not be made from a stale snapshot. `update::dispatch_signal`
is the fourth: `Ctrl-K`'s signal confirm asks AGAIN at `Enter`, because the pid it
captured is worse than stale data once the confirm has sat open — it may name an
unrelated process by then. The job-id arm of that same confirm deliberately does
NOT re-ask (a stale job id makes `claude stop` fail safe), and that asymmetry is
argued at the call site so a "symmetry" refactor cannot erase it. The reason is the same one
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
`App::apply_reload` — which puts it in the same category as the synchronous
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
loops on; `spawn_model_alias_thread` and `spawn_settings_model_thread` each take
the `probe` they run once. All four are named exactly once, in `EventLoop`, where
production passes `agents::reported_agents`, the real crossterm `poll`+`read` pair,
`model_aliases::installed_model_aliases`, and `claude_settings::model_defaults`
for the launch dir. `claude_catalog::spawn_fetch_with` takes its `fetch` the same
way, and `spawn_fetch_in` builds that `fetch` from a `trust_of`, a program and a
timeout, all three named exactly once, in `spawn_fetch`
(`claude_trust::folder_trust`, `claude`, `CATALOG_FETCH_TIMEOUT`). One level further
down `fetch_in` takes the same three and `fetch_with` the argv, the child's
environment, the request and the timeout, so the suite states a trust record, and
drives the pipe, the child's environment, the group kill, the direct kill, the
reap and the reader join with stand-in children (`sh`, and one `perl` leader:
[Testing patterns](#testing-patterns)), and never spawns `claude`. Nothing else
may pass anything else: the seam exists so a test can state a poll's answer without spawning `claude`, state
an input event without a TTY, state an alias set without walking a 290 MB
binary, and state the settings' answers without reading the machine's real
settings or environment — which is what makes each thread's own behavior assertable at all (the
idle gate the poller obeys, the board-activity stamp it writes, and the probe's
load-bearing property that the SPAWN returns before the scan does).

What the seam does NOT cover is the production source on the far side of it, and
that gap is **accepted, not overlooked**: `watch::read_terminal_event` has no
direct test, because exercising it needs a real TTY — without a controlling
terminal (CI) crossterm's `poll` errors immediately, so any test of it would
assert the error path and call that coverage. It carries no decision of its own
(a `poll` + `read` pair where either half's `Err` propagates unchanged), and
everything downstream of it is pinned through the seam. The `run_inner` lines that
START these threads (`spawn_agents_poller`, `spawn_model_alias_probe`,
`spawn_settings_model_probe`, and the `claude_catalog::spawn_fetch` call behind
`compose::take_catalog_fetch`) are
accepted the same way and for the same reason: `run_inner` needs a real terminal,
so there is nothing to assert them from, while the thread's shape is pinned
through the seam and the event's effect through `update::dispatch` — the gap is
one call, not a behaviour. Leave all of these untested rather than "fixing" them
with a proxy assertion — see the false-clean modes below.

The compose pick list adds two more BOUNDED synchronous reads, in the same class as
`defined_agents::discover_agents` and `send::plan_send`: a reply's transcript
listing (`store::skills::read_listing`, once per reply draft and only for a
top-level `@`, never for `/` or a background draft, and not at all once the
folder's catalog has landed) and a
folder walk (`complete::list_tree`, once per resolved folder, capped by
`COMPLETION_MAX_DIR_ENTRIES` per folder and
`COMPLETION_MAX_TREE_ENTRIES` in all, at any depth, on both drafts). They run from
`compose::refresh_completion`, called by the key and paste handlers and by
`update::dispatch`'s `CatalogFetched` arm — which reads nothing new in practice,
since every read is keyed to a token a key handler already resolved — and NEVER
from the render path, which only reads the cached `visible` list. claude's own
list is not one of them: it is the event-delivering fetch above.

## 7. Restrained, terminal-safe styling

The preview and list are styled with ratatui `Style` only — **never** embedded
ANSI escape sequences. Prefer `Modifier`s (BOLD/ITALIC/DIM/UNDERLINED) plus a
small palette of **named** ANSI `Color`s (they adapt to the user's terminal
theme). Do **not** hardcode RGB (it can vanish on a light background) and do not
syntax-highlight code (code is DIM). The markdown pass in `store::preview` is
hand-rolled and self-contained — no external markdown crate.

**A mark laid OVER existing style is a `Modifier`, never a color.** The two
search-match highlights are the pair to compare: a list row's label is plain text
the view owns, so it takes a color plus BOLD, whereas a preview line arrives
already styled by `store::preview`, so `view::PREVIEW_MATCH_MODIFIER`
(`REVERSED`) is COMPOSED onto whatever style it lands on. A foreground color
there would erase the line's own meaning — and on the wrong terminal theme, the
text with it. `REVERSED` in particular is the one attribute this board already
relies on being honored (the list's selection highlight), unlike the blink
attribute below.

A preview LINK is not such a mark. Its `LightBlue` + ITALIC + UNDERLINED
(`store::preview::link_style`) is the element's OWN style, chosen by the parser
that knows it is a link, so it may carry a color; it is PATCHED onto the
enclosing run (`base.patch(link_style())`) so a bold, italic, or quoted run's
modifiers survive on the label. The color is the NAMED bright blue — the
conventional link hue, readable on a dark theme where ANSI `Blue` often is not —
never an RGB value; the italic underline echoes how terminals such as JetBrains'
mark a url they auto-detect. Nothing else in the preview pane is `LightBlue`
(the list's search-match highlight shares the hue, but in the other pane and
BOLD), and its color and italic are what tell a link from an H1, which is
underlined too. A link inside a DIM run (a blockquote) keeps that DIM and draws
a fainter blue — an accepted trade.

That look is an AFFORDANCE, so it is worn only where a click can land. The one
inline parser takes a `store::preview::LinkRender` switch — `Clickable` patches
`link_style`, `Inert` leaves the label in its run's own style with the SAME text
and the same recorded columns — and a caller picks by whether its link regions
reach the pane. A GFM table in its narrow-pane RECORD layout records none, and a
click there resolves to `LinkClick::NoLink`, which writes nothing (§11), so it
parses `Inert`: a label that looked clickable there would fail silently. Pick the
variant; never restyle a link at a call site.

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

**The one exception is a held drag's AUTOSCROLL**, and it is deliberately narrow:
it is a MOTION that phases no animation, and it runs only while the gesture
lasts. The rule exists to keep phased animations from drifting apart and to keep
the idle board cheap, and the autoscroll threatens neither — nothing reads a
phase off it, so nothing can drift against `blink_visible`, and with no drag held
past the edge the loop blocks with no deadline, exactly as before. So it runs on a
short frame deadline of its own rather than the tick: while
`App::autoscroll_due_in` reports a drag held above or below the transcript rect,
`tui::run_inner` waits for the next event with an `app::AUTOSCROLL_FRAME` (33 ms)
timeout instead of blocking (`watch::EventLoop::wait`, whose `Waited` tells an
event, a timeout and a closed channel apart), and after EVERY wake-up — an event
or the deadline — calls `App::autoscroll_preview_selection`, which pays out the
time that really elapsed through the pure `app::autoscroll_rows`: a page per
`AUTOSCROLL_PAGE_PERIOD` for each row past the edge, up to
`AUTOSCROLL_MAX_DISTANCE`, carrying the part of a row between frames. The speed
therefore cannot depend on how often the loop wakes or the mouse reports a move,
and the tick no longer steps it at all — riding the 250 ms tick made it jump a
quarter page at a time. The exception covers exactly that: a new animation that
PHASES anything, or a deadline armed outside a held gesture, is not it. A lost
release would otherwise scroll forever, so a plain move while the press is held
resolves as the release (`update::mouse_effect`).

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
control-wrapper collapse** (`store::command::collapse_control_wrappers`, the
one parse the preview, the content index and the label share). Claude
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

No magic numbers. Every tunable is a named `const` carrying a rationale comment,
declared at module scope — near the top where a module owns a handful of them
(`watch`, `store`, `parse`, `label`), beside the code it governs where it is one
of many (`tui::view`'s pane/modal/compose geometry). The complete set of
CADENCES and LIMITS, so a retune knows what it is next to:

| Module | Tunables |
| --- | --- |
| `watch` | `DEBOUNCE` (200 ms) · `TICK` (250 ms) · `AGENTS_REFRESH` (5 s) · `AGENTS_IDLE_AFTER` (60 s) · `INPUT_POLL_INTERVAL` (50 ms, private — the reader's wake-up cadence, so short teardown beats a busy spin) |
| `store` | `MTIME_SETTLE_WINDOW` (2 s) |
| `store::parse` | `CONTENT_INDEX_CAP` (1 MB) |
| `store::label` | `LABEL_MAX` (180) |
| `search` | `PROXIMITY_WINDOW_MIN_BYTES` (200) · `PROXIMITY_WINDOW_QUERY_MULTIPLIER` (3) · `PROXIMITY_MAX_ANCHORS` (512 — asked of the rarest atom alone; see [DOMAIN.md](DOMAIN.md#content-index-storeparse)) |
| `model_aliases` | `MAX_ALIAS_ARRAY_BYTES` (4096) · `SCAN_CHUNK_BYTES` (1 MiB) · `SCAN_OVERLAP_BYTES` (= `MAX_ALIAS_ARRAY_BYTES` by definition — that equality is what proves no match straddles a chunk unseen, so neither is retuned alone) |
| `store::preview` | `MODEL_VERSION_MAX_DIGITS` (2) · `MODEL_DATE_DIGITS` (8 — kept above the version cap so a date never reads as a version) · `TABLE_MIN_COL_WIDTH` (10) · `RECORD_RULE_WIDTH` (32) · `COLUMN_RULE_WIDTH` (3) · `ELLIPSIS_WIDTH` (1) · `PEER_STEM_LEN` (17 — the agent-stem length a peer sender must match before it renders as an `@handle`, so a socket path or an agent TYPE name falls back to the generic label) · `PEER_HEADER_BLOCK_ROW` (1 — not a knob but a SHAPE: every fold node's header index — peer and injected alike — inside its own `[blank, header, body…]` block, named so the fold region and the body links rebase off one number) |
| `send` | `SEND_ERROR_MAX` (200) |
| `claude_catalog` | `CATALOG_FETCH_TIMEOUT` (10 s — bounds the reply, the close of stdout AND the exit; the reply measured 0.21–0.22 s) · `CATALOG_READER_GRACE` (500 ms — how long a timed-out fetch waits, after the group and child kills, for its stdout reader to see EOF; the worker's worst case is the timeout plus this) · `CATALOG_EXIT_POLL` (20 ms — claude exits 25–50 ms after its stdin's EOF) |
| `claude_trust` | `GIT_POINTER_MAX_BYTES` (8192 — a git pointer file holds one path line, and a path is at most `PATH_MAX`, so a longer file is not a pointer and gets the untrusted-direction fallback rather than a whole read) |
| `tui::complete` | `COMPLETION_MAX_DIR_ENTRIES` (2000 — one keystroke's worst read of a huge folder) · `COMPLETION_MAX_TREE_ENTRIES` (25000 — the whole `@` walk, breadth first, no depth cap: a depth cap hid a 9-level monorepo path) · `COMPLETION_VISIBLE_ROWS` (8) |
| `tui::app` | `PREVIEW_WHEEL_STEP` (2) · `LIST_WHEEL_STEP` (1) · `STATUS_DWELL_TICKS` (16) · `MIN_PANE_WIDTH` (15) · the list's share of the body per split `PaneLayout` stop: `PREVIEW_WIDE_LIST_PERCENT` (25) / `DEFAULT_LIST_PERCENT` (48) / `LIST_WIDE_LIST_PERCENT` (75) · a held drag's autoscroll: `AUTOSCROLL_FRAME` (33 ms, the run loop's wait deadline while one is held past the edge — §7's one exception to animating from the tick) / `AUTOSCROLL_PAGE_PERIOD` (1 s, a page per row past the edge) / `AUTOSCROLL_MAX_DISTANCE` (4) |
| `tui::update` | `PASTE_MAX_CHARS` (4096) |
| `tui::view` | `BLINK_TICKS` (2) · `CHILD_ID_CHARS` (8) · `MATCH_JUMP_LEAD_DIVISOR` (3 — a jumped-to match parks `h / 3` rows down) · `WIDE_GLYPH_COLUMNS` (2) · `LINK_PROBE_BYTE_BUDGET` (131_072) · the layout rows `PREVIEW_BANNER_ROWS` / `BOARD_CHROME_ROWS` / `COMPOSE_*` / `MODAL_WIDTH` / `MODAL_*_CHROME_ROWS` / `MODAL_BORDER_ROWS` / `MODAL_BORDER_COLS` / `MODAL_LIST_MAX_ROWS` (12 — the most CHOICES a `List` picker offers before it scrolls, so an overlay stays an overlay on a tall terminal; a wrapped row's extra lines are paid on top, see [§5](#5-selection-and-scroll-survive-reloads)) · the compose pick list's `COMPLETION_BORDER_ROWS` / `COMPLETION_COLUMN_GAP` (2 — the table's only separator) / `COMPLETION_NAME_MAX_PERCENT` (50 — descriptions keep the other half of the box) |

Add a new tunable the same way. The rule is not only about numbers — a literal
with a meaning gets a name whatever its type: the undocumented `claude` wire
tokens (`agents::KIND_*` / `QUALIFIER_*`; `claude_catalog`'s `CATALOG_REQUEST_ID`,
`CATALOG_SETTINGS`, `USER_SETTING_SOURCES`, the git-prefetch switch
`DISABLE_GIT_INSTRUCTIONS_ENV` / `DISABLE_GIT_INSTRUCTIONS_ON`, `CLAUDE_PROGRAM`,
`CONTROL_RESPONSE_MARKER`, the `process_group` argument `NEW_PROCESS_GROUP` and
the pinned built-in names `CLAUDE_HIDDEN_BUILTINS`;
`claude_trust`'s record and pointer names — `GLOBAL_CONFIG_FILE`,
`CUSTOM_OAUTH_GLOBAL_CONFIG_FILE`, `LEGACY_GLOBAL_CONFIG_FILE`,
`CUSTOM_OAUTH_URL_ENV`, `PROJECTS_KEY`, `TRUST_ACCEPTED_KEY`, `GIT_ENTRY`,
`GITDIR_PREFIX`, `COMMONDIR_FILE`, `GITDIR_FILE`, `WORKTREES_DIR` — and the
spellings and locations it refuses a worktree pointer into, `UNC_PREFIX`,
`BACKSLASH`, `NT_OBJECT_MARKER`, `REFUSED_MOUNT_ROOTS`, `HOME_MOUNT_ROOT`,
`FIRMLINK_PREFIX` and `MOUNT_NAME_FORMAT_CHARS`; `store::skills`'
`AGENT_LISTING_MARKER` and the line shapes `LISTING_LINE_PREFIX`,
`NAME_DESCRIPTION_SEPARATOR` and `AGENT_TOOLS_SUFFIX`;
`tui::complete`'s `AGENT_MENTION_PREFIX` / `AGENT_LABEL_SUFFIX`), the raw control bytes the
terminal seams write because crossterm publishes no typed command for them
(`tui`'s `CAN` / `ST` / `KITTY_DISABLE_KEYBOARD` / `DECSTR` / `DECCKM_OFF`, and
`tui::clipboard`'s OSC 52 wire bytes `OSC_INTRODUCER` / `OSC52_COMMAND` /
`OSC52_CLIPBOARD_SELECTOR` / `OSC_PARAM_SEPARATOR` / `OSC_STRING_TERMINATOR`), and
the path literals the grouping heuristic scans for (`store::group`'s
`PATH_SEPARATOR` / `HIDDEN_DIR_PREFIX`). `tui::TICK` is not a second knob — it is
an alias of `watch::TICK`, which stays the one definition.

A const whose rationale depends on ANOTHER const says so and names it:
`BLINK_TICKS` is meaningless without `watch::TICK` (they multiply into the pulse's
500ms phase), so its doc comment shows the arithmetic and points at `TICK`.
Likewise, `tui::app::STATUS_DWELL_TICKS` is meaningless without `watch::TICK`
(`16 * 250 ms = 4 s`), so its doc comment names `TICK` and shows the arithmetic.
That naming is what makes the coupling discoverable when the other value is retuned.

A const that is a **SWITCH rather than a limit** says so loudest, because retuning
it changes which code path runs rather than how far one goes.
`store::preview::TABLE_MIN_COL_WIDTH` is the instance: it is a table column's width
floor, but since a grid that cannot seat every column at that floor is abandoned
for the stacked record layout, the same number decides WHICH LAYOUT a table gets.
At a 62-column preview it seats 5 columns exactly and sends 6 to records, so a
nudge either way silently re-shapes ordinary tables. Where a const has a switching
range like that, pin the switch points in a test rather than trusting the doc
comment — `the_record_fallback_switch_points_are_pinned_at_a_62_column_pane` is
what makes a retune's blast radius visible.

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

1. `key_to_action(key, query_empty, has_preview_matches)` → an `Action` (every
   printable char types into the query; arrows, Enter, Tab, and `Ctrl-*` always
   act so search never blocks navigation).
2. `apply_action` mutates the `App` and returns an `Outcome`
   (`Continue`/`Quit`/`Resume`/`Send`/`Interrupt`/`BgLaunch`/`Signal`; `Copy` comes
   from `handle_chord_key` and `FinishCopy` from `handle_event`, below). `Send`,
   `Interrupt` and `BgLaunch` carry a confirmed `SendRequest` / `InterruptRequest` /
   `BgLaunchRequest` the driver spawns without a teardown (the board stays up), the
   way `Resume` carries a confirmed `Ready` — the decision is data, the effect is
   the driver's. `Signal { pid }` carries a re-verified pid the same way; the driver
   sends it a SIGTERM inline rather than on a thread (see §6). Add a new effect
   this way, not by spawning or signalling inside the handler.
   The clipboard copy is the same shape in two steps: the chord's
   `handle_chord_key` returns `Outcome::Copy` (the full id) — and a finished
   preview drag returns it with the selected text — for the driver to start, and `handle_event` turns the worker's `AppEvent::CopyFinished` into
   `Outcome::FinishCopy` rather than finishing it itself, because its OSC 52
   fallback is a terminal write only the driver may make (§6).
   Which of the two shapes a new action takes is decided by the CHILD, not by what
   it is called: a background-agent launch is `--bg` (returns at once, needs no
   TTY) so it stays on the no-teardown side, while its `Ctrl-O` twin hands the
   terminal over and is therefore an ordinary `Resume`.
3. Modal state owns the keyboard: ONE `App.modal: Option<Modal>` serves every
   titled overlay — the running-session choice, the new-session agent picker, a
   compose's `Ctrl-L` model picker, and
   the hard-delete confirm — through the generic `modal_key` → `confirm_modal`
   machine, dispatching each choice's `ModalAction` tag (a `Row` layout binds the
   horizontal `←`/`→`/`h`/`l` to MOVE its highlight; a `List` never moves sideways,
   binds `←`/`→` to `ModalNav::Adjust` instead and leaves `h`/`l` unbound — the two
   maps are never unioned). A key that belongs to ONE overlay rather than
   to modals in general is narrowed twice, at both stages: the picker's `Ctrl-O` is
   bound on the `List` layout in `modal_key` and acted on only for a
   `ModalAction::New` choice in `launch_pick_interactively`, and the model picker's
   `←`/`→` are bound on the `List` layout the same way and acted on only for a
   `ModalAction::SetModel(Some(_))` row in `App::adjust_modal_effort` (the agent
   picker and the `default` row stay inert), so neither a new `Row` modal nor a
   future `List` one can inherit a verb it has no meaning for. Because the modal
   owns the keyboard, those arrows can never also reach the board's search caret
   (a test pins it against a caret parked mid-query). The FOOTER follows the verbs,
   not the layout: each constructor names its own (`Modal::footer`, one const per
   overlay kind), because the two `List` pickers share a key map but not what the
   keys do — a layout-derived footer advertised the agent picker's
   `Enter draft · ^O interactive` on the model picker, where `Ctrl-O` is inert. Four
   more keyboard owners sit alongside it: the `Ctrl-X` leader chord (while
   `App.pending_chord` is set, `chord_key` routes the next key — `x` hide, `d`
   delete-confirm, `h` show-hidden, `r` forced full store re-read, `y` copy
   session ID, `f` fold / expand the selected row's lineage, anything else
   cancels), the "stop the
   waiting agent?" confirmation via `App.pending_stop` (a plain Enter/Esc gate
   before compose, for the `needs input` quick-reply path), its `Ctrl-K` sibling
   `App.pending_interrupt` (the same Enter/Esc gate, but resolving to a bare
   `Outcome::Interrupt` on the job-id route, or — after a re-probe — to
   `Outcome::Signal` on the pid route, rather than into compose), and the compose
   zone via
   `App.compose` + its `compose_key_to_action` machine — ONE keyboard owner for
   BOTH drafts, since which one is open is a `ComposeTarget` rather than a
   second piece of state. `handle_event` checks each in turn before the board,
   and the MODAL first of all — which is what lets a compose's `Ctrl-L` open the
   model picker OVER the still-open compose: while the picker is up it owns every
   key, and its close (`Enter` after writing the pick, or `Esc`) hands the keyboard
   straight back to the untouched draft, with no state saved or restored.
   `App::preview_pointer_blocked` (`modal.is_some() || draft.is_some() ||
   pending_stop.is_some() || pending_interrupt.is_some() || pending_chord`) gates
   the mouse's three actions over the preview — toggling a fold node, opening a
   preview link, and starting a preview selection (a drag-select, or a
   double-click's word-select) — so none fires while any is up. An open QUICK
   REPLY (`compose` with no `draft`) is deliberately absent: it owns the keyboard
   but previews the REAL transcript above its own docked box, and none of the
   three actions touches what it holds — its text and caret, its target session
   id, or the row selection that id is addressed by. A selection is mouse state
   that any key ends (so typing never fights it), a fold toggle re-renders the
   same session, and a link opens in the browser. A new-session draft is still
   gated, by its `draft` card (`App::open_compose` installs editor and card
   together). All three begin from a left PRESS the one gate
   (`update::press_starts_selection`) admits; the press only RECORDS where it
   landed — a second admitted press on the same cell within
   `DOUBLE_CLICK_INTERVAL` records the word under it instead — and the RELEASE
   resolves it: a drag or a double-clicked word selects and copies, a plain click
   toggles the fold under the press, else opens the link there. So a press the
   gate refuses leaves its release nothing to act on, and is no first half of a
   double-click either. A mouse
   wheel is handled **before** and **independent of** that gate: it never routes
   into an overlay handler, it only scrolls a pane, and it (like any keypress)
   clears an active preview text selection first. The selection is
   content-anchored, so the scroll alone would not strand it; the notch ends it
   because a notch during a HELD drag would otherwise have to extend that drag
   too, and it does not — a drag moves the pane only through its own autoscroll.
   A new
   keyboard owner or pane owner must be asked the same question before it is
   added to `preview_pointer_blocked` or left out: does a click, a fold toggle or
   a selection over the transcript act on a decision it is waiting for, or on
   text it has hidden? If so it belongs there, or the mouse will act underneath
   it; if the owner leaves the transcript on screen and the three actions leave
   its state alone (the quick reply), blocking them only costs the user the mouse.
   What stays blocked while a reply is open is everything that would change which
   row is selected, which is the target's identity: the wheel over the LIST (below)
   and every key, since all of them are the reply's.

   The wheel takes exactly ONE condition, and `update::wheel_target` owns it as a
   parameter (`composing`) the way `key_to_action` owns its own. It hit-tests
   THREE zones — inside the preview, inside the list, outside both — and
   `composing` narrows exactly ONE of them: **while the compose zone is open the
   list is not a wheel target at all**, and a notch over it resolves to
   `WheelTarget::Ignore`, which does nothing. The list arm is the reason, and it is
   the only arm that earns this: alone among the three it does not scroll a
   VIEWPORT, it MOVES THE SELECTION, and the selection is what the preview shows.
   A stray notch there would take the session being replied to off screen (and
   reset its scroll) while the draft went on targeting its id. The notch is DROPPED
   rather than redirected to the preview — a pointer parked over the list is not
   asking for the transcript, so scrolling a pane it is not over would just swap
   one surprise for another. The cost is accepted: the list is a silent dead zone
   mid-draft, with no feedback that the notch was eaten.

   The other two zones are untouched, and that is load-bearing. Inside the preview
   a notch scrolls the transcript being written to exactly as always, and the
   composer needs no arm of its own when docked — it is drawn INSIDE the preview
   rect (`App::open_compose` brings a hidden pane back at 1:1, so there is always
   one).
   OUTSIDE BOTH rects the preview stays the default surface, which is what keeps
   the wheel alive over the SHORT-PANE fallback composer in the bottom bar, the
   search line and the help line: all three render outside the two body panes, so
   the strict reading — dead everywhere but the preview — would have made a notch
   over the box you are typing in inert.

   A **terminal paste** is routed by that same list, and `update::handle_paste`
   walks it in the identical order — the per-owner table is
   [DOMAIN.md](DOMAIN.md#terminal-paste-routing-eventpaste). Two rules follow for
   anyone editing this area. A new keyboard owner must be added to `handle_paste`
   as well, not only to `handle_event` (and `preview_pointer_blocked`, when the
   above says it belongs there), or pasted text lands on the surface underneath it. And `handle_paste` returns no `Outcome` on
   purpose: a paste is DATA, so it structurally cannot send, resume, or answer a
   confirmation. That is the shape of the fix for the bug where a pasted newline
   arrived as a bare `Enter` — `ComposeAction::Send` — and submitted a draft's
   first line before resuming on its second. `compose_key_to_action` is SHARED by
   both compose targets, so that hit BOTH boxes: a quick reply sent one line, and a
   `Ctrl-N` background draft launched an agent on one.
   `compose_key_to_action` also takes `list_open`: with the pick list open (in
   either draft — the router never sees the target) bare `Enter`/`Tab` pick, `Up`/`Down` choose and `Esc` closes the list;
   `Ctrl-J`, `Alt`/`Shift+Enter` and `Ctrl-O` decode as ever, and `false` decodes
   exactly as before. Each key needs its decode test AND a `handle_event` test.

   `draft` is the one arm that is not a keyboard owner: it owns the **pane**. While
   the new-session draft card is drawn the transcript is not, so the cached link
   AND fold regions describe text no longer on screen, and a click would open a
   link or toggle a fold in a session the user cannot see, while a drag would
   select the placeholder card rather than any transcript. It outlives the
   compose editor by AT MOST one in-flight launch, which is the window nothing
   else covers — so a pane owner
   earns an arm here for the same reason a keyboard owner does. "At most" is the
   operative bound: a pane owner that outlives its keyboard owner also outlives the
   gate that used to end it, so it needs its own end conditions — see
   [DOMAIN.md](DOMAIN.md#background-agent-draft-pane-ctrl-n) for the two the card
   carries.

The board's search query is a **widget** that is deliberately NOT a keyboard
owner. `App::query_input` is a `ratatui_textarea::TextArea`, so it measures,
scrolls and draws its own caret — but every key that reaches it came through the
pipeline above, and two rules keep it that way.

**SINGLE LINE is an invariant, not a setting.** `App::query` reads `lines()[0]`
and the search row is one row tall, so a second line is dropped from the filter
while the widget tries to paint it into a row that does not exist: the two halves
disagree, and NEITHER reports an error. The guard sits at the one path that can
carry a newline — a terminal paste — where `update::flatten_for_query` turns each
one into a space before `push_query_str` is called. It is not implied by the
mutators: `TextArea::insert_str` really does open a second line, so a NEW way
into the query must flatten at its own call site or move the guard deliberately.

**NEVER drive the board through `TextArea::input`.** That is the widget's own key
map, and it collides with the board on four keys at once: `Ctrl-C` is copy where
the board QUITS, `Ctrl-K` is delete-to-line-end where the board stops the agent,
`Ctrl-X` is cut where the board opens the leader chord, and `Tab` is insert-tab
where the board toggles the search mode. Forwarding raw keys would hijack all
four, and each theft reads as the key doing nothing. The board binds its keys in
`key_to_action` like any other and drives the widget with EXPLICIT method calls
(`insert_char`, `insert_str`, `delete_char`, and a `clear` + `insert_str` rebuild
for the word delete that also puts the caret back at the cut without the widget's
`u16` column jump — `App::pop_query_word`'s doc comment says why), each
routed through `App::apply_query_change` exactly ONCE so a keypress still costs
one re-filter. The caret itself moves a character on `←`/`→`
(`App::move_query_caret`, `CursorMove::Back`/`Forward`) and a word on the word
hops — `Alt-←`/`Alt-→`, `Alt-b`/`Alt-f`, `Ctrl-←`/`Ctrl-→`
(`App::move_query_caret_by_word`, the widget's own
`CursorMove::WordBack`/`WordForward`, so a hop lands where the reply box's does).
Those are the widget calls that must NOT go through the funnel: the text did not
change, and the funnel would re-arm the preview's match jump on a key that edited
nothing. Every edit acts AT the caret —
a typed character, a paste, `Backspace` and the word delete alike — so no mutator
may assume it sits at the end of the line. `tui::compose` is the opposite case and
stays that way: it IS a keyboard owner, so forwarding to `TextArea::input` is
correct there — and that is precisely why the same crate can answer
`Alt-Backspace` in the reply box without the board ever inheriting the rest of the
map.

Add a keybinding by extending the `Action` enum + `key_to_action` + `apply_action`.
Cover it with a `key_to_action` unit test AND one test that presses the key through
`handle_event`. Both, because they pin different things and neither implies the
other: the decode test stops at the `Action`, and a handler test that calls the
`App` method directly starts after it, so the `apply_action` arm BETWEEN them is
pinned by nothing. That gap is not hypothetical — the word-delete binding
(`Action::BackspaceWord`) was landed with both halves covered, and swapping its arm
from `pop_query_word()` to `pop_query_char()` left the ENTIRE suite green while the
feature silently deleted one character per press, the exact defect it existed to
fix. A key the user presses is the unit; assert the state the press produced.
Then satisfy the KEEP KEY DOCS IN SYNC rule in [AGENTS.md](../../AGENTS.md), which
owns the list of surfaces that must agree — do not re-enumerate them here.

A `Ctrl-X` FOLLOW-UP is added the parallel way, never as an `Action`: a
`ChordOutcome` variant + its `chord_key` arm (bare letter, shifted form too) + its
`handle_chord_key` completion, pinned by the `chord_key` table test AND one test
that feeds `Ctrl-X` then the letter through `handle_event`. Its verb also has to
fit `view::chord_hint`, whose widest form is budgeted against an 80-column help
row — the COLUMN BUDGET note there says how to pay for a new one.

A COMPOSE key (`Ctrl-J`, `Ctrl-O`, `Ctrl-L`) is added a third way: a
`ComposeAction` variant + its `compose_key_to_action` arm (both letter cases, for
the kitty path) + its `handle_compose_key` arm. It belongs to the compose box and
not to the board, so it is named in `view::compose_hint` (the reply hint fits 80
columns exactly, so a new segment is paid for by shrinking another; a draft's key
lands in `BG_DRAFT_HINT`, which the draft card shows too through `draft_hint`) and
NOT in the board keymap or `chord_hint`. A NEW key must be FREE on the whole path to that
router, and each claim needs evidence rather than a guess, because a key stolen
anywhere upstream reads as the key doing nothing: the pinned `ratatui-textarea`
must not bind it (`TextArea::input` at `=0.9.2` binds `Ctrl-` + `m h d k j w n p
f b a e u r y x c v`, so a bump re-checks it — `Ctrl-J` is the one deliberate
exception, taken from the widget's delete-to-line-head because it is the newline
byte a raw-mode terminal sends); the terminal must not alias it
(crossterm delivers `Ctrl-M`/`Ctrl-I`/`Ctrl-H`/`Ctrl-[` as Enter/Tab/Backspace/Esc,
and `Ctrl-S`/`Ctrl-Q`/`Ctrl-Z`/`Ctrl-C` are flow or job control); and a common
multiplexer must not take it first (Zellij's default keymap swallows `Ctrl-G` and
`Ctrl-T`). `Ctrl-L` passed all three, and `ComposeAction::PickModel` keeps the
argument next to the arm.

The deliberate exception to "free on the whole path" is the board's transcript
scroll set (`ComposeAction::PreviewTop` / `PreviewBottom` / `PreviewPageUp` /
`PreviewPageDown` / `PreviewHalfUp` / `PreviewHalfDown`: `Ctrl-T`/`Ctrl-E`/`Home`/
`End`, `PgUp`/`PgDn`, `Ctrl-U`/`Ctrl-D`), shared with an open quick REPLY because
it previews the real transcript. The router mirrors `update::key_to_action`'s
modifier rule (the `Ctrl` letters whatever else is held, the named keys only
without `Ctrl`). Several are editor keys (`Ctrl-U` delete-to-head, `Ctrl-D`
delete-char, `Ctrl-E`/`Home`/`End`/`PgUp`/`PgDn` caret moves), taken from the reply
editor on purpose; all fall through to the editor on a new-session draft, whose
pane shows a placeholder card. The handler only calls the board's `App::preview_*`
methods, so the follow-bottom rules above apply unchanged and neither selection
nor draft text/caret move. This is a conscious loss of those editing keys on a
reply, not an oversight: do not add alternative chords for them.

A binding that is only meaningful sometimes is **CONDITIONAL, and falls through**
rather than going inert. `key_to_action` takes the conditions as parameters
(`query_empty`, `has_preview_matches`) so the decision stays pure and the
fall-through is what a test can pin; the guarded arm sits ABOVE the unguarded one
and the unguarded one is reached whenever the guard fails. The shifted VERTICAL
arrows are the instance: with nothing marked to move between they are
bit-for-bit the `MoveUp`/`MoveDown` they have always been, so a user who never
searches loses nothing AND a terminal that drops the modifier degrades to a
working key. The shifted HORIZONTAL arrows are the contrast: `Shift-←`/`Shift-→`
step the `PaneLayout` UNCONDITIONALLY, taking neither parameter — but their arms
still sit above the plain `Left`/`Right` caret arms, because the unguarded arm
matches the shifted key too and the first matching arm wins
(`the_layout_arms_win_over_the_plain_caret_arms` pins the order) — and above the
`Alt` word-hop arms as well, so `Shift-Alt-←` still steps the layout
(`a_held_shift_wins_over_the_alt_word_hop_arms`). A dropped
modifier degrades them to a search-caret step, a working key if not the one
pressed. The plain arrows are unconditional too: they move the caret with or
without a query, and the lineage fold is the `Ctrl-X f` chord verb, so caret
movement and folding never share a key. Prefer
`Shift`+key over `Alt`+key for anything new here: snapback never pushes the kitty
keyboard protocol and clears it on every board (re)entry
(`tui::reset_terminal_state`), so on default macOS terminals `Alt` arrives as a
composed character that types junk into the query, and a split `ESC` read
surfaces as a bare `Esc` — which quits an empty-query board. `Shift` rides the ordinary
`CSI 1;2<final>` encoding crossterm already decodes into a `KeyModifiers::SHIFT`.

`Alt` is bindable in ONE narrow case: the binding MIRRORS a gesture the compose
editor already answers, and it ships alongside a non-`Alt` key for the same
action, so a terminal that composes Option still leaves the user a working key.
Two sets are the instances. The WORD DELETE — `Alt-Backspace` and `Alt-H` are two
of the three keys `TextArea::input` maps to `delete_word`, and `Ctrl-W` is the
third, needing no `Alt` at all. The WORD HOP — `Alt-b`/`Alt-f` and
`Ctrl-←`/`Ctrl-→` are the keys `TextArea::input` maps to
`WordBack`/`WordForward`, the `Ctrl` pair being the non-`Alt` twin, and
`Alt-←`/`Alt-→` (`CSI 1;3D`/`C`) joins them as the OTHER bytes a terminal may
send for the same `⌥←`/`⌥→` gesture (RustRover's sends `ESC b`/`ESC f`). The
reply box answers that gesture only in its `ESC b` form — the widget ignores
`CSI 1;3D` — so on the word hop the board's set is the wider one. Binding each
set whole is what makes the board answer the gesture whatever the terminal sends
for Option.

The exception does not WAIVE the hazard above, it ACCEPTS it: a split `ESC` read
on a slow or multiplexed link can surface an `ESC`-prefixed key (`Alt-Backspace`,
`Alt-b`) as a bare `Esc`, and a bare `Esc` quits an empty-query board — so the keys this
case blesses can drop the user off the board instead of deleting or hopping a
word. It is taken anyway on two grounds. `⌥⌫` and `⌥←`/`⌥→` are the gestures
users actually press, and were the originating requests for the two features;
and `Ctrl-W` and `Ctrl-←`/`Ctrl-→` are the non-`Alt` siblings, which no Option
setting can break, so the worst case is a key that is unreliable rather than an
action that is unreachable. That sibling is the exception's precondition, not a
nicety — an `Alt` binding with no non-`Alt` twin would be paying this hazard for
a gesture the user has no other way to make, and is still forbidden.

The ordering constraints that come with it are all pinned by tests in
`update.rs`: an `alt`-guarded arm must sit ABOVE the unguarded arm for the same
`KeyCode` (a guarded `Backspace` placed below the plain one never fires, and the
miss is invisible — it just deletes one character; an `Alt-←` below the plain
`Left` steps one character instead of a word), and the
`KeyCode::Char(_) if alt => Ignore` catch-all must sit BELOW every bound `Alt`
printable while still existing, since it is what stops an unbound `Alt-J` from
typing `j` into the query. A `Ctrl` binding of a NON-letter key — `Ctrl-←`/`Ctrl-→`
— still belongs INSIDE `key_to_action`'s `ctrl` early-return block, exactly as
`Ctrl-W` does: written in the lower match it is never reached.

## 11. Status-line ownership

`App::status` carries only **outcomes and refusals** — facts true at a single
point in time: a send result, a resume refusal, a paste-too-long warning, an
empty-buffer nudge. A fact that is true over an **interval** lives in typed
state and renders on the surface that owns it:

- the quick reply's in-flight echo lives in `App::sending` (one entry per
  session) and renders **inline** in that session's preview pane
  (`view::sending_tail`), not on the help line;
- a background-agent launch lives in `App::draft.launch_id` and renders on the
  draft card (`view::draft_card`), not on the help line;
- an interrupt in flight lives in `App::interrupting` and deliberately has **no**
  visible label — `claude stop` is fast and the badge clears on the next agents
  poll — but the guard still prevents a stale completion from landing on a
  surface that has moved on;
- what a compose will run on lives in `ComposeState::model` (its `Ctrl-L` pick)
  and, with no pick, in `App::compose_default`, and renders on that compose box's
  own bottom border (`view::compose_model_label`), not on the help line: the
  picker's confirm sets no status, and neither `AppEvent::SettingsModel` nor
  `AppEvent::ModelAliases` writes `App::status` when it lands;
- how long a reported session has been running lives in typed state and renders
  on the preview banner (`live busy · 46m`, after the pinned turn marker for a live
  agent, alone as the fallback for a marker-less transcript), not on the help
  line: the record's
  `startedAt` (`ReportedAgent::started_at_ms`) against `App::reported_at_ms`, the
  wall-clock instant the poller's map was answered. The poller stamps it
  (`AppEvent::ReportedAgents { agents, reported_at_ms }`) and the event arm only
  stores it, so the age is "as of the last poll", no clock is read in render, and
  applying a map never writes `App::status`.

`Ctrl-K`'s SIGTERM is the opposite case, an OUTCOME: the driver hands the
syscall's result to `update::show_signal_result`, which sets `SIGNAL_SENT` /
`SIGNAL_ALREADY_GONE` transient and any failure sticky, because a sent signal is a
fact about one instant. Its effect, the row's `live` badge clearing, arrives
through the agents poll like any other change.

The help line renders `App::status` and nothing else. `set_status` is sticky
(failures and refusals persist until the next actionable keypress);
`set_status_transient` expires after `STATUS_DWELL_TICKS` ticks so confirmations
and nudges do not squat on the keymap row. This keeps each fact told exactly
once and prevents interval-scoped facts from colonizing a keypress-scoped
surface.

Some confirmations are deliberately sticky (`set_status`) all the same:

- **The `Ctrl-X y` copy's line, on BOTH routes.** `update::finish_copy` sets it
  with `set_status` for whichever route ran (`copy_status_is_sticky` answers true
  for a `CopyPayload::SessionId`, and its doc comment says why):
  `Copied session ID <uuid>` when a clipboard tool exited 0, and the OSC 52
  path's `osc52_sent_status` line (`Sent session ID <uuid> …`), which never says
  "Copied". The line reports which route ACTUALLY ran, which the user cannot see
  any other way. On the OSC 52 path it is also the copy's FALLBACK: when the
  terminal ignores the escape, the full id on the status line is what the user
  selects by hand (Shift/Option-drag past mouse capture), and a
  `STATUS_DWELL_TICKS` × `watch::TICK` = 4 s dwell is too short for that.
- **A lineage delete's tally.** `confirm_delete` sets
  `delete::status_for_delete`'s line with `set_status`, even when every member
  went (`3 deleted`). That one line can carry `skipped (running)` refusals and
  `failed to remove` errors beside the count, and those must stay. A clean
  SINGLE delete says nothing at all: the row leaving the board is the message.

Both still clear on the next actionable keypress like any sticky status. Do not
"fix" either to `set_status_transient`.

The preview **drag-selection copy** is decided the other way, on purpose, although
it goes through the very same `finish_copy` and clipboard path: its line —
`Copied selection (N lines)` from a tool, `Sent selection (N lines) …` from the
OSC 52 fallback — is TRANSIENT on both routes (`copy_status_is_sticky` answers
false for a `CopyPayload::Selection`, and a test pins it). The `Ctrl-X y` reasons
do not carry over. The id line sticks because the user may have to act on its
TEXT — select the full id by hand when the terminal drops the escape — and a
selection's line carries no text to act on: it names only a row count, since a
multi-row selection cannot fit the one help row. The fallback's fallback is a
native Shift/Option selection of the transcript itself, which is still on screen
and still highlighted in the pane. Nor does the line carry a failure or a refusal.
The route it names is read the moment the button comes up, while the user is
looking at the board, so the dwell is enough; and a sticky line would park on the
keymap row after EVERY drag until a key was pressed, which a reader working with
the mouse alone may never do.

A drag over blank cells alone gets NO line at all, and that is not a missing
nudge. It selects no drawn text, so its release copy (`view::preview_selection_copy`)
is `None`, the release requests no copy, and there is no outcome to report — the
clipboard is untouched and
nothing is highlighted, which the user can already see. Do not add a status for it.

A new confirmation earns stickiness only by the same kind of argument: the user
has to act on the text itself, or the line carries a failure or refusal beside the
success.

**A MOUSE click is scoped like a keypress, and which kind it is depends on WHAT it
resolved to.** `update::note_link_click` is the instance, and it is the ONE place a
preview-link click's four outcomes are mapped to status treatment — a single site, so
each is decided once and any of them can be re-decided without hunting. It runs on
the click's RELEASE (`update::click_effect`), never on the press: a press only
records where it landed, and a press that became a drag resolves no link at all,
so it says nothing here.

| `LinkClick` | Status | Why |
| --- | --- | --- |
| `Opening(url)` | TRANSIENT `opening <url>` | A confirmation: what the user aimed at is under way, so it dwells rather than squatting on the keymap row. |
| `RefusedScheme(url)` | STICKY `not opening <url> - …` | A refusal the reader may act on: a `[label](url)` target is scheme-checked nowhere on the way in, so a label CAN render underlined over a url the opener will not take. |
| `Unresolvable` | STICKY `cannot tell which link …` | The hit-test ABSTAINED — the clicked line is past `view`'s probe budget (`probe_within_budget`). Not the same claim as "no link": that budget bounds the line's SIZE times its candidate count, so a line with no regions has a product of zero and is always within it, and abstaining IMPLIES a rendered link there. A rendered, underlined affordance that does nothing must say so. |
| `NoLink` | nothing | A NO-OP, which must not wipe a refusal the reader has not read yet (the `WheelTarget::Ignore` arm is the same principle for the wheel). |

The first three are ACTIONABLE inputs — the user aimed at something and it either
happens or is refused — so each may clear whatever the line held, exactly as an
actionable keypress does. That asymmetry against the fourth is load-bearing beyond
tidiness: `resume::open_url` nulls all child stdio and swallows every error, so the
status line is the ONLY thing separating a resolved click from a dead opener — a
message with no browser indicts the opener, and silence means the click resolved
nothing. Let a pending sticky message SUPPRESS a resolved click's message and silence
stops meaning that, so anything but `NoLink` must always be able to speak.

Making the over-budget case silent again is deliberately a ONE-LINE change to that
table's `Unresolvable` arm. Keep the mapping a single site so it stays that way.

**What splits `Opening` from `RefusedScheme` is ONE predicate, asked once.** snapback
opens `http`/`https` only — `store::preview::has_openable_scheme`, which both
`store::preview::match_autolink` and `resume::opener_argv` read, so the renderer and
the opener cannot disagree about what a link is. `update::resolve_link_click` asks
that same predicate rather than a rule of its own, which is why the two variants can
never describe a url differently from the opener that gets it.

Across all four rows the governing rule is the same: saying nothing was never an
option for a click that landed on something. An affordance that renders, records a
region and then silently does nothing is the very indistinguishable silence this
hit-test was built to remove, and leaving it at the far end of the path — whether by
refusing the scheme or by refusing to probe — would only have moved it.

## Testing patterns

Tests are **inline** `#[cfg(test)] mod tests` at the bottom of each source file
(no separate integration crate). Conventions to match:

- **Fixture store**: `tests/fixtures/store/` holds representative JSONL — a
  normal session, a no-summary session, a malformed-line session, a worktree
  cwd, a sidecar (no `cwd`), a nested subagent, a **background-fork pair**
  (two files sharing one tree root, `cwd`, branch and label — the duplicate-row
  shape), a **root-less** session (no `parentUuid: null` record), four
  **failed-background-task pairs** under `-Users-me-project-epsilon` (`failed` vs
  `completed`; a quick reply vs an `sdk`-marked notification after the failure; a
  `turnOrigin: "sdk"` slash command vs a bare one; a `failed` notice with vs
  without its `<summary>`, reusing the first pair's failed half) — each pair one
  transcript whose halves differ only in their last record, and root-less so the
  seven never fold into a false lineage — and four
  record-level `origin` shapes, one session file each so a guard is asserted in
  isolation: a PEER hand-back (stem-shaped `from`, the `The report follows:`
  preamble, a uniformly indented report), a peer whose `from` is NOT a stem and
  whose body has neither the preamble nor the indent (one record, three fail-soft
  paths), the two BARE kinds (`human`, `task-notification` — no `from`, no
  `body`), and a record with NO `origin` whose content quotes the
  `<agent-message …>` frame inside a fenced block, which pins that the text frame
  alone collapses nothing. Under `-Users-me-project-zeta`, one COMMAND-STARTED
  session in the real record order: an `isMeta` caveat, a LOCAL `/model` with its
  `<local-command-stdout>`, then a PROMPT command whose `isMeta` skill body
  follows it with `parentUuid` pointing back — it pins the `/name args` label,
  the content index dropping injected text and command tags, and the preview's
  injected node. Reach it
  via `env!("CARGO_MANIFEST_DIR")`. Add a fixture when you add a format edge
  case, and update the counts in `store::mod`'s discovery/session-count tests.
  A fixture pair must **differ in the field under test**, and the fork pair
  differs twice on purpose: its leading user records differ (a pair that agrees
  everywhere passes against the wrong lineage key too and cannot distinguish it),
  and its members carry different turn counts (2 vs 4, behind three copied
  `attachment` records each) so the child row's count column has something to
  tell apart — a pair that agreed there could not test the one field that exists
  to separate the stub from the member holding the work.
- **Per-reader fixtures** sit beside the store, one directory per reader, each
  read by that module's own tests and never discovered as a store:
  `tests/fixtures/preview/` (single session files for the answering-model and
  effort readings), `tests/fixtures/skill_listing/` (`listing.jsonl`: real
  record shapes with shortened descriptions — the `agent_listing_delta` records
  `store::skills` reads, beside a `skill_listing` record and a non-JSON line it
  must skip) and `tests/fixtures/claude_catalog/` (two trimmed claude 2.1.284
  `initialize` replies, every body key but `commands` and `agents` removed —
  `account` among them — whose `request_id` the test restamps:
  `initialize_response.json`, and `initialize_hidden_builtins.json`, which keeps
  the five hidden built-ins the reply listed beside `compact`, two project skills
  and one agent). A captured `claude` reply is committed only after that trim.
- **Synthetic models**: build `Session`/`ReportedAgent` values directly in tests
  (see the `session(...)` helpers) rather than round-tripping through disk.
- **Isolated temp dirs**: watcher/app tests create a unique
  `snapback-<tag>-<pid>-<nanos>` dir under `std::env::temp_dir()` and never
  touch the real `~/.claude/projects`. Clean up with `remove_dir_all`. The trust
  reader's tests do the same with claude's record: `claude_trust::folder_trust_in`
  takes the record's path, so each test writes its own under a canonicalised temp
  dir (on macOS `/var` is `/private/var`) and never reads the real
  `~/.claude.json`.
- **Test the pure helper, not the impure driver**: exit handling is tested via
  `status_for_exit`, teardown via the `Write`-generic `disable_mouse`, argv via
  `build_argv` — no real `claude` process is ever spawned, and no real `git`
  either (the worktree set is stated through `App::set_worktree_probe`, and the
  porcelain parser is fed captured sample text; see
  [§6](#6-off-ui-thread-for-anything-that-can-block) for the probe seam). No
  real clipboard tool runs either: `clipboard::spawn_tool_copy` and
  `tui::start_copy` take the tool list as a parameter, so a test hands in a
  harmless stand-in (`sh`, `cat`, `false`), and the OSC 52 fallback reaches a
  `Vec<u8>` through the `Write`-generic `update::finish_copy`, never the test
  run's terminal. No browser opens either: a link click is decided by the pure
  `update::mouse_effect`, which RETURNS the url a release would open
  (`MouseEffect::OpenLink`) instead of handing it to `resume::open_url` — only
  the thin `handle_mouse` does that — so a test presses and releases over a real
  drawn link and asserts the url.
- **Stand-in children need `perl` once**: the catalog fetch's child tests
  (`claude_catalog`'s unix-only `children` module) run `sh`, `true` and `sleep`
  in `claude`'s place. One of them,
  `a_timed_out_leader_that_left_its_group_is_still_killed`, needs `perl` on
  `PATH` (macOS and the `ubuntu-latest` CI runner ship it), because `sh` cannot
  `setpgid` itself. Without `perl` it FAILS on its marker assertion, never passes
  vacuously.
- **Assert structure, not styling**: preview tests flatten `Text` to plain
  strings to check markers, and separately assert `Style`/`Modifier` on specific
  spans.

Every new pure function gets a unit test in the same file.

## Watch every test fail before you trust it

A test that has never been observed red is an unverified claim. Before reporting
work green: temporarily break what the test pins, confirm it FAILS, restore. If
it still passes, it was never testing what you thought. NEVER break a guard in
front of a real side effect (a syscall, a spawn, a file delete) while any test
can still reach that effect — the break turns that test into the very accident
the guard exists to prevent.

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
