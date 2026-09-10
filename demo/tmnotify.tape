Output demo/tmnotify.gif

Require bash
Require cargo
Require tmux

Set Shell "bash"
Set FontSize 18
Set Width 1200
Set Height 720
Set Padding 16
Set Theme "Catppuccin Mocha"
Set TypingSpeed 35ms
Set Framerate 30

Hide
Type "CARGO_BUILD_BUILD_DIR=target/build CARGO_TARGET_DIR=target cargo build --quiet"
Enter
Type "demo/run-isolated.sh"
Enter
Sleep 2s
Show

Type "tmnotify send --key build --timeout never --title Build 'Compiling tmnotify...'"
Enter
Sleep 2s

Type "tmnotify update --key build --level success --timeout 4s 'Build finished'"
Enter
Sleep 2s

Type "tmnotify send --attention --title 'Needs attention' 'Press Enter to return to the Source Pane'"
Enter
Sleep 2s
Enter
Sleep 1s

Type "tmnotify history --plain"
Enter
Sleep 3s

Type "tmnotify config show"
Enter
Sleep 3s

Type "tmnotify hook status"
Enter
Sleep 3s

Ctrl+B
Type "d"
Sleep 1s

Hide
Type "exit"
Enter
