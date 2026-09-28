//! Process-wide diagnostics log: a bounded in-memory ring of lines mirrored to
//! `ferropipe.log` in the state directory, and a `snapshot` of both for the
//! "Copy diagnostics" button. Zero dependencies beyond `chrono` for the clock.
//!
//! Never log secrets: passwords, passphrases, decrypted vault material or the
//! console's command lines. Host names, targets and error messages only.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

/// Lines kept in memory for `snapshot`.
const RING_CAP: usize = 500;
/// A log file larger than this at startup is rotated to `.1` first.
const ROTATE_BYTES: u64 = 1024 * 1024;
/// The log file's name inside the state directory.
const FILE_NAME: &str = "ferropipe.log";
/// Separator that replaces line breaks so every entry stays on one line.
const NEWLINE_SUBST: &str = " | ";

static LOG: OnceLock<Mutex<Diag>> = OnceLock::new();

/// Severity of a log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    fn tag(self) -> &'static str {
        match self {
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }
}

/// The log: its recent lines and, when one could be opened, the file they
/// are also appended to.
pub struct Diag {
    ring: VecDeque<String>,
    file: Option<BufWriter<File>>,
    /// Where `file` lives; `None` when logging is in-memory only.
    path: Option<PathBuf>,
}

impl Diag {
    /// A log that keeps lines in memory only.
    fn ring_only() -> Diag {
        Diag {
            ring: VecDeque::with_capacity(RING_CAP),
            file: None,
            path: None,
        }
    }

    /// A log mirrored to `<dir>/ferropipe.log` (created private, rotated when
    /// oversized). Any failure is reported on stderr once and the log stays
    /// in-memory only; nothing here panics.
    pub fn new_in(dir: &Path) -> Diag {
        let path = dir.join(FILE_NAME);
        let mut diag = Diag::ring_only();
        match open_log_file(&path) {
            Ok(file) => {
                diag.file = Some(file);
                diag.path = Some(path);
            }
            Err(e) => eprintln!(
                "diag: cannot open {}: {e}; keeping diagnostics in memory only",
                path.display()
            ),
        }
        diag
    }

    /// Remember `line`, evicting the oldest once the ring is full, and
    /// append it to the file. A write failure drops the file (reported once)
    /// rather than failing every later line.
    fn push(&mut self, line: String) {
        if let Some(file) = self.file.as_mut()
            && let Err(e) = writeln!(file, "{line}").and_then(|()| file.flush())
        {
            eprintln!("diag: cannot write log file: {e}; continuing in memory only");
            self.file = None;
        }
        if self.ring.len() == RING_CAP {
            self.ring.pop_front();
        }
        self.ring.push_back(line);
    }

    /// The header followed by every remembered line, one per row.
    fn snapshot_text(&self) -> String {
        let mut out = header(self.path.as_deref());
        for line in &self.ring {
            out.push_str(line);
            out.push('\n');
        }
        out
    }
}

/// Create `path`'s directory, rotate an oversized log aside and open the
/// file for private (0600) appending.
fn open_log_file(path: &Path) -> io::Result<BufWriter<File>> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    rotate_if_large(path)?;
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(path)?;
    #[cfg(unix)]
    {
        // Tighten a file that already existed with wider permissions.
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(BufWriter::new(file))
}

/// Move `path` to `path.1` when it is over `ROTATE_BYTES`; no file is fine.
fn rotate_if_large(path: &Path) -> io::Result<()> {
    let Ok(meta) = fs::metadata(path) else {
        return Ok(());
    };
    if meta.len() > ROTATE_BYTES {
        fs::rename(path, rotated_path(path))?;
    }
    Ok(())
}

/// `ferropipe.log` -> `ferropipe.log.1`.
fn rotated_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(ToOwned::to_owned).unwrap_or_default();
    name.push(".1");
    path.with_file_name(name)
}

