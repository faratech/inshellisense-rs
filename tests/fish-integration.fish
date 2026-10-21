# Invoked by the CLI regression test with the integration script path.
set -l integration $argv[1]
set -e ISTERM_TESTING
set -e INSH_RS_DISABLE_FISH_AUTOSUGGESTIONS

function expect --argument-names actual expected context
    if test "$actual" != "$expected"
        printf '%s: expected %s, got %s\n' "$context" (string escape -- "$expected") (string escape -- "$actual") >&2
        exit 1
    end
end

set -l ps (printf '\e]6973;PS\a')
set -l pe (printf '\e]6973;PE\a')
function fish_prompt; printf 'original> '; end
source "$integration"
set -l original (functions is_user_prompt | string collect)
source "$integration"
expect (functions is_user_prompt | string collect) "$original" 'second load preserves original function'
expect (fish_prompt | string collect) "$ps"'original> '"$pe" 'duplicate load has one marker pair'

# Redefining a theme's prompt must be adopted even after an earlier wrapper.
function fish_prompt; printf 'replacement> '; end
source "$integration"
source "$integration"
expect (fish_prompt | string collect) "$ps"'replacement> '"$pe" 'replacement prompt is wrapped once'
expect (functions --handlers | string match -r '^fish_prompt[[:space:]]+__is_update_cwd$' | count) 1 'one cwd event handler remains registered'
set -l cwd_events (__is_update_cwd | string collect)
set -l cwd_marker (printf '\e]6973;CWD;')
expect (string split -- "$cwd_marker" "$cwd_events" | count) 2 'cwd marker remains functional'

function fish_prompt; printf 'first line\nsecond> '; end
source "$integration"
set -l multiline (printf '%sfirst line\nsecond> %s' "$ps" "$pe" | string collect)
expect (fish_prompt | string collect) "$multiline" 'multiline original prompt is preserved'

function fish_prompt; printf '%s\n\n' '100% %s %n> '; end
source "$integration"
set -l exact_prompt (printf '%s100%% %%s %%n> \n\n%s' "$ps" "$pe" | string collect)
expect (fish_prompt | string collect) "$exact_prompt" 'format characters and trailing newlines are preserved'

function fish_prompt; end
source "$integration"
expect (fish_prompt | string collect) "$ps$pe" 'empty original prompt is preserved'

# Marker output must not erase the status information seen by user themes.
function fish_prompt
    printf 'status=%s pipeline=%s> ' "$status" (string join , -- $pipestatus)
end
source "$integration"
false | true
set -l status_prompt (fish_prompt | string collect)
expect "$status_prompt" "$ps"'status=0 pipeline=1,0> '"$pe" 'original pipeline status reaches user prompt'
false
set status_prompt (fish_prompt | string collect)
expect "$status_prompt" "$ps"'status=1 pipeline=1> '"$pe" 'original failure status reaches user prompt'

set -gx ISTERM_TESTING 1
function fish_prompt; printf 'must not appear'; end
source "$integration"
source "$integration"
expect (fish_prompt | string collect) "$ps"'> '"$pe" 'testing prompt stays deterministic on repeated loads'

# Provider policy is enforced before the duplicate-source guard. False and
# unset preserve a user's preference; they do not force autosuggestions on.
set -gx INSH_RS_DISABLE_FISH_AUTOSUGGESTIONS 0
set -g fish_autosuggestion_enabled 1
source "$integration"
expect "$fish_autosuggestion_enabled" 1 'Popup preserves native suggestions'
set -gx INSH_RS_DISABLE_FISH_AUTOSUGGESTIONS 1
source "$integration"
expect "$fish_autosuggestion_enabled" 0 'Ghost disables the conflicting native provider'
set -g fish_autosuggestion_enabled 1
source "$integration"
expect "$fish_autosuggestion_enabled" 0 'duplicate source still enforces provider policy'
set -gx INSH_RS_DISABLE_FISH_AUTOSUGGESTIONS 0
source "$integration"
expect "$fish_autosuggestion_enabled" 0 'Popup preserves an existing disabled preference'
set -e INSH_RS_DISABLE_FISH_AUTOSUGGESTIONS
set -g fish_autosuggestion_enabled 1
source "$integration"
expect "$fish_autosuggestion_enabled" 1 'unwrapped parent preference is preserved'

# Copying an original function uses a Fish builtin, even with no sed on PATH.
set -e ISTERM_TESTING
set -g PATH
function fish_prompt; printf 'builtin copy> '; end
source "$integration"
source "$integration"
expect (fish_prompt | string collect) "$ps"'builtin copy> '"$pe" 'prompt copying requires no subprocess'
printf 'Fish integration regressions passed\n'
