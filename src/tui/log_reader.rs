//! Log file reader for TUI

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use super::data::LogEntry;

/// Maximum number of log entries to keep in memory
const MAX_LOG_ENTRIES: usize = 1000;

/// Size of chunks to read backwards from end of file
const READ_CHUNK_SIZE: u64 = 64 * 1024; // 64KB

/// Read the last N lines from a log file by seeking from the end,
/// and find the last "Server listening" line.
/// Returns (log entries, optional server start line)
pub fn read_last_lines_and_start(
    path: &Path,
    count: usize,
) -> (VecDeque<LogEntry>, Option<String>) {
    let mut entries = VecDeque::new();
    let mut server_start_line: Option<String> = None;

    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return (entries, None),
    };

    let file_len = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return (entries, None),
    };

    if file_len == 0 {
        return (entries, None);
    }

    // Read backwards from end in chunks to find enough lines
    // We need `count` lines for display + extra to find "Server listening"
    let mut collected_lines: Vec<String> = Vec::new();
    let mut remaining = file_len;

    loop {
        let chunk_size = remaining.min(READ_CHUNK_SIZE);
        let offset = remaining - chunk_size;
        let _ = file.seek(SeekFrom::Start(offset));

        let mut buf = vec![0u8; chunk_size as usize];
        if file.read_exact(&mut buf).is_err() {
            break;
        }

        // Convert to string and split into lines
        let chunk_str = String::from_utf8_lossy(&buf);
        let mut chunk_lines: Vec<String> = chunk_str.lines().map(String::from).collect();

        // If we're not at the start of the file, the first line may be partial
        if offset > 0 && !chunk_lines.is_empty() {
            // Prepend partial first line to the previously collected last line
            let partial = chunk_lines.remove(0);
            if let Some(last) = collected_lines.last_mut() {
                *last = format!("{}{}", partial, last);
            }
            // If no collected lines yet, discard the partial line
        }

        // Prepend chunk lines (they're earlier in the file)
        chunk_lines.append(&mut collected_lines);
        collected_lines = chunk_lines;

        remaining = offset;

        // Stop if we have enough lines or reached start of file
        // Need extra lines beyond `count` to search for "Server listening"
        if collected_lines.len() > count + 200 || remaining == 0 {
            break;
        }
    }

    // Search backwards for "Server listening"
    for line in collected_lines.iter().rev() {
        if line.contains("Server listening") {
            server_start_line = Some(line.clone());
            break;
        }
    }

    // Take the last `count` lines
    let start = collected_lines.len().saturating_sub(count);
    for line in &collected_lines[start..] {
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
