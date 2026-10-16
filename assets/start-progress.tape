# fog startup progress: docker-style per-service start checklist on `fog dev -d`.
# Regenerate with: scripts/record-start-progress.sh
Output assets/start-progress.gif
Set Shell zsh
Set FontSize 16
Set Width 1200
Set Height 700
Set Framerate 30
Set TypingSpeed 40ms

Hide
Type@1ms 'export PATH="$PWD/target/release:$PATH"'
Enter
Type@1ms 'export TMPDIR=/tmp/fog-start-demo-tmp'
Enter
Type@1ms 'cd /tmp/fog-start-demo && clear'
Enter
Sleep 500ms
Show

Type "fog dev -d"
Enter
Sleep 7s

Type "fog ls"
Enter
Sleep 3s

Type "fog kill"
Enter
Sleep 6s
