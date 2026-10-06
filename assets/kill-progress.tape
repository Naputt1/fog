# fog kill progress: docker-style per-service shutdown checklist.
# Regenerate with: scripts/record-kill-progress.sh
Output assets/kill-progress.gif
Set Shell zsh
Set FontSize 16
Set Width 1200
Set Height 700
Set Framerate 30
Set TypingSpeed 40ms

Hide
Type@1ms 'export PATH="$PWD/target/release:$PATH"'
Enter
Type@1ms 'export TMPDIR=/tmp/fog-kill-demo-tmp'
Enter
Type@1ms 'cd /tmp/fog-kill-demo && clear'
Enter
Sleep 500ms
Show

Type "fog dev -d"
Enter
Sleep 4s

Type "fog ls"
Enter
Sleep 2.5s

Type "fog kill"
Enter
Sleep 9s
