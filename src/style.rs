//! Minimal ANSI styling that is off for NO_COLOR, TERM=dumb and when neither stdout nor stderr is a terminal.
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

static ON: AtomicBool = AtomicBool::new(false);

pub fn init() {
    let tty = std::io::stdout().is_terminal() || std::io::stderr().is_terminal();
    let off = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) || std::env::var("TERM").is_ok_and(|t| t == "dumb");
    ON.store(tty && !off, Ordering::Relaxed);
}

fn wrap(s: &dyn std::fmt::Display, code: &str) -> String {
    if ON.load(Ordering::Relaxed) { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
}

pub trait Paint: std::fmt::Display + Sized {
    fn dimmed(&self) -> String { wrap(self, "2") }
    fn italic(&self) -> String { wrap(self, "3") }
    fn bold(&self) -> String { wrap(self, "1") }
    fn red(&self) -> String { wrap(self, "31") }
    fn green(&self) -> String { wrap(self, "32") }
    fn yellow(&self) -> String { wrap(self, "33") }
    fn cyan(&self) -> String { wrap(self, "36") }
}
impl<T: std::fmt::Display> Paint for T {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plain_when_off_and_escaped_when_on() {
        ON.store(false, Ordering::Relaxed);
        assert_eq!("x".dimmed(), "x");
        ON.store(true, Ordering::Relaxed);
        assert_eq!("x".dimmed().italic(), "\x1b[3m\x1b[2mx\x1b[0m\x1b[0m");
        assert_eq!(String::from("y").red(), "\x1b[31my\x1b[0m");
        ON.store(false, Ordering::Relaxed);
    }
}
