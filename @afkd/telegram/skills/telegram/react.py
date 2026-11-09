#!/usr/bin/env python3
# react.py — set the bot's reaction to the message the run works.
#
# The skill's half of the plugin's `react`: it sets the bot's reaction to the message the run
# was handed, or to --message, to its one argument, one of Telegram's reaction emoji (👍 👌 👀
# 🔥 🎉 ❤), or takes it off with "". Telegram refuses any other emoji with REACTION_INVALID,
# which it passes on. It reads TELEGRAM_BOT_TOKEN, TELEGRAM_API_URL, TELEGRAM_CHAT_ID and
# TELEGRAM_MESSAGE_ID, and exits 1 naming any failure.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import telegram_api as telegram  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="React to the message.")
    parser.add_argument("emoji", help='one of Telegram\'s reaction emoji, or "" to take it off')
    telegram.add_message_arg(parser)
    telegram.add_chat_arg(parser)
    args = parser.parse_args()

    chat = telegram.chat_ref(args)
    message = telegram.message_ref(args)
    reaction = [{"type": "emoji", "emoji": args.emoji}] if args.emoji else []
    telegram.call(
        "react",
        "setMessageReaction",
        {"chat_id": telegram.chat_id(chat), "message_id": message, "reaction": reaction},
    )
    if args.emoji:
        print(f"react.py: reacted {args.emoji} to message {message} in chat {chat}")
    else:
        print(f"react.py: took the reaction off message {message} in chat {chat}")


if __name__ == "__main__":
    telegram.run(main)
