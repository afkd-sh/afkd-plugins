//! The **owed-release ledger**: the cards whose `afkd/claimed` label is still on after their
//! run ended, and so owed a release.
//!
//! afkd's beat is `poll`, the pre-run slot, the attempts, `finish`, the post-run slot, and the
//! next beat. The label must come off only after `on_done`, `on_park` or `on_fail` has run, and
//! those run after `finish`, so `finish` cannot take it off: it records the card here, and the
//! child's next `poll` takes the label off ([`crate::service::Service::poll`]).
//!
//! Every `service(afkd_board)` in a home, and every pool copy of one, is handed the same
//! `state_dir` and the same identity, so the one file is shared by children that cannot tell
//! one another apart by anything but their pid. Each entry therefore carries its writer: the
//! board's uuid, the `owner` `hello` named, and the writing child's own pid. A child takes off
//! only the entries it wrote, for its own board, and the entries for its own board left by an
//! afkd process that is not the current one, whose children are gone: the next start's sweep.
//!
//! The file is `<state_dir>/owed-releases.json`, changed only while holding an exclusive lock on
//! `<state_dir>/owed-releases.lock` and written as a temporary file renamed over it, so a reader
//! sees the file before a change or after it, never half of one.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The ledger's file, under the state directory.
const LEDGER_FILE: &str = "owed-releases.json";

/// The file a writer holds the lock on, so the ledger itself can be renamed over.
const LOCK_FILE: &str = "owed-releases.lock";

/// One card owed a release, and who wrote it down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Entry {
    /// The card handle's key, `afkd_board:<board uuid>/<card uuid>`.
    pub(crate) key: String,
    /// The card's number.
    pub(crate) number: i64,
    /// The board's uuid.
    pub(crate) board: String,
    /// The `owner` `hello` named: one afkd process's.
    pub(crate) owner: String,
    /// The pid of the child that wrote it, or took it over.
    pub(crate) pid: u32,
}

impl Entry {
    /// Whether the entry is `owner`'s and `pid`'s own.
    fn is_by(&self, owner: &str, pid: u32) -> bool {
        self.owner == owner && self.pid == pid
    }

    /// Whether a child of `owner` and `pid` may take the entry: it is of `board`, and the child
    /// wrote it or it was left by an afkd process that is not `owner`, whose children are gone.
    /// A sibling of the same `owner`, a live child with another pid, keeps its own.
    fn is_takeable_by(&self, board: &str, owner: &str, pid: u32) -> bool {
        self.board == board && (self.is_by(owner, pid) || self.owner != owner)
    }
}

/// The ledger of one state directory.
#[derive(Debug, Clone)]
pub(crate) struct Ledger {
    dir: PathBuf,
}

impl Ledger {
    /// The ledger under `state_dir`, which need not exist yet.
    pub(crate) fn new(state_dir: &Path) -> Self {
        Self {
            dir: state_dir.to_path_buf(),
        }
    }

    /// Write `entry` down: appended, replacing any entry of the same board and card, since a
    /// card is owed one release.
    ///
    /// # Errors
    ///
    /// Returns the error of the lock, the read or the write; nothing is recorded then.
    pub(crate) fn record(&self, entry: Entry) -> io::Result<()> {
        self.change(|entries| {
            entries.retain(|e| !(e.board == entry.board && e.key == entry.key));
            entries.push(entry);
        })
    }

    /// Take the entries of `board` that are `owner`'s and `pid`'s own, or were left by another
    /// owner, and re-tag them as the caller's, so no sibling takes them too. They come back.
    ///
    /// # Errors
    ///
    /// Returns the error of the lock, the read or the write; nothing is taken then.
    pub(crate) fn take(&self, board: &str, owner: &str, pid: u32) -> io::Result<Vec<Entry>> {
        let mut taken = Vec::new();
        self.change(|entries| {
            for entry in entries.iter_mut() {
                if entry.is_takeable_by(board, owner, pid) {
                    entry.owner = owner.to_string();
                    entry.pid = pid;
                    taken.push(entry.clone());
                }
            }
        })?;
        Ok(taken)
    }

