#!/usr/bin/env python3
# edit.py — replace the text of a message the bot sent.
#
# The skill's half of the plugin's `edit`: it replaces the text of the message whose id is its
# first argument, one send.py or reply.py printed, with its second, or the message's caption
# when it carries a file. It is never split: a text over Telegram's 4096 UTF-16 units, or a
# caption over 1024, is refused before it is sent. It reads TELEGRAM_BOT_TOKEN,
# TELEGRAM_API_URL and TELEGRAM_CHAT_ID. It refuses SKILL.md's placeholder (exit 2) before any
# request, and exits 1 naming any failure.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import telegram_api as telegram  # noqa: E402

PLACEHOLDER = "<REPLACE WITH THE NEW TEXT>"


def main():
    parser = argparse.ArgumentParser(description="Edit a message the bot sent.")
    parser.add_argument("message", help="the message's id, as send.py printed it")
    parser.add_argument("text", help="the new text, verbatim")
    telegram.add_chat_arg(parser)
    args = parser.parse_args()
    telegram.refuse_placeholder(args.text, PLACEHOLDER)
    message = telegram.message_id(args.message, "the message")

    chat = telegram.chat_ref(args)
    telegram.edit("edit", chat, message, args.text)
    print(f"edit.py: edited message {message} in chat {chat}")


if __name__ == "__main__":
    telegram.run(main)
