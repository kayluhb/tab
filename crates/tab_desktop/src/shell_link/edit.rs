//! The shell's edit buffer as the desktop last saw it, and how popup inserts
//! change it. The shell owns the buffer; the desktop sends back a whole new
//! buffer tagged with the `seq` it was computed from, and the shell drops it if
//! the user typed in between.

/// An insert from the popup, in the shape of `FigtermCommand::InsertText`
#[derive(Debug, Clone, Default)]
pub struct Insert {
    pub insertion: Option<String>,
    pub deletion: Option<i64>,
    pub offset: Option<i64>,
    pub immediate: Option<bool>,
    pub insertion_buffer: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Line {
    pub seq: u64,
    pub text: String,
    /// In characters
    pub cursor: usize,
}

impl Line {
    pub fn new(seq: u64, text: String, cursor: usize) -> Self {
        let cursor = cursor.min(text.chars().count());
        Self { seq, text, cursor }
    }

    /// The cursor as a byte offset, which is what the popup expects
    pub fn cursor_bytes(&self) -> usize {
        char_to_byte(&self.text, self.cursor)
    }

    /// Applies an insert in the order the PTY typed it: catch up with
    /// `insertion_buffer`, move by `offset`, delete, insert. Returns the new
    /// line, at the same `seq`, and whether to run it.
    pub fn apply(&self, insert: &Insert) -> (Line, bool) {
        let mut line = self.clone();

        if let Some(expected) = &insert.insertion_buffer {
            if line.text != *expected
                && (line.text.starts_with(expected.as_str()) || expected.starts_with(line.text.as_str()))
            {
                // The popup computed the insert against `expected`; edit from there
                line.text.clone_from(expected);
                line.cursor = line.text.chars().count();
            }
        }

        if let Some(offset) = insert.offset {
            let len = line.text.chars().count() as i64;
            line.cursor = (line.cursor as i64 + offset).clamp(0, len) as usize;
        }

        for _ in 0..insert.deletion.unwrap_or(0).clamp(0, line.cursor as i64) {
            line.cursor -= 1;
            let at = char_to_byte(&line.text, line.cursor);
            line.text.remove(at);
        }

        if let Some(insertion) = &insert.insertion {
            let at = char_to_byte(&line.text, line.cursor);
            line.text.insert_str(at, insertion);
            line.cursor += insertion.chars().count();
        }

        (line, insert.immediate == Some(true))
    }
}

fn char_to_byte(text: &str, chars: usize) -> usize {
    text.char_indices().nth(chars).map_or(text.len(), |(at, _)| at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, cursor: usize) -> Line {
        Line::new(3, text.into(), cursor)
    }

    fn insert(insertion: &str) -> Insert {
        Insert {
            insertion: Some(insertion.into()),
            ..Default::default()
        }
    }

    #[test]
    fn appends_at_cursor() {
        let (new, execute) = line("git ch", 6).apply(&insert("eckout"));
        assert_eq!(new, Line::new(3, "git checkout".into(), 12));
        assert!(!execute);
    }

    #[test]
    fn deletes_then_inserts() {
        let (new, _) = line("git chx", 7).apply(&Insert {
            deletion: Some(1),
            ..insert("eckout")
        });
        assert_eq!(new.text, "git checkout");
    }

    #[test]
    fn moves_by_offset_first() {
        let (new, _) = line("ab", 2).apply(&Insert {
            offset: Some(-1),
            ..insert("X")
        });
        assert_eq!((new.text.as_str(), new.cursor), ("aXb", 2));
    }

    #[test]
    fn catches_up_with_insertion_buffer() {
        let (new, _) = line("git c", 5).apply(&Insert {
            insertion_buffer: Some("git ch".into()),
            ..insert("eckout")
        });
        assert_eq!(new.text, "git checkout");

        let (new, _) = line("git che", 7).apply(&Insert {
            insertion_buffer: Some("git ch".into()),
            ..insert("eckout")
        });
        assert_eq!(new.text, "git checkout");
    }

    #[test]
    fn ignores_an_unrelated_insertion_buffer() {
        let (new, _) = line("ls", 2).apply(&Insert {
            insertion_buffer: Some("git".into()),
            ..insert(" -la")
        });
        assert_eq!(new.text, "ls -la");
    }

    #[test]
    fn handles_multibyte_text() {
        let (new, _) = line("echo é", 6).apply(&Insert {
            deletion: Some(1),
            ..insert("ü✓")
        });
        assert_eq!((new.text.as_str(), new.cursor), ("echo ü✓", 7));
        assert_eq!(new.cursor_bytes(), "echo ü✓".len());
    }

    #[test]
    fn reports_immediate() {
        let (_, execute) = line("ls", 2).apply(&Insert {
            immediate: Some(true),
            ..Default::default()
        });
        assert!(execute);
    }

    #[test]
    fn deletion_stops_at_start() {
        let (new, _) = line("ab", 1).apply(&Insert {
            deletion: Some(10),
            ..Default::default()
        });
        assert_eq!((new.text.as_str(), new.cursor), ("b", 0));
    }

    #[test]
    fn clamps_cursor_to_text() {
        assert_eq!(Line::new(0, "ab".into(), 9).cursor, 2);
    }
}
