import os
from xonsh.main import XSH

def __is_prompt_start() -> str:
    return "\001" + "\x1b]6973;PS\x07"


def __is_prompt_end() -> str:
    return "\001" + "\x1b]6973;PE\x07" + "\002"

def __is_escape_value(value: str) -> str:
    # Escape per character, not per UTF-8 byte: decoding an individual
    # continuation byte raises UnicodeDecodeError, so any non-ASCII cwd used
    # to break the prompt outright. Non-ASCII characters are emitted as-is and
    # reach the reader as raw UTF-8 bytes, matching shellIntegration.bash.
    escapes = {
        "\\": "\\\\",
        ";": "\\x3b",
        "\n": "\\x0a",
        "\x1b": "\\x1b",
        "\x07": "\\x07",
    }
    return "".join([escapes.get(ch, ch) for ch in value])

def __is_update_cwd() -> str:
    return f"\x1b]6973;CWD;{__is_escape_value(os.getcwd())}\x07" + "\002"

__is_original_prompt = $PROMPT

$PROMPT_FIELDS['__is_prompt_start'] = __is_prompt_start
$PROMPT_FIELDS['__is_prompt_end'] = __is_prompt_end
$PROMPT_FIELDS['__is_update_cwd'] = __is_update_cwd
if 'ISTERM_TESTING' in ${...}:
    $PROMPT = "> "

$PROMPT = "{__is_prompt_start}{__is_update_cwd}" + $PROMPT + "{__is_prompt_end}"