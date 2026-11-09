#!/usr/bin/env python3
# send.py — send a text to the run's Telegram chat.
#
# The skill's half of the plugin's `send`: it sends its one argument, verbatim, to the chat
# the run works in, or to --chat-id. A text over Telegram's 4096 UTF-16 units goes as several
# messages, each cut after a newline where it can be, and a 429 is waited out. It prints the
# id of every message it sent, so a later edit.py or delete.py can name one. It reads
# TELEGRAM_BOT_TOKEN, TELEGRAM_API_URL and TELEGRAM_CHAT_ID. It refuses SKILL.md's placeholder
# (exit 2) before any request, and exits 1 naming any failure.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import telegram_api as telegram  # noqa: E402

PLACEHOLDER = "<REPLACE WITH YOUR MESSAGE>"


def main():
    parser = argparse.ArgumentParser(description="Send a text to the chat.")
    parser.add_argument("text", help="the message, verbatim")
    telegram.add_chat_arg(parser)
    args = parser.parse_args()
    telegram.refuse_placeholder(args.text, PLACEHOLDER)

    chat = telegram.chat_ref(args)
    for sent in telegram.send_text("send", chat, args.text):
        print(f"send.py: sent message {sent} to chat {chat}")


if __name__ == "__main__":
    telegram.run(main)
