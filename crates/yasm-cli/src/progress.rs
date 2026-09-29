use std::io::IsTerminal;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

pub struct Progress {
    bar: Option<ProgressBar>,
}

impl Progress {
    pub fn start(message: impl Into<String>, quiet: bool) -> Self {
        if quiet || !std::io::stderr().is_terminal() {
            return Self { bar: None };
        }

        let bar = ProgressBar::new_spinner().with_message(message.into());
        bar.set_draw_target(ProgressDrawTarget::stderr());
        bar.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner()),
        );
        bar.enable_steady_tick(Duration::from_millis(100));
        Self { bar: Some(bar) }
    }

    pub fn set_message(&self, message: impl Into<String>) {
        if let Some(bar) = &self.bar {
            bar.set_message(message.into());
        }
    }

    pub fn warn(&self, message: impl Into<String>) {
        let message = format!("warning: {}", message.into());
        if let Some(bar) = &self.bar {
            bar.println(message);
        } else {
            eprintln!("{message}");
        }
    }

    pub fn finish_and_clear(&self) {
        if let Some(bar) = &self.bar {
            bar.finish_and_clear();
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        if let Some(bar) = &self.bar {
            bar.finish_and_clear();
        }
    }
}
