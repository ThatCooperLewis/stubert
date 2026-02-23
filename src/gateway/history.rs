use chrono::{DateTime, Local, NaiveDate};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct HistoryWriter {
    base_dir: PathBuf,
}

pub struct SearchResult {
    pub date: String,
    pub line_number: usize,
    pub context: Vec<String>,
}

impl HistoryWriter {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    pub fn write(&self, platform: &str, role: &str, text: &str) {
        self.write_at(platform, role, text, Local::now());
    }

    fn write_at(&self, platform: &str, role: &str, text: &str, timestamp: DateTime<Local>) {
        if let Err(e) = self.try_write(platform, role, text, timestamp) {
            tracing::warn!("Failed to write history: {e}");
        }
    }

    fn try_write(
        &self,
        platform: &str,
        role: &str,
        text: &str,
        timestamp: DateTime<Local>,
    ) -> std::io::Result<()> {
        fs::create_dir_all(&self.base_dir)?;
        let path = self.history_path(platform, &timestamp.date_naive());
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let entry = format!(
            "[{}] {}: {}\n",
            timestamp.format("%Y-%m-%d %H:%M:%S"),
            role,
            text
        );
        file.write_all(entry.as_bytes())
    }

    fn history_path(&self, platform: &str, date: &NaiveDate) -> PathBuf {
        self.base_dir
            .join(format!("{}-{}.md", date.format("%Y-%m-%d"), platform))
    }

    pub fn search(&self, platform: &str, query: &str, max_results: usize) -> Vec<SearchResult> {
        if query.is_empty() {
            return Vec::new();
        }

        let mut results = Vec::new();
        let suffix = format!("-{}.md", platform);

        let entries = match fs::read_dir(&self.base_dir) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!("Failed to read history directory: {e}");
                return Vec::new();
            }
        };

        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.ends_with(&suffix))
                    .unwrap_or(false)
            })
            .collect();

        files.sort();

        let query_lower = query.to_lowercase();

        for file_path in files {
            if results.len() >= max_results {
                break;
            }

            let date = Self::date_from_filename(&file_path).unwrap_or_default();

            let content = match fs::read_to_string(&file_path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("Failed to read history file {}: {e}", file_path.display());
                    continue;
                }
            };

            let lines: Vec<&str> = content.lines().collect();

            for (i, line) in lines.iter().enumerate() {
                if results.len() >= max_results {
                    break;
                }

                if line.to_lowercase().contains(&query_lower) {
                    let prev = if i > 0 {
                        lines[i - 1].to_string()
                    } else {
                        String::new()
                    };
                    let next = if i + 1 < lines.len() {
                        lines[i + 1].to_string()
                    } else {
                        String::new()
                    };
                    let context = vec![prev, line.to_string(), next];

                    results.push(SearchResult {
                        date: date.clone(),
                        line_number: i + 1,
                        context,
                    });
                }
            }
        }

        results
    }

    fn date_from_filename(path: &Path) -> Option<String> {
        let name = path.file_stem()?.to_str()?;
        // Filename format: "YYYY-MM-DD-platform", date is first 10 chars
        if name.len() >= 10 {
            Some(name[..10].to_string())
        } else {
            None
        }
    }
}

#[cfg(test)]
fn make_timestamp(year: i32, month: u32, day: u32, h: u32, m: u32, s: u32) -> DateTime<Local> {
    use chrono::TimeZone;
    Local
        .with_ymd_and_hms(year, month, day, h, m, s)
        .unwrap()
}

#[cfg(test)]
mod test_write {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn creates_file_with_correct_name_and_format() {
        let tmp = TempDir::new().unwrap();
        let writer = HistoryWriter::new(tmp.path().to_path_buf());
        let ts = make_timestamp(2026, 2, 23, 14, 30, 0);

        writer.write_at("telegram", "user", "hello", ts);

        let path = tmp.path().join("2026-02-23-telegram.md");
        assert!(path.exists());
        let content = fs::read_to_string(path).unwrap();
        assert_eq!(content, "[2026-02-23 14:30:00] user: hello\n");
    }

    #[test]
    fn multiple_writes_append_to_same_file() {
        let tmp = TempDir::new().unwrap();
        let writer = HistoryWriter::new(tmp.path().to_path_buf());
        let ts1 = make_timestamp(2026, 2, 23, 14, 30, 0);
        let ts2 = make_timestamp(2026, 2, 23, 14, 31, 0);

        writer.write_at("telegram", "user", "hello", ts1);
        writer.write_at("telegram", "assistant", "hi there", ts2);

        let path = tmp.path().join("2026-02-23-telegram.md");
        let content = fs::read_to_string(path).unwrap();
        assert_eq!(
            content,
            "[2026-02-23 14:30:00] user: hello\n[2026-02-23 14:31:00] assistant: hi there\n"
        );
    }

