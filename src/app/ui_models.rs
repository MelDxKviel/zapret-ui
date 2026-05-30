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
    let (pretty, alt) = split_alt(&s.id);
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
    let mut list: Vec<crate::contracts::Strategy> = catalog
        .all()
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

/// Split a raw log line into (timestamp, level, message) for coloured display.
fn parse_log_line(no: usize, raw: &str) -> LogLineItem {
    let mut rest = raw.trim_end();
    let mut timestamp = String::new();
    let mut level = String::new();

    // Leading ISO-8601 timestamp, e.g. 2026-05-23T16:14:34.808277Z
    if let Some((first, tail)) = rest.split_once(char::is_whitespace) {
        let looks_ts = first.len() >= 20
            && first.as_bytes()[0].is_ascii_digit()
            && first.contains('T')
            && first.ends_with('Z');
        if looks_ts {
            timestamp = first.to_string();
            rest = tail.trim_start();
        }
    }

    // Level tag
    if let Some((first, tail)) = rest.split_once(char::is_whitespace) {
        let up = first.to_uppercase();
        if matches!(up.as_str(), "INFO" | "WARN" | "WARNING" | "ERROR" | "ERR" | "DEBUG" | "TRACE") {
            level = if up.starts_with("ERR") { "ERROR".to_string() }
                else if up.starts_with("WARN") { "WARN".to_string() }
                else { up };
            rest = tail.trim_start();
        }
    }

    LogLineItem {
        line_no: no as i32,
        timestamp: timestamp.into(),
        level: level.into(),
        message: rest.to_string().into(),
    }
}

fn line_passes(raw: &str, grep: &str, level: &str) -> bool {
    if level != "ALL" {
        let up = raw.to_uppercase();
        let want = if level == "ERROR" { "ERR" } else { level };
        if !up.contains(want) {
            return false;
        }
    }
    if !grep.is_empty() && !raw.to_lowercase().contains(&grep.to_lowercase()) {
        return false;
    }
    true
}

/// Re-parse + re-filter the whole buffer into the Slint `log_lines` model.
pub(super) fn rebuild_logs(ui: &MainWindow) {
    let (grep, level) = LOG_FILTER.with(|f| f.borrow().clone());
    let (items, text) = LOG_BUF.with(|b| {
        let mut items: Vec<LogLineItem> = Vec::new();
        let mut text = String::new();
        for raw in b.borrow().iter().filter(|raw| line_passes(raw, &grep, &level)) {
            items.push(parse_log_line(items.len() + 1, raw));
            text.push_str(raw);
            text.push('\n');
        }
        (items, text)
    });
    ui.set_log_lines(Rc::new(slint::VecModel::from(items)).into());
    // Plain-text mirror for the selectable / copyable terminal view.
    ui.set_log_text(text.into());
}

/// Rebuild the Slint `test_results` model from the thread-local buffer.
/// Sorts live by reachability (then latency) so the best strategies bubble to
/// the top as results stream in, rather than appearing in catalog/name order.
pub(super) fn rebuild_test_results(ui: &MainWindow) {
    let best_id = ui.get_test_best_id().to_string();
    let mut sorted = TEST_RESULTS.with(|b| b.borrow().clone());
    sorted.sort_by(|a, b| {
        b.ok.cmp(&a.ok).then_with(|| {
            let al = if a.avg_latency_ms == 0 { u32::MAX } else { a.avg_latency_ms };
            let bl = if b.avg_latency_ms == 0 { u32::MAX } else { b.avg_latency_ms };
            al.cmp(&bl)
        })
    });
    let items: Vec<TestResultItem> = sorted
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let (pretty, alt) = split_alt(&r.id);
            TestResultItem {
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
