# fog demo: a human and two agents on one branch, sharing a borrowed DB
Output assets/demo.gif
Set Shell zsh
Set FontSize 18
Set Width 1280
Set Height 720
Set Framerate 30
Set TypingSpeed 60ms

Hide
Type@1ms 'export PATH="$PWD/target/release:$PATH"'
Enter
Type@1ms 'export TMPDIR=/tmp/fog-demo-tmp'
Enter
Type@1ms 'cd /tmp/fog-demo && clear'
Enter
Sleep 500ms
Show

Type@5ms 'echo "> 1. your coding agent starts the stack, headless"'
Enter
Sleep 500ms
Type "fog dev -d"
Enter
Sleep 3.5s

Type@5ms 'echo "> 2. db + api + web, all healthy"'
Enter
Sleep 500ms
Type "fog ls"
Enter
Sleep 2.5s

Type@5ms 'echo "> 3. you open the same stack in the TUI; the db is borrowed, not duplicated"'
Enter
Sleep 500ms
Type "fog dev"
Enter
Sleep 3s
Type "j"
Sleep 2s
Type "s"
Sleep 1.2s
Type "f"
Sleep 600ms
Type "feat"
Sleep 800ms
Enter
Sleep 3s
Type "q"
Sleep 800ms

Type@5ms 'echo "> 4. a second agent joins the same branch; still one shared db"'
Enter
Sleep 500ms
Type "fog dev -d --branch main"
Enter
Sleep 3.5s
Type "fog ls"
Enter
Sleep 3s
