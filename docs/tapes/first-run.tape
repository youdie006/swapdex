# First run: swapdex finds the logins you already have and saves them.
#
# Recorded against the throwaway store seed-demo.sh builds in --fresh mode
# (live logins, nothing saved yet), so no real account reaches the recording.
# Regenerate with:
#   vhs docs/tapes/first-run.tape
# from the repository root, with swapdex on PATH.

Output docs/ui-demo.gif

Set Shell bash
Set FontSize 20
Set Width 1180
Set Height 620
Set Padding 26
Set Theme { "background": "#1b1a20", "foreground": "#e9e8ee", "cursor": "#9d6bff", "black": "#26242c", "brightBlack": "#8b8a95", "white": "#e9e8ee", "brightWhite": "#ffffff", "blue": "#9d6bff", "brightBlue": "#b78dff", "cyan": "#7fd6c4", "brightCyan": "#7fd6c4", "green": "#3ab85f", "brightGreen": "#5bd47f", "yellow": "#e0b23a", "brightYellow": "#e0b23a", "red": "#e05a52", "brightRed": "#e05a52", "magenta": "#9d6bff", "brightMagenta": "#b78dff" }

Hide
Type "export SWAPDEX_ROOT=/tmp/swapdex-first PS1='$ '"
Enter
Type "sh docs/tapes/seed-demo.sh $SWAPDEX_ROOT --fresh >/dev/null && clear"
Enter
Show

Sleep 700ms
Type "swapdex ui"
Sleep 400ms
Enter
Sleep 2.5s

Type "s"
Sleep 1.5s
Type "main"
Sleep 900ms
Enter
Sleep 2.5s

Type "q"
Sleep 1.5s
