# telegram_api.py — what every script of the bundled `telegram` skill shares.
#
# Each script parses its arguments and prints; this module holds the rest, once: the
# environment a run carries, the Bot API requests with their 429 waits, the multipart body an
# upload is sent in, and the caps and the cut of a long text into messages that the plugin's
# own calls keep.
#
# THE ENVIRONMENT is the one a `service(telegram)` puts on every run (src/common.rs):
# TELEGRAM_BOT_TOKEN, the bot's token; TELEGRAM_API_URL, the Bot API's root; TELEGRAM_CHAT_ID,
# the chat of the message the run works; and TELEGRAM_MESSAGE_ID, that message's id. Any other
# service wires them through its `env`. TELEGRAM_API_URL defaults to Telegram's own root and is
# also the seam the tests point at a fake Bot API. A script that needs one that is unset exits
# 1 naming it, before any request.
#
# EVERY REQUEST is one Bot API method at `<root>/bot<token>/<method>`, its parameters a JSON
# body, or a `multipart/form-data` one for an upload, as the plugin sends them (src/client.rs).
# Telegram answers a refusal with its JSON envelope on a 4xx as on a 200, so every reply is
# read for `ok` and `description`. A 429 is waited out for the `retry_after` Telegram asks,
# up to MAX_ATTEMPTS tries: a script has no deadline, where the plugin's calls do. The token is
# in every URL, so no failure says the URL, and the token is taken out of every reason.
#
# THE CUT OF A LONG TEXT ports src/message.rs's `split_text`, and tests/skills.rs sends the same
# texts through both and holds the messages each sent to be the same. Telegram counts its caps
# in UTF-16 code units and Python counts characters, so the units are counted here, and the
# whitespace Rust's `trim` takes is spelled out as Rust defines it.
#
# Python 3.9 or newer, and the standard library only. Not executable: the scripts import it.

import http.client
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
import uuid

# --- the environment -------------------------------------------------------------------

ENV_TOKEN = "TELEGRAM_BOT_TOKEN"
ENV_API_URL = "TELEGRAM_API_URL"
ENV_CHAT_ID = "TELEGRAM_CHAT_ID"
ENV_MESSAGE_ID = "TELEGRAM_MESSAGE_ID"

# The Bot API's root, which TELEGRAM_API_URL replaces.
TELEGRAM_BASE = "https://api.telegram.org"

# The script running, as its failures name it.
SCRIPT = os.path.basename(sys.argv[0]) if sys.argv and sys.argv[0] else "telegram"

# How long one request may take, the plugin's own read timeout.
TIMEOUT = 30

# How many times a request is sent while Telegram answers it with a 429.
MAX_ATTEMPTS = 5

# --- Telegram's caps -------------------------------------------------------------------

# The most UTF-16 code units Telegram takes in one message's text, and in a caption.
TEXT_CAP = 4096
CAPTION_CAP = 1024

MIB = 1024 * 1024

# The most Telegram takes from a bot in one upload: 10 MB for a photo, 50 MB for the rest.
FILE_KINDS = ("document", "photo", "video", "audio", "voice")
UPLOAD_CAP = {"photo": 10 * MIB}
DEFAULT_UPLOAD_CAP = 50 * MIB

# The most a bot may fetch of a file it received.
FETCH_CAP = 20 * MIB

# Telegram's description when `editMessageText` is asked to edit a message that carries a file.
NO_TEXT_TO_EDIT = "Bad Request: there is no text in the message to edit"

# The characters Rust's `char::is_whitespace` reads as whitespace (Unicode's White_Space),
# which Python's `str.strip` takes more than: it also strips U+001C to U+001F.
WHITESPACE = (
    "\t\n\x0b\x0c\r \x85\xa0\u1680\u2000\u2001\u2002\u2003\u2004\u2005\u2006\u2007\u2008"
    "\u2009\u200a\u2028\u2029\u202f\u205f\u3000"
)