    #[test]
    fn date_rollover_creates_new_file() {
        let tmp = TempDir::new().unwrap();
        let writer = HistoryWriter::new(tmp.path().to_path_buf());
        let day1 = make_timestamp(2026, 2, 23, 23, 59, 0);
        let day2 = make_timestamp(2026, 2, 24, 0, 1, 0);

        writer.write_at("telegram", "user", "late night", day1);
        writer.write_at("telegram", "user", "early morning", day2);

        assert!(tmp.path().join("2026-02-23-telegram.md").exists());
        assert!(tmp.path().join("2026-02-24-telegram.md").exists());

        let content1 = fs::read_to_string(tmp.path().join("2026-02-23-telegram.md")).unwrap();
        assert!(content1.contains("late night"));

        let content2 = fs::read_to_string(tmp.path().join("2026-02-24-telegram.md")).unwrap();
        assert!(content2.contains("early morning"));
    }
}

#[cfg(test)]
mod test_search {
    use super::*;
    use tempfile::TempDir;

    fn write_entries(writer: &HistoryWriter, ts: DateTime<Local>, entries: &[(&str, &str)]) {
        for (i, (role, text)) in entries.iter().enumerate() {
            let offset_ts = ts + chrono::Duration::minutes(i as i64);
            writer.write_at("telegram", role, text, offset_ts);
        }
    }

    #[test]
    fn finds_substring_matches_case_insensitive() {
        let tmp = TempDir::new().unwrap();
        let writer = HistoryWriter::new(tmp.path().to_path_buf());
        let ts = make_timestamp(2026, 2, 23, 14, 0, 0);

        writer.write_at("telegram", "user", "Hello World", ts);
        writer.write_at(
            "telegram",
            "assistant",
            "HELLO back",
            ts + chrono::Duration::minutes(1),
        );
        writer.write_at(
            "telegram",
            "user",
            "goodbye",
            ts + chrono::Duration::minutes(2),
        );

        let results = writer.search("telegram", "hello", 20);
        assert_eq!(results.len(), 2);
        assert!(results[0].context.iter().any(|l| l.contains("Hello World")));
        assert!(results[1].context.iter().any(|l| l.contains("HELLO back")));
    }

    #[test]
    fn returns_context_lines() {
        let tmp = TempDir::new().unwrap();
        let writer = HistoryWriter::new(tmp.path().to_path_buf());
        let ts = make_timestamp(2026, 2, 23, 14, 0, 0);

        let entries = vec![
            ("user", "first message"),
            ("assistant", "second message"),
            ("user", "TARGET message"),
            ("assistant", "fourth message"),
            ("user", "fifth message"),
        ];
        write_entries(&writer, ts, &entries);

        let results = writer.search("telegram", "TARGET", 20);
        assert_eq!(results.len(), 1);

        let ctx = &results[0].context;
        assert_eq!(ctx.len(), 3);
        assert!(ctx[0].contains("second message"));
        assert!(ctx[1].contains("TARGET message"));
        assert!(ctx[2].contains("fourth message"));
        assert_eq!(results[0].line_number, 3);
    }

    #[test]
    fn respects_max_results_cap() {
        let tmp = TempDir::new().unwrap();
        let writer = HistoryWriter::new(tmp.path().to_path_buf());
        let ts = make_timestamp(2026, 2, 23, 14, 0, 0);

        for i in 0..25 {
            let offset_ts = ts + chrono::Duration::minutes(i);
            writer.write_at("telegram", "user", &format!("match item {i}"), offset_ts);
        }

        let results = writer.search("telegram", "match", 20);
        assert_eq!(results.len(), 20);
    }

