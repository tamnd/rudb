//! Printing. A run of this takes long enough that the output is the only thing anybody sees, so it
//! is worth the fifty lines to make it a table that can be read in a terminal and pasted into the
//! report without editing.

/// A byte count as a human reads it. Powers of 1024, because these are memory and file sizes and
/// that is what the tools they will be checked against use.
pub fn bytes(count: usize) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = count as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{count} B") } else { format!("{size:.2} {}", UNITS[unit]) }
}

/// A count with thousands separators.
pub fn count(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

pub fn ratio(part: usize, whole: usize) -> String {
    if whole == 0 {
        return "-".to_string();
    }
    format!("{:.2}", part as f64 / whole as f64)
}

/// A table of strings that knows its own column widths.
#[derive(Debug)]
pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    pub fn new(headers: &[&str]) -> Self {
        Self { headers: headers.iter().map(|head| (*head).to_string()).collect(), rows: Vec::new() }
    }

    pub fn row(&mut self, cells: &[String]) {
        self.rows.push(cells.to_vec());
    }

    pub fn print(&self, markdown: bool) {
        let mut widths: Vec<usize> = self.headers.iter().map(|head| head.len()).collect();
        for row in &self.rows {
            for (index, cell) in row.iter().enumerate() {
                if index < widths.len() {
                    widths[index] = widths[index].max(cell.len());
                }
            }
        }
        let last = widths.len() - 1;
        let line = |cells: &[String]| {
            let mut out = String::new();
            if markdown {
                out.push_str("| ");
            }
            for (index, cell) in cells.iter().enumerate() {
                // The first column is names and the last is free text, both read better ragged
                // right. Everything between is a number and reads better ragged left.
                if index == 0 || index == last {
                    out.push_str(&format!("{:<width$}", cell, width = widths[index]));
                } else {
                    out.push_str(&format!("{:>width$}", cell, width = widths[index]));
                }
                if index < last {
                    out.push_str(if markdown { " | " } else { "  " });
                }
            }
            if markdown {
                out.push_str(" |");
            }
            out.trim_end().to_string()
        };
        let rule: Vec<String> = widths.iter().map(|width| "-".repeat(*width)).collect();
        println!("{}", line(&self.headers));
        println!("{}", line(&rule));
        for row in &self.rows {
            println!("{}", line(row));
        }
    }
}
