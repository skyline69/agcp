//! Log file reader for TUI

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

use super::data::LogEntry;

/// Maximum number of log entries to keep in memory
const MAX_LOG_ENTRIES: usize = 1000;

/// Read the last N lines from a log file and find the server start line in a single pass
/// Returns (log entries, optional server start line)
pub fn read_last_lines_and_start(
    path: &Path,
    count: usize,
) -> (VecDeque<LogEntry>, Option<String>) {
    let mut entries = VecDeque::new();
    let mut server_start_line: Option<String> = None;

    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return (entries, None),
    };

    let reader = BufReader::new(file);
    let lines: Vec<String> = reader.lines().map_while(Result::ok).collect();

    // Search backwards for "Server listening" during the same pass
    for line in lines.iter().rev() {
        if line.contains("Server listening") {
            server_start_line = Some(line.clone());
            break;
        }
    }

    let start = lines.len().saturating_sub(count);
    for line in &lines[start..] {
        entries.push_back(LogEntry::new(line.clone()));
    }

    (entries, server_start_line)
}

/// Read new lines from a log file (for follow mode)
pub struct LogTailer {
    reader: Option<BufReader<File>>,
    path: std::path::PathBuf,
}

impl LogTailer {
    pub fn new(path: &Path) -> Self {
        let reader = File::open(path).ok().map(|mut f| {
            let _ = f.seek(SeekFrom::End(0));
            BufReader::new(f)
        });

        Self {
            reader,
            path: path.to_path_buf(),
        }
    }

    /// Read any new lines since last call
    pub fn read_new_lines(&mut self) -> Vec<LogEntry> {
        let mut entries = Vec::new();

        let reader = match &mut self.reader {
            Some(r) => r,
            None => {
                // Try to open the file if it wasn't available before
                if let Ok(mut f) = File::open(&self.path) {
                    let _ = f.seek(SeekFrom::End(0));
                    self.reader = Some(BufReader::new(f));
                }
                return entries;
            }
        };

        // Reuse a single buffer across iterations instead of allocating per line
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break, // No new data
                Ok(_) => {
                    let trimmed = line.trim_end();
                    if !trimmed.is_empty() {
                        entries.push(LogEntry::new(trimmed.to_string()));
                    }
                }
                Err(_) => break,
            }
        }

        entries
    }
}

/// Append entries to log buffer, respecting max size
pub fn append_entries(logs: &mut VecDeque<LogEntry>, new_entries: Vec<LogEntry>) {
    for entry in new_entries {
        logs.push_back(entry);
        if logs.len() > MAX_LOG_ENTRIES {
            logs.pop_front();
        }
    }
}
