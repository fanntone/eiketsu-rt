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
