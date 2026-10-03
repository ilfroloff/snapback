# snapback demo — records website/public/demo.gif from SYNTHETIC data.
#
# Run ONLY through website/demo/record.sh: it points HOME at a throwaway copy of
# website/demo/home, shadows `claude` with website/demo/bin/claude and puts the
# freshly built release binary on PATH. Run bare, this tape would record your
# real sessions.
#
# NEVER press Enter on the BOARD itself: it resumes or attaches the selected
# session. Enter is fine inside the stop-confirm prompt and the compose box.

Output "../public/demo.gif"
Output "out/demo.mp4"

Require snapback

Set Shell "bash"
Set FontSize 18
Set Width 1280
Set Height 640
Set Padding 24
Set TypingSpeed 45ms
Set Theme "Catppuccin Mocha"

# --- Scene 1: every repo, its worktrees grouped, four badged agents ---
Type "snapback -a"
Enter
Sleep 3.5s

# --- Scene 2: the agent that needs you ---
Down 1
Sleep 2.5s

# --- Scene 3: reply without leaving the board (Ctrl-R) ---
Ctrl+R
Sleep 2s
Enter
Sleep 1s
Type "Make the stub synchronous, then run the test 50 times."
Sleep 1s
Enter
Sleep 7s

# --- Scene 4: find a session by what was said (Tab = search transcripts) ---
Tab
Sleep 500ms
Type "backoff"
Sleep 3.5s
Backspace 7
Tab
Sleep 800ms

# --- Scene 5: background hand-offs folded into one (+N) row ---
Down 6
Sleep 1s
Ctrl+X
Sleep 300ms
Type "f"
Sleep 2.5s
Ctrl+X
Sleep 300ms
Type "f"
Sleep 1.5s