class TelegramError(Exception):
    """A request that did not do what it asked: `why`, the sentence that says so, under the
    `stage` of the script that asked, and Telegram's own `description` when it refused."""

    def __init__(self, why, stage=None, description=None):
        self.why, self.stage, self.description = why, stage, description
        super().__init__(f"{stage}: {why}" if stage else why)

    def at(self, stage):
        """This failure, said as `stage`'s."""
        return type(self)(self.why, stage, self.description)


class RateLimited(TelegramError):
    """A 429 Telegram kept answering past MAX_ATTEMPTS tries."""


def die(sentence, code=1):
    """Print `<script>: <sentence>` to stderr and exit `code`."""
    print(f"{SCRIPT}: {sentence}", file=sys.stderr)
    sys.exit(code)


def run(main):
    """Run a script's `main`, a TelegramError ending it as its one-line failure, exit 1."""
    try:
        main()
    except TelegramError as e:
        die(str(e))


def refuse_placeholder(text, placeholder):
    """Exit 2 when `text` is SKILL.md's placeholder, before any request."""
    if text == placeholder:
        die("refusing to send the placeholder text; substitute your own", 2)


def need_env(*names):
    """The values of `names` from the environment, in order; exits 1 naming each one unset."""
    values = [os.environ.get(name) for name in names]
    missing = [name for name, value in zip(names, values) if not value]
    if missing:
        die(f"{' and '.join(missing)} must be set in the environment")
    return values


def add_chat_arg(parser):
    """Give `parser` the `--chat-id` override of the run's chat."""
    parser.add_argument(
        "--chat-id",
        metavar="ID",
        help=f"the chat, by its id or @channel (defaults to ${ENV_CHAT_ID})",
    )


def add_message_arg(parser):
    """Give `parser` the `--message` override of the run's message."""
    parser.add_argument(
        "--message",
        metavar="ID",
        help=f"the message, by its id in the chat (defaults to ${ENV_MESSAGE_ID})",
    )


def chat_ref(args):
    """The chat the script acts in: `--chat-id`, else `$TELEGRAM_CHAT_ID`; exits 1 with neither."""
    chat = args.chat_id or os.environ.get(ENV_CHAT_ID)
    if not chat:
        die(f"no chat (set {ENV_CHAT_ID} or pass --chat-id)")
    return chat


def message_id(text, what="--message"):
    """A message id: a whole number of at least 1; exits 2 naming `what` otherwise."""
    if not re.fullmatch(r"[0-9]+", text or "") or int(text) < 1:
        die(f"{what} is {text!r}, not a message id (a whole number)", 2)
    return int(text)


def message_ref(args):
    """The message the script acts on: `--message`, else `$TELEGRAM_MESSAGE_ID`; exits 1 with
    neither."""
    if args.message:
        return message_id(args.message)
    ref = os.environ.get(ENV_MESSAGE_ID)
    if not ref:
        die(f"no message (set {ENV_MESSAGE_ID} or pass --message)")
    return message_id(ref, ENV_MESSAGE_ID)


def chat_id(chat):
    """A chat as the Bot API takes it: an id as a number, `@channel` as a string."""
    if re.fullmatch(r"[+-]?[0-9]+", chat) and -(2**63) <= int(chat) < 2**63:
        return int(chat)
    return chat


# --- texts -----------------------------------------------------------------------------


def utf16_len(text):
    """How many UTF-16 code units `text` takes, the unit Telegram counts its caps in."""
    return sum(2 if ord(c) > 0xFFFF else 1 for c in text)


def is_blank(text):
    """Whether `text` is empty once Rust's `trim` has taken its whitespace."""
    return not text.strip(WHITESPACE)


def split_text(text, cap=TEXT_CAP):
    """`text` cut into the messages Telegram takes, each at most `cap` UTF-16 units: a part
    ends after the last newline inside its window, or at the cap when the window holds none,
    and never inside a character. A part that is only whitespace is not sent, and a text with
    nothing else is refused, as src/message.rs's `split_text` does."""
    parts = []
    rest = text
    while utf16_len(rest) > cap:
        units, end, newline = 0, 0, None
        for at, c in enumerate(rest):
            width = 2 if ord(c) > 0xFFFF else 1
            if units + width > cap:
                break
            units += width
            end = at + 1
            if c == "\n":
                newline = end
        cut = newline if newline is not None else end
        parts.append(rest[:cut])
        rest = rest[cut:]
    parts.append(rest)
    parts = [p for p in parts if not is_blank(p)]
    if not parts:
        raise TelegramError("the text is empty, and Telegram sends no empty message")
    return parts


