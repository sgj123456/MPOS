//! The sweep's own report: what it is doing while it runs, and what it did when
//! it stopped.
//!
//! §15.94 made the gate fast enough that its *output* became the bottleneck for
//! a person. The command printed a line per lane, and nothing that said in one
//! place how long each stage took, which lane carried the slow program, what the
//! slowest programs were, or whether this run was better or worse than the last
//! one - the numbers existed, but they came out of a conversation rather than
//! out of the tool, and a number a tool does not print stops being looked at.
//!
//! Two shapes over one body of facts, because the same facts are wanted twice.
//! [`Panel`] is the live dashboard: one line per lane with a bar, the program it
//! is running and the milliseconds it has taken, drawn with ratatui while the
//! guests boot and run. [`report`] is what the command ends with, and it is a
//! pure function over the finished sweep - which is the point: a report that can
//! only be checked by looking at a terminal is a report that is never checked,
//! so its shape is pinned by host tests here.
//!
//! The panel is *not* the report and never carries a fact the report does not:
//! it exists so that a run that takes a minute (the second machine under TCG,
//! `cargo xtask test --arch aarch64`) says what it is doing while it does it.
//! When stdout is not a terminal - a pipe, a redirect, CI - the panel is not
//! started at all and the command prints exactly the lines it always printed,
//! because a dashboard that writes escape codes into a log is worse than no
//! dashboard.

use std::collections::BTreeMap;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// One lane as the finished sweep reports it.
///
/// `programs` and `millis` are the two numbers a reader compares - a lane with
/// one program taking as long as a lane with forty is not a mistake, and
/// `heaviest` is the fact that explains it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneRow {
    pub index: usize,
    pub programs: usize,
    pub millis: u64,
    pub ok: bool,
    pub heaviest: Option<(String, u64)>,
}

/// Stage 1, as the report shows it: the host unit tests that ran beside the
/// guest, and how long they took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostStage {
    pub passed: usize,
    pub failed: usize,
    pub millis: u64,
}

/// Everything [`report`] draws from.
pub struct Summary<'a> {
    pub host: Option<HostStage>,
    pub lanes: &'a [LaneRow],
    /// The sweep's own summary line (`pass=192 fail=0`), or the reason there is
    /// none.
    pub verdict: String,
    /// The kernel's `mpos-kunit` line, as the protocol parses it.
    pub kunit: Option<(usize, usize)>,
    /// The head of the cost table, heaviest first: what the run's time went on.
    pub slowest: &'a [(String, u64)],
    pub total_millis: u64,
    /// The last green run's total, when there is one (`gate_history`).
    pub previous_millis: Option<u64>,
    /// Whether the whole command passed. The report still prints on a failure -
    /// it is the thing a failure is read on.
    pub ok: bool,
}

/// A bar of `width` cells for `done` of `total`.
///
/// Full and empty blocks rather than shades: a log that is being read on a
/// terminal with one colour, or copied into an issue, still has to say how far
/// along the lane is. A lane with no programs is an empty bar - it has nothing
/// to be part of the way through.
fn bar(done: usize, total: usize, width: usize) -> String {
    if total == 0 || width == 0 {
        return "░".repeat(width);
    }
    let done = done.min(total);
    let full = done * width / total;
    // A lane that has started and has not finished keeps one cell of progress
    // visible, so "0 of 38" and "the first one is running" do not look the same.
    let full = if done > 0 { full.max(1) } else { full };
    format!("{}{}", "█".repeat(full), "░".repeat(width - full))
}

/// What one lane has answered so far, and the program it is inside now.
///
/// The protocol's pair, read off the lines the poll marked: a `=== RUN <name>`
/// opens a window, a `=== PASS <name>`/`=== FAIL <name>` closes it and counts.
/// A `RUN` still open at the end is the program running now - which is exactly
/// what a live panel needs and exactly what `measure_costs` refuses to price.
fn progress(marks: &[(u64, String)], total: usize) -> (usize, Option<String>) {
    let mut done = 0usize;
    let mut open: Option<String> = None;
    for (_, line) in marks {
        if let Some(name) = line.strip_prefix("=== RUN ") {
            open = Some(name.trim().to_string());
        } else if line.starts_with("=== PASS ") || line.starts_with("=== FAIL ") {
            done += 1;
            open = None;
        }
    }
    (done.min(total), open)
}