    #[test]
    fn searches_across_multiple_date_files() {
        let tmp = TempDir::new().unwrap();
        let writer = HistoryWriter::new(tmp.path().to_path_buf());
        let day1 = make_timestamp(2026, 2, 22, 10, 0, 0);
        let day2 = make_timestamp(2026, 2, 23, 10, 0, 0);

        writer.write_at("telegram", "user", "findme on day one", day1);
        writer.write_at("telegram", "user", "findme on day two", day2);

        let results = writer.search("telegram", "findme", 20);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].date, "2026-02-22");
        assert_eq!(results[1].date, "2026-02-23");
    }

    #[test]
    fn empty_query_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let writer = HistoryWriter::new(tmp.path().to_path_buf());
        let ts = make_timestamp(2026, 2, 23, 14, 0, 0);

        writer.write_at("telegram", "user", "some text", ts);

        let results = writer.search("telegram", "", 20);
        assert!(results.is_empty());
    }

    #[test]
    fn no_matches_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let writer = HistoryWriter::new(tmp.path().to_path_buf());
        let ts = make_timestamp(2026, 2, 23, 14, 0, 0);

        writer.write_at("telegram", "user", "hello world", ts);

        let results = writer.search("telegram", "xyzzyx", 20);
        assert!(results.is_empty());
    }

    #[test]
    fn context_at_first_line() {
        let tmp = TempDir::new().unwrap();
        let writer = HistoryWriter::new(tmp.path().to_path_buf());
        let ts = make_timestamp(2026, 2, 23, 14, 0, 0);

        let entries = vec![
            ("user", "FIRST target"),
            ("assistant", "second line"),
            ("user", "third line"),
        ];
        write_entries(&writer, ts, &entries);

        let results = writer.search("telegram", "FIRST", 20);
        assert_eq!(results.len(), 1);
        let ctx = &results[0].context;
        assert_eq!(ctx.len(), 3);
        assert_eq!(ctx[0], ""); // no previous line
        assert!(ctx[1].contains("FIRST target"));
        assert!(ctx[2].contains("second line"));
    }

    #[test]
    fn context_at_last_line() {
        let tmp = TempDir::new().unwrap();
        let writer = HistoryWriter::new(tmp.path().to_path_buf());
        let ts = make_timestamp(2026, 2, 23, 14, 0, 0);

        let entries = vec![
            ("user", "first line"),
            ("assistant", "second line"),
            ("user", "LAST target"),
        ];
        write_entries(&writer, ts, &entries);

        let results = writer.search("telegram", "LAST", 20);
        assert_eq!(results.len(), 1);
        let ctx = &results[0].context;
        assert_eq!(ctx.len(), 3);
        assert!(ctx[0].contains("second line"));
        assert!(ctx[1].contains("LAST target"));
        assert_eq!(ctx[2], ""); // no next line
    }

    #[test]
    fn context_single_line_file() {
        let tmp = TempDir::new().unwrap();
        let writer = HistoryWriter::new(tmp.path().to_path_buf());
        let ts = make_timestamp(2026, 2, 23, 14, 0, 0);

        writer.write_at("telegram", "user", "only line", ts);

        let results = writer.search("telegram", "only", 20);
        assert_eq!(results.len(), 1);
        let ctx = &results[0].context;
        assert_eq!(ctx.len(), 3);
        assert_eq!(ctx[0], "");
        assert!(ctx[1].contains("only line"));
        assert_eq!(ctx[2], "");
    }

    #[test]
    fn search_nonexistent_directory() {
        let tmp = TempDir::new().unwrap();
        let writer = HistoryWriter::new(tmp.path().join("does_not_exist"));

        let results = writer.search("telegram", "anything", 20);
        assert!(results.is_empty());
    }

    #[test]
    fn search_filters_by_platform() {
        let tmp = TempDir::new().unwrap();
        let writer = HistoryWriter::new(tmp.path().to_path_buf());
        let ts = make_timestamp(2026, 2, 23, 14, 0, 0);

        writer.write_at("telegram", "user", "shared keyword", ts);
        writer.write_at(
            "discord",
            "user",
            "shared keyword",
            ts + chrono::Duration::minutes(1),
        );

        let tg_results = writer.search("telegram", "shared", 20);
        assert_eq!(tg_results.len(), 1);

        let dc_results = writer.search("discord", "shared", 20);
        assert_eq!(dc_results.len(), 1);
    }
}

#[cfg(test)]
mod test_error_handling {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_failure_does_not_propagate() {
        let tmp = TempDir::new().unwrap();
        // Point to a path inside a non-existent, unwritable location
        let bad_path = tmp.path().join("nonexistent").join("deeply").join("nested");

        // Create the parent as a file (not directory) to make the write fail
        fs::create_dir_all(tmp.path().join("nonexistent").join("deeply")).unwrap();
        fs::write(&bad_path, "I am a file, not a directory").unwrap();

        let writer = HistoryWriter::new(bad_path);

        // This should not panic — write failures are logged, not propagated
        writer.write("telegram", "user", "this should fail silently");
    }
}
