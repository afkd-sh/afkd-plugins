---
name: telegram
description: Talk in the Telegram chat this run works for, as its bot — reply to the message that started the run, react to it, send a text or a file to the chat, edit or delete a message the bot sent, and fetch a file a message carries. Reply or send to report a meaningful event — a finished task, a blocker you could not resolve, a decision that changed the approach — and react to acknowledge the message; send a file to hand over a report, a log or an artifact that is the evidence for such an event. Not for routine chatter.
allowed-tools: Bash(*/send.py *), Bash(*/reply.py *), Bash(*/react.py *), Bash(*/send_file.py *), Bash(*/edit.py *), Bash(*/delete.py *), Bash(*/fetch_file.py *)
requires-executables: [python3]
---

## The message this run works

A run of a `service(telegram)` starts on one message the bot received, and its brief,
`task.md`, holds it: who sent it, in which chat, when, what it replies to, its text, and a
line per file it carries with the file id `fetch_file.py` takes. That brief is the only way
to read an incoming message: Telegram hands a bot's messages to one reader, the service, so
this skill reads none. The service also sets the env every script reads, so you pass no
credential:

- `TELEGRAM_BOT_TOKEN` — the bot's token.
- `TELEGRAM_API_URL` — the Bot API's root.
- `TELEGRAM_CHAT_ID` — the message's chat, where every script acts unless you pass
  `--chat-id ID`.
- `TELEGRAM_MESSAGE_ID` — the message's id, which `reply.py` and `react.py` act on unless
  you pass `--message ID`.

A run of any other service has them only when its config sets them in `env`. A script that
needs one that is unset exits 1 naming it.

## Reply to the message

Run the bundled helper yourself through the Bash tool, **substituting your real reply** for
the placeholder. Never run it with the placeholder text — the helper rejects it and exits 2.

!`"${CLAUDE_PLUGIN_ROOT}/skills/telegram/reply.py" "<REPLACE WITH YOUR REPLY>"`

The reply is sent verbatim, as plain text. It prints a `sent message <id>` line for every
message it sent; keep the id to edit or delete it later.

## React to the message

!`"${CLAUDE_PLUGIN_ROOT}/skills/telegram/react.py" 👀`

It sets the bot's reaction to the message: 👀 when you take it on, 👍 when it is done. Only
Telegram's own reaction emoji work — 👍 👌 👀 🔥 🎉 ❤ 🙏 🤝 among them, ✅ not — and Telegram
refuses any other with `REACTION_INVALID`, which the helper passes on. `""` takes the
reaction off.

## Send a text to the chat

!`"${CLAUDE_PLUGIN_ROOT}/skills/telegram/send.py" "<REPLACE WITH YOUR MESSAGE>"`

It sends the text verbatim to the chat, as a message of its own rather than a reply, and
prints a `sent message <id>` line for every message it sent.

## Send a file to the chat

!`"${CLAUDE_PLUGIN_ROOT}/skills/telegram/send_file.py" "<REPLACE WITH THE FILE'S PATH>" --caption "what it is"`

It sends the file's bytes as they are, under its own name, and prints the id of the message
it sent. Optional flags:

- `--kind KIND` — how Telegram shows it: `document` (the default, any file), `photo`,
  `video`, `audio` or `voice`.
- `--caption TEXT` — the text under it, at most 1024. A longer caption is refused, not
  cut: put the rest in a message of its own.

A file over 50 MB, or a photo over 10 MB, is refused before anything is sent.

## Edit or delete a message the bot sent

Pass the id a helper printed. To replace its text, **substituting your real text** for the
placeholder:

!`"${CLAUDE_PLUGIN_ROOT}/skills/telegram/edit.py" 4712 "<REPLACE WITH THE NEW TEXT>"`

A message that carries a file has its caption replaced instead. An edit is never split: a
text over 4096, or over 1024 for a caption, is refused. To delete one:

!`"${CLAUDE_PLUGIN_ROOT}/skills/telegram/delete.py" 4712`

## Fetch a file a message carries

Pass the file id the brief lists, and the path to write it to, whose directory must exist:

!`"${CLAUDE_PLUGIN_ROOT}/skills/telegram/fetch_file.py" BQACAgQAAxkBAAIBPWcS "$AFKD_SCRATCH_DIR/deploy.log"`

It prints the path it wrote. Telegram hands a bot files of up to 20 MB and refuses a larger
one.

## Long texts and rate limits

A text over Telegram's 4096 is sent as several messages, each cut after its last newline
inside the cap, or at the cap when there is none; only the first part of a reply replies,
and every part's id is printed. Telegram counts its caps in UTF-16 units, so an emoji counts
two. When Telegram answers 429, a helper waits the time it asks and tries again, five tries
in all.

## When to message

Send only for meaningful events: a finished task, a blocker you could not resolve, an
approach that changed. Do not send routine progress chatter.

## Failure handling

If a helper exits non-zero it has failed — **surface that failure** (report it as a
blocker), do not retry it blindly.
