# The switch, end to end: see the accounts, move to one, confirm, undo.
#
# Recorded against the throwaway store seed-demo.sh builds, so no real login
# ever reaches the recording. Regenerate with:
#   vhs docs/tapes/switch.tape
# from the repository root, with swapdex on PATH.

Output docs/demo.gif

Set Shell bash
Set FontSize 21
Set Width 1180
Set Height 520
Set Padding 26
Set Theme { "background": "#1b1a20", "foreground": "#e9e8ee", "cursor": "#9d6bff", "black": "#26242c", "brightBlack": "#8b8a95", "white": "#e9e8ee", "brightWhite": "#ffffff", "blue": "#9d6bff", "brightBlue": "#b78dff", "cyan": "#7fd6c4", "brightCyan": "#7fd6c4", "green": "#3ab85f", "brightGreen": "#5bd47f", "yellow": "#e0b23a", "brightYellow": "#e0b23a", "red": "#e05a52", "brightRed": "#e05a52", "magenta": "#9d6bff", "brightMagenta": "#b78dff" }

Hide
Type "export SWAPDEX_ROOT=/tmp/swapdex-demo PS1='$ '"
Enter
Type "sh docs/tapes/seed-demo.sh $SWAPDEX_ROOT >/dev/null && clear"
Enter
Show

Sleep 700ms
Type "swapdex ls"
Sleep 400ms
Enter
Sleep 2.2s

Type "swapdex use personal"
Sleep 400ms
Enter
Sleep 2.4s

Type "swapdex status"
Sleep 400ms
Enter
Sleep 2.4s

Type "swapdex restore"
Sleep 400ms
Enter
Sleep 2.6s

Sleep 1.2s
