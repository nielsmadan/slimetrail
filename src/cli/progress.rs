use slimetrail::{
    Observer, Progress, Rebuild, RefreshEvent,
    index::estimate::{count, duration, sessions, size, size_progress},
};
use std::{
    cell::Cell,
    io::Write,
    time::{Duration, Instant},
};

const REDRAW: Duration = Duration::from_millis(250);

pub(super) struct Stderr {
    waiting: bool,
    progress: bool,
    started: Cell<Option<Instant>>,
    shown: Cell<Option<Instant>>,
}

impl Stderr {
    pub(super) const fn new(waiting: bool, progress: bool) -> Self {
        Self {
            waiting,
            progress,
            started: Cell::new(None),
            shown: Cell::new(None),
        }
    }
}

impl Observer for Stderr {
    fn observe(&self, event: RefreshEvent) {
        match event {
            RefreshEvent::Waiting if self.waiting => {
                eprintln!("waiting for another slimetrail refresh…");
            }
            RefreshEvent::Rebuilding(rebuild) if self.progress && rebuild.total_entries > 0 => {
                let done = rebuild.entries >= rebuild.total_entries;
                if done || self.due() {
                    show_line(&rebuild_line(rebuild));
                }
                if done {
                    eprintln!();
                }
            }
            RefreshEvent::Started(_) => {
                self.started.set(Some(Instant::now()));
                self.shown.set(None);
            }
            RefreshEvent::Advanced(progress) if self.progress && progress.total_sessions > 0 => {
                if self.due() {
                    show_line(&progress_line(progress));
                }
            }
            RefreshEvent::Finished(progress) if self.progress && progress.total_sessions > 0 => {
                show_line(&progress_line(progress));
                let elapsed = self.started.get().map_or(0, |started| {
                    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
                });
                eprintln!();
                eprintln!(
                    "{}",
                    summary_line(progress.sessions, progress.bytes, elapsed)
                );
            }
            _ => {}
        }
    }
}

impl Stderr {
    fn due(&self) -> bool {
        let due = self
            .shown
            .get()
            .is_none_or(|shown| shown.elapsed() >= REDRAW);
        if due {
            self.shown.set(Some(Instant::now()));
        }
        due
    }
}

fn show_line(line: &str) {
    let mut stderr = std::io::stderr().lock();
    let _ = write!(stderr, "\r{line}");
    let _ = stderr.flush();
}

fn rebuild_line(rebuild: Rebuild) -> String {
    format!(
        "rebuilding search index: {}/{} entries",
        count(rebuild.entries),
        count(rebuild.total_entries)
    )
}

fn progress_line(progress: Progress) -> String {
    format!(
        "indexing: {}/{} sessions, {}",
        count(progress.sessions),
        count(progress.total_sessions),
        size_progress(progress.bytes, progress.total_bytes)
    )
}

fn summary_line(indexed: u64, bytes: u64, millis: u64) -> String {
    format!(
        "indexed {} ({} of logs) in {}",
        sessions(indexed),
        size(bytes),
        duration(millis)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_and_summary_lines_are_human_readable() {
        let line = |sessions, total_sessions, bytes, total_bytes| {
            progress_line(Progress {
                sessions,
                total_sessions,
                bytes,
                total_bytes,
            })
        };
        assert_eq!(
            line(1_204, 4_210, 800_000_000, 3_100_000_000),
            "indexing: 1,204/4,210 sessions, 0.8/3.1 GB"
        );
        assert_eq!(line(0, 3, 0, 52_400), "indexing: 0/3 sessions, 0.0/52.4 KB");
        assert_eq!(
            summary_line(4_210, 3_100_000_000, 125_000),
            "indexed 4,210 sessions (3.1 GB of logs) in 3 min"
        );
        assert_eq!(
            summary_line(1, 200, 5),
            "indexed 1 session (0.2 KB of logs) in 1 s"
        );
        assert_eq!(
            rebuild_line(Rebuild {
                entries: 120_000,
                total_entries: 1_260_000
            }),
            "rebuilding search index: 120,000/1,260,000 entries"
        );
    }
}