# --- HTTP ------------------------------------------------------------------------------


def _credentials():
    """The bot's token and the API root."""
    (token,) = need_env(ENV_TOKEN)
    base = (os.environ.get(ENV_API_URL) or TELEGRAM_BASE).rstrip("/")
    return token, base


def _scrub(reason, token):
    """`reason` with the token taken out."""
    return str(reason).replace(token, "<token>") if token else str(reason)


def _send(url, body, content_type, token, cap=None):
    """Send one request: its status and its body, whatever the status."""
    request = urllib.request.Request(url, data=body, method="POST" if body is not None else "GET")
    if content_type:
        request.add_header("Content-Type", content_type)
    try:
        try:
            with urllib.request.urlopen(request, timeout=TIMEOUT) as response:
                return response.status, _read(response, cap)
        except urllib.error.HTTPError as e:
            with e:
                return e.code, _read(e, cap)
    except TelegramError:
        raise
    except (urllib.error.URLError, http.client.HTTPException, OSError, ValueError) as e:
        reason = getattr(e, "reason", None) or e
        raise TelegramError(f"Telegram is out of reach: {_scrub(reason, token)}") from None


def _read(response, cap):
    """The body of `response`, up to `cap` bytes; past it, a TelegramError."""
    if cap is None:
        return response.read()
    body = response.read(cap + 1)
    if len(body) > cap:
        raise TelegramError(f"the file is over the {cap // MIB} MB Telegram hands a bot")
    return body


def _envelope(status, body):
    """The Bot API's envelope `body` carries, or a TelegramError naming the status."""
    try:
        reply = json.loads(body.decode("utf-8"))
    except (UnicodeDecodeError, ValueError):
        reply = None
    if not isinstance(reply, dict) or "ok" not in reply:
        raise TelegramError(
            f"Telegram's answer did not read: HTTP {status} without the Bot API's JSON"
        )
    return reply


def _retry_after(reply):
    """The wait a 429 asked for, in seconds, or None when it named none."""
    params = reply.get("parameters")
    wait = params.get("retry_after") if isinstance(params, dict) else None
    return wait if isinstance(wait, int) and wait >= 0 else None


def _patiently(send):
    """`send()` until Telegram answers it with something other than a 429, waiting out each
    429's `retry_after`, at most MAX_ATTEMPTS tries: the result, or the TelegramError saying
    why not."""
    for attempt in range(1, MAX_ATTEMPTS + 1):
        status, body = send()
        reply = _envelope(status, body)
        if reply.get("ok") is True:
            return reply.get("result")
        description = reply.get("description") or f"HTTP {status}"
        if (reply.get("error_code") or status) != 429:
            raise TelegramError(f"Telegram refused it: {description}", description=description)
        if attempt < MAX_ATTEMPTS:
            wait = _retry_after(reply)
            time.sleep(1 if wait is None else wait)
    raise RateLimited(
        f"Telegram still rate-limited the bot after {MAX_ATTEMPTS} tries", description=description
    )


def _post(stage, method, body, content_type):
    """POST `body` to `method` and return its result, a failure said as `stage`'s."""
    token, base = _credentials()
    url = f"{base}/bot{token}/{method}"
    try:
        return _patiently(lambda: _send(url, body, content_type, token))
    except TelegramError as e:
        raise e.at(stage) from None


def call(stage, method, params):
    """Call `method` with `params` as a JSON body, and return its result."""
    return _post(stage, method, json.dumps(params).encode("utf-8"), "application/json")


