pub mod audio;
pub mod autoplay;
pub mod battle;
pub mod control;
pub mod cpu;
pub mod dos;
pub mod host;
pub mod journal;
pub mod keys;
pub mod machine;
pub mod mem;
pub mod opl;
pub mod png;
pub mod script;
pub mod ser;
pub mod saveui;
pub mod vga;

/// Emulated instructions per millisecond (the clock of everything: PIT, retrace,
/// OPL timers, journal timestamps).
pub const IPS_PER_MS: u64 = 25_000;

/// Counts writes to the game folder (DOS writes, creates, deletes, renames and
/// the save-state files), so a host that keeps the folder elsewhere (the web
/// build's browser storage) knows when to persist it.
pub static FS_WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn note_fs_write() {
    FS_WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Replace a file's contents through a temporary file and a rename, so a failed
/// write never leaves half a file; directly where the file system cannot rename
/// (the web build's in-memory one may not).
pub fn replace_file(p: &std::path::Path, b: &[u8]) -> Result<(), String> {
    note_fs_write();
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, b).and_then(|_| std::fs::rename(&tmp, p)).is_ok() {
        return Ok(());
    }
    std::fs::remove_file(&tmp).ok();
    std::fs::write(p, b).map_err(|e| format!("{}: {e}", p.display()))
}
