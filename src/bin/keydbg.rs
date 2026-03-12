// tiny key event debugger: run with `cargo run --bin keydbg`
// press keys to see what crossterm reports, ctrl+c to exit

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers,
            KeyboardEnhancementFlags, PushKeyboardEnhancementFlags, PopKeyboardEnhancementFlags},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use std::io;

fn main() -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    let enhanced = execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    eprintln!("kitty protocol: {}\r", if enhanced.is_ok() { "enabled" } else { "not supported" });
    eprintln!("press keys to inspect, ctrl+c to exit\r");

    loop {
        if let Event::Key(key) = event::read()? {
            let mods = key.modifiers;
            eprintln!(
                "code={:?}  mods={:?}  shift={}  alt={}  ctrl={}  super={}\r",
                key.code,
                mods,
                mods.contains(KeyModifiers::SHIFT),
                mods.contains(KeyModifiers::ALT),
                mods.contains(KeyModifiers::CONTROL),
                mods.contains(KeyModifiers::SUPER),
            );
            if mods.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                break;
            }
        }
    }

    let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    disable_raw_mode()?;
    Ok(())
}
