//! The Recordings window: every recording in the save folder, to open,
//! rename, show or move to the Trash.

/// What a recording is in use for, which limits what can be done to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    Recording,
    Open,
    Adding,
}

/// How a click picks cards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    Only,
    /// ⌘-click: in or out of the selection.
    Toggle,
    /// ⇧-click: everything from the last card picked.
    Range,
}

/// The cards picked, by index, in order, and the last one clicked (where
/// a ⇧-click range starts).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    cards: Vec<usize>,
    anchor: Option<usize>,
}

impl Selection {
    pub fn cards(&self) -> &[usize] {
        &self.cards
    }

    pub fn contains(&self, card: usize) -> bool {
        self.cards.contains(&card)
    }

    pub fn click(&mut self, card: usize, pick: Pick) {
        match (pick, self.anchor) {
            (Pick::Toggle, _) => {
                match self.cards.iter().position(|&c| c == card) {
                    Some(i) => {
                        self.cards.remove(i);
                    }
                    None => self.cards.push(card),
                }
                self.cards.sort_unstable();
            }
            (Pick::Range, Some(from)) => {
                self.cards = (from.min(card)..=from.max(card)).collect();
                return;
            }
            _ => self.cards = vec![card],
        }
        self.anchor = Some(card);
    }

    /// Moves to the card `by` away from the last one picked (a row is
    /// `by` = ±columns), staying among `count` cards.
    pub fn step(&mut self, by: isize, count: usize) {
        if count == 0 {
            return;
        }
        let card = match self.anchor.or(self.cards.last().copied()) {
            None => 0,
            Some(from) => {
                let to = from as isize + by;
                if to < 0 || to >= count as isize {
                    from.min(count - 1)
                } else {
                    to as usize
                }
            }
        };
        self.click(card, Pick::Only);
    }

    /// Forgets cards past the end of a list of `count`.
    pub fn keep_within(&mut self, count: usize) {
        self.cards.retain(|&c| c < count);
        self.anchor = self.anchor.filter(|&a| a < count);
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// Why the recordings `selected` (name, and what it's in use for) can't
/// go to the Trash, if one can't.
pub fn trash_refusal(selected: &[(&str, Option<Busy>)]) -> Option<String> {
    selected.iter().find_map(|(name, busy)| {
        busy.map(|b| match b {
            Busy::Open => format!("Close “{name}” in the editor first."),
            Busy::Recording => format!("“{name}” is being recorded; stop it first."),
            Busy::Adding => {
                format!("Videos are being added to “{name}”; wait for that to finish.")
            }
        })
    })
}

/// What to ask before moving `names` to the Trash.
pub fn trash_question(names: &[&str]) -> String {
    match names {
        [one] => format!("Move “{one}” to the Trash?"),
        _ => {
            let list: Vec<String> = names.iter().map(|n| format!("“{n}”")).collect();
            format!(
                "Move {} recordings to the Trash? ({})",
                names.len(),
                list.join(", ")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clicks_pick_one_toggle_or_a_range() {
        let mut s = Selection::default();
        s.click(2, Pick::Only);
        assert_eq!(s.cards(), [2]);
        s.click(5, Pick::Toggle);
        assert_eq!(s.cards(), [2, 5]);
        s.click(2, Pick::Toggle);
        assert_eq!(s.cards(), [5]);
        // From the last one clicked, as Finder does.
        s.click(4, Pick::Range);
        assert_eq!(s.cards(), [2, 3, 4]);
        s.click(5, Pick::Toggle);
        s.click(3, Pick::Range);
        assert_eq!(s.cards(), [3, 4, 5]);
        s.click(0, Pick::Only);
        assert_eq!(s.cards(), [0]);
        // Arrows: one along, a row (4 cards) up or down, kept inside.
        s.step(1, 10);
        assert_eq!(s.cards(), [1]);
        s.step(4, 10);
        assert_eq!(s.cards(), [5]);
        s.step(4, 10);
        assert_eq!(s.cards(), [9]);
        s.step(4, 10);
        assert_eq!(s.cards(), [9]);
        s.step(-1, 10);
        assert_eq!(s.cards(), [8]);
        // The list shrank (trashed): what's past its end goes.
        s.click(9, Pick::Toggle);
        s.keep_within(9);
        assert_eq!(s.cards(), [8]);
        let mut empty = Selection::default();
        empty.step(1, 3);
        assert_eq!(empty.cards(), [0]);
    }

    #[test]
    fn busy_recordings_are_not_trashed() {
        assert_eq!(trash_refusal(&[("Intro", None), ("Demo", None)]), None);
        assert_eq!(
            trash_refusal(&[("Intro", None), ("Demo", Some(Busy::Open))]),
            Some("Close “Demo” in the editor first.".to_string())
        );
        assert_eq!(
            trash_refusal(&[("Demo", Some(Busy::Recording))]),
            Some("“Demo” is being recorded; stop it first.".to_string())
        );
        assert_eq!(
            trash_refusal(&[("Demo", Some(Busy::Adding))]),
            Some("Videos are being added to “Demo”; wait for that to finish.".to_string())
        );
        assert_eq!(trash_question(&["Intro"]), "Move “Intro” to the Trash?");
        assert_eq!(
            trash_question(&["Intro", "Demo", "Outro"]),
            "Move 3 recordings to the Trash? (“Intro”, “Demo”, “Outro”)"
        );
    }
}