    /// Drop `settled`, entries [`take`](Self::take) gave whose labels have come off. One a
    /// sibling has since written for the same card is left.
    ///
    /// # Errors
    ///
    /// Returns the error of the lock, the read or the write; nothing is dropped then.
    pub(crate) fn settle(&self, settled: &[Entry]) -> io::Result<()> {
        if settled.is_empty() {
            return Ok(());
        }
        self.change(|entries| entries.retain(|e| !settled.contains(e)))
    }

    /// Drop the entries of the card `key` of `board` the caller could take: its own, and those
    /// another owner left. A card released by its journalled key is owed nothing more.
    ///
    /// # Errors
    ///
    /// Returns the error of the lock, the read or the write; nothing is dropped then.
    pub(crate) fn forget(&self, board: &str, key: &str, owner: &str, pid: u32) -> io::Result<()> {
        self.change(|entries| {
            entries.retain(|e| !(e.key == key && e.is_takeable_by(board, owner, pid)));
        })
    }

    /// Every entry now, for a reader that holds no lock: the file is whole or absent.
    ///
    /// # Errors
    ///
    /// Returns the error of the read, or of a file that is no ledger.
    pub(crate) fn entries(&self) -> io::Result<Vec<Entry>> {
        match fs::read_to_string(self.dir.join(LEDGER_FILE)) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Apply `edit` to the entries while holding the lock, and write them back when it changed
    /// them.
    fn change(&self, edit: impl FnOnce(&mut Vec<Entry>)) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let lock = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(LOCK_FILE))?;
        lock.lock()?;
        let done = self.edit_locked(edit);
        let unlocked = lock.unlock();
        done?;
        unlocked
    }

    /// The body of [`change`](Self::change), run under the lock.
    fn edit_locked(&self, edit: impl FnOnce(&mut Vec<Entry>)) -> io::Result<()> {
        let before = self.entries()?;
        let mut after = before.clone();
        edit(&mut after);
        if after == before {
            return Ok(());
        }
        let temp = self
            .dir
            .join(format!("{LEDGER_FILE}.{}.tmp", std::process::id()));
        let written = (|| {
            let mut file = File::create(&temp)?;
            file.write_all(serde_json::to_string(&after)?.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temp, self.dir.join(LEDGER_FILE))
        })();
        if written.is_err() {
            let _ = fs::remove_file(&temp);
        }
        written
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;

    /// A fresh directory under the system's, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(what: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "afkd_board-owed-{what}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn entry(board: &str, number: i64, owner: &str, pid: u32) -> Entry {
        Entry {
            key: format!("afkd_board:{board}/card-{number}"),
            number,
            board: board.to_string(),
            owner: owner.to_string(),
            pid,
        }
    }

    fn numbers(entries: &[Entry]) -> Vec<i64> {
        entries.iter().map(|e| e.number).collect()
    }

    /// Two children of one afkd process, pids 10 and 11, and one entry left by an earlier afkd
    /// process share one directory: each takes what is its own, the earlier process's entry goes
    /// to the first to ask, and what has been taken is not taken by a sibling.
    #[test]
    fn each_writer_takes_only_what_is_its_own() {
        let dir = TempDir::new("own");
        let ledger = Ledger::new(&dir.0);
        ledger.record(entry("b1", 1, "afkd-2", 10)).unwrap();
        ledger.record(entry("b1", 2, "afkd-2", 11)).unwrap();
        ledger.record(entry("b1", 3, "afkd-1", 5)).unwrap();

        let first = ledger.take("b1", "afkd-2", 10).unwrap();
        assert_eq!(
            numbers(&first),
            [1, 3],
            "its own, and the earlier process's"
        );
        assert!(first.iter().all(|e| e.is_by("afkd-2", 10)), "re-tagged");

        let second = ledger.take("b1", "afkd-2", 11).unwrap();
        assert_eq!(
            numbers(&second),
            [2],
            "the sibling's own, and nothing it was left"
        );
        assert_eq!(
            numbers(&ledger.take("b1", "afkd-2", 10).unwrap()),
            [1, 3],
            "still the first's, until it settles them"
        );
        assert_eq!(numbers(&ledger.take("b1", "afkd-2", 11).unwrap()), [2]);
    }

    /// An entry of another board is left alone by a take of this one, and taken by its own.
    #[test]
    fn an_entry_of_another_board_is_left_alone() {
        let dir = TempDir::new("board");
        let ledger = Ledger::new(&dir.0);
        ledger.record(entry("b2", 7, "afkd-1", 5)).unwrap();
        ledger.record(entry("b1", 8, "afkd-2", 10)).unwrap();
        assert_eq!(ledger.take("b1", "afkd-2", 10).unwrap().len(), 1);
        assert_eq!(
            ledger.entries().unwrap(),
            [entry("b2", 7, "afkd-1", 5), entry("b1", 8, "afkd-2", 10)],
            "b2's entry is as it was written"
        );
        assert_eq!(ledger.take("b2", "afkd-2", 10).unwrap().len(), 1);
    }

    /// A settled entry is dropped and stays dropped, and a take that follows gets only what is
    /// left.
    #[test]
    fn a_settled_entry_is_dropped_and_taken_once_only() {
        let dir = TempDir::new("settle");
        let ledger = Ledger::new(&dir.0);
        ledger.record(entry("b1", 1, "afkd-2", 10)).unwrap();
        ledger.record(entry("b1", 2, "afkd-2", 10)).unwrap();
        let taken = ledger.take("b1", "afkd-2", 10).unwrap();
        ledger.settle(&taken[..1]).unwrap();
        assert_eq!(
            ledger.take("b1", "afkd-2", 10).unwrap(),
            [entry("b1", 2, "afkd-2", 10)],
            "only the one left unsettled"
        );
        ledger.settle(&taken[1..]).unwrap();
        assert_eq!(ledger.entries().unwrap(), []);
        assert_eq!(ledger.take("b1", "afkd-2", 10).unwrap(), []);
        ledger.settle(&[]).unwrap();
    }

    /// Recording a card again replaces its entry, and `forget` drops the ones a caller could take
    /// and no live sibling's.
    #[test]
    fn a_card_is_owed_once_and_forget_drops_only_what_the_caller_could_take() {
        let dir = TempDir::new("forget");
        let ledger = Ledger::new(&dir.0);
        let key = entry("b1", 1, "", 0).key;
        ledger.record(entry("b1", 1, "afkd-2", 10)).unwrap();
        ledger.record(entry("b1", 1, "afkd-2", 11)).unwrap();
        assert_eq!(ledger.entries().unwrap(), [entry("b1", 1, "afkd-2", 11)]);
        ledger.forget("b1", &key, "afkd-2", 10).unwrap();
        assert_eq!(ledger.entries().unwrap().len(), 1, "a live sibling's stays");
        ledger.forget("b1", &key, "afkd-3", 10).unwrap();
        assert_eq!(ledger.entries().unwrap(), [], "another owner's goes");
    }

    /// Many threads record at once, each through a handle of its own as a child does, and none
    /// is lost; no half-written file is ever read.
    #[test]
    fn many_threads_recording_at_once_lose_none() {
        let dir = TempDir::new("threads");
        let ledger = Arc::new(Ledger::new(&dir.0));
        let writers: Vec<_> = (0..16u32)
            .map(|t| {
                let ledger = Arc::clone(&ledger);
                thread::spawn(move || {
                    for n in 0..8i64 {
                        let number = i64::from(t) * 100 + n;
                        ledger.record(entry("b1", number, "afkd-2", t)).unwrap();
                        ledger
                            .entries()
                            .expect("the file is whole whenever it is read");
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        let mut found = numbers(&ledger.entries().unwrap());
        found.sort_unstable();
        let mut want: Vec<i64> = (0..16i64)
            .flat_map(|t| (0..8).map(move |n| t * 100 + n))
            .collect();
        want.sort_unstable();
        assert_eq!(found, want);
        let left: Vec<String> = fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(left.is_empty(), "no temporary file is left: {left:?}");
    }

    /// A file that is no ledger is an error and is left as it is, not overwritten.
    #[test]
    fn a_corrupt_file_is_an_error_and_is_not_overwritten() {
        let dir = TempDir::new("corrupt");
        fs::write(dir.0.join(LEDGER_FILE), "not json").unwrap();
        let ledger = Ledger::new(&dir.0);
        assert!(ledger.record(entry("b1", 1, "afkd-2", 10)).is_err());
        assert!(ledger.take("b1", "afkd-2", 10).is_err());
        assert_eq!(
            fs::read_to_string(dir.0.join(LEDGER_FILE)).unwrap(),
            "not json"
        );
    }
}
