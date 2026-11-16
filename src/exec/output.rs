//! Output during a switch (SCOPE 2.2, N9): every write drops its error, so a closed terminal (EIO, EPIPE) never stops a commit.
use std::io::Write;

fn write_dropping(to: &mut dyn Write, text: &str, newline: bool) {
    let _ = to.write_all(text.as_bytes());
    if newline && !text.ends_with('\n') {
        let _ = to.write_all(b"\n");
    }
    let _ = to.flush();
}

/// A line on stdout; a failed write is dropped.
pub fn say(text: &str) {
    write_dropping(&mut std::io::stdout().lock(), text, true);
}

/// A line on stderr; a failed write is dropped.
pub fn warn(text: &str) {
    write_dropping(&mut std::io::stderr().lock(), text, true);
}

/// Text on stdout as it is.
pub fn raw(text: &str) {
    write_dropping(&mut std::io::stdout().lock(), text, false);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Hungup(usize);

    impl Write for Hungup {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            self.0 += 1;
            Err(std::io::Error::from_raw_os_error(libc::EIO))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::from_raw_os_error(libc::EPIPE))
        }
    }

    #[test]
    fn a_hung_up_terminal_is_written_to_without_failing_or_panicking() {
        let mut tty = Hungup(0);
        write_dropping(&mut tty, "installing glibc", true);
        write_dropping(&mut tty, "x\n", false);
        assert!(tty.0 >= 2, "every line was attempted");
    }
}