/// The timed lines to name as the run's slowest: the head of the cost table,
/// heaviest first.
fn slowest(costs: &BTreeMap<String, u64>, count: usize) -> Vec<(String, u64)> {
    let mut pairs: Vec<(String, u64)> = costs.iter().map(|(n, m)| (n.clone(), *m)).collect();
    pairs.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    pairs.truncate(count);
    pairs
}

/// Seconds with two decimals, which is the resolution the sweep is measured at.
fn seconds(millis: u64) -> String {
    format!("{:.2} s", millis as f64 / 1000.0)
}

/// The report a finished sweep ends with.
///
/// It says five things and no more: what each stage cost, what each lane
/// carried, what the verdict was, which programs were the slow ones, and how
/// this total compares with the last green run. Everything here is measured by
/// the run itself - there is no baseline inside the binary, because a number
/// that lives in the code goes stale the day the machine or the suite changes
/// (the comparison is with the previous run, which is a fact about *this* tree).
pub fn report(summary: &Summary<'_>) -> String {
    let mut text = String::new();
    let rule = "─".repeat(72);
    text.push_str(&format!("── self-test {rule}\n"));

    if let Some(host) = summary.host {
        text.push_str(&format!(
            "  stage 1  host unit tests   {:>4} tests, {} failed{:>10}  {}\n",
            host.passed,
            host.failed,
            "",
            seconds(host.millis),
        ));
    }
    let programs: usize = summary.lanes.iter().map(|lane| lane.programs).sum();
    let boot_ms = summary
        .lanes
        .iter()
        .map(|lane| lane.millis)
        .max()
        .unwrap_or_default();
    text.push_str(&format!(
        "  stage 2  boot + sweep      {programs:>4} programs over {} lane(s){:>4}  {}\n",
        summary.lanes.len(),
        "",
        seconds(boot_ms),
    ));

    for lane in summary.lanes {
        let heaviest = match &lane.heaviest {
            Some((name, millis)) => format!("  heaviest {name} {}", seconds(*millis)),
            None => String::new(),
        };
        text.push_str(&format!(
            "  lane {:>2}  {:>3} program(s)  {:>8}  {}{}\n",
            lane.index,
            lane.programs,
            seconds(lane.millis),
            if lane.ok { "ok  " } else { "FAIL" },
            heaviest,
        ));
    }

    text.push_str(&format!("  verdict  {}\n", summary.verdict));
    match summary.kunit {
        Some((passed, failed)) => text.push_str(&format!(
            "  kunit    mpos-kunit {passed} passed, {failed} failed\n"
        )),
        None => text.push_str("  kunit    the kernel printed no mpos-kunit line\n"),
    }

    // The programs the time went on, up to the caller's count and wrapped so a
    // narrow terminal does not turn the one line a person reads into three
    // ragged ones. An empty list is said in words, because "no program was
    // timed" is a fact about the run and a blank line is not.
    if summary.slowest.is_empty() {
        text.push_str("  slowest  no program was timed\n");
    } else {
        let mut line = String::from("  slowest  ");
        let indent = "           ";
        for (index, (name, millis)) in summary.slowest.iter().enumerate() {
            let item = format!("{name} {}", seconds(*millis));
            if index > 0 {
                if line.chars().count() + item.chars().count() + 3 > 78 {
                    text.push_str(&line);
                    text.push('\n');
                    line = String::from(indent);
                } else {
                    line.push_str(" · ");
                }
            }
            line.push_str(&item);
        }
        text.push_str(&line);
        text.push('\n');
    }

    let mut total = format!("  total    {}", seconds(summary.total_millis));
    match summary.previous_millis {
        Some(previous) if previous > 0 => {
            let delta = summary.total_millis as f64 - previous as f64;
            total.push_str(&format!(
                "   (previous green run {}, {}{:.1}%)",
                seconds(previous),
                if delta >= 0.0 { "+" } else { "" },
                delta / previous as f64 * 100.0
            ));
        }
        _ => total.push_str("   (no previous green run to compare with)"),
    }
    text.push_str(&total);
    text.push('\n');
    if !summary.ok {
        text.push_str("  result   FAILED\n");
    }
    text
}

