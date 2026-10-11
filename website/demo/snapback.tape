# snapback demo — records website/public/demo.gif from SYNTHETIC data.
#
# Run ONLY through website/demo/record.sh: it points HOME at a throwaway copy of
# website/demo/home, builds the throwaway web-app repo and its worktrees, shadows
# `claude` with website/demo/bin/claude and puts the freshly built release binary
# on PATH. Run bare, this tape would record your real sessions.
#
# NEVER press Enter on the BOARD itself: it resumes or attaches the selected
# session. Enter is fine inside the stop-confirm prompt, a compose box, its pick
# list and the move picker. Never press Ctrl-O either: it runs claude
# interactively.

Output "../public/demo.gif"
Output "out/demo.mp4"

Require snapback

Set Shell "bash"
Set FontSize 18
Set Width 1280
Set Height 640
Set Padding 24
Set TypingSpeed 45ms
# vhs 0.12.1's default 50 fps capture fell behind here and played the tape back
# ~25% fast (measured 2026-10-09); 25 fps keeps the sleeps in real time. A steady
# cursor keeps idle frames identical, which keeps the GIF small.
Set Framerate 25
Set CursorBlink false
Set Theme "Catppuccin Mocha"

# Launch from web-app's main checkout: Ctrl-X w moves within the launch repo.
Hide
Type "cd ~/code/web-app && clear"
Enter
Sleep 300ms
Show

# --- Scene 1: every repo, its worktrees grouped, four badged agents ---
Type "snapback -a"
Enter
Sleep 3s

# --- Scene 2: the agent that needs you ---
Down 1
Sleep 2s

# --- Scene 3: reply without leaving the board (Ctrl-R) ---
Ctrl+R
Sleep 1.5s
Enter
Sleep 700ms
Type "Make the stub synchronous, then run the test 50 times."
Sleep 600ms
Enter
Sleep 5s

# --- Scene 4: find a session by what was said (Tab = search transcripts) ---
Tab
Sleep 400ms
Type "backoff"
Sleep 2.5s
Backspace 7
Tab
Sleep 600ms

# --- Scene 5: background hand-offs folded into one (+N) row ---
Down 6
Sleep 800ms
Ctrl+X
Sleep 300ms
Type "f"
Sleep 2s
Ctrl+X
Sleep 300ms
Type "f"
Sleep 1.2s

# --- Scene 6: move a main-checkout session into its worktree (Ctrl-X w) ---
Down 1
Sleep 1s
Ctrl+X
Sleep 700ms
Type "w"
Sleep 1.5s
Down 2
Sleep 800ms
Enter
Sleep 3.5s

# --- Scene 7: fork headless (Ctrl-F), a command from the / pick list ---
Down 3
Sleep 800ms
Ctrl+F
Sleep 1.2s
Type "/"
Sleep 2s
Type "sec"
Sleep 1s
Enter
Sleep 400ms
Type "the retry loop"
Sleep 800ms
Enter
Sleep 5s
