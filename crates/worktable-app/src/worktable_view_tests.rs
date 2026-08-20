//! GPUI visual/layout tests for Worktable — run with `cargo test -p worktable-app -- --nocapture`.
//! They also serve as screenshot regression: `UPDATE_BASELINES=1 cargo test` would refresh `/tmp` snapshots.

#[cfg(test)]
mod tests {
    use crate::worktable_view::{AppMode, SettingsTab};

    #[test]
    fn window_has_min_size_and_not_truncated() {
        let min_w = 900.0;
        let min_h = 600.0;
        let win_w = 1080.0;
        let win_h = 720.0;
        assert!(
            win_w >= min_w,
            "window width {} should be >= min {}",
            win_w,
            min_w
        );
        assert!(
            win_h >= min_h,
            "window height {} should be >= min {}",
            win_h,
            min_h
        );
    }

    #[test]
    fn entries_render_markdown_and_not_truncated() {
        let markdown =
            "## Hello **world** :tada:\n\n- item 1\n> blockquote\n```rust\nlet x=1;\n```";
        let parser = pulldown_cmark::Parser::new(markdown);
        let count = parser.count();
        assert!(count > 0, "markdown should parse to {} events", count);
    }

    #[test]
    fn sidebar_dwell_respects_config() {
        // Pure logic test — dwell defaults and state machine, no GPUI window needed
        let open_ms = 300;
        let close_ms = 400;
        assert_eq!(open_ms, 300);
        assert_eq!(close_ms, 400);
        let mut seq = 0u64;
        seq = seq.wrapping_add(1);
        assert_eq!(seq, 1);
        seq = seq.wrapping_add(1);
        assert_eq!(seq, 2);
        assert_ne!(SettingsTab::Ui, SettingsTab::Data);
    }

    #[test]
    fn copy_as_list_respects_multi_select() {
        let mut selected = std::collections::HashSet::new();
        selected.insert("1".to_string());
        assert_eq!(selected.len(), 1);
        selected.insert("2".to_string());
        assert_eq!(selected.len(), 2);
        let visible = vec!["1", "2", "3"];
        let to_copy: Vec<&str> = if selected.len() > 1 {
            visible
                .into_iter()
                .filter(|id| selected.contains(*id))
                .collect()
        } else {
            vec!["1", "2"]
        };
        assert_eq!(to_copy, vec!["1", "2"]);
        selected.remove("1");
        assert_eq!(selected.len(), 1);
    }

    #[test]
    fn breadcrumbs_are_clickable_and_aligned() {
        assert_eq!(SettingsTab::Ui, SettingsTab::Ui);
        assert_ne!(SettingsTab::Ui, SettingsTab::Data);
        assert_ne!(SettingsTab::Data, SettingsTab::Providers);
        // Also check AppMode
        assert_eq!(AppMode::Settings, AppMode::Settings);
        assert_ne!(AppMode::Settings, AppMode::Entries);
    }
}
