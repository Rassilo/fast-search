//! Completion of the filters typed in a search: `ext:`, `size:` and
//! `modified:`, then their usual values. Shared by the window and the
//! terminal interface.

/// The filters, each with a meaning and its ready-made values.
const FILTERS: [(&str, &str, &[(&str, &str)]); 3] = [
    (
        "ext:",
        "by extension",
        &[
            ("pdf", "PDF documents"),
            ("jpg;jpeg;png;gif;webp", "pictures"),
            ("mp4;mkv;avi;mov;webm", "videos"),
            ("mp3;flac;ogg;wav;m4a", "music"),
            ("doc;docx;odt;rtf", "text documents"),
            ("xls;xlsx;ods;csv", "spreadsheets"),
            ("zip;tar.gz;7z;rar;xz", "archives"),
            ("txt;md", "plain text"),
        ],
    ),
    (
        "size:",
        "by size",
        &[
            (">1G", "larger than 1 GB"),
            (">100M", "larger than 100 MB"),
            (">10M", "larger than 10 MB"),
            ("<1M", "smaller than 1 MB"),
            ("<1K", "smaller than 1 KB"),
            ("10M..1G", "between 10 MB and 1 GB"),
            ("0", "empty files"),
        ],
    ),
    (
        "modified:",
        "by date modified",
        &[
            ("today", "since midnight"),
            ("yesterday", "yesterday"),
            ("1h", "in the last hour"),
            ("24h", "in the last 24 hours"),
            ("7d", "in the last 7 days"),
            ("30d", "in the last 30 days"),
            ("365d", "in the last 365 days"),
        ],
    ),
];

/// Other spellings of `modified:` that take the same values.
const DATE_ALIASES: [&str; 2] = ["dm:", "datemodified:"];

/// What could complete `word`: the filters whose name starts like it,
/// or, once it names a filter, that filter's values. `today` is
/// (year, month, day), for the date examples.
pub fn suggestions(word: &str, today: (i64, i64, i64)) -> Vec<(String, &'static str)> {
    // A leading "!" (not) is kept in front of whatever is chosen.
    let (not, word) = match word.strip_prefix('!') {
        Some(rest) => ("!", rest),
        None => ("", word),
    };
    let lower = word.to_lowercase();
    let filter = FILTERS.iter().map(|&(name, _, values)| (name, values));
    let aliases = DATE_ALIASES.iter().map(|&name| (name, FILTERS[2].2));
    if let Some((name, values)) = filter.chain(aliases).find(|(name, _)| lower.starts_with(name)) {
        // The name stays as it was typed ("dm:" stays "dm:").
        let (typed_name, typed) = word.split_at(name.len());
        let typed = typed.to_lowercase();
        let mut out: Vec<(String, &'static str)> =
            values.iter().filter(|(value, _)| value.starts_with(&typed) && *value != typed).map(|&(value, means)| (format!("{not}{typed_name}{value}"), means)).collect();
        if name != "ext:" && name != "size:" {
            let (y, m, d) = today;
            let dates = [(format!("{y:04}-{m:02}-{d:02}"), "on that day"), (format!("{y:04}-01-01.."), "since a date"), (format!("..{y:04}-01-01"), "before a date")];
            out.extend(dates.into_iter().filter(|(value, _)| value.starts_with(&typed) && *value != typed).map(|(value, means)| (format!("{not}{typed_name}{value}"), means)));
        }
        return out;
    }
    FILTERS.iter().filter(|(name, ..)| name.starts_with(&lower)).map(|&(name, means, _)| (format!("{not}{name}"), means)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(word: &str) -> Vec<String> {
        suggestions(word, (2026, 11, 17)).into_iter().map(|(text, _)| text).collect()
    }

    #[test]
    fn filters_then_their_values() {
        assert_eq!(texts(""), ["ext:", "size:", "modified:"]);
        assert_eq!(texts("s"), ["size:"]);
        assert_eq!(texts("Mod"), ["modified:"]);
        assert!(texts("report").is_empty());
        assert_eq!(texts("size:>1"), ["size:>1G", "size:>100M", "size:>10M"]);
        assert_eq!(texts("size:0"), Vec::<String>::new()); // already complete
        assert_eq!(texts("ext:p"), ["ext:pdf"]);
        // Dates: the short name is kept, and today's date is offered.
        assert_eq!(texts("dm:t"), ["dm:today"]);
        assert_eq!(texts("modified:2"), ["modified:24h", "modified:2026-11-17", "modified:2026-01-01.."]);
        // "not" stays in front.
        assert_eq!(texts("!ext:pd"), ["!ext:pdf"]);
        assert_eq!(texts("!m"), ["!modified:"]);
    }
}
