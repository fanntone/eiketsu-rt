//! BIOS behaviour that DOS programs rely on, checked by running tiny real-mode
//! programs through the real DOS loader.

use eiketsu_rt::machine::{Config, Machine, Stop};
use std::path::PathBuf;

fn run_com(name: &str, code: &[u8], ms: u64) -> Stop {
    let dir = std::env::temp_dir().join(format!("eiketsu-bios-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("T.COM"), code).unwrap();
    let cfg = Config {
        game_dir: PathBuf::from(&dir),
        headless: true,
        trace: false,
        journal: None,
        replay: None,
        max_steps: Some(ms * eiketsu_rt::IPS_PER_MS),
        script: Vec::new(),
        program: r"C:\T.COM".into(),
        audio: false,
        window: None,
        control: None,
        speed: 1,
        battle_log: None,
        debug_grid: false,
    };
    let stop = Machine::new(cfg).unwrap().run();
    std::fs::remove_dir_all(&dir).ok();
    stop
}

/// The BIOS timer handler saves DS/AX/DX around INT 1Ch, so a 1Ch hook that
/// changes DS does not disturb the interrupted code. MAIN.EXE's tick hook does
/// exactly that; without the save its DS leaked into the menu code, which then drew
/// a window from garbage coordinates and overwrote the interrupt table.
#[test]
fn int1c_hook_may_clobber_ds() {
    #[rustfmt::skip]
    let code = [
        0xb8, 0x1c, 0x25,             // 100 mov ax,251Ch
        0xba, 0x22, 0x01,             // 103 mov dx,handler
        0xcd, 0x21,                   // 106 int 21h
        0x8c, 0xdb,                   // 108 mov bx,ds
        0x2e, 0x80, 0x3e, 0x2e, 0x01, 0x00, // 10A cmp byte cs:[flag],0
        0x74, 0xf8,                   // 110 je 10A
        0x8c, 0xd8,                   // 112 mov ax,ds
        0x39, 0xd8,                   // 114 cmp ax,bx
        0x75, 0x05,                   // 116 jne bad
        0xb8, 0x00, 0x4c,             // 118 mov ax,4C00h
        0xcd, 0x21,                   // 11B int 21h
        0xb8, 0x01, 0x4c,             // 11D bad: mov ax,4C01h
        0xcd, 0x21,                   // 120 int 21h
        0x2e, 0xc6, 0x06, 0x2e, 0x01, 0x01, // 122 handler: mov byte cs:[flag],1
        0xb8, 0x34, 0x12,             // 128 mov ax,1234h
        0x8e, 0xd8,                   // 12B mov ds,ax
        0xcf,                         // 12D iret
        0x00,                         // 12E flag
    ];
    match run_com("1c", &code, 500) {
        Stop::Exit(0) => {}
        other => panic!("DS not preserved across INT 1Ch: {other:?}"),
    }
}
