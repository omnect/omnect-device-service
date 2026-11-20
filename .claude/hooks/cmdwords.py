#!/usr/bin/env python3
"""Split a shell command into simple commands and their words.

Reads the command on stdin. For each simple command it writes the number of
words, then the words, each ended by NUL. It follows the shell's quoting:
quotes are removed, and quoted text stays one word. Operators split commands
only when they are not quoted. Redirections and their targets are dropped.

Commands inside other commands are written as commands of their own, right
after the command that contains them: the contents of $( ), <( ), >( ) and
backticks (also inside double quotes and unquoted heredocs), the argument of
bash/sh -c, a heredoc fed to a shell, and the command behind a wrapper such
as timeout, env or sudo. Leading reserved words (then, do, !) are skipped.

Code that runs in a subshell, ( ) or one of the forms above, is enclosed in
two marker commands, SUBSHELL_START and SUBSHELL_END: a cd in there does not
change the directory of the commands after it.

Exits 1 when the command cannot be parsed (e.g. an open quote); the caller
then uses its own, rougher split.
"""
import sys

SUBSHELL_START = "\x01("
SUBSHELL_END = "\x01)"
MAX_NESTING = 10
SHELLS = {"bash", "sh", "zsh", "dash", "ksh"}
# Commands that run a heredoc body as code.
CODE_READERS = SHELLS | {"source", ".", "eval"}
RESERVED = {"!", "{", "}", "then", "do", "else", "elif", "if", "while", "until",
            "fi", "done", "esac"}
# Wrapper -> its options that take a value. The command follows the options
# (and for timeout the duration).
WRAPPERS = {
    "timeout": {"-s", "--signal", "-k", "--kill-after"},
    "env": {"-u", "--unset", "-C", "--chdir", "-S", "--split-string"},
    "command": set(),
    "exec": {"-a"},
    "nice": {"-n", "--adjustment"},
    "nohup": set(),
    "stdbuf": {"-i", "-o", "-e"},
    "time": set(),
    "sudo": {"-u", "--user", "-g", "--group", "-C", "-D", "--chdir", "-h", "--host",
             "-p", "--prompt", "-r", "--role", "-t", "--type", "-U", "--other-user", "-T"},
}
OPERATORS = ["&&", "||", ";;", "|&", ";", "&", "|", "(", ")", "\n"]
REDIRECTS = ["&>>", "<<<", "<<-", "&>", ">>", ">&", "<&", ">|", "<>", "<<", ">", "<"]


class ParseError(Exception):
    pass


def is_assignment(word):
    name, eq, _ = word.partition("=")
    return bool(eq) and bool(name) and name.replace("_", "a").isalnum() and not name[0].isdigit()


def unwrap(words):
    """The command behind a wrapper, with the NAME=value words before it, or
    None."""
    k = 0
    while k < len(words) and is_assignment(words[k]):
        k += 1
    if k >= len(words):
        return None
    name = words[k].rsplit("/", 1)[-1]
    if name not in WRAPPERS:
        return None
    takes_value = WRAPPERS[name]
    j = k + 1
    while j < len(words) and words[j].startswith("-") and words[j] != "-":
        j += 2 if words[j] in takes_value else 1
    if name == "timeout" and j < len(words):
        j += 1
    inner = words[:k] + words[j:]
    return inner if len(inner) > k else None


def command_name(words):
    """The program a simple command runs, after NAME=value words and
    wrappers."""
    while True:
        inner = unwrap(words)
        if inner is None:
            break
        words = inner
    for w in words:
        if not is_assignment(w):
            return w.rsplit("/", 1)[-1]
    return ""


def find_close(s, i):
    """Index of the ) that closes the ( at s[i-1], following quotes."""
    depth = 1
    j = i
    while j < len(s):
        c = s[j]
        if c == "\\":
            j += 2
            continue
        if c == "'":
            k = s.find("'", j + 1)
            if k < 0:
                raise ParseError("open quote")
            j = k + 1
            continue
        if c == '"':
            j += 1
            while j < len(s) and s[j] != '"':
                j += 2 if s[j] == "\\" else 1
            j += 1
            continue
        if c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
            if depth == 0:
                return j
        j += 1
    raise ParseError("open (")