def form(fields, field, filename, content):
    """A `multipart/form-data` body: one text part per `(name, value)` of `fields`, then the
    file part `field` holding `content` verbatim under `filename`. Returns the Content-Type
    and the bytes; the boundary is drawn again until it is not in `content`."""
    boundary = uuid.uuid4().hex
    while boundary.encode() in content:
        boundary = uuid.uuid4().hex
    marker = f"--{boundary}".encode()
    quoted = filename.replace("\\", "\\\\").replace('"', '\\"').replace("\r", "").replace("\n", "")
    parts = []
    for name, value in fields:
        parts += [marker, f'Content-Disposition: form-data; name="{name}"'.encode(), b""]
        parts.append(value.encode("utf-8"))
    parts += [
        marker,
        f'Content-Disposition: form-data; name="{field}"; filename="{quoted}"'.encode("utf-8"),
        b"Content-Type: application/octet-stream",
        b"",
        content,
        f"--{boundary}--".encode(),
        b"",
    ]
    return f"multipart/form-data; boundary={boundary}", b"\r\n".join(parts)


def upload(stage, method, fields, field, filename, content):
    """Send `content` as the file part `field` of `method`, with `fields`, and return the
    message Telegram made."""
    content_type, body = form(fields, field, filename, content)
    return _post(stage, method, body, content_type)


def download(stage, file_path):
    """The bytes of the file at a `getFile` path, at most FETCH_CAP of them."""
    token, base = _credentials()
    url = f"{base}/file/bot{token}/{file_path}"
    try:
        status, body = _send(url, None, None, token, FETCH_CAP)
        if status == 200:
            return body
        try:
            description = _envelope(status, body).get("description") or f"HTTP {status}"
        except TelegramError:
            description = f"HTTP {status}"
        raise TelegramError(f"Telegram refused it: {description}", description=description)
    except TelegramError as e:
        raise e.at(stage) from None


# --- the calls -------------------------------------------------------------------------


def send_text(stage, chat, text, reply_to=None):
    """Send `text` to `chat`, split at Telegram's cap, the first part replying to `reply_to`
    when given: the id of every message sent, in order. A part refused after others went out
    says how many did."""
    try:
        parts = split_text(text)
    except TelegramError as e:
        raise e.at(stage) from None
    sent = []
    for n, part in enumerate(parts):
        params = {"chat_id": chat_id(chat), "text": part}
        if reply_to is not None and n == 0:
            params["reply_parameters"] = {"message_id": reply_to}
        try:
            message = call(stage, "sendMessage", params)
        except TelegramError as e:
            if not sent:
                raise
            why = f"sent {len(sent)} of {len(parts)} parts, then: {e.why}"
            raise type(e)(why, stage, e.description) from None
        sent.append(sent_id(stage, message))
    return sent


def sent_id(stage, message):
    """The id of the message a send gave back."""
    if not isinstance(message, dict) or not isinstance(message.get("message_id"), int):
        raise TelegramError("Telegram sent no message back", stage)
    return message["message_id"]


def edit(stage, chat, message, text):
    """Replace the text of `message` in `chat` with `text`, or its caption when it carries a
    file: `editMessageText` first, then `editMessageCaption` when Telegram says the message has
    no text, since nothing says which it has. Never split: a text over the cap it is held to is
    refused before it is sent."""
    if is_blank(text):
        raise TelegramError("the text is empty, and Telegram sends no empty message", stage)
    units = utf16_len(text)
    if units > TEXT_CAP:
        raise TelegramError(
            f"the text is {units} UTF-16 units, over the {TEXT_CAP} Telegram takes; an edit is "
            "not split",
            stage,
        )
    target = {"chat_id": chat_id(chat), "message_id": message}
    try:
        call(stage, "editMessageText", dict(target, text=text))
    except TelegramError as e:
        if isinstance(e, RateLimited) or e.description != NO_TEXT_TO_EDIT:
            raise
        if units > CAPTION_CAP:
            raise TelegramError(
                f"the message carries a file, and the text is {units} UTF-16 units, over the "
                f"{CAPTION_CAP} a caption holds",
                stage,
            ) from None
        call(stage, "editMessageCaption", dict(target, caption=text))