/// The head of the cost table, heaviest first - the report's `slowest` line.
///
/// The cost table is a hint with a floor (`selftest::cost_of`), so a program it
/// never measured keeps its floor rather than looking free; what is named here
/// is what the *next* deal will also weight most, which is what makes a lane's
/// `heaviest` and this list agree.
pub fn slowest_list(costs: &BTreeMap<String, u64>, count: usize) -> Vec<(String, u64)> {
    slowest(costs, count)
}

/// Whether the live panel belongs in this invocation: a terminal on stdout, and
/// neither `MPOS_TEST_TUI=0` nor `TERM=dumb`.
///
/// The check is here rather than at the call site so that "does this command
/// draw a dashboard" has one answer, and it is the answer the logs need: a
/// redirect gets the plain lines, always.
pub fn panel_wanted() -> bool {
    if std::env::var_os("MPOS_TEST_TUI").is_some_and(|value| value == "0") {
        return false;
    }
    if std::env::var_os("TERM").is_some_and(|term| term == "dumb") {
        return false;
    }
    std::io::stdout().is_terminal()
}

/// Where the last green runs' totals are kept, one millisecond count per line:
/// **per machine**, because the two arms are not comparable - the first runs the
/// suite over lanes on KVM and the second boots one disk under TCG, and a shared
/// file would read the difference between the machines as a regression in the
/// harness.
fn history_path(build_dir: &Path, machine: &str) -> PathBuf {
    build_dir.join(format!("test-gate-history-{machine}.txt"))
}

/// The last green run's total for this machine, if there is one.
///
/// A hint like the cost table: a line that does not parse is skipped, and a
/// missing file is "no previous run" rather than a failure.
pub fn previous_total(build_dir: &Path, machine: &str) -> Option<u64> {
    let text = fs::read_to_string(history_path(build_dir, machine)).ok()?;
    text.lines()
        .filter_map(|line| line.trim().parse().ok())
        .next_back()
}

/// Append this run's total. Only a green run's total is recorded: a red run's
/// duration is the duration of a run that stopped early, and comparing a normal
/// run with it would read as a regression that never happened.
pub fn record_total(build_dir: &Path, machine: &str, millis: u64) -> std::io::Result<()> {
    use std::io::Write;
    let path = history_path(build_dir, machine);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(file, "{millis}")
}

/// Watch the keyboard for the one key the panel took away.
///
/// A no-op when the panel is not drawn (the terminal is untouched then, and the
/// shell's own ctrl-c works); started once by [`Panel::start`] otherwise.
fn spawn_interrupt_watcher() {
    use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

    std::thread::spawn(|| {
        while let Ok(event) = ratatui::crossterm::event::read() {
            let Event::Key(key) = event else {
                continue;
            };
            if key.kind == KeyEventKind::Press
                && key.code == KeyCode::Char('c')
                && key.modifiers.contains(KeyModifiers::CONTROL)
            {
                // Restore before exiting: `process::exit` runs no destructors,
                // and a terminal left in raw mode with the alternate screen
                // active is a terminal the user has to reset by hand.
                ratatui::restore();
                std::process::exit(130);
            }
        }
    });
}

/// The live dashboard: one line per lane, drawn while the guests run.
///
/// A `Panel` that was not wanted owns no terminal, and every method on it is a
/// no-op - so the sweep code has one path and does not ask whether it is being
/// watched.
pub struct Panel {
    terminal: Option<ratatui::DefaultTerminal>,
    totals: Vec<usize>,
    lanes: Vec<LaneRow>,
    running: Vec<(usize, Option<String>)>,
    started: Instant,
    last_draw: Instant,
}

