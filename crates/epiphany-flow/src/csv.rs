//! A small, dependency-free CSV reader for the flat-file flow data source.
//!
//! Handles the common RFC 4180 shape: a header row names the columns, each later
//! row becomes a record keyed by column name. Quoted fields may contain commas,
//! newlines, and doubled quotes (`""`). This is deliberately minimal (no type
//! inference, no streaming): a flow's `ctx.input()` returns these records and the
//! flow's JavaScript does the rest.

/// One parsed record: column name to field value, in column order.
pub type Row = Vec<(String, String)>;

/// A CSV parse failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvError {
    /// What went wrong.
    pub message: String,
    /// 1-based line where the problem was detected.
    pub line: usize,
}

impl std::fmt::Display for CsvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (line {})", self.message, self.line)
    }
}

impl std::error::Error for CsvError {}

/// Parse CSV `text` into records keyed by the header row's column names. An empty
/// input (or header-only input) yields no records.
pub fn parse_csv(text: &str) -> Result<Vec<Row>, CsvError> {
    let records = split_records(text)?;
    let mut iter = records.into_iter();
    let header = match iter.next() {
        Some(h) => h,
        None => return Ok(Vec::new()),
    };
    // A trailing newline produces a final empty record; drop empty records.
    let mut rows = Vec::new();
    for fields in iter {
        if fields.len() == 1 && fields[0].is_empty() {
            continue;
        }
        let mut row: Row = Vec::with_capacity(header.len());
        for (i, name) in header.iter().enumerate() {
            let value = fields.get(i).cloned().unwrap_or_default();
            row.push((name.clone(), value));
        }
        rows.push(row);
    }
    Ok(rows)
}

/// Split CSV text into records (each a vector of fields), honoring quoting.
fn split_records(text: &str) -> Result<Vec<Vec<String>>, CsvError> {
    let chars: Vec<char> = text.chars().collect();
    let mut records = Vec::new();
    let mut record: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut i = 0;
    let mut line = 1usize;
    let mut started = false; // whether the current record has any content

    while i < chars.len() {
        let c = chars[i];
        if c == '"' {
            started = true;
            i += 1;
            // Quoted field: copy until the closing quote, doubling `""` to `"`.
            loop {
                match chars.get(i) {
                    None => {
                        return Err(CsvError {
                            message: "unterminated quoted field".to_string(),
                            line,
                        })
                    }
                    Some('"') if chars.get(i + 1) == Some(&'"') => {
                        field.push('"');
                        i += 2;
                    }
                    Some('"') => {
                        i += 1;
                        break;
                    }
                    Some(&ch) => {
                        if ch == '\n' {
                            line += 1;
                        }
                        field.push(ch);
                        i += 1;
                    }
                }
            }
        } else if c == ',' {
            started = true;
            record.push(std::mem::take(&mut field));
            i += 1;
        } else if c == '\r' {
            i += 1; // fold CRLF
        } else if c == '\n' {
            line += 1;
            record.push(std::mem::take(&mut field));
            records.push(std::mem::take(&mut record));
            started = false;
            i += 1;
        } else {
            started = true;
            field.push(c);
            i += 1;
        }
    }
    // A final field/record without a trailing newline.
    if started || !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_csv() {
        let rows = parse_csv("Region,Value\nNorth,100\nSouth,200\n").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            vec![
                ("Region".into(), "North".into()),
                ("Value".into(), "100".into())
            ]
        );
        assert_eq!(rows[1][1], ("Value".to_string(), "200".to_string()));
    }

    #[test]
    fn handles_no_trailing_newline() {
        let rows = parse_csv("A,B\n1,2").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0],
            vec![("A".into(), "1".into()), ("B".into(), "2".into())]
        );
    }

    #[test]
    fn handles_quoted_fields() {
        let rows = parse_csv("Name,Note\n\"Smith, Jr.\",\"says \"\"hi\"\"\"\n").unwrap();
        assert_eq!(rows[0][0].1, "Smith, Jr.");
        assert_eq!(rows[0][1].1, "says \"hi\"");
    }

    #[test]
    fn quoted_newline_is_one_field() {
        let rows = parse_csv("A\n\"line1\nline2\"\n").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].1, "line1\nline2");
    }

    #[test]
    fn header_only_and_empty() {
        assert_eq!(parse_csv("A,B,C\n").unwrap().len(), 0);
        assert_eq!(parse_csv("").unwrap().len(), 0);
    }

    #[test]
    fn missing_trailing_columns_default_empty() {
        let rows = parse_csv("A,B,C\n1,2\n").unwrap();
        assert_eq!(rows[0][2], ("C".to_string(), String::new()));
    }

    #[test]
    fn unterminated_quote_errors() {
        assert!(parse_csv("A\n\"oops\n").is_err());
    }
}
