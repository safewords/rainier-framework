//! Talking to whoever is running the command — [`table`], [`ask`],
//! [`secret`], [`confirm`], [`confirm_by_typing`].
//!
//! Every non-trivial command re-implements these, and the hand-rolled
//! versions get the same three things wrong:
//!
//! - **Column widths counted in bytes.** `"café"` is five bytes and four
//!   characters, so a byte-padded table goes crooked the first time a name has
//!   an accent in it — and stays crooked for the one row you were reading.
//! - **A prompt with nowhere to read from.** Under `cron`, in CI, behind a
//!   pipe, stdin is closed. `read_line` returns `Ok(0)` forever, and a loop
//!   that re-asks spins until something kills it.
//! - **A password echoed to the terminal**, into the scrollback and often into
//!   the CI log.
//!
//! # Prompting is interactive by definition
//!
//! Everything here that asks a question returns an error at end-of-input
//! rather than a default. A command that must also run unattended should take
//! the answer as an argument and only prompt when it is missing — which is
//! what [`confirm_by_typing`] and `--force` are for.

use std::io::{self, BufRead, IsTerminal, Write};

use rainier_support::{Error, Result};

/// Render `rows` under `headers`, aligned.
///
/// ```
/// use rainier_console::io;
///
/// let rendered = io::table_to_string(
///     &["Name", "Queue"],
///     &[vec!["café".into(), "default".into()], vec!["résumé".into(), "mail".into()]],
/// );
///
/// // Padded by character, so the accented names line up with everything else.
/// assert_eq!(rendered.lines().next().unwrap(), "+--------+---------+");
/// ```
pub fn table(headers: &[&str], rows: &[Vec<String>]) {
    print!("{}", table_to_string(headers, rows));
}

/// [`table`], as a string. For a test, or for writing somewhere else.
///
/// Widths are counted in **characters**, not bytes. Wide CJK glyphs and emoji
/// still overhang — measuring those properly means a Unicode width table, and
/// this crate does not carry one — but the common case of an accent no longer
/// bends the column.
pub fn table_to_string(headers: &[&str], rows: &[Vec<String>]) -> String {
    let columns = headers.len().max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if columns == 0 {
        return String::new();
    }

    let mut widths = vec![0usize; columns];
    for (index, header) in headers.iter().enumerate() {
        widths[index] = width(header);
    }
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(width(cell));
        }
    }

    let rule = {
        let mut rule = String::from("+");
        for width in &widths {
            rule.push_str(&"-".repeat(width + 2));
            rule.push('+');
        }
        rule.push('\n');
        rule
    };

    let mut out = String::new();
    out.push_str(&rule);
    if !headers.is_empty() {
        out.push_str(&render_row(headers.iter().map(|h| h.to_string()), &widths));
        out.push_str(&rule);
    }
    for row in rows {
        out.push_str(&render_row(row.iter().cloned(), &widths));
    }
    if !rows.is_empty() {
        out.push_str(&rule);
    }
    out
}

/// Ask a question and read a line.
///
/// The answer is trimmed. An empty line is an empty answer, not an error —
/// use [`ask_with_default`] when blank should mean something.
///
/// # Errors
///
/// At end of input: stdin is closed, so there is no answer coming and asking
/// again would spin.
pub fn ask(question: &str) -> Result<String> {
    prompt(&format!("{question} "))?;
    read_line()
}

/// Ask, and take an empty answer as `default`.
///
/// ```text
/// Queue name [default]:
/// ```
///
/// # Errors
///
/// At end of input — see [`ask`]. A default is what an empty *answer* means,
/// not what a closed stdin means: nobody read the question.
pub fn ask_with_default(question: &str, default: &str) -> Result<String> {
    prompt(&format!("{question} [{default}]: "))?;

    let answer = read_line()?;
    Ok(if answer.is_empty() { default.to_string() } else { answer })
}

/// Ask for something that should not be echoed.
///
/// # Errors
///
/// At end of input, or if the terminal will not turn echo off — in which case
/// this refuses rather than reading the secret in the clear. A password in the
/// scrollback is a password in the CI log.
pub fn secret(question: &str) -> Result<String> {
    prompt(&format!("{question} "))?;

    let secret = rpassword::read_password().map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => closed_stdin(),
        _ => Error::internal(format!("could not read without echoing: {e}")),
    })?;

    // `read_password` consumes the newline but does not print one, so without
    // this the next line of output lands on the prompt.
    println!();
    Ok(secret.trim().to_string())
}

/// Ask a yes/no question. An empty answer takes `default`.
///
/// Accepts `y`, `yes`, `n`, `no`, and re-asks anything else — because a
/// question answered "sure" is a question that has not been answered.
///
/// # Errors
///
/// At end of input.
pub fn confirm(question: &str, default: bool) -> Result<bool> {
    let hint = if default { "Y/n" } else { "y/N" };

    loop {
        prompt(&format!("{question} [{hint}]: "))?;

        match read_line()?.to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("Please answer `y` or `n`."),
        }
    }
}

