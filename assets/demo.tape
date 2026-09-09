# fog demo: human + agent on the same branch, then branch switch
Output assets/demo.gif
Set Shell zsh
Set FontSize 18
Set Width 1280
Set Height 720
Set Framerate 30
Set TypingSpeed 60ms

Hide
Type 'export PATH=/Users/naputt/code/fog/target/release:$PATH'
Enter
Type 'export TMPDIR=/tmp/fog-demo-tmp'
Enter
Type 'cd /tmp/fog-demo && clear'
Enter
Sleep 1s
Show

Type@15ms 'echo "> 1. your coding agent starts the stack, headless"'
Enter
Sleep 1s
Type "fog dev -d"
Enter
Sleep 8s

Type@15ms 'echo "> 2. db + api + web, all healthy"'
Enter
Sleep 1s
Type "fog ls"
Enter
Sleep 4s

Type@15ms 'echo "> 3. you open the same stack in the TUI; the db is borrowed, not duplicated"'
Enter
Sleep 1s
Type "fog dev"
Enter
Sleep 6s
Type "j"
Sleep 4s
Type "s"
Sleep 2s
Type "f"
Sleep 1s
Type "feat"
Sleep 1.5s
Enter
Sleep 5s
Type "k"
Sleep 3s
Type "q"
Sleep 2s

Type@15ms 'echo "> 4. a second agent joins the same branch; still one shared db"'
Enter
Sleep 1s
Type "fog dev -d --branch master"
Enter
Sleep 8s
Type "fog ls"
Enter
Sleep 6s
