# quince: zsh integration.
#
# Tells the PTY wrapper what is on the command line, and applies the suggestions it sends back.
# Each report is a private escape sequence written to the terminal, which the wrapper reads and
# removes before the real terminal sees it. Both directions are documented in src/protocol.rs.

[[ -o interactive && -n ${QUINCE_SESSION-} && -z ${_quince_loaded-} ]] || return 0
typeset -g _quince_loaded=1 _quince_environment=

# Every report carries the session's token, which is how the wrapper knows it from text that
# merely appears on the screen.
_quince_send() {
  builtin print -rn -- $'\e]6973;'"$QUINCE_SESSION;$1"$'\a' >/dev/tty
}

# Sets REPLY to $1 with the characters that would break the message, or that the terminal
# driver would rewrite on the way out, escaped.
_quince_escape() {
  REPLY=${1//\\/\\\\}
  REPLY=${REPLY//$'\n'/\\n}
  REPLY=${REPLY//$'\r'/\\r}
  REPLY=${REPLY//$'\t'/\\t}
  REPLY=${REPLY//$'\e'/\\e}
  REPLY=${REPLY//$'\a'/\\a}
  REPLY=${REPLY//$'\0'/\\0}
}

# Runs before every redraw, so the wrapper hears about each change to the line. It is sent
# even when nothing changed: the wrapper relies on one report following every key it forwards.
_quince_report_buffer() {
  local REPLY
  _quince_escape "$BUFFER"
  _quince_send "B;$CURSOR;$KEYMAP;$REPLY"
}

_quince_line_finish() {
  _quince_send F
}

# Applies a suggestion. The wrapper sends the key this is bound to, then the payload's length
# as eight digits, then the payload: hex for four NUL-separated fields. The edit is made only
# if the line is still the one the suggestion was computed for.
_quince_accept() {
  emulate -L zsh
  setopt extended_glob
  local header payload decoded
  local -a field
  read -k 8 header || return 0
  [[ $header == <-> ]] || return 0
  read -k $(( 10#$header )) payload || return 0
  decoded=${(g::)${payload//(#b)(??)/\\x$match[1]}}
  field=("${(@ps:\0:)decoded}")
  if [[ $CURSOR == "$field[1]" && $BUFFER == "$field[3]" ]]; then
    LBUFFER=$field[4]
    (( CURSOR -= field[2] ))
  fi
}
# The widget's name does not start with an underscore, unlike everything else here. Plugins
# that follow the line as it changes, zsh-autosuggestions among them, wrap the widgets that
# edit it and take a leading underscore to mean one that does not.
zle -N quince-accept _quince_accept

# Reports the exported variables, so the commands that compute suggestions run with what this
# shell has set up rather than what the wrapper happened to start with. Sent when they change.
# The directory is left out: it changes with every cd and is reported on its own.
_quince_report_environment() {
  emulate -L zsh
  local name packed= REPLY
  for name in ${(ok)parameters[(R)*export*]}; do
    [[ $name == (PWD|OLDPWD|_) ]] || packed+="$name=${(P)name}"$'\0'
  done
  [[ $packed == "$_quince_environment" ]] && return
  typeset -g _quince_environment=$packed
  _quince_escape "$packed"
  _quince_send "E;$REPLY"
}

_quince_precmd() {
  # Startup files and plugins can rebuild keymaps after this file is read, so the binding is
  # made again before every prompt. Were it missing, the payload would be typed as text.
  local keymap
  for keymap in emacs viins vicmd; do
    bindkey -M $keymap '\e[6973~' quince-accept
  done
  _quince_report_environment
  local REPLY
  _quince_escape "$PWD"
  _quince_send "P;$REPLY"
}

_quince_preexec() {
  _quince_send X
}

autoload -Uz add-zsh-hook add-zle-hook-widget
add-zsh-hook precmd _quince_precmd
add-zsh-hook preexec _quince_preexec
# line-init covers the first draw of a new line, which the redraw hook does not see.
add-zle-hook-widget line-init _quince_report_buffer
add-zle-hook-widget line-pre-redraw _quince_report_buffer
add-zle-hook-widget line-finish _quince_line_finish
