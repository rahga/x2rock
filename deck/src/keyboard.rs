//! The on-screen keyboard: which key is where, and where the D-pad goes from
//! each. Ten columns; a key spans one or more, and Up and Down land on the key
//! that covers the same column, so the wide bottom row behaves.

use slint::{ModelRc, VecModel};

use crate::{KeyCap, KeyRow};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyKind {
    Char(char),
    Space,
    Delete,
    Clear,
    Done,
}

#[derive(Clone, Copy, Debug)]
pub struct Key {
    pub kind: KeyKind,
    pub row: usize,
    /// The first of the ten columns it covers, and how many.
    pub col: usize,
    pub span: usize,
}

pub struct Layout {
    pub keys: Vec<Key>,
}

impl Layout {
    pub fn new() -> Self {
        Self::with_third_row("uvwxyz'-&.")
    }

    /// For an address: `/` and `:` where a search has `'` and `&`.
    pub fn for_urls() -> Self {
        Self::with_third_row("uvwxyz/:-.")
    }

    fn with_third_row(third: &str) -> Self {
        let mut keys = Vec::new();
        for (row, chars) in ["abcdefghij", "klmnopqrst", third, "1234567890"]
            .iter()
            .enumerate()
        {
            for (col, c) in chars.chars().enumerate() {
                keys.push(Key {
                    kind: KeyKind::Char(c),
                    row,
                    col,
                    span: 1,
                });
            }
        }
        for (kind, col, span) in [
            (KeyKind::Space, 0, 4),
            (KeyKind::Delete, 4, 2),
            (KeyKind::Clear, 6, 2),
            (KeyKind::Done, 8, 2),
        ] {
            keys.push(Key {
                kind,
                row: 4,
                col,
                span,
            });
        }
        Layout { keys }
    }

    /// Where a D-pad press from key `at` lands; the same key at an edge.
    pub fn step(&self, at: usize, dx: i32, dy: i32) -> usize {
        let Some(key) = self.keys.get(at) else {
            return 0;
        };
        if dx != 0 {
            let row: Vec<usize> = (0..self.keys.len())
                .filter(|&i| self.keys[i].row == key.row)
                .collect();
            let pos = row.iter().position(|&i| i == at).unwrap_or(0) as i32 + dx;
            return row
                .get(pos.clamp(0, row.len() as i32 - 1) as usize)
                .copied()
                .unwrap_or(at);
        }
        let target = key.row as i32 + dy;
        let column = key.col + key.span / 2;
        (0..self.keys.len())
            .find(|&i| {
                let k = &self.keys[i];
                k.row as i32 == target && column >= k.col && column < k.col + k.span
            })
            .unwrap_or(at)
    }

    pub fn rows(&self, done_label: &str) -> ModelRc<KeyRow> {
        let mut rows: Vec<Vec<KeyCap>> = vec![Vec::new(); 5];
        for (i, key) in self.keys.iter().enumerate() {
            let (label, kind) = match key.kind {
                KeyKind::Char(c) => (c.to_string(), 0),
                KeyKind::Space => ("Space".to_owned(), 1),
                KeyKind::Delete => ("Delete".to_owned(), 2),
                KeyKind::Clear => ("Clear".to_owned(), 3),
                KeyKind::Done => (done_label.to_owned(), 4),
            };
            rows[key.row].push(KeyCap {
                label: label.into(),
                span: key.span as i32,
                focus: i as i32,
                kind,
            });
        }
        let rows: Vec<KeyRow> = rows
            .into_iter()
            .map(|keys| KeyRow {
                keys: ModelRc::new(VecModel::from(keys)),
            })
            .collect();
        ModelRc::new(VecModel::from(rows))
    }
}

/// What a key does to the text: `None` for Done, which the caller acts on.
pub fn apply(kind: KeyKind, text: &mut String) -> Option<()> {
    match kind {
        KeyKind::Char(c) => text.push(c),
        KeyKind::Space => text.push(' '),
        KeyKind::Delete => {
            text.pop();
        }
        KeyKind::Clear => text.clear(),
        KeyKind::Done => return None,
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn down_from_a_letter_lands_on_the_wide_key_below_it() {
        let l = Layout::new();
        let d = l
            .keys
            .iter()
            .position(|k| k.kind == KeyKind::Char('1'))
            .unwrap();
        let space = l.step(d, 0, 1);
        assert_eq!(l.keys[space].kind, KeyKind::Space);
        let nine = l
            .keys
            .iter()
            .position(|k| k.kind == KeyKind::Char('9'))
            .unwrap();
        assert_eq!(l.keys[l.step(nine, 0, 1)].kind, KeyKind::Done);
    }

    #[test]
    fn up_from_a_wide_key_lands_on_the_key_over_its_middle() {
        let l = Layout::new();
        let done = l.keys.iter().position(|k| k.kind == KeyKind::Done).unwrap();
        assert_eq!(l.keys[l.step(done, 0, -1)].kind, KeyKind::Char('0'));
    }

    #[test]
    fn the_address_keyboard_has_a_slash_and_a_colon() {
        let l = Layout::for_urls();
        assert!(l.keys.iter().any(|k| k.kind == KeyKind::Char('/')));
        assert!(l.keys.iter().any(|k| k.kind == KeyKind::Char(':')));
        assert_eq!(l.keys.len(), Layout::new().keys.len());
    }

    #[test]
    fn edges_stay_put_and_keys_edit_the_text() {
        let l = Layout::new();
        assert_eq!(l.step(0, -1, 0), 0);
        assert_eq!(l.step(0, 0, -1), 0);
        let mut t = String::from("ab");
        apply(KeyKind::Delete, &mut t);
        apply(KeyKind::Space, &mut t);
        apply(KeyKind::Char('c'), &mut t);
        assert_eq!(t, "a c");
        assert_eq!(apply(KeyKind::Done, &mut t), None);
    }
}
