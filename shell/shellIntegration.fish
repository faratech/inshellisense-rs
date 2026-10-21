# The parent selects the suggestion provider for this child only. Apply before
# the duplicate-source guard so reloading still enforces the selected policy.
if test "$INSH_RS_DISABLE_FISH_AUTOSUGGESTIONS" = 1
	set -g fish_autosuggestion_enabled 0
end

# A profile can source this file before the wrapped spawn sources it again.
# Compare the actual wrapper definition: a theme replacing fish_prompt should
# become the new original on the next load, while our own wrapper must never
# be copied into is_user_prompt (which would recurse).
set -l __is_current_prompt (string join \x1f -- (functions fish_prompt))
if set -q __is_fish_prompt_wrapper; and test "$__is_current_prompt" = "$__is_fish_prompt_wrapper"
	return 0
end

functions --erase is_user_prompt
if not functions --copy fish_prompt is_user_prompt
	function is_user_prompt; printf '> '; end
end

function __is_prompt_start; printf '\e]6973;PS\a'; end
function __is_prompt_end; printf '\e]6973;PE\a'; end

function __is_escape_value
	echo $argv \
	| string replace -a '\\' '\\\\' \
	| string replace -a ';' '\\x3b' \
	| string replace -a \e '\\x1b' \
	| string replace -a \a '\\x07' \
	| string split \n | string join '\x0a' \
	;
end
function __is_update_cwd --on-event fish_prompt; set __is_cwd (__is_escape_value "$PWD"); printf "\e]6973;CWD;%s\a" $__is_cwd; end

if [ "$ISTERM_TESTING" = "1" ]
	function is_user_prompt; printf '> '; end
end

function fish_prompt;
	# Evaluate the user's prompt first so it sees the original $status and
	# $pipestatus, before marker printf calls change them.
	set --local __user_prompt (is_user_prompt | string collect --allow-empty --no-trim-newlines)
	__is_prompt_start
	printf '%s' "$__user_prompt"
	__is_prompt_end
end

set -g __is_fish_prompt_wrapper (string join \x1f -- (functions fish_prompt))