class Lexer:
    def __init__(self, text, start=0, closing=False):
        self.s = text
        self.i = start
        self.closing = closing  # stop at the ) that closes a $( )
        self.depth = 0          # open ( inside it
        self.cases = 0          # open case ... esac: a ) ends a pattern
        self.quoted_words = set()  # indexes of words with a quoted part
        self.segments = []      # [words, operator that ended it]
        self.nested = []        # (segment index, kind, text): commands to parse
        self.words = []
        self.word = []
        self.started = False    # current word has started (also "" counts)
        self.quoted = False     # current word has a quoted part
        self.after_space = True
        self.skip_word = False  # next word is a redirection target
        self.heredoc_word = None  # next word is a heredoc delimiter: strip tabs?
        self.heredocs = []      # pending heredocs, read after the next newline

    def peek(self, k=0):
        j = self.i + k
        return self.s[j] if j < len(self.s) else ""

    def here(self):
        return len(self.segments)

    def end_word(self):
        if not self.started:
            return
        word = "".join(self.word)
        if self.heredoc_word is not None:
            self.heredocs.append((word, self.heredoc_word, self.quoted, self.here()))
            self.heredoc_word = None
        elif self.skip_word:
            self.skip_word = False
        else:
            if self.quoted:
                self.quoted_words.add(len(self.words))
            self.words.append(word)
        self.word = []
        self.started = False
        self.quoted = False

    def end_segment(self, operator=""):
        self.end_word()
        if self.words:
            self.segments.append([self.words, operator])
            first = next((k for k, w in enumerate(self.words)
                          if w not in RESERVED and not is_assignment(w)), None)
            if first is not None and self.words[first] == "case":
                if first in self.quoted_words:
                    raise ParseError("quoted case")
                self.cases += 1
            if "esac" in self.words[:1] and self.cases:
                self.cases -= 1
        self.words = []
        self.quoted_words = set()
        if operator in ("(", ")") and self.cases:
            return
        if operator in ("(", ")"):
            marker = SUBSHELL_START if operator == "(" else SUBSHELL_END
            self.segments.append([[marker], ""])

    def add(self, text):
        self.word.append(text)
        self.started = True

    def substitution(self):
        """$( ), $(( )) or backticks at self.i. The text stays in the word;
        the commands are parsed on their own."""
        s = self.s
        if s.startswith("$((", self.i):
            close = find_close(s, self.i + 3)
            if close + 1 < len(s) and s[close + 1] == ")":
                self.arithmetic(close + 2)
                return
        if self.peek() == "`":
            j = self.i + 1
            inner = []
            while j < len(s) and s[j] != "`":
                if s[j] == "\\" and j + 1 < len(s):
                    inner.append(s[j + 1])
                    j += 2
                    continue
                inner.append(s[j])
                j += 1
            if j >= len(s):
                raise ParseError("open backtick")
            self.nested.append((self.here(), "text", "".join(inner)))
            self.add(s[self.i:j + 1])
            self.i = j + 1
            return
        self.substitution_at(self.i + 1)

    def arithmetic(self, end):
        """$(( )) or (( )) up to end: one word, but a $( ) or backtick in it
        still runs."""
        text = self.s[self.i:end]
        inner = text[3:-2] if text.startswith("$((") else text[2:-2]
        for _, kind, body in Lexer(inner).scan_substitutions():
            self.nested.append((self.here(), kind, body))
        self.add(text)
        self.i = end

    def substitution_at(self, paren):
        """$( ), <( ) or >( ) with its ( at paren."""
        # The tokenizer finds the end, so heredocs and quotes inside count.
        inner = Lexer(self.s, paren + 1, closing=True)
        inner.run()
        close = inner.i - 1
        self.nested.append((self.here(), "text", self.s[paren + 1:close]))
        self.add(self.s[self.i:close + 1])
        self.i = close + 1

    def scan_substitutions(self):
        """Find $( ) and backticks in text that is not split into words (an
        unquoted heredoc body)."""
        while self.i < len(self.s):
            c = self.peek()
            if c == "\\":
                self.i += 2
            elif c == "`" or (c == "$" and self.peek(1) == "("):
                self.substitution()
            else:
                self.i += 1
        return self.nested

    def single_quote(self):
        end = self.s.find("'", self.i + 1)
        if end < 0:
            raise ParseError("open single quote")
        self.add(self.s[self.i + 1:end])
        self.quoted = True
        self.i = end + 1

    def ansi_quote(self):
        j = self.i + 2
        out = []
        escapes = {"n": "\n", "t": "\t", "r": "\r", "\\": "\\", "'": "'", '"': '"'}
        while j < len(self.s) and self.s[j] != "'":
            if self.s[j] == "\\" and j + 1 < len(self.s):
                out.append(escapes.get(self.s[j + 1], self.s[j + 1]))
                j += 2
                continue
            out.append(self.s[j])
            j += 1
        if j >= len(self.s):
            raise ParseError("open $' quote")
        self.add("".join(out))
        self.quoted = True
        self.i = j + 1

    def double_quote(self):
        self.i += 1
        self.started = True
        self.quoted = True
        while True:
            c = self.peek()
            if c == "":
                raise ParseError("open double quote")
            if c == '"':
                self.i += 1
                return
            if c == "\\":
                n = self.peek(1)
                if n == "\n":
                    self.i += 2
                elif n in '$`"\\':
                    self.add(n)
                    self.i += 2
                else:
                    self.add(c)
                    self.i += 1
                continue
            if c == "`" or (c == "$" and self.peek(1) == "("):
                self.substitution()
                continue
            self.add(c)
            self.i += 1

    def heredoc_bodies(self):
        # The bodies start after the newline that ends the command line.
        for delimiter, strip_tabs, quoted, segment in self.heredocs:
            lines = []
            while self.i < len(self.s):
                end = self.s.find("\n", self.i)
                line = self.s[self.i:] if end < 0 else self.s[self.i:end]
                self.i = len(self.s) if end < 0 else end + 1
                if (line.lstrip("\t") if strip_tabs else line) == delimiter:
                    break
                lines.append(line)
            body = "\n".join(lines)
            self.nested.append((segment, "heredoc", body))
            if not quoted:
                for _, kind, text in Lexer(body).scan_substitutions():
                    self.nested.append((segment, kind, text))
        self.heredocs = []

    def operator(self):
        s = self.s
        if s.startswith(("<(", ">("), self.i):
            self.substitution_at(self.i + 1)
            return True
        # In "let" and "(( ))" the shift operators are arithmetic.
        arithmetic = self.words[:1] == ["let"]
        for op in [] if arithmetic else REDIRECTS:
            if s.startswith(op, self.i):
                # "2>file": a number that touches the operator is its fd.
                if (self.started and not self.quoted and not self.after_space
                        and "".join(self.word).isdigit()):
                    self.word = []
                    self.started = False
                self.end_word()
                self.i += len(op)
                if op in ("<<", "<<-"):
                    self.heredoc_word = op == "<<-"
                elif op in (">&", "<&") and (self.peek().isdigit() or self.peek() == "-"):
                    while self.peek().isdigit() or self.peek() == "-":
                        self.i += 1
                else:
                    self.skip_word = True
                return True
        if s.startswith("((", self.i) and not self.words and not self.started:
            close = find_close(s, self.i + 2)
            if close + 1 < len(s) and s[close + 1] == ")":
                self.arithmetic(close + 2)
                return True
        for op in OPERATORS:
            if s.startswith(op, self.i):
                if self.closing and op == ")" and self.depth == 0:
                    self.end_segment()
                    self.i += 1
                    self.done = True
                    return True
                if self.closing and op in ("(", ")"):
                    self.depth += 1 if op == "(" else -1
                self.end_segment(op)
                self.i += len(op)
                if op == "\n" and self.heredocs:
                    self.heredoc_bodies()
                return True
        if arithmetic:
            self.add(self.peek())
            self.i += 1
            return True
        return False

    def run(self):
        self.done = False
        while self.i < len(self.s) and not self.done:
            c = self.peek()
            if c in " \t\r":
                self.end_word()
                self.i += 1
                self.after_space = True
                continue
            if c == "\\":
                n = self.peek(1)
                if n == "\n":
                    self.i += 2
                    continue
                self.add(n)
                self.quoted = True
                self.i += 2
                self.after_space = False
                continue
            if c == "#" and not self.started:
                end = self.s.find("\n", self.i)
                self.i = len(self.s) if end < 0 else end
                continue
            if c in ";&|()<>\n" and self.operator():
                self.after_space = True
                continue
            self.after_space = False
            if c == "'":
                self.single_quote()
            elif c == "$" and self.peek(1) == "'":
                self.ansi_quote()
            elif c == '"':
                self.double_quote()
            elif c == "`" or (c == "$" and self.peek(1) == "("):
                self.substitution()
            elif c == "$" and self.peek(1) == "[":
                end = self.s.find("]", self.i)
                if end < 0:
                    raise ParseError("open $[")
                self.add(self.s[self.i:end + 1])
                self.i = end + 1
            else:
                self.add(c)
                self.i += 1
        if self.closing and not self.done:
            raise ParseError("open $(")
        if self.cases:
            raise ParseError("open case")
        self.end_segment()
        if self.heredocs:
            self.heredoc_bodies()
        return self.segments, self.nested


