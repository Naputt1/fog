# fog TUI still: db + api + web healthy on the demo stack
# Regenerate assets/tui.png with:
#   vhs assets/tui.tape && ffmpeg -y -ss 11 -i assets/tui.gif -frames:v 1 -update 1 assets/tui.png && rm assets/tui.gif
Output assets/tui.gif
Set Shell zsh
Set FontSize 18
Set Width 1280
Set Height 720
Set Framerate 30
Set TypingSpeed 40ms

Hide
Type@1ms 'export PATH="$HOME/.cargo/bin:$PATH"'
Enter
Type@1ms 'cd temp/demo-app && clear'
Enter
Sleep 500ms
Show

Type "fog dev"
Enter
Sleep 6s
Type "j"
Sleep 1s
Type "j"
Sleep 5s
