#![allow(
  clippy::float_cmp,
  clippy::cast_possible_truncation,
  clippy::cast_precision_loss
)]

//! This module is meant to provide some developer tooling that will help monitor where our
//! processes are spending their time.

use std::fmt::Write;

/// This type is used to track the time spent executing method calls.
#[derive(Debug)]
pub struct Stats {
  /// When the current time exceeds some duration threshold since this value, we will print and
  /// clear our call history.
  last_print: std::time::Instant,
  /// The storage of our calls. The user provides a label, and we will manage the count and the
  /// cumulative total ms spent performing those calls.
  calls: std::collections::HashMap<&'static str, (u16, u128)>,
  /// A label used to provide more contextual logging.
  name: &'static str,
}

impl Default for Stats {
  fn default() -> Self {
    Self {
      name: "",
      last_print: std::time::Instant::now(),
      calls: std::collections::HashMap::default(),
    }
  }
}

impl Stats {
  /// Sets the contextual logging label.
  pub fn rename(mut self, name: &'static str) -> Self {
    self.name = name;
    self
  }

  /// Takes the current instant, returning a function that will update our call map when it is
  /// executed.
  pub fn take(&mut self, label: &'static str) -> impl FnOnce() + '_ {
    let start = std::time::Instant::now();
    move || {
      let (ref mut count, ref mut duration) = self.calls.entry(label).or_insert((0, 0));
      let call_duration = std::time::Instant::now().duration_since(start).as_millis();
      *count += 1;
      *duration += call_duration;
      self.print_calls();
    }
  }

  /// Checks the time, and will print + clear our call history if the duration since our last time
  /// has exceeded a threshold.
  fn print_calls(&mut self) {
    let before = std::time::Instant::now();
    if before.duration_since(self.last_print).as_millis() < 1000 {
      return;
    }

    let mut calls = String::default();
    for (label, (count, duration)) in &self.calls {
      #[allow(clippy::cast_lossless)]
      let average_ms = (*duration as f64 / *count as f64) / 1000f64;
      let _ = writeln!(&mut calls, "\t{label: >30} = {average_ms:.3} ({count} counts)]");
    }
    log::info!("{}\n{calls}", self.name);
    self.calls.clear();
    self.last_print = before;
  }

  /// Given some label and a function, we will update our call history for that label, calling the
  /// function + recording the time spend during that call.
  pub fn increment<F, B>(&mut self, label: &'static str, function: F) -> B
  where
    F: FnOnce() -> B,
  {
    let before = std::time::Instant::now();
    let output = function();
    let duration = std::time::Instant::now().duration_since(before).as_micros();
    let (ref mut count, ref mut value) = self.calls.entry(label).or_insert((0, 0));
    *count += 1;
    *value += duration;

    self.print_calls();

    output
  }
}