def in_subshell(commands):
    return [[SUBSHELL_START]] + commands + [[SUBSHELL_END]] if commands else []


def parse(text, depth=0):
    if depth > MAX_NESTING:
        raise ParseError("nested too deep")
    segments, nested = Lexer(text).run()
    result = []
    for k, (words, operator) in enumerate(segments):
        while words and words[0] in RESERVED:
            words = words[1:]
        if words:
            result.append(words)
            inner = unwrap(words)
            while inner:
                result.append(inner)
                inner = unwrap(inner)
            if command_name(words) in SHELLS:
                for j in range(1, len(words) - 1):
                    w = words[j]
                    if w.startswith("-") and not w.startswith("--") and "c" in w:
                        result.extend(in_subshell(parse(words[j + 1], depth + 1)))
                        break
        for index, kind, body in nested:
            if index != k:
                continue
            if kind == "text":
                result.extend(in_subshell(parse(body, depth + 1)))
                continue
            # A heredoc body is code only when a shell reads it: the command
            # itself, or a later one in the same pipeline.
            readers = [command_name(words)]
            j = k
            while j + 1 < len(segments) and segments[j][1] == "|":
                j += 1
                readers.append(command_name(segments[j][0]))
            if any(name in CODE_READERS for name in readers):
                result.extend(in_subshell(parse(body, depth + 1)))
    return result


def balanced(commands):
    depth = 0
    for words in commands:
        depth += {SUBSHELL_START: 1, SUBSHELL_END: -1}.get(words[0], 0)
        if depth < 0:
            return False
    return depth == 0


def main():
    try:
        result = parse(sys.stdin.read())
        if not balanced(result):
            raise ParseError("unbalanced subshells")
    except (ParseError, RecursionError):
        return 1
    out = sys.stdout.buffer
    for words in result:
        out.write(str(len(words)).encode() + b"\0")
        for word in words:
            out.write(word.encode("utf-8", "surrogateescape") + b"\0")
    return 0


if __name__ == "__main__":
    sys.exit(main())
