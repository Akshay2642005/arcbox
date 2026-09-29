//! Serial port read loop for a VM's console and agent-log output.
//!
//! Every macOS VM gets one of these for as long as it runs. The console
//! pipes are the only sink for what the guest writes to `hvc0`/`hvc1`, and
//! VZ's serial attachment blocks the guest's virtio-console queue once the
//! 64 KiB host pipe is full: the guest's console write then never completes,
//! the vCPUs spin on the stalled queue at 100% each, and the guest's vsock
//! side stops answering too (measured 2026-09-28, a Debian machine whose
//! `agetty` on `hvc0` filled the pipe within a day). Draining is therefore
//! load-bearing for every VM, not a logging nicety for the System VM.

use std::sync::Arc;
use std::time::Duration;

use super::MachineManager;

/// Base polling interval (ms) when serial output is actively being produced.
const SERIAL_ACTIVE_INTERVAL_MS: u64 = 100;

/// Maximum number of doublings for idle backoff (100ms → 1600ms).
const SERIAL_MAX_IDLE_SHIFT: u32 = 4;

/// Drains `machine`'s hvc0 (console) and hvc1 (agent log) pipes until the
/// machine stops, logging complete lines.
///
/// One loop with exponential backoff: 100ms while output arrives, doubling
/// up to 1600ms when idle, so an idle VM costs about one wakeup a second.
/// The loop ends when the console read is rejected, which the state-gated
/// reader does as soon as the machine leaves `Running`.
pub(super) async fn drain_serial(machine_manager: Arc<MachineManager>, machine: String) {
    const MAX_LINE_BUF: usize = 64 * 1024;

    let console_label = if machine == super::DEFAULT_MACHINE_NAME {
        "Guest".to_owned()
    } else {
        format!("Guest[{machine}]")
    };
    let agent_label = if machine == super::DEFAULT_MACHINE_NAME {
        "Agent".to_owned()
    } else {
        format!("Agent[{machine}]")
    };
    // Only the System VM's console is worth INFO: a user machine's getty
    // and journal chatter is diagnostic, not operational.
    let console_info = machine == super::DEFAULT_MACHINE_NAME;

    let mut console_buf = String::new();
    let mut agent_buf = String::new();
    let mut idle_streak: u32 = 0;

    loop {
        // Any byte read means the guest is writing and the pipe needs the
        // fast poll, whatever the bytes are: NUL padding is not worth
        // logging, but a guest that fills the pipe with it (`head -c N
        // /dev/zero > /dev/hvc0`) still blocks until the pipe is drained.
        let mut had_output = false;

        if let Ok(output) = machine_manager.read_console_output(&machine) {
            had_output |= !output.is_empty();
            process_serial_output(
                &mut console_buf,
                &output,
                &console_label,
                console_info,
                MAX_LINE_BUF,
            );
        } else {
            flush_line_buf(&mut console_buf, &console_label, console_info);
            flush_line_buf(&mut agent_buf, &agent_label, false);
            tracing::debug!(machine, "serial drain stopped: machine no longer running");
            break;
        }

        if let Ok(output) = machine_manager.read_agent_log_output(&machine) {
            had_output |= !output.is_empty();
            process_serial_output(&mut agent_buf, &output, &agent_label, false, MAX_LINE_BUF);
        }
        // Agent log failure is non-fatal — console may still work.

        if had_output {
            idle_streak = 0;
        } else {
            idle_streak = idle_streak.saturating_add(1);
        }

        // Adaptive delay: 100ms when active, doubling up to 1600ms when idle.
        let delay_ms = SERIAL_ACTIVE_INTERVAL_MS * (1u64 << idle_streak.min(SERIAL_MAX_IDLE_SHIFT));
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }
}

/// Process raw serial output into line-buffered log messages.
fn process_serial_output(
    line_buf: &mut String,
    output: &str,
    label: &str,
    level_info: bool,
    max_buf: usize,
) {
    let trimmed = output.trim_matches('\0');
    if trimmed.is_empty() {
        return;
    }

    line_buf.push_str(trimmed);

    while let Some(pos) = line_buf.find('\n') {
        let line = line_buf[..pos].trim_end().to_owned();
        line_buf.drain(..=pos);
        if line.is_empty() {
            continue;
        }
        if level_info {
            tracing::info!("{label}: {line}");
        } else {
            tracing::debug!("{label}: {line}");
        }
    }

    // Only an unterminated line can grow without bound; a burst of complete
    // lines was already logged above.
    if line_buf.len() > max_buf {
        tracing::warn!("{label}: line buffer overflow, flushing");
        line_buf.clear();
    }
}

/// Flush any remaining partial line from a serial buffer.
fn flush_line_buf(line_buf: &mut String, label: &str, level_info: bool) {
    let trailing = line_buf.trim().to_owned();
    if !trailing.is_empty() {
        if level_info {
            tracing::info!("{label}: {trailing}");
        } else {
            tracing::debug!("{label}: {trailing}");
        }
    }
    line_buf.clear();
}

#[cfg(test)]
mod tests {
    use super::process_serial_output;

    #[test]
    fn a_burst_of_complete_lines_is_not_an_overflow() {
        let mut buf = String::new();
        let burst: String = std::iter::repeat_n("line\n", 100).collect();
        process_serial_output(&mut buf, &burst, "T", false, 64);
        assert!(buf.is_empty(), "every complete line was consumed");
    }

    #[test]
    fn an_unterminated_line_past_the_bound_is_dropped() {
        let mut buf = String::new();
        process_serial_output(&mut buf, &"x".repeat(65), "T", false, 64);
        assert!(buf.is_empty());
    }

    #[test]
    fn a_partial_line_waits_for_its_newline() {
        let mut buf = String::new();
        process_serial_output(&mut buf, "abc", "T", false, 64);
        assert_eq!(buf, "abc");
        process_serial_output(&mut buf, "def\n", "T", false, 64);
        assert!(buf.is_empty());
    }

    #[test]
    fn nul_padding_is_not_buffered() {
        let mut buf = String::new();
        process_serial_output(&mut buf, "\0\0", "T", false, 64);
        assert!(buf.is_empty());
        process_serial_output(&mut buf, "\0abc\0", "T", false, 64);
        assert_eq!(buf, "abc");
    }
}
