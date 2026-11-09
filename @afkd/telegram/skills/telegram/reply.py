#!/usr/bin/env python3
# reply.py — reply to the message the run works.
#
# The skill's half of the plugin's `reply`: it sends its one argument, verbatim, as a reply
# to the message the run was handed, or to --message, in its chat. It is split as send.py
# splits a text, and only the first part replies. It prints the id of every message it sent.
# It reads TELEGRAM_BOT_TOKEN, TELEGRAM_API_URL, TELEGRAM_CHAT_ID and TELEGRAM_MESSAGE_ID. It
# refuses SKILL.md's placeholder (exit 2) before any request, and exits 1 naming any failure.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import telegram_api as telegram  # noqa: E402

PLACEHOLDER = "<REPLACE WITH YOUR REPLY>"


def main():
    parser = argparse.ArgumentParser(description="Reply to the message.")
    parser.add_argument("text", help="the reply, verbatim")
    telegram.add_message_arg(parser)
    telegram.add_chat_arg(parser)
    args = parser.parse_args()
    telegram.refuse_placeholder(args.text, PLACEHOLDER)

    chat = telegram.chat_ref(args)
    message = telegram.message_ref(args)
    for sent in telegram.send_text("reply", chat, args.text, reply_to=message):
        print(f"reply.py: sent message {sent} to chat {chat}")


if __name__ == "__main__":
    telegram.run(main)
