//! markdownify's table rules: `colspan`, and the `<tr>` converter that
//! synthesises the pipe rules around a row.
//!
//! The interesting rule is the **empty header row**. GitHub-flavoured Markdown
//! has no way to express a table without a header, so when the first row is not
//! a header row markdownify emits an empty one plus a separator above it:
//!
//! ```text
//! |  |  |
//! | --- | --- |
//! | a | b |
//! ```
//!
//! # `<tbody>` and the parser
//!
//! markdownify parses with Python's `html.parser`, which never synthesises a
//! `<tbody>`; `html5ever` always does. Both branches of `convert_tr` reach the
//! same conclusion for the common shapes (`table > tr …` and
//! `table > tbody > tr …` produce identical Markdown), because a first row in a
//! `<thead>`-less `<tbody>` is "head row missing" exactly like a first row
//! directly under `<table>`. See `ARCHITECTURE.md` §11.6.

use browser_dom::NodeRef;

use crate::traversal::Walker;

/// markdownify's colspan parsing: a digit string, clamped to `1..=1000`.
#[must_use]
pub fn colspan(el: NodeRef<'_>) -> usize {
    let Some(value) = el.attr("colspan") else {
        return 1;
    };
    if value.is_empty() || !value.chars().all(|c| c.is_ascii_digit()) {
        return 1;
    }
    value.parse::<usize>().unwrap_or(1000).clamp(1, 1000)
}

impl Walker<'_> {
    /// markdownify's `convert_tr`.
    pub(crate) fn convert_tr(&self, el: NodeRef<'_>, text: &str) -> String {
        let cells: Vec<NodeRef<'_>> = el
            .descendant_elements()
            .filter(|node| matches!(node.local_name(), Some("td" | "th")))
            .collect();
        let is_first_row = self.prev_element_sibling_of(el).is_none();
        let parent = self.parent_of(el);
        let parent_name = parent.and_then(|parent| parent.local_name());

        let all_th = cells.iter().all(|cell| cell.local_name() == Some("th"));
        let single_tr_thead = parent_name == Some("thead")
            && parent.is_some_and(|parent| {
                parent
                    .descendant_elements()
                    .filter(|node| node.local_name() == Some("tr"))
                    .count()
                    == 1
            });
        // `all([])` is True in Python: a `<tr>` with no cells counts as a head row.
        let is_headrow = all_th || single_tr_thead;

        let head_row_missing_in_tbody = is_first_row
            && parent_name == Some("tbody")
            && parent.is_some_and(|parent| self.count_theads_around(parent) < 1);
        let is_head_row_missing =
            (is_first_row && parent_name != Some("tbody")) || head_row_missing_in_tbody;

        let full_colspan: usize = cells.iter().copied().map(colspan).sum();
        let infer = self.options().table_infer_header;

        let mut overline = String::new();
        let mut underline = String::new();
        if (is_headrow || (is_head_row_missing && infer)) && is_first_row {
            underline.push_str(&separator_row(full_colspan));
        } else if (is_head_row_missing && !infer)
            || (is_first_row
                && (parent_name == Some("table")
                    || (parent_name == Some("tbody")
                        && parent
                            .and_then(|parent| self.prev_element_sibling_of(parent))
                            .is_none())))
        {
            overline.push_str(&empty_row(full_colspan));
            overline.push_str(&separator_row(full_colspan));
        }
        format!("{overline}|{text}\n{underline}")
    }

    /// Python's `el.parent.parent.find_all(['thead'])`, where `el.parent` is the
    /// `<tbody>`.
    ///
    /// When the `<tbody>` is itself the conversion root its Python "parent" is
    /// the synthetic `[document]`, whose descendants are the `<tbody>` and
    /// everything under it — hence the second branch.
    fn count_theads_around(&self, tbody: NodeRef<'_>) -> usize {
        let is_thead = |node: &NodeRef<'_>| node.local_name() == Some("thead");
        match self.parent_of(tbody) {
            Some(grandparent) => grandparent.descendant_elements().filter(is_thead).count(),
            None => std::iter::once(tbody)
                .chain(tbody.descendant_elements())
                .filter(is_thead)
                .count(),
        }
    }
}

/// `| --- | --- |` for `columns` columns.
fn separator_row(columns: usize) -> String {
    let mut out = String::from("| ");
    for index in 0..columns {
        if index > 0 {
            out.push_str(" | ");
        }
        out.push_str("---");
    }
    out.push_str(" |\n");
    out
}

/// `|  |  |` for `columns` columns.
fn empty_row(columns: usize) -> String {
    let mut out = String::from("| ");
    for index in 0..columns {
        if index > 0 {
            out.push_str(" | ");
        }
    }
    out.push_str(" |\n");
    out
}

#[cfg(test)]
mod tests {
    use crate::convert::html_to_markdown as md;

    #[test]
    fn header_row_of_th_cells() {
        assert_eq!(
            md("<table><tr><th>a</th><th>b</th></tr><tr><td>1</td><td>2</td></tr></table>"),
            "| a | b |\n| --- | --- |\n| 1 | 2 |"
        );
    }

    #[test]
    fn empty_header_row_is_synthesised() {
        assert_eq!(
            md("<table><tr><td>a</td><td>b</td></tr></table>"),
            "|  |  |\n| --- | --- |\n| a | b |"
        );
    }

    #[test]
    fn explicit_tbody_matches_implicit_one() {
        assert_eq!(
            md("<table><tbody><tr><td>a</td><td>b</td></tr></tbody></table>"),
            md("<table><tr><td>a</td><td>b</td></tr></table>")
        );
    }

    #[test]
    fn thead_with_one_row_is_a_header() {
        assert_eq!(
            md("<table><thead><tr><td>a</td></tr></thead><tbody><tr><td>1</td></tr></tbody></table>"),
            "| a |\n| --- |\n| 1 |"
        );
    }

    #[test]
    fn colspan_widens_the_rules() {
        assert_eq!(
            md("<table><tr><td colspan=\"2\">a</td></tr><tr><td>1</td><td>2</td></tr></table>"),
            "|  |  |\n| --- | --- |\n| a | |\n| 1 | 2 |"
        );
    }
}