impl Panel {
    /// Start the panel for `totals.len()` lanes, each dealt `totals[i]`
    /// programs.
    pub fn start(totals: &[usize]) -> Self {
        let terminal = panel_wanted().then(ratatui::init);
        // Raw mode turns off the terminal's own signal keys, so the ctrl-c that
        // used to end a sweep would be swallowed by the panel and the sweep
        // would become uninterruptible - a regression a progress display is not
        // allowed to cost. One watcher thread reads the keyboard for exactly
        // that key, puts the terminal back and exits with the status a shell
        // reports for SIGINT.
        if terminal.is_some() {
            spawn_interrupt_watcher();
        }
        let now = Instant::now();
        Self {
            // A panel that could not open a terminal (a `TERM` that lies, a
            // stdout that stopped being a tty) is simply not drawn: the sweep
            // must never fail because its progress display could not start.
            terminal,
            totals: totals.to_vec(),
            lanes: Vec::new(),
            running: (0..totals.len()).map(|index| (index, None)).collect(),
            started: now,
            last_draw: now - Duration::from_millis(100),
        }
    }

    /// Fold one poll of the lanes into the panel and redraw, at most ten times
    /// a second - the sweep's own poll is five times that, and a dashboard
    /// redrawn at 50 Hz is a terminal doing work the guests need.
    pub fn tick(&mut self, ticks: &[crate::qemu::LaneTick<'_>]) {
        let Some(terminal) = self.terminal.as_mut() else {
            return;
        };
        self.lanes = ticks
            .iter()
            .zip(&self.totals)
            .enumerate()
            .map(|(index, (tick, total))| {
                let (done, current) = progress(tick.marks, *total);
                self.running[index] = (done, current);
                LaneRow {
                    index,
                    programs: *total,
                    millis: tick.millis,
                    // A lane that has not exited yet is not a failure; the
                    // dashboard only turns red once the machine has said so.
                    ok: tick.exited.map(|code| code == 33).unwrap_or(true),
                    heaviest: None,
                }
            })
            .collect();
        if self.last_draw.elapsed() < Duration::from_millis(100) {
            return;
        }
        self.last_draw = Instant::now();
        let elapsed = self.started.elapsed();
        let lanes = self.lanes.clone();
        let running = self.running.clone();
        let totals = self.totals.clone();
        let _ = terminal.draw(|frame| draw(frame, &lanes, &running, &totals, elapsed));
    }

    /// Leave the dashboard and restore the terminal, keeping the final frame
    /// out of the way of the report that follows it.
    pub fn stop(self) {}
}

impl Drop for Panel {
    /// The terminal is given back on **every** way out of the sweep, including
    /// the one nobody plans for: `capture_many` returns `Err` on a lane that
    /// outlives the deadline, and a `?` there would otherwise leave the user's
    /// terminal in raw mode with the alternate screen active - a broken terminal
    /// as the last word of a failed run. `ratatui::restore` is safe to call when
    /// there is nothing to restore, so a panel that never owned a terminal is
    /// still a no-op.
    fn drop(&mut self) {
        if self.terminal.is_some() {
            ratatui::restore();
        }
    }
}

/// Draw the dashboard: a header, then a line per lane.
fn draw(
    frame: &mut Frame<'_>,
    lanes: &[LaneRow],
    running: &[(usize, Option<String>)],
    totals: &[usize],
    elapsed: Duration,
) {
    let area = frame.area();
    let done: usize = running.iter().map(|(count, _)| *count).sum();
    let total: usize = totals.iter().sum();
    let header = Line::from(vec![
        Span::styled(
            " mpos self-test ",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("{done}/{total} programs  ")),
        Span::styled(
            format!("{:.1} s", elapsed.as_secs_f64()),
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    let mut lines = vec![header, Line::raw("")];
    for lane in lanes {
        let (done, current) = running.get(lane.index).cloned().unwrap_or((0, None));
        let total = totals.get(lane.index).copied().unwrap_or(lane.programs);
        let state = match lane.ok {
            true => Span::styled("ok  ", Style::default().fg(Color::Green)),
            false => Span::styled(
                "FAIL",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        };
        lines.push(Line::from(vec![
            Span::raw(format!(" lane {:<2} ", lane.index)),
            Span::raw(bar(done, total, 12)),
            Span::raw(format!(" {done:>3}/{total:<3} ")),
            Span::styled(
                format!("{:<20}", current.unwrap_or_default()),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw(format!(" {:>7}  ", seconds(lane.millis))),
            state,
        ]));
    }
    if area.height as usize > lines.len() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            " ctrl-c stops the sweep; per-lane logs are build/test-lane-<n>/serial.log",
            Style::default().fg(Color::DarkGray),
        ));
    }
    let paragraph = ratatui::widgets::Paragraph::new(lines);
    frame.render_widget(paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marks(lines: &[(u64, &str)]) -> Vec<(u64, String)> {
        lines
            .iter()
            .map(|(millis, line)| (*millis, (*line).to_string()))
            .collect()
    }

    /// The bar has to be a proportion of its width and it has to survive the
    /// two ends: nothing answered, everything answered, and a lane with no
    /// programs at all.
    #[test]
    fn a_bar_is_a_proportion_of_its_width() {
        assert_eq!(super::bar(0, 4, 4), "░░░░");
        assert_eq!(super::bar(4, 4, 4), "████");
        assert_eq!(super::bar(2, 4, 4), "██░░");
        assert_eq!(super::bar(0, 0, 3), "░░░", "a lane with nothing to do");
        // A started lane shows a cell even when its share rounds to nothing,
        // so "0 of 38" and "the first is running" are different pictures.
        assert_eq!(super::bar(1, 100, 8), "█░░░░░░░");
    }

    /// The protocol's pair, from the same marks the costs are measured from: a
    /// verdict counts, an open `RUN` is the program running now.
    #[test]
    fn progress_counts_verdicts_and_names_the_one_running() {
        let seen = marks(&[
            (10, "=== RUN t-a"),
            (20, "ok:    something"),
            (30, "=== PASS t-a"),
            (40, "=== RUN t-b"),
        ]);
        assert_eq!(super::progress(&seen, 5), (1, Some("t-b".to_string())));
        assert_eq!(super::progress(&seen, 1), (1, Some("t-b".to_string())));
        let all_done = marks(&[(10, "=== RUN t-a"), (20, "=== FAIL t-a")]);
        assert_eq!(super::progress(&all_done, 1), (1, None));
    }

    /// The report names every lane, the verdict, the kernel's line and the
    /// total, and it compares with the previous run only when there is one -
    /// "no previous run" is a fact, not a zero.
    #[test]
    fn the_report_names_every_lane_the_verdict_and_the_total() {
        let lanes = [
            LaneRow {
                index: 0,
                programs: 1,
                millis: 3210,
                ok: true,
                heaviest: Some(("t-udpblock".to_string(), 3210)),
            },
            LaneRow {
                index: 1,
                programs: 38,
                millis: 3090,
                ok: true,
                heaviest: Some(("t-waitid".to_string(), 210)),
            },
        ];
        let slowest = [
            ("t-udpblock".to_string(), 3210u64),
            ("t-waitid".to_string(), 1210),
            ("t-wait4".to_string(), 850),
        ];
        let text = super::report(&Summary {
            host: Some(HostStage {
                passed: 717,
                failed: 0,
                millis: 3120,
            }),
            lanes: &lanes,
            verdict: "pass=39 fail=0".to_string(),
            kunit: Some((32, 0)),
            slowest: &slowest,
            total_millis: 6510,
            previous_millis: Some(6470),
            ok: true,
        });
        assert!(
            text.contains("stage 1  host unit tests    717 tests"),
            "{text}"
        );
        assert!(text.contains("boot + sweep"), "{text}");
        assert!(text.contains("39 programs over 2 lane(s)"), "{text}");
        assert!(text.contains("lane  0    1 program(s)"), "{text}");
        assert!(text.contains("lane  1   38 program(s)"), "{text}");
        assert!(text.contains("pass=39 fail=0"), "{text}");
        assert!(text.contains("mpos-kunit 32 passed, 0 failed"), "{text}");
        assert!(
            text.contains("slowest  t-udpblock 3.21 s · t-waitid 1.21 s · t-wait4 0.85 s"),
            "the slowest programs are named in order: {text}"
        );
        assert!(text.contains("6.51 s"), "{text}");
        assert!(text.contains("+0.6%"), "{text}");
        assert!(!text.contains("FAILED"), "a green run does not say FAILED");

        let no_history = super::report(&Summary {
            host: None,
            lanes: &lanes[..1],
            verdict: "pass=1 fail=0".to_string(),
            kunit: None,
            slowest: &[],
            total_millis: 5000,
            previous_millis: None,
            ok: true,
        });
        assert!(no_history.contains("no previous green run"), "{no_history}");
        assert!(!no_history.contains("stage 1"), "no stage 1 was reported");
        assert!(no_history.contains("no mpos-kunit line"), "{no_history}");
        assert!(no_history.contains("no program was timed"), "{no_history}");

        let failed = super::report(&Summary {
            host: None,
            lanes: &lanes[..1],
            verdict: "pass=0 fail=1".to_string(),
            kunit: Some((32, 0)),
            slowest: &slowest[..1],
            total_millis: 900,
            previous_millis: Some(900),
            ok: false,
        });
        assert!(failed.contains("result   FAILED"), "{failed}");
    }

    /// A lane that did not end on the debug exit is not `ok`, and the report
    /// still prints (that is the run a person reads it on).
    #[test]
    fn a_lane_that_did_not_exit_cleanly_is_not_ok() {
        let lanes = [LaneRow {
            index: 2,
            programs: 4,
            millis: 1234,
            ok: false,
            heaviest: None,
        }];
        let text = super::report(&Summary {
            host: None,
            lanes: &lanes,
            verdict: "no summary line".to_string(),
            kunit: None,
            slowest: &[],
            total_millis: 1234,
            previous_millis: None,
            ok: false,
        });
        assert!(text.contains("FAIL"), "{text}");
        assert!(text.contains("result   FAILED"), "{text}");
    }

    /// The history is a hint like the cost table: the last line wins, a broken
    /// line is not a crash, and no file is "no previous run".
    #[test]
    fn the_last_green_run_is_the_last_line_of_the_history() {
        let dir = std::env::temp_dir().join("mpos-gate-history-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(super::previous_total(&dir, "x86_64"), None);
        super::record_total(&dir, "x86_64", 7000).unwrap();
        super::record_total(&dir, "x86_64", 6510).unwrap();
        assert_eq!(super::previous_total(&dir, "x86_64"), Some(6510));
        assert_eq!(
            super::previous_total(&dir, "aarch64"),
            None,
            "one history per machine"
        );
        fs::write(super::history_path(&dir, "x86_64"), "6510\nnot-a-number\n").unwrap();
        assert_eq!(super::previous_total(&dir, "x86_64"), Some(6510));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The dashboard itself: every lane gets a line, the program a lane is
    /// inside is on it, and a lane that failed says so. Rendered into
    /// ratatui's own test backend, which is the only way to pin "what a
    /// terminal would show" without a terminal.
    #[test]
    fn the_panel_draws_each_lane_and_the_program_it_is_running() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let lanes = [
            LaneRow {
                index: 0,
                programs: 2,
                millis: 3400,
                ok: true,
                heaviest: None,
            },
            LaneRow {
                index: 1,
                programs: 3,
                millis: 1200,
                ok: false,
                heaviest: None,
            },
        ];
        let running = [(0, Some("t-udpblock".to_string())), (1, None)];
        let totals = [2usize, 3];
        let mut terminal = Terminal::new(TestBackend::new(90, 12)).unwrap();
        terminal
            .draw(|frame| {
                super::draw(
                    frame,
                    &lanes,
                    &running,
                    &totals,
                    Duration::from_millis(3400),
                )
            })
            .unwrap();
        let screen = terminal.backend().buffer().clone();
        let text: String = (0..screen.area.height)
            .map(|y| {
                (0..screen.area.width)
                    .map(|x| screen[(x, y)].symbol().to_string())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        assert!(text.contains("mpos self-test"), "{text}");
        assert!(text.contains("lane 0"), "{text}");
        assert!(text.contains("t-udpblock"), "{text}");
        assert!(text.contains("FAIL"), "the failed lane is named: {text}");
        assert!(text.contains("3.40 s"), "{text}");
    }

    /// A panel nobody asked for draws nothing: the same methods, no terminal.
    #[test]
    fn a_panel_that_was_not_wanted_is_a_no_op() {
        // `panel_wanted` reads the process's own stdout, which in a test is a
        // pipe, so this is the not-wanted path by construction.
        assert!(!super::panel_wanted() || std::io::stdout().is_terminal());
        let mut panel = super::Panel::start(&[1, 2]);
        panel.tick(&[]);
        panel.stop();
    }
}
