//! Pure helpers that build Slint models (strategies, logs, test results) from the
//! catalog / thread-local buffers. Extracted from `app.rs` to keep the
//! orchestrator focused on wiring. These run on the Slint UI thread and read the
//! UI-thread thread-locals (`LOG_BUF`, `LOG_FILTER`, `TEST_RESULTS`, `FAVORITES`)
//! that live in the parent module.

use std::rc::Rc;
use std::sync::Arc;

use crate::contracts::split_alt;
use crate::ports::StrategyCatalog;

// Slint-generated row/window types + the parent's UI-thread thread-locals.
use super::{LogLineItem, MainWindow, StrategyItem, TestResultItem};
use super::{FAVORITES, LOG_BUF, LOG_FILTER, TEST_RESULTS};

/// Whether `id` is currently a favorite (reads the UI-thread mirror).
pub(super) fn is_favorite(id: &str) -> bool {
    FAVORITES.with(|f| f.borrow().iter().any(|x| x == id))
}

/// Map a catalog strategy to its Slint row, tagging its current favorite state.
pub(super) fn to_item(s: &crate::contracts::Strategy) -> StrategyItem {
    let (mut pretty, alt) = split_alt(&s.display_name);
    if cfg!(windows) {
        pretty = format!("{} · {}", s.engine.slug(), pretty);
    }
    StrategyItem {
        id: s.id.as_str().into(),
        display_name: s.display_name.as_str().into(),
        category: format!("{:?}", s.category).into(),
        description: s.description.as_str().into(),
        pretty: pretty.into(),
        alt: alt.into(),
        favorite: is_favorite(&s.id),
    }
}

/// Rebuild the Slint `strategies` model from the catalog, applying the current
/// search query and floating favorites to the top (keeping catalog order within
/// each group). Runs on the UI thread.
pub(super) fn rebuild_strategies(ui: &MainWindow, catalog: &Arc<dyn StrategyCatalog>) {
    let q = ui.get_strategies_query().to_string().trim().to_lowercase();
    let all = catalog.all();
    ui.set_total_strategy_count(all.len().min(i32::MAX as usize) as i32);
    let mut list: Vec<crate::contracts::Strategy> = all
        .into_iter()
        .filter(|s| {
            q.is_empty()
                || format!("{} {} {}", s.id, s.display_name, s.description)
                    .to_lowercase()
                    .contains(&q)
        })
        .collect();
    // Stable sort: favorites first, original (catalog) order preserved otherwise.
    list.sort_by_key(|s| if is_favorite(&s.id) { 0 } else { 1 });
    let items: Vec<StrategyItem> = list.iter().map(to_item).collect();
    ui.set_strategies(Rc::new(slint::VecModel::from(items)).into());
}

/// Split a raw log line into (timestamp, level, message) for display.
///
/// Tracing lines look like `2026-06-10T14:23:45.123+03:00  INFO winws: …`.
/// For display the timestamp is reduced to `HH:MM:SS` (it is already local
/// time — see `log::init_logging`; the full date stays in app.log) and the
/// `zapret_ui::…::module:` target is shortened to its last segment.
fn parse_log_line(no: usize, raw: &str) -> LogLineItem {
    let mut rest = raw.trim_end();
    let mut timestamp = String::new();
    let mut level = String::new();

    // Leading ISO-8601 timestamp: `2026-06-10T14:23:45.123+03:00` (local) or
    // the legacy UTC `2026-05-23T16:14:34.808277Z` form.
    if let Some((first, tail)) = rest.split_once(char::is_whitespace) {
        let looks_ts = first.len() >= 19
            && first.as_bytes()[0].is_ascii_digit()
            && first.contains('T')
            && (first.ends_with('Z') || first.contains('+') || first.matches('-').count() > 2);
        if looks_ts {
            timestamp = first.get(11..19).unwrap_or(first).to_string();
            rest = tail.trim_start();
        }
    }

    // Level tag
    if let Some((first, tail)) = rest.split_once(char::is_whitespace) {
        let up = first.to_uppercase();
        if matches!(
            up.as_str(),
            "INFO" | "WARN" | "WARNING" | "ERROR" | "ERR" | "DEBUG" | "TRACE"
        ) {
            level = if up.starts_with("ERR") {
                "ERROR".to_string()
            } else if up.starts_with("WARN") {
                "WARN".to_string()
            } else {
                up
            };
            rest = tail.trim_start();
        }
    }

    // `zapret_ui::zapret::tester: …` → `tester: …`. Only for lines that came
    // from tracing (a level was parsed), so plain text is never mangled.
    let message = match rest.split_once(' ') {
        Some((head, tail)) if !level.is_empty() && head.ends_with(':') && head.contains("::") => {
            let short = head
                .trim_end_matches(':')
                .rsplit("::")
                .next()
                .unwrap_or(head);
            format!("{short}: {}", tail.trim_start())
        }
        _ => rest.to_string(),
    };

    LogLineItem {
        line_no: no as i32,
        timestamp: timestamp.into(),
        level: level.into(),
        message: message.into(),
    }
}

fn line_passes(raw: &str, grep: &str, level: &str) -> bool {
    // Match the severity token, not words in the message or module name.
    if level != "ALL" && parse_log_line(0, raw).level.as_str() != level {
        return false;
    }
    if !grep.is_empty() && !raw.to_lowercase().contains(&grep.to_lowercase()) {
        return false;
    }
    true
}

