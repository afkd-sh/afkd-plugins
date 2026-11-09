#!/usr/bin/env python3
# fetch_file.py — download a file a message carries.
#
# The skill's half of the plugin's `fetch_file`: it writes the file of the Telegram file id
# that is its first argument, one the run's brief lists, to the path that is its second,
# whose directory must exist, through a temp file renamed into place, and prints the path
# written. Telegram hands a bot files of up to 20 MB and refuses a larger one, which it passes
# on. It reads TELEGRAM_BOT_TOKEN and TELEGRAM_API_URL, and exits 1 naming any failure.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import telegram_api as telegram  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Download a file a message carries.")
    parser.add_argument("file", help="the file id, as the brief lists it")
    parser.add_argument("to", help="the path to write it to")
    args = parser.parse_args()
    directory = os.path.dirname(os.path.abspath(args.to))
    if not os.path.isdir(directory):
        telegram.die(f"the directory of {args.to} does not exist")

    info = telegram.call("fetch_file", "getFile", {"file_id": args.file})
    path = info.get("file_path") if isinstance(info, dict) else None
    if not path:
        raise telegram.TelegramError("Telegram gave no path to fetch the file from", "fetch_file")
    data = telegram.download("fetch_file", path)
    temp = f"{args.to}.part"
    try:
        with open(temp, "wb") as handle:
            handle.write(data)
        os.replace(temp, args.to)
    except OSError as e:
        if os.path.exists(temp):
            os.unlink(temp)
        telegram.die(f"cannot write {args.to}: {e}")
    print(args.to)


if __name__ == "__main__":
    telegram.run(main)
