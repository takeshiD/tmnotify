pub mod attention;

use std::io::{self, Stdout, stdout};

use crossterm::{
    cursor::{Hide, Show},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

/// Owns every terminal mode changed by an interactive view.
/// Drop is best-effort so errors and panics still attempt all cleanup.
pub struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut output = stdout();
        if let Err(error) = execute!(output, EnterAlternateScreen, Hide) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        match Terminal::new(CrosstermBackend::new(output)) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                let mut output = stdout();
                let _ = execute!(output, Show, LeaveAlternateScreen);
                let _ = disable_raw_mode();
                Err(error)
            }
        }
    }

    pub fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        let _ = execute!(self.terminal.backend_mut(), Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, panic::AssertUnwindSafe, rc::Rc};

    trait TerminalControl {
        fn enter(&mut self);
        fn restore(&mut self);
    }

    struct TestGuard<T: TerminalControl> {
        control: T,
    }

    impl<T: TerminalControl> TestGuard<T> {
        fn enter(mut control: T) -> Self {
            control.enter();
            Self { control }
        }
    }

    impl<T: TerminalControl> Drop for TestGuard<T> {
        fn drop(&mut self) {
            self.control.restore();
        }
    }

    struct FakeControl(Rc<RefCell<Vec<&'static str>>>);

    impl TerminalControl for FakeControl {
        fn enter(&mut self) {
            self.0.borrow_mut().push("enter");
        }
        fn restore(&mut self) {
            self.0.borrow_mut().push("restore");
        }
    }

    #[test]
    fn terminal_modes_are_restored_during_unwind() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let result = std::panic::catch_unwind(AssertUnwindSafe({
            let events = Rc::clone(&events);
            move || {
                let _guard = TestGuard::enter(FakeControl(events));
                panic!("simulated view panic");
            }
        }));
        assert!(result.is_err());
        assert_eq!(&*events.borrow(), &["enter", "restore"]);
    }
}