/// Re-parse + re-filter the whole buffer into the Slint `log_lines` model.
pub(super) fn rebuild_logs(ui: &MainWindow) {
    use std::fmt::Write as _;

    let (grep, level) = LOG_FILTER.with(|f| f.borrow().clone());
    let (items, text) = LOG_BUF.with(|b| {
        let mut items: Vec<LogLineItem> = Vec::new();
        let mut text = String::new();
        for raw in b
            .borrow()
            .iter()
            .filter(|raw| line_passes(raw, &grep, &level))
        {
            // The terminal mirror shows the prettified form (`HH:MM:SS LEVEL
            // source: message`), not the raw line with its full ISO timestamp
            // and module path.
            let item = parse_log_line(items.len() + 1, raw);
            if !item.timestamp.is_empty() {
                text.push_str(item.timestamp.as_str());
                text.push(' ');
            }
            if !item.level.is_empty() {
                let _ = write!(text, "{:<5} ", item.level.as_str());
            }
            text.push_str(item.message.as_str());
            text.push('\n');
            items.push(item);
        }
        (items, text)
    });
    ui.set_log_lines(Rc::new(slint::VecModel::from(items)).into());
    // Plain-text mirror for the selectable / copyable terminal view.
    ui.set_log_text(text.into());
}

/// Rebuild the Slint `test_results` model from the thread-local buffer.
/// Uses the tester's baseline/reachability/latency ordering as results stream in.
pub(super) fn rebuild_test_results(ui: &MainWindow) {
    let mut best_id = ui.get_test_best_id().to_string();
    let mode = crate::contracts::EngineMode::from_slug(&ui.get_engine_mode());
    let mut sorted = TEST_RESULTS.with(|buffer| results_for_mode(&buffer.borrow(), mode));
    if !best_id.is_empty() && !sorted.iter().any(|result| result.id == best_id) {
        best_id.clear();
        ui.set_test_best_id("".into());
    }
    sorted.sort_by(|a, b| {
        b.eligible()
            .cmp(&a.eligible())
            .then_with(|| {
                (i64::from(b.improved) - i64::from(b.regressed))
                    .cmp(&(i64::from(a.improved) - i64::from(a.regressed)))
            })
            .then_with(|| b.ok.cmp(&a.ok))
            .then_with(|| a.regressed.cmp(&b.regressed))
            .then_with(|| {
                let al = if !cfg!(windows) && a.avg_latency_ms == 0 {
                    u32::MAX
                } else {
                    a.avg_latency_ms
                };
                let bl = if !cfg!(windows) && b.avg_latency_ms == 0 {
                    u32::MAX
                } else {
                    b.avg_latency_ms
                };
                al.cmp(&bl)
            })
    });
    let items: Vec<TestResultItem> = sorted
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let (pretty, alt) = split_alt(&r.display_name);
            TestResultItem {
                engine: r.engine.slug().into(),
                baseline: r.baseline_ok as i32,
                improved: r.improved as i32,
                regressed: r.regressed as i32,
                id: r.id.as_str().into(),
                display_name: r.display_name.as_str().into(),
                pretty: pretty.into(),
                alt: alt.into(),
                ok: r.ok as i32,
                total: r.total as i32,
                latency: r.avg_latency_ms as i32,
                rank: i as i32 + 1,
                is_best: !best_id.is_empty() && r.id == best_id,
                favorite: is_favorite(&r.id),
            }
        })
        .collect();
    ui.set_test_results(Rc::new(slint::VecModel::from(items)).into());
}

/// Keep the complete saved scan while showing only results that the current
/// engine mode can launch. macOS does not have Windows engine selection.
fn results_for_mode(
    results: &[crate::contracts::StrategyTestResult],
    mode: crate::contracts::EngineMode,
) -> Vec<crate::contracts::StrategyTestResult> {
    results
        .iter()
        .filter(|result| !cfg!(windows) || mode.includes(result.engine))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{line_passes, parse_log_line, results_for_mode};
    use crate::contracts::{EngineKind, EngineMode, StrategyTestResult};

    #[test]
    fn saved_test_results_follow_engine_mode_without_losing_other_engine_history() {
        let history = vec![
            StrategyTestResult {
                id: "general".into(),
                engine: EngineKind::Zapret1,
                ..Default::default()
            },
            StrategyTestResult {
                id: "zapret2:multisplit".into(),
                engine: EngineKind::Zapret2,
                ..Default::default()
            },
        ];
        let first = results_for_mode(&history, EngineMode::Zapret1);
        let second = results_for_mode(&history, EngineMode::Zapret2);
        if cfg!(windows) {
            assert_eq!(first.len(), 1);
            assert_eq!(first[0].id, "general");
            assert_eq!(second.len(), 1);
            assert_eq!(second[0].id, "zapret2:multisplit");
        } else {
            assert_eq!(first, history);
            assert_eq!(second, history);
        }
        assert_eq!(results_for_mode(&history, EngineMode::Auto), history);
    }

    #[test]
    fn log_filter_matches_severity_instead_of_message() {
        let warning = "2026-09-30T12:34:56.123+03:00 WARN tester: INFO endpoint returned ERROR";
        assert!(line_passes(warning, "endpoint", "WARN"));
        assert!(!line_passes(warning, "", "INFO"));
        assert!(!line_passes(warning, "", "ERROR"));
        assert!(!line_passes("an INFO message without severity", "", "INFO"));
        assert!(line_passes("WARNING tester: unavailable", "", "WARN"));
        assert!(line_passes("ERR tester: unavailable", "", "ERROR"));
        assert!(line_passes(warning, "error", "ALL"));
    }

    #[test]
    fn log_line_keeps_local_time_and_shortens_target() {
        let item = parse_log_line(
            7,
            "2026-09-30T12:34:56.123+03:00  INFO zapret_ui::zapret::tester: ready",
        );
        assert_eq!(item.line_no, 7);
        assert_eq!(item.timestamp.as_str(), "12:34:56");
        assert_eq!(item.level.as_str(), "INFO");
        assert_eq!(item.message.as_str(), "tester: ready");
    }
}