/// The snapshot's preamble: build, platform and where the file is.
fn header(path: Option<&Path>) -> String {
    let file = path.map_or_else(|| "(none)".to_string(), |p| p.display().to_string());
    format!(
        "Ferropipe {} ({} {})\nlog file: {file}\n---\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

/// `2026-09-28T12:34:56Z`: the current UTC time to the second.
fn utc_timestamp() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// One log line: timestamp, level, area and the message flattened onto one
/// line, separated by two spaces.
fn format_line(timestamp: &str, level: Level, area: &str, msg: &str) -> String {
    let flat = msg
        .replace("\r\n", NEWLINE_SUBST)
        .replace(['\n', '\r'], NEWLINE_SUBST);
    format!("{timestamp}  {}  {area}  {flat}", level.tag())
}

/// The process-wide log, in-memory only until `init` attaches a file.
fn lock() -> MutexGuard<'static, Diag> {
    LOG.get_or_init(|| Mutex::new(Diag::ring_only()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Start mirroring the log to `<state_dir>/ferropipe.log`. Lines logged
/// before this stay in memory. Never panics.
pub fn init(state_dir: &Path) {
    let opened = Diag::new_in(state_dir);
    let mut diag = lock();
    diag.file = opened.file;
    diag.path = opened.path;
}

/// Record `msg` under `area` at `level`.
pub fn log(level: Level, area: &str, msg: impl AsRef<str>) {
    let line = format_line(&utc_timestamp(), level, area, msg.as_ref());
    lock().push(line);
}

/// Header plus the most recent lines, ready for the clipboard.
pub fn snapshot() -> String {
    lock().snapshot_text()
}

/// Where the log file is, when one is open.
pub fn log_path() -> Option<PathBuf> {
    lock().path.clone()
}

/// `diag_info!(area, fmt, args...)`: an `Info` line.
macro_rules! diag_info {
    ($area:expr, $($arg:tt)*) => {
        $crate::diag::log($crate::diag::Level::Info, $area, format!($($arg)*))
    };
}

/// `diag_warn!(area, fmt, args...)`: a `Warn` line.
macro_rules! diag_warn {
    ($area:expr, $($arg:tt)*) => {
        $crate::diag::log($crate::diag::Level::Warn, $area, format!($($arg)*))
    };
}

/// `diag_error!(area, fmt, args...)`: an `Error` line.
macro_rules! diag_error {
    ($area:expr, $($arg:tt)*) => {
        $crate::diag::log($crate::diag::Level::Error, $area, format!($($arg)*))
    };
}

pub(crate) use {diag_error, diag_info, diag_warn};

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &std::path::Path) -> String {
        std::fs::read_to_string(path).expect("read log")
    }

    // -- format_line -------------------------------------------------------

    #[test]
    fn format_line_is_timestamp_level_area_message_separated_by_two_spaces() {
        let line = format_line("2026-09-28T12:34:56Z", Level::Warn, "smb", "message");
        assert_eq!(line, "2026-09-28T12:34:56Z  WARN  smb  message");
    }

    #[test]
    fn format_line_spells_out_each_level() {
        assert!(format_line("t", Level::Info, "a", "m").contains("  INFO  "));
        assert!(format_line("t", Level::Warn, "a", "m").contains("  WARN  "));
        assert!(format_line("t", Level::Error, "a", "m").contains("  ERROR  "));
    }

    #[test]
    fn format_line_flattens_newlines_into_pipes() {
        // Arrange: a multi-line error with both line-ending styles.
        let msg = "first\nsecond\r\nthird";
        // Act
        let line = format_line("t", Level::Error, "sftp", msg);
        // Assert
        assert_eq!(line, "t  ERROR  sftp  first | second | third");
        assert!(!line.contains('\n'));
    }

    #[test]
    fn utc_timestamp_is_iso_8601_with_a_z_suffix() {
        let ts = utc_timestamp();
        assert_eq!(ts.len(), "2026-09-28T12:34:56Z".len());
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[10..11], "T");
    }

    // -- ring --------------------------------------------------------------

    #[test]
    fn ring_evicts_the_oldest_line_once_full() {
        // Arrange
        let mut d = Diag::ring_only();
        // Act
        for i in 0..(RING_CAP + 3) {
            d.push(format!("line {i}"));
        }
        // Assert
        assert_eq!(d.ring.len(), RING_CAP);
        assert_eq!(d.ring.front().map(String::as_str), Some("line 3"));
        assert_eq!(
            d.ring.back().cloned(),
            Some(format!("line {}", RING_CAP + 2))
        );
    }

    #[test]
    fn snapshot_text_starts_with_the_header_and_lists_every_ring_line() {
        // Arrange
        let mut d = Diag::ring_only();
        d.push("one".into());
        d.push("two".into());
        // Act
        let text = d.snapshot_text();
        // Assert
        assert!(text.starts_with(&format!("Ferropipe {}", env!("CARGO_PKG_VERSION"))));
        assert!(text.contains(std::env::consts::OS));
        assert!(text.contains("log file: (none)"));
        assert!(text.ends_with("one\ntwo\n"));
    }

    // -- file --------------------------------------------------------------

    #[test]
    fn new_in_creates_the_log_file_and_appends_pushed_lines() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("state/ferropipe");
        let mut d = Diag::new_in(&nested);
        // Act
        d.push("hello".into());
        d.push("world".into());
        // Assert
        let path = nested.join(FILE_NAME);
        assert_eq!(d.path.as_deref(), Some(path.as_path()));
        assert_eq!(read(&path), "hello\nworld\n");
    }

    #[cfg(unix)]
    #[test]
    fn new_in_makes_the_log_file_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let _d = Diag::new_in(dir.path());
        let mode = std::fs::metadata(dir.path().join(FILE_NAME))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn new_in_rotates_a_log_over_the_limit_and_starts_a_fresh_one() {
        // Arrange: an oversized existing log.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, vec![b'x'; ROTATE_BYTES as usize + 1]).expect("write big log");
        // Act
        let mut d = Diag::new_in(dir.path());
        d.push("fresh".into());
        // Assert
        let rotated = dir.path().join(format!("{FILE_NAME}.1"));
        assert_eq!(
            std::fs::metadata(&rotated).expect("rotated").len(),
            ROTATE_BYTES + 1
        );
        assert_eq!(read(&path), "fresh\n");
    }

    #[test]
    fn new_in_keeps_a_log_under_the_limit_and_appends_to_it() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "old\n").expect("write small log");
        // Act
        let mut d = Diag::new_in(dir.path());
        d.push("new".into());
        // Assert
        assert!(!dir.path().join(format!("{FILE_NAME}.1")).exists());
        assert_eq!(read(&path), "old\nnew\n");
    }

    #[test]
    fn new_in_falls_back_to_ring_only_when_the_directory_cannot_be_created() {
        // Arrange: a regular file where the directory should go.
        let dir = tempfile::tempdir().expect("tempdir");
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, "").expect("write blocker");
        // Act
        let mut d = Diag::new_in(&blocker);
        d.push("still logged".into());
        // Assert
        assert!(d.file.is_none());
        assert!(d.path.is_none());
        assert_eq!(d.ring.len(), 1);
    }

    #[test]
    fn rotated_path_appends_dot_one_to_the_file_name() {
        let p = std::path::Path::new("/var/state/ferropipe.log");
        assert_eq!(
            rotated_path(p),
            std::path::PathBuf::from("/var/state/ferropipe.log.1")
        );
    }
}
