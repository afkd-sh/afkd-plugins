# `@afkd/telegram`

A **provider** plugin with one trigger kind, [`service(telegram)`](#servicetelegram-messages):
it starts a run on each message a Telegram bot receives in the chats you list, a text, a
file or both, and gives every config the calls that send a text or a file, reply, react,
edit, delete and fetch a file a message carries. It also ships the `telegram` skill an agent
does the same through.

A message the bot received and a message it sent are one type, `telegram.Message`, so
`telegram.edit(msg, …)` takes what `telegram.send` gave back as readily as `telegram.reply`
takes what the service was passed. See [`docs/plugins.md`](https://afkd.sh/docs/plugins/)
for the wire it speaks, and [config language v2](https://afkd.sh/docs/lang-v2/) for the
configs that use it.

It is a Rust program, released as a finished binary.

## Install it

```console
$ afkd install @afkd/telegram
```

afkd downloads the release built for this host's platform — `x86_64-linux` or
`aarch64-linux`, a static binary that runs on any Linux of that CPU — and places it as it
is: the manifest, the `afkd-telegram` binary and its `skills/`, nothing else. Nothing is
compiled on your host, and no Rust toolchain is needed there. It needs **afkd 0.2.575 or
newer**, the first whose plugin actions give back the handle they made; `afkd update
@afkd/telegram` moves an installed copy onto the next release.

From a checkout of this repository, build and pack it the way the release does, then name
the packed directory:

```console
$ cargo build --release --locked --manifest-path @afkd/telegram/Cargo.toml
$ tools/pack-release telegram /tmp/afkd-release @afkd/telegram/target/release/afkd-telegram
$ afkd install /tmp/afkd-release/afkd-telegram
```

A config file uses the plugin by importing it, and then names it by its leaf, `telegram`.
This service hands every message in two chats to an agent, which answers through the skill:

```conf
import "afkd"
import "agent:claude"
import "std:env"
import "@afkd/telegram"

BOT_TOKEN :: env.get("TELEGRAM_BOT_TOKEN") ?? ""

helper :: claude.agent(timeout=30m, skills=[ "@afkd/telegram/telegram" ])

ops :: service(telegram) {
  token    BOT_TOKEN
  chats    "-1001234567890", "111111111"
  work_dir "/srv/acme/widgets"

  on_claim proc(message: telegram.Message) {
    seen := telegram.react(message, "👀")
    if seen != nil { fail("telegram.react: #{seen.message}") }
  }

  on_run proc(run: afkd.Run, message: telegram.Message) {
    match helper(
      "Do what #{message.sender_name} asks in #{run.scratch_dir}/task.md, and answer them with the telegram skill's reply.py.",
    ) {
      .ok(_) => { }
      .err(e) => fail("helper: #{e.message}")
    }
  }

  on_done proc(run: afkd.Run, message: telegram.Message, outcome: afkd.Outcome) {
    done := telegram.react(message, "👍")
    if done != nil { fail("telegram.react: #{done.message}") }
  }

  on_fail proc(run: afkd.Run, message: telegram.Message, outcome: afkd.Outcome) {
    match telegram.reply(message, "That failed after #{outcome.duration}; the run log says why.") {
      .ok(_) => { }
      .err(e) => fail("telegram.reply: #{e.message}")
    }
  }
}
```

Any other service calls the same actions, as the bot your settings file names:

```conf
// ~/.config/afkd/plugins/@afkd/telegram.afkd
token "7712345678:AAHfiqksKZ8WmR2zSjiQ7_v4TMAKdiHm9T0"
```

```conf
import "afkd"
import "service:interval"
import "@afkd/telegram"

nightly :: service(interval) {
  every    24h
  work_dir "/srv/acme/widgets"

  on_run proc(run: afkd.Run) {
    match telegram.send("-1001234567890", "nightly build — ビルド started") {
      .ok(sent) => {
        $ make nightly
        edited := telegram.edit(sent, "nightly build — ビルド done ✓")
        if edited != nil { fail("telegram.edit: #{edited.message}") }
        match telegram.send_file(
          "-1001234567890",
          "/srv/acme/widgets/build.log",
          caption="the build log",
        ) {
          .ok(_) => { }
          .err(e) => fail("telegram.send_file: #{e.message}")
        }
      }
      .err(e) => fail("telegram.send: #{e.message}")
    }
  }
}
```

## `service(telegram)` (messages)

A `service(telegram)` reads its bot's messages and starts one run per message, in the order
they arrived. A bot token's messages have one reader — Telegram answers a second reader with
a 409 — so the service is the only thing that reads them: run **one service per token**, set
no webhook beside it, and let the skill read the message from the run's brief, never from
Telegram.

| Setting    | Type        | Default                    |                                                |
|------------|-------------|----------------------------|------------------------------------------------|
| `token`    | `str`       | required                   | the bot's token, as BotFather gave it          |
| `chats`    | `List[str]` | required                   | the chat ids whose messages start a run        |
| `base_url` | `str`       | `https://api.telegram.org` | the Bot API's root, for a local Bot API server |

afkd's own claim keys work as on any claiming service: `every`, the beat, and
`max_attempts`.

**What starts a run.** A message in a chat `chats` lists, with a text, a caption or a file:
a document, a photo, a video, an audio file or a voice note. A message from any other chat,
and one with none of them (a sticker, a member joining), is skipped with one line in the
service log naming its chat and never its text, so a bot anyone can message runs nothing for
a stranger. A group's id is negative, `-1001234567890`; a private chat's is the user's id.

**At most once.** Before a message is handed over, the service confirms it to Telegram, so a
daemon restart never starts a second run for it. If afkd dies between the confirm and the
run, that message is missed: a missed message costs less than an agent acting twice in a
chat.

**The slots.** Each is passed the message as a `telegram.Message`:

| Slot       | When                    | Parameters                                                        |
|------------|-------------------------|-------------------------------------------------------------------|
| `on_claim` | before the run          | `message: telegram.Message`                                       |
| `on_run`   | the run                 | `run: afkd.Run, message: telegram.Message`                        |
| `on_done`  | after a run that passed | `run: afkd.Run, message: telegram.Message, outcome: afkd.Outcome` |
| `on_fail`  | after a run that failed | `run: afkd.Run, message: telegram.Message, outcome: afkd.Outcome` |

There is no `on_park`: nothing parks a message.

**The message.** `message.id` is Telegram's own message id (`4711`) and `message.key` is
`telegram:<chat>/<id>`. Its fields are always there, `""` or an empty list for what the
message does not carry:

| Field         | Type        |                                                                               |
|---------------|-------------|-------------------------------------------------------------------------------|
| `chat`        | `str`       | the chat's id                                                                 |
| `sender`      | `str`       | the sender's user id, `""` for a message sent on behalf of a chat             |
| `sender_name` | `str`       | their first and last name, or `@username` when they have none                 |
| `text`        | `str`       | the text, or a file's caption                                                 |
| `reply_to`    | `str`       | the id of the message it replies to                                           |
| `files`       | `List[str]` | the file ids of its document, photo (the largest size), video, audio or voice |

**What a run gets.** The run's thread is `telegram:<chat>`, so an agent with
`session=.per_item` carries a chat's conversation on. Its brief, `task.md`, holds who sent
the message, in which chat, when, what it replies to, its text, and a line per file with its
kind, name, size and file id. Its env holds what the skill reads:

| Variable              |                       |
|-----------------------|-----------------------|
| `TELEGRAM_BOT_TOKEN`  | the service's `token` |
| `TELEGRAM_API_URL`    | the Bot API's root    |
| `TELEGRAM_CHAT_ID`    | the message's chat    |
| `TELEGRAM_MESSAGE_ID` | the message's id      |

**When Telegram refuses.** A token Telegram refuses (401), and a second reader or a webhook
(409), end the service, saying why in its log: no later beat can fix either. A rate limit or
a Telegram out of reach is a quiet beat, said in the log, and the next beat tries again.

## The actions

Every action is one call any service makes from any slot. A chat is its id as a string, or
`@channel` for a public channel; a file is a Telegram file id, as `message.files` lists it;
a message is a `telegram.Message`, one a `service(telegram)` was passed or one an action gave
back.

| Action                                                      | Gives back               |
|-------------------------------------------------------------|--------------------------|
| `send(chat, text)`                                          | `Result[Message, Error]` |
| `send_file(chat, path, ~kind := .document, ~caption := "")` | `Result[Message, Error]` |
| `reply(message, text)`                                      | `Result[Message, Error]` |
| `react(message, emoji)`                                     | `Error?`                 |
| `edit(message, text)`                                       | `Error?`                 |
| `delete(message)`                                           | `Error?`                 |
| `fetch_file(file, to)`                                      | `Error?`                 |

- **`send`** sends a text to a chat. A text over Telegram's 4096 is sent as several
  messages, each cut after its last newline inside the cap, or at the cap when it has none,
  and the **last** one is the message given back, so an `edit` of it edits only that part.
  Telegram counts its caps in UTF-16 units, so an emoji counts two.
- **`send_file`** sends the file at the absolute `path`, its bytes as they are under its own
  name, as `~kind`: `.document` (any file), `.photo`, `.video`, `.audio` or `.voice`. A
  `~caption` over 1024 is refused, not cut, and so is a file over 50 MB, or a photo over
  10 MB, before anything is sent. The message given back lists the file's new file id in
  `files`.
- **`reply`** sends a text to the message's chat as a reply to it, split as `send` splits;
  only the first part replies, and the last is given back.
- **`react`** sets the bot's reaction to the message to `emoji`, or takes it off with `""`.
  Only Telegram's own reaction emoji work — 👍 👌 👀 🔥 🎉 ❤ among them, ✅ not — and
  Telegram's refusal of any other, `REACTION_INVALID`, is the error.
- **`edit`** replaces the text of a message the bot sent, or its caption when it carries a
  file. It is never split: a text over 4096, or over 1024 for a caption, is refused.
- **`delete`** deletes the message.
- **`fetch_file`** writes the file of a file id to the absolute path `to`, whose directory
  must exist. Telegram hands a bot files of up to 20 MB, and refuses a larger one.

**Nothing is queued.** `send`, `send_file` and `reply` give back the message Telegram made,
which a queued send could not, so every action is done at once or refused, and a Telegram
out of reach is an error. A 429 is waited out while its wait fits the call's 45 seconds, and
is an error naming the wait otherwise.

**The bot.** A `service(telegram)` calls as its own bot. Any other service calls as the bot
of the plugin's settings file, `plugins/@afkd/telegram.afkd` in your afkd config directory,
which takes `token` and `base_url`. Neither is required, so a config that only runs a
`service(telegram)` needs no file; a call that finds no token says so.

## Name the skill

The `telegram` skill gives an agent the same calls, one script each: `reply.py`, `react.py`,
`send.py`, `send_file.py`, `edit.py`, `delete.py` and `fetch_file.py`. Name it in the
agent's `skills`, as `@afkd/telegram/telegram`. The scripts read the env a
`service(telegram)` run carries, so an agent replies to and reacts on the message it was
handed with no argument but its text; a run of any other service sets the four variables in
its `env`. They split a long text and keep the caps as the actions do, print the id of every
message they send for a later `edit.py` or `delete.py`, and wait out a 429 for the time
Telegram asks, five tries in all. The skill reads no message: the run's brief holds the one
it works.

## How it is tested

```console
$ cd @afkd/telegram && cargo test --locked
```

The plugin's own suite never reaches Telegram. Its wire suite speaks to the built binary as
afkd does, and its skill suite runs each script with `python3`, both against a loopback fake
Bot API (`tests/common/fake.rs`) whose answers are the Bot API's own objects, transcribed
into `tests/fixtures/` from the documentation, with each request's values filled in. The fake
holds every request to the methods and parameters the Bot API documents, and answers as
Telegram does where Telegram checks a value: an unknown reaction emoji, an edit of a message
with no text, a file too big to fetch. The skill suite also sends the same texts through the
plugin's `send` and through `send.py`, and holds the messages each left in the chat to be the
same.

The drift gate under `drift/@afkd/telegram` installs the packed release into a real afkd,
the `afkd` first on `PATH`, and runs a daemon against the same fake.
