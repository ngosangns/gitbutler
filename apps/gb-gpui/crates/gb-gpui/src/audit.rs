//! Static check that clickable controls in this crate carry a tooltip.

#[cfg(test)]
mod tests {
    use std::path::Path;

    const CONSTRUCTORS: &[&str] = &[
        "IconButton::new",
        "Button::new",
        "ns_ui::button(",
        "ns_ui::quiet_button(",
        "ns_ui::quiet_button_md(",
        "ns_ui::danger_button(",
        "ns_ui::warn_button(",
        "ns_ui::outline_button(",
    ];

    #[test]
    fn clickable_controls_have_tooltips() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut hits = Vec::new();
        walk(&root, &mut hits);
        assert!(
            hits.is_empty(),
            "a button or clickable chip is missing a tooltip:\n{}",
            hits.join("\n")
        );
    }

    fn walk(dir: &Path, hits: &mut Vec<String>) {
        let entries = std::fs::read_dir(dir).unwrap_or_else(|error| {
            panic!("{}: {error}", dir.display());
        });
        for entry in entries {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                walk(&path, hits);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            if path.file_name().is_some_and(|name| name == "audit.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            for (index, line) in lines.iter().enumerate() {
                if CONSTRUCTORS.iter().any(|name| line.contains(name)) {
                    let start = index.saturating_sub(15);
                    let end = (index + 35).min(lines.len());
                    let window = lines[start..end].join("\n");
                    if !window.contains(".tooltip") {
                        hits.push(format!(
                            "{}:{}: constructor without a nearby tooltip",
                            path.display(),
                            index + 1
                        ));
                    }
                }
                if line.contains(".on_click") && !line.contains("cx.listener") {
                    // The listener line is the callback, not the control.
                }
                if line.contains(".cursor_pointer") {
                    let start = index.saturating_sub(8);
                    let end = (index + 12).min(lines.len());
                    let window = lines[start..end].join("\n");
                    if window.contains(".on_click") && !window.contains(".tooltip") {
                        hits.push(format!(
                            "{}:{}: clickable row without a tooltip",
                            path.display(),
                            index + 1
                        ));
                    }
                }
            }
        }
    }
}
