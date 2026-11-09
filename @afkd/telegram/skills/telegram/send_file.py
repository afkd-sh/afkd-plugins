#!/usr/bin/env python3
# send_file.py — send a file to the run's Telegram chat.
#
# The skill's half of the plugin's `send_file`: it sends the file at its one argument, its
# bytes as they are under its own name, to the chat the run works in, or to --chat-id, as a
# --kind (document, the default, photo, video, audio or voice), with --caption under it. A
# caption over Telegram's 1024 UTF-16 units is refused, not cut, and so is a file over 50 MB,
# or a photo over 10 MB, each before anything is sent; a 429 is waited out. It prints the id
# of the message it sent. It reads TELEGRAM_BOT_TOKEN, TELEGRAM_API_URL and TELEGRAM_CHAT_ID.
# It refuses SKILL.md's placeholder (exit 2) before any request, and exits 1 naming any
# failure.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import telegram_api as telegram  # noqa: E402

PLACEHOLDER = "<REPLACE WITH THE FILE'S PATH>"

METHODS = {
    "document": "sendDocument",
    "photo": "sendPhoto",
    "video": "sendVideo",
    "audio": "sendAudio",
    "voice": "sendVoice",
}


def main():
    parser = argparse.ArgumentParser(description="Send a file to the chat.")
    parser.add_argument("path", help="the file to send")
    parser.add_argument("--kind", choices=telegram.FILE_KINDS, default="document")
    parser.add_argument("--caption", default="", help="the text under it, at most 1024")
    telegram.add_chat_arg(parser)
    args = parser.parse_args()
    telegram.refuse_placeholder(args.path, PLACEHOLDER)

    chat = telegram.chat_ref(args)
    units = telegram.utf16_len(args.caption)
    if units > telegram.CAPTION_CAP:
        telegram.die(
            f"the caption is {units} UTF-16 units, over the {telegram.CAPTION_CAP} Telegram "
            "takes; a caption is not cut"
        )
    if not os.path.isfile(args.path):
        telegram.die(f"{args.path} is not a file")
    size = os.path.getsize(args.path)
    cap = telegram.UPLOAD_CAP.get(args.kind, telegram.DEFAULT_UPLOAD_CAP)
    if size > cap:
        telegram.die(
            f"{args.path} is {size} bytes, over the {cap} bytes Telegram takes in a {args.kind}"
        )
    try:
        with open(args.path, "rb") as handle:
            content = handle.read()
    except OSError as e:
        telegram.die(f"cannot read {args.path}: {e}")

    fields = [("chat_id", chat)]
    if args.caption:
        fields.append(("caption", args.caption))
    name = os.path.basename(args.path)
    message = telegram.upload("send_file", METHODS[args.kind], fields, args.kind, name, content)
    sent = telegram.sent_id("send_file", message)
    print(f"send_file.py: sent message {sent} to chat {chat}")


if __name__ == "__main__":
    telegram.run(main)