/// Require the exact phrase to be typed before going ahead.
///
/// The pattern behind GitHub's "type the repository name to delete it".
/// For the operations where a stray `y` is expensive: dropping
/// a database, purging a queue, rewriting a table.
///
/// ```ignore
/// if !io::confirm_by_typing("This will drop every table.", "production")? {
///     return Ok(exit::FAILURE);
/// }
/// ```
///
/// Returns `false` when what was typed does not match — one attempt, no
/// retry loop. Someone who typed the wrong thing gets to think about it.
///
/// # Errors
///
/// When there is no terminal. A destructive confirmation cannot be satisfied
/// by a closed stdin, and must not be satisfiable by an empty pipe — a command
/// that needs to run unattended should take `--force` and check it before
/// getting here.
pub fn confirm_by_typing(warning: &str, phrase: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        return Err(Error::internal(format!(
            "this needs `{phrase}` typed at a terminal to confirm, and there is not one \
             — pass --force if the command should run unattended"
        )));
    }

    println!("{warning}");
    prompt(&format!("Type `{phrase}` to continue: "))?;

    let typed = read_line()?;
    if typed == phrase {
        Ok(true)
    } else {
        println!("That did not match. Nothing was done.");
        Ok(false)
    }
}

/// Whether anyone is actually watching.
///
/// For deciding whether to prompt at all, or whether a progress display is
/// worth printing. `false` under `cron`, in CI, and behind a pipe.
pub fn is_interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

// --- the bits underneath ---------------------------------------------------

/// Width in characters. Bytes would bend a column the first time a name has an
/// accent in it.
fn width(text: &str) -> usize {
    text.chars().count()
}

fn render_row(cells: impl Iterator<Item = String>, widths: &[usize]) -> String {
    let mut out = String::from("|");
    let mut cells = cells.fuse();

    for width_of in widths {
        let cell = cells.next().unwrap_or_default();
        let padding = width_of.saturating_sub(width(&cell));
        out.push(' ');
        out.push_str(&cell);
        out.push_str(&" ".repeat(padding));
        out.push_str(" |");
    }
    out.push('\n');
    out
}

/// Print without a newline, and make sure it is on screen before we block on
/// input — an unflushed prompt looks like a hang.
fn prompt(text: &str) -> Result<()> {
    print!("{text}");
    io::stdout().flush().map_err(|e| Error::internal(format!("could not write the prompt: {e}")))
}

fn read_line() -> Result<String> {
    let mut line = String::new();

    match io::stdin().lock().read_line(&mut line) {
        Ok(0) => Err(closed_stdin()),
        Ok(_) => Ok(line.trim().to_string()),
        Err(e) => Err(Error::internal(format!("could not read the answer: {e}"))),
    }
}

fn closed_stdin() -> Error {
    Error::internal(
        "there is nothing to read from — stdin is closed, so the question cannot be answered \
         (a command that runs unattended should take the answer as an argument)",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_lines_up() {
        let rendered = table_to_string(
            &["Name", "Queue"],
            &[vec!["digest".into(), "default".into()], vec!["backup".into(), "maintenance".into()]],
        );

        assert_eq!(
            rendered,
            "\
+--------+-------------+
| Name   | Queue       |
+--------+-------------+
| digest | default     |
| backup | maintenance |
+--------+-------------+
"
        );
    }

    #[test]
    fn columns_are_padded_by_character_not_byte() {
        // The whole point. "café" is 5 bytes and 4 characters; padding by
        // bytes puts one space too few after it and bends the column.
        let rendered = table_to_string(&["Name"], &[vec!["café".into()], vec!["abcd".into()]]);

        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines[3].chars().count(), lines[4].chars().count());
        assert_eq!(lines[3], "| café |");
        assert_eq!(lines[4], "| abcd |");
    }

    #[test]
    fn a_short_row_is_padded_out() {
        // A row with fewer cells than headers is a bug in the caller, but a
        // panic in the middle of printing a report helps nobody.
        let rendered = table_to_string(&["A", "B"], &[vec!["only".into()]]);

        assert_eq!(rendered.lines().nth(3).unwrap(), "| only |   |");
    }

    #[test]
    fn a_long_row_widens_the_table_rather_than_being_cut() {
        let rendered = table_to_string(&["A"], &[vec!["one".into(), "two".into()]]);

        assert_eq!(rendered.lines().next().unwrap(), "+-----+-----+");
        assert_eq!(rendered.lines().nth(3).unwrap(), "| one | two |");
    }

    #[test]
    fn an_empty_table_is_empty_rather_than_a_lone_rule() {
        assert_eq!(table_to_string(&[], &[]), "");
    }

    #[test]
    fn headers_with_no_rows_still_render() {
        // `queue:failed` with nothing failed should print the header and stop,
        // not print nothing and look broken.
        assert_eq!(
            table_to_string(&["Id", "Queue"], &[]),
            "\
+----+-------+
| Id | Queue |
+----+-------+
"
        );
    }

    #[test]
    fn a_cell_wider_than_its_header_sets_the_width() {
        let rendered = table_to_string(&["Id"], &[vec!["01HXQ".into()]]);

        assert_eq!(rendered.lines().next().unwrap(), "+-------+");
        assert_eq!(rendered.lines().nth(1).unwrap(), "| Id    |");
    }
}
