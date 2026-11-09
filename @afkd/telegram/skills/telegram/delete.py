#!/usr/bin/env python3
# delete.py — delete a message.
#
# The skill's half of the plugin's `delete`: it deletes the message whose id is its one
# argument, one send.py or reply.py printed, in the run's chat or --chat-id. It reads
# TELEGRAM_BOT_TOKEN, TELEGRAM_API_URL and TELEGRAM_CHAT_ID, and exits 1 naming any failure.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import telegram_api as telegram  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Delete a message.")
    parser.add_argument("message", help="the message's id, as send.py printed it")
    telegram.add_chat_arg(parser)
    args = parser.parse_args()
    message = telegram.message_id(args.message, "the message")

    chat = telegram.chat_ref(args)
    telegram.call(
        "delete", "deleteMessage", {"chat_id": telegram.chat_id(chat), "message_id": message}
    )
    print(f"delete.py: deleted message {message} in chat {chat}")


if __name__ == "__main__":
    telegram.run(main)
