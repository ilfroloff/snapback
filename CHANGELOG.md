# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.8.1](https://github.com/ilfroloff/snapback/compare/v0.8.0...v0.8.1) - 2026-08-29

### Fixed

- stop the preview from cutting a long transcript short

### Other

- *(tui)* widen the preview scroll offset past a 16-bit ceiling
- cut the per-keystroke cost of marking search hits in the preview

## [0.8.0](https://github.com/ilfroloff/snapback/compare/v0.7.0...v0.8.0) - 2026-08-19

### Added

- mark and scroll to search hits inside the previewed transcript

### Fixed

- stop a two-word content query from matching half the board
- make the row-label highlight agree with the search filter
- *(store/parse)* let content search reach a session's recent work

## [0.7.0](https://github.com/ilfroloff/snapback/compare/v0.6.1...v0.7.0) - 2026-08-11

### Added

- reload only the transcripts that changed, with a forced rescan key
- add a project scope spanning the launch repo's worktrees

### Fixed

- bound the background threads and restore non-UTF-8 discovery
- stop the watcher and the agents poll from burning idle CPU
- count the project's conversations, not every session file

### Other

- correct the agent guide against the code it describes

## [0.6.1](https://github.com/ilfroloff/snapback/compare/v0.6.0...v0.6.1) - 2026-08-03

### Fixed

- stop release-plz scoring internal renames as downstream breaks
- unify status-line ownership and bound transient confirmations
- gate a hard delete on a writer, not on claude knowing the session

### Other

- adopt a shared wtp worktree layout
- *(cargo)* allow uninstalling the locally installed binaries

## [0.6.0](https://github.com/ilfroloff/snapback/compare/v0.5.0...v0.6.0) - 2026-07-28

### Added

- reply to, stop, and start agents without leaving the board

## [0.5.0](https://github.com/ilfroloff/snapback/compare/v0.4.0...v0.5.0) - 2026-07-24

### Added

- mark an interrupted background agent with a steady badge

### Other

- publish to npm via trusted publishing (OIDC), dropping the token

## [0.4.0](https://github.com/ilfroloff/snapback/compare/v0.3.0...v0.4.0) - 2026-07-23

### Added

- hide and delete sessions from the board
- install snapback with npx or bunx, without a Rust toolchain

## [0.3.0](https://github.com/ilfroloff/snapback/compare/v0.2.1...v0.3.0) - 2026-07-21

### Added

- make "needs input" read clearly on the board list row
- show the agent that produced each turn in the preview

### Other

- drop the deprecated agent-guide changelog section

## [0.2.1](https://github.com/ilfroloff/snapback/compare/v0.2.0...v0.2.1) - 2026-07-20

### Other

- answer per-keystroke search with membership, not nucleo ranking

## [0.2.0](https://github.com/ilfroloff/snapback/compare/v0.1.0...v0.2.0) - 2026-07-16

### Added

- fold background-fork lineages behind one expandable row
- make a live session's state legible at a glance

### Other

- *(agents)* make the shell-out's exit-status decision testable

## [0.1.0](https://github.com/ilfroloff/snapback/releases/tag/v0.1.0) - 2026-07-16

### Added

- *(tui/view)* distinguish dev builds in the header version label
- pick an agent when starting a new session
- add snapback TUI for browsing and resuming Claude Code sessions

### Fixed

- *(tui)* restore the terminal to a known-good state on child return

### Other

- require an explicit release token and narrow the built-in one
- run only on pull requests for any branch
- restructure the README to lead with the user problem
- automate versioning and releases with release-plz
- gate formatting, lint, and tests on a pinned toolchain
- add cargo aliases for local run, install, and release
- Initial commit
