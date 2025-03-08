use std::io::Read;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

pub fn new_progress_bar(bar_length: Option<u64>) -> ProgressBar {
    let bar_style = (match bar_length {
        Some(_) => ProgressStyle::default_bar(),
        None => ProgressStyle::default_spinner(),
    })
    .template(
        "{percent:>3}%[{bar:60.cyan/blue}] {bytes:>7}/{total_bytes:7} {bytes_per_sec} {wide_msg}",
    )
    .expect("template should be correct")
    .progress_chars("#|-");

    let bar = ProgressBar::new(bar_length.unwrap_or(!0)).with_style(bar_style);
    bar.set_draw_target(ProgressDrawTarget::stderr_with_hz(5));
    bar.tick();
    bar
}

pub struct ProgressTrackingRead<R> {
    pub bar: ProgressBar,
    inner: R,
}

impl<R: Read> Read for ProgressTrackingRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let amt = match self.inner.read(buf) {
            Ok(v) => v,
            Err(e) => {
                self.bar
                    .abandon_with_message(format!("Failed to download: {}", e));
                return Err(e);
            }
        };
        self.bar.inc(amt as u64);
        Ok(amt)
    }
}

impl<R> Drop for ProgressTrackingRead<R> {
    fn drop(&mut self) {
        if !self.bar.is_finished() {
            self.bar.finish();
        }
    }
}

pub trait ProgressTrackable
where
    Self: Sized + Read,
{
    fn track_with(self, bar: ProgressBar) -> ProgressTrackingRead<Self> {
        ProgressTrackingRead { bar, inner: self }
    }
}

impl<R: Read> ProgressTrackable for R {}
